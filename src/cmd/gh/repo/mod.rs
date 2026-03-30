//! Repository-level GitHub commands. Shared helpers for repo resolution
//! live here; individual commands are in submodules.

pub mod apply_preferred_settings;
pub mod main_protect;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use std::path::Path;
use tracing::info;

#[derive(Args)]
pub struct ApplyPreferredSettingsArgs {
    /// Repository name (e.g., owner/repo). Detected from current directory if omitted.
    pub repo: Option<String>,

    /// Apply to all non-fork, non-archived repositories (with confirmation)
    #[arg(long)]
    pub all: bool,

    /// Force update even if settings already match
    #[arg(short, long)]
    pub force: bool,

    /// Show settings changes without applying them
    #[arg(long)]
    pub dry_run: bool,

    /// Skip confirmation prompts (useful with --all)
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct MainProtectArgs {
    /// Repository name (e.g., owner/repo). Detected from current directory if omitted.
    pub repo: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Apply preferred merge settings to a repository
    ///
    /// Configures squash-merge-only with PR title/body, disables merge commits
    /// and rebase merges, and enables auto-delete of head branches. Use --all
    /// to apply across all non-fork, non-archived repositories.
    ApplyPreferredSettings(ApplyPreferredSettingsArgs),
    /// Create/update a "main-protect" ruleset on the default branch
    ///
    /// Ensures a ruleset named "main-protect" exists with required linear history
    /// and force-push blocking, then lets you interactively pick which CI status
    /// checks should be required to pass before merging.
    MainProtect(MainProtectArgs),
}

/// Determine the `owner/repo` to operate on. If the user passed one
/// explicitly, use it; otherwise auto-detect by parsing the `origin`
/// remote URL from the git config in `dir`.
pub fn resolve_repo(repo_arg: Option<&str>, dir: &Path) -> anyhow::Result<String> {
    match repo_arg {
        Some(r) => Ok(r.to_string()),
        None => {
            let git_dir = dir.join(".git");
            if !git_dir.exists() {
                bail!("not in a git repository");
            }
            let config_path = git_dir.join("config");
            let config = std::fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            let url = parse_origin_url(&config)?;
            let repo = parse_github_remote(&url)?;
            info!("Detected repository: {}", repo);
            Ok(repo)
        }
    }
}

/// Extract the `url` value from the `[remote "origin"]` section of a
/// `.git/config` file. We do minimal INI-style parsing rather than
/// shelling out to `git config` so this works without a git binary.
fn parse_origin_url(config: &str) -> anyhow::Result<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == "[remote \"origin\"]";
            continue;
        }
        if in_origin && let Some(url) = trimmed.strip_prefix("url = ") {
            return Ok(url.to_string());
        }
    }
    bail!("no origin remote found in git config");
}

/// Turn a GitHub remote URL into `owner/repo`. Supports SSH, HTTPS,
/// and `ssh://` URL forms, with or without a `.git` suffix.
fn parse_github_remote(url: &str) -> anyhow::Result<String> {
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .with_context(|| format!("not a GitHub remote: {}", url))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.matches('/').count() != 1 || path.is_empty() {
        bail!("unexpected GitHub remote format: {}", url);
    }
    Ok(path.to_string())
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::ApplyPreferredSettings(args) => apply_preferred_settings::run(args),
            Commands::MainProtect(args) => main_protect::run(args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_fake_repo(dir: &Path, remote_url: &str) {
        let git_dir = dir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            format!("[remote \"origin\"]\n\turl = {}\n", remote_url),
        )
        .unwrap();
    }

    /// When a repo is explicitly provided, use it as-is without auto-detection.
    #[test]
    fn resolve_repo_returns_explicit_arg() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            resolve_repo(Some("owner/repo"), tmp.path()).unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn resolve_repo_auto_detects_from_remote() {
        let tmp = TempDir::new().unwrap();
        init_fake_repo(tmp.path(), "git@github.com:testowner/testrepo.git");
        assert_eq!(
            resolve_repo(None, tmp.path()).unwrap(),
            "testowner/testrepo"
        );
    }

    #[test]
    fn resolve_repo_fails_outside_git_repo() {
        let tmp = TempDir::new().unwrap();
        assert!(!tmp.path().join(".git").exists());
        let err = resolve_repo(None, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("not in a git repository"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn resolve_repo_fails_without_remote() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), "").unwrap();
        let err = resolve_repo(None, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("no origin remote"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_github_remote_ssh() {
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo.git").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_https() {
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_no_dot_git_suffix() {
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_ssh_url_form() {
        assert_eq!(
            parse_github_remote("ssh://git@github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_rejects_non_github() {
        assert!(parse_github_remote("git@gitlab.com:owner/repo.git").is_err());
    }
}
