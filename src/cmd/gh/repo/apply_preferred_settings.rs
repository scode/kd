//! Enforce a consistent set of merge/branch settings across GitHub repos.
//!
//! The "preferred" configuration is squash-merge-only using the PR title and
//! body, merge commits and rebase disabled, and head branches auto-deleted
//! after merge. These choices keep the commit history linear and tidy while
//! preserving PR context in each squashed commit message.

use super::{ApplyPreferredSettingsArgs, resolve_repo};
use serde::Deserialize;
use std::io::{self, Write};
use tracing::info;
use xshell::{Shell, cmd};

/// Subset of `gh repo list --json` output used to filter repos for batch mode.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoListEntry {
    name_with_owner: String,
    is_fork: bool,
    is_archived: bool,
}

/// The merge/branch settings we care about, as returned by `gh api repos/{repo}`.
#[derive(Deserialize)]
struct RepoSettings {
    allow_merge_commit: bool,
    allow_squash_merge: bool,
    squash_merge_commit_title: String,
    squash_merge_commit_message: String,
    allow_rebase_merge: bool,
    delete_branch_on_merge: bool,
}

impl RepoSettings {
    /// Compare the current settings against the preferred values and return
    /// human-readable descriptions of each difference. An empty result means
    /// the repo already matches.
    fn deltas(&self) -> Vec<String> {
        let mut deltas = Vec::new();
        if self.allow_merge_commit {
            deltas.push("allow_merge_commit: true -> false".to_string());
        }
        if !self.allow_squash_merge {
            deltas.push("allow_squash_merge: false -> true".to_string());
        }
        if self.squash_merge_commit_title != "PR_TITLE" {
            deltas.push(format!(
                "squash_merge_commit_title: {} -> PR_TITLE",
                self.squash_merge_commit_title
            ));
        }
        if self.squash_merge_commit_message != "PR_BODY" {
            deltas.push(format!(
                "squash_merge_commit_message: {} -> PR_BODY",
                self.squash_merge_commit_message
            ));
        }
        if self.allow_rebase_merge {
            deltas.push("allow_rebase_merge: true -> false".to_string());
        }
        if !self.delete_branch_on_merge {
            deltas.push("delete_branch_on_merge: false -> true".to_string());
        }
        deltas
    }
}

pub fn run(args: ApplyPreferredSettingsArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;

    if args.all {
        run_all(&sh, args.force, args.dry_run, args.yes)
    } else {
        let repo = resolve_repo(args.repo.as_deref(), &std::env::current_dir()?)?;
        check_and_apply(&sh, &repo, args.force, false, args.dry_run, args.yes)
    }
}

/// Fetch the repo's current merge/branch settings from the GitHub API.
fn get_settings(sh: &Shell, repo: &str) -> anyhow::Result<RepoSettings> {
    let output = cmd!(sh, "gh api repos/{repo}").read()?;
    Ok(serde_json::from_str(&output)?)
}

/// Check whether a single repo needs updating and, if so, apply the
/// preferred settings. When `prompt` is true (batch mode), the user is
/// asked for confirmation per-repo unless `--yes` or `--dry-run` are set.
fn check_and_apply(
    sh: &Shell,
    repo: &str,
    force: bool,
    prompt: bool,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let settings = get_settings(sh, repo)?;
    let deltas = settings.deltas();

    if deltas.is_empty() && !force {
        info!("{} already configured correctly", repo);
        return Ok(());
    }

    if !deltas.is_empty() {
        info!("{} needs updates:", repo);
        for delta in &deltas {
            info!("  {}", delta);
        }
    }

    if dry_run {
        if deltas.is_empty() {
            info!(
                "Dry run: {} already matches preferred settings; no changes would be applied",
                repo
            );
        } else {
            info!("Dry run: would apply settings to {}", repo);
        }
        return Ok(());
    }

    if should_prompt(prompt, yes, dry_run) && !confirm(&format!("Apply settings to {}?", repo))? {
        info!("Skipping {}", repo);
        return Ok(());
    }

    apply_settings(sh, repo)
}

/// Apply preferred settings across every repo the authenticated user owns,
/// skipping forks (not ours to configure) and archived repos (read-only).
fn run_all(sh: &Shell, force: bool, dry_run: bool, yes: bool) -> anyhow::Result<()> {
    info!("Fetching repository list...");
    let output = cmd!(
        sh,
        "gh repo list --json nameWithOwner,isFork,isArchived --limit 1000"
    )
    .read()?;

    let repos: Vec<RepoListEntry> = serde_json::from_str(&output)?;
    let eligible: Vec<_> = repos
        .into_iter()
        .filter(|r| !r.is_fork && !r.is_archived)
        .collect();

    info!("Found {} eligible repositories", eligible.len());

    for repo in eligible {
        check_and_apply(sh, &repo.name_with_owner, force, true, dry_run, yes)?;
    }

    Ok(())
}

/// Only prompt interactively when in batch mode (`prompt`), and the user
/// hasn't opted out of prompts (`--yes`) or actual changes (`--dry-run`).
fn should_prompt(prompt: bool, yes: bool, dry_run: bool) -> bool {
    prompt && !yes && !dry_run
}

/// Push the preferred settings to the repo via `gh api`.
// NOTE: If you change the settings below, update RepoSettings::deltas() to match!
fn apply_settings(sh: &Shell, repo: &str) -> anyhow::Result<()> {
    info!("Configuring {}...", repo);
    cmd!(
        sh,
        "gh api -X PATCH repos/{repo}
            -F allow_merge_commit=false
            -F allow_squash_merge=true
            -f squash_merge_commit_title=PR_TITLE
            -f squash_merge_commit_message=PR_BODY
            -F allow_rebase_merge=false
            -F delete_branch_on_merge=true"
    )
    .ignore_stdout()
    .run()?;
    info!("Done: {}", repo);
    Ok(())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    print!("{} [y/N] ", prompt);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::{RepoSettings, should_prompt};

    fn preferred_settings() -> RepoSettings {
        RepoSettings {
            allow_merge_commit: false,
            allow_squash_merge: true,
            squash_merge_commit_title: "PR_TITLE".to_string(),
            squash_merge_commit_message: "PR_BODY".to_string(),
            allow_rebase_merge: false,
            delete_branch_on_merge: true,
        }
    }

    #[test]
    fn deltas_is_empty_when_repo_matches_preferred_settings() {
        let settings = preferred_settings();
        assert!(settings.deltas().is_empty());
    }

    #[test]
    fn deltas_reports_delete_branch_on_merge_when_disabled() {
        let mut settings = preferred_settings();
        settings.delete_branch_on_merge = false;

        assert_eq!(
            settings.deltas(),
            vec!["delete_branch_on_merge: false -> true".to_string()]
        );
    }

    #[test]
    fn deltas_reports_allow_merge_commit_when_enabled() {
        let mut settings = preferred_settings();
        settings.allow_merge_commit = true;
        assert_eq!(settings.deltas(), vec!["allow_merge_commit: true -> false"]);
    }

    #[test]
    fn deltas_reports_allow_squash_merge_when_disabled() {
        let mut settings = preferred_settings();
        settings.allow_squash_merge = false;
        assert_eq!(settings.deltas(), vec!["allow_squash_merge: false -> true"]);
    }

    #[test]
    fn deltas_reports_squash_merge_commit_title_mismatch() {
        let mut settings = preferred_settings();
        settings.squash_merge_commit_title = "COMMIT_OR_PR_TITLE".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["squash_merge_commit_title: COMMIT_OR_PR_TITLE -> PR_TITLE"]
        );
    }

    #[test]
    fn deltas_reports_squash_merge_commit_message_mismatch() {
        let mut settings = preferred_settings();
        settings.squash_merge_commit_message = "BLANK".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["squash_merge_commit_message: BLANK -> PR_BODY"]
        );
    }

    #[test]
    fn deltas_reports_allow_rebase_merge_when_enabled() {
        let mut settings = preferred_settings();
        settings.allow_rebase_merge = true;
        assert_eq!(settings.deltas(), vec!["allow_rebase_merge: true -> false"]);
    }

    #[test]
    fn should_prompt_for_all_repos_by_default() {
        assert!(should_prompt(true, false, false));
    }

    #[test]
    fn should_not_prompt_when_yes_is_set() {
        assert!(!should_prompt(true, true, false));
    }

    #[test]
    fn should_not_prompt_for_dry_run() {
        assert!(!should_prompt(true, false, true));
    }
}
