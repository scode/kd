use super::ApplyPreferredSettingsArgs;
use serde::Deserialize;
use std::io::{self, Write};
use tracing::info;
use xshell::{Shell, cmd};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoListEntry {
    name_with_owner: String,
    is_fork: bool,
    is_archived: bool,
}

#[derive(Deserialize)]
struct RepoSettings {
    allow_merge_commit: bool,
    allow_squash_merge: bool,
    squash_merge_commit_title: String,
    squash_merge_commit_message: String,
    allow_rebase_merge: bool,
}

impl RepoSettings {
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
        deltas
    }
}

pub fn run(args: ApplyPreferredSettingsArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;

    if args.all {
        run_all(&sh, args.force)
    } else {
        let repo = args.repo.expect("repo required when --all not specified");
        check_and_apply(&sh, &repo, args.force, false)
    }
}

fn get_settings(sh: &Shell, repo: &str) -> anyhow::Result<RepoSettings> {
    let output = cmd!(sh, "gh api repos/{repo}").read()?;
    Ok(serde_json::from_str(&output)?)
}

fn check_and_apply(sh: &Shell, repo: &str, force: bool, prompt: bool) -> anyhow::Result<()> {
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

    if prompt && !confirm(&format!("Apply settings to {}?", repo))? {
        info!("Skipping {}", repo);
        return Ok(());
    }

    apply_settings(sh, repo)
}

fn run_all(sh: &Shell, force: bool) -> anyhow::Result<()> {
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
        check_and_apply(sh, &repo.name_with_owner, force, true)?;
    }

    Ok(())
}

// NOTE: If you change the settings below, update RepoSettings::deltas() to match!
fn apply_settings(sh: &Shell, repo: &str) -> anyhow::Result<()> {
    info!("Configuring {}...", repo);
    cmd!(
        sh,
        "gh api -X PATCH repos/{repo}
            -f allow_merge_commit=false
            -f allow_squash_merge=true
            -f squash_merge_commit_title=PR_TITLE
            -f squash_merge_commit_message=PR_BODY
            -f allow_rebase_merge=false"
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
