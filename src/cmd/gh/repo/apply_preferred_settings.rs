//! Enforce a consistent set of merge/branch and GitHub Actions settings
//! across GitHub repos.
//!
//! The "preferred" merge/branch configuration is squash-merge-only using the
//! PR title and body, merge commits and rebase disabled, and head branches
//! auto-deleted after merge. These choices keep the commit history linear
//! and tidy while preserving PR context in each squashed commit message.
//!
//! On top of that, every repo also gets two Actions settings locked down.
//! The motivation is billing/security, not style: a public repo accepts
//! pull requests from any fork, and by default a fork PR's workflow run
//! executes on the *base* repo's runners -- including paid, self-hosted-style
//! runners billed to the owner -- once that contributor has had a single PR
//! merged into the repo. Neither setting below is a hard guarantee on its
//! own; together they're defense-in-depth against that billing/execution
//! risk:
//!
//! - The fork-PR approval policy (`all_external_contributors`) makes every
//!   fork *pull-request* workflow run triggered by someone outside both the
//!   repo and its organization wait for an explicit maintainer approval.
//!   Org members are exempt even without repo access -- they're not
//!   "external" to the policy -- and `pull_request_target` workflows are
//!   never gated by it at all: GitHub always runs the *base* branch's copy
//!   of that workflow because it treats it as trusted, regardless of who
//!   opened the PR. Other triggers such as `issue_comment` aren't gated by
//!   this policy either.
//! - `default_workflow_permissions=read` sets the default `GITHUB_TOKEN`
//!   scope to read-only, but it's a default, not a ceiling: a workflow's own
//!   `permissions:` key can still request more, where org policy allows it.
//!   A public-fork `pull_request` run getting a read-only token regardless
//!   of this setting is actually GitHub's separate fork-isolation behavior,
//!   not something this setting itself guarantees.
//!
//! The fork-PR approval endpoint is inapplicable -- and GitHub rejects it
//! with a 422 -- for every non-public repo (private or internal), so it's
//! skipped there without any output: being non-public is the expected state
//! for that setting, not something worth warning about. Because every run
//! re-checks every setting from scratch, the setting lands automatically on
//! the first run after the repo goes public; nothing needs to notice the
//! transition. Non-public repos get a stricter counterpart instead: the
//! `fork-pr-workflows-private-repos` endpoint, asserted all-`false`, which
//! stops fork-PR workflows from running on the repo's runners at all rather
//! than merely gating them behind approval.

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
///
/// `visibility` drives whether the fork-PR-approval endpoint is applicable
/// at all (see the module docs); it isn't itself part of the
/// preferred-settings delta, since it's not a setting `kd` enforces. GitHub
/// reports one of `"public"`, `"private"`, or (Enterprise-only) `"internal"`
/// here -- the boolean `private` field GitHub also returns can't distinguish
/// `private` from `internal`, so `visibility` is the only reliable way to
/// test "is this repo public".
#[derive(Deserialize)]
struct RepoSettings {
    allow_merge_commit: bool,
    allow_squash_merge: bool,
    squash_merge_commit_title: String,
    squash_merge_commit_message: String,
    allow_rebase_merge: bool,
    delete_branch_on_merge: bool,
    visibility: String,
}

/// Actions workflow token defaults, as returned by
/// `gh api repos/{repo}/actions/permissions/workflow`. Applies to public and
/// private repos alike.
#[derive(Deserialize)]
struct WorkflowPermissions {
    default_workflow_permissions: String,
    can_approve_pull_request_reviews: bool,
}

/// The fork-PR contributor approval policy, as returned by
/// `gh api repos/{repo}/actions/permissions/fork-pr-contributor-approval`.
/// GitHub only exposes this endpoint for public repos; see
/// `LiveSettings::fork_pr_approval`.
#[derive(Deserialize)]
struct ForkPrApproval {
    approval_policy: String,
}

/// The private/internal-repo fork-PR-workflow lockdown, as returned by
/// `gh api repos/{repo}/actions/permissions/fork-pr-workflows-private-repos`.
/// This is the non-public mirror of `ForkPrApproval`: GitHub only exposes
/// *this* endpoint for private and internal repos (see
/// `LiveSettings::private_fork_workflows`). Preferred is all four `false`.
/// Setting `run_workflows_from_fork_pull_requests=false` alone already means
/// fork-PR workflows never run on the repo's runners at all, which makes the
/// other three moot in practice -- but they're still asserted `false` so
/// that a future re-enable of fork workflows doesn't silently inherit
/// permissive companions from whatever state predates that flip.
#[derive(Deserialize)]
struct PrivateForkWorkflows {
    run_workflows_from_fork_pull_requests: bool,
    send_write_tokens_to_workflows: bool,
    send_secrets_and_variables: bool,
    require_approval_for_fork_pr_workflows: bool,
}

/// Everything read from GitHub before a single apply decision gets made.
/// Bundling the reads behind one type keeps `deltas`/`planned_writes` pure
/// functions of "what does this repo look like right now", so the GitHub
/// calls stay confined to `get_settings`/`apply_settings` and the decision
/// logic stays unit-testable without touching the network.
struct LiveSettings {
    repo: RepoSettings,
    workflow: WorkflowPermissions,
    /// `Some` iff `repo.visibility == "public"`, mirroring
    /// `private_fork_workflows` below: exactly one of the two is ever
    /// `Some`, since GitHub exposes exactly one of the two fork-workflow
    /// endpoints depending on visibility and 422s on the other. `None` when
    /// the repo isn't public: the fork-PR-approval endpoint 422s on private
    /// and internal repos alike, so it's neither read nor asserted there.
    /// This is the single signal downstream code uses to decide whether the
    /// fork-approval setting applies -- see `planned_writes`, which keys off
    /// `is_some()` rather than re-deriving applicability from
    /// `repo.visibility` itself, so the two can't drift out of sync.
    fork_pr_approval: Option<ForkPrApproval>,
    /// `Some` iff `repo.visibility != "public"` -- the mirror image of
    /// `fork_pr_approval` above, applicable to exactly the repos that field
    /// isn't. Same applicability contract: `planned_writes` keys off
    /// `is_some()`, never off `repo.visibility` directly.
    private_fork_workflows: Option<PrivateForkWorkflows>,
}

#[derive(Debug, PartialEq)]
enum ApplyDecision {
    AlreadyConfigured,
    DryRun,
    Confirm,
    Apply,
}

#[derive(Clone, Copy)]
struct ApplyOptions {
    force: bool,
    prompt: bool,
    dry_run: bool,
    yes: bool,
}

/// Per-endpoint drift, so `planned_writes` can decide *which* GitHub write
/// each repo needs rather than always issuing all of them. The grouping
/// mirrors the four `gh api` endpoints `apply_settings` writes to: a
/// group's list is non-empty exactly when that endpoint's live values don't
/// match preferred, using the same human-readable delta strings `deltas()`
/// has always reported. `fork_pr_approval` and `private_fork_workflows` are
/// mutually exclusive in practice -- at most one is ever non-empty for a
/// given repo, since only one of the two source fields is ever `Some` (see
/// `LiveSettings`).
struct Drift {
    merge: Vec<String>,
    workflow: Vec<String>,
    fork_pr_approval: Vec<String>,
    private_fork_workflows: Vec<String>,
}

impl Drift {
    /// Every delta across all four groups, concatenated in the fixed
    /// `[merge, workflow, fork_pr_approval, private_fork_workflows]` order
    /// -- the first three preserved from before grouping existed, so
    /// existing callers (and their exact-match assertions) don't need to
    /// care that grouping exists.
    fn all(&self) -> Vec<String> {
        self.merge
            .iter()
            .chain(&self.workflow)
            .chain(&self.fork_pr_approval)
            .chain(&self.private_fork_workflows)
            .cloned()
            .collect()
    }
}

impl LiveSettings {
    /// Compare the current settings against the preferred values, grouped
    /// by which GitHub endpoint each setting belongs to. `planned_writes`
    /// uses the per-group emptiness to decide which writes an apply
    /// actually needs to issue. When `fork_pr_approval` is `None`
    /// (non-public repo) or `private_fork_workflows` is `None` (public
    /// repo), that group is always empty -- the setting isn't applicable,
    /// not merely satisfied.
    fn drift(&self) -> Drift {
        let mut merge = Vec::new();
        if self.repo.allow_merge_commit {
            merge.push("allow_merge_commit: true -> false".to_string());
        }
        if !self.repo.allow_squash_merge {
            merge.push("allow_squash_merge: false -> true".to_string());
        }
        if self.repo.squash_merge_commit_title != "PR_TITLE" {
            merge.push(format!(
                "squash_merge_commit_title: {} -> PR_TITLE",
                self.repo.squash_merge_commit_title
            ));
        }
        if self.repo.squash_merge_commit_message != "PR_BODY" {
            merge.push(format!(
                "squash_merge_commit_message: {} -> PR_BODY",
                self.repo.squash_merge_commit_message
            ));
        }
        if self.repo.allow_rebase_merge {
            merge.push("allow_rebase_merge: true -> false".to_string());
        }
        if !self.repo.delete_branch_on_merge {
            merge.push("delete_branch_on_merge: false -> true".to_string());
        }

        let mut workflow = Vec::new();
        if self.workflow.default_workflow_permissions != "read" {
            workflow.push(format!(
                "default_workflow_permissions: {} -> read",
                self.workflow.default_workflow_permissions
            ));
        }
        if self.workflow.can_approve_pull_request_reviews {
            workflow.push("can_approve_pull_request_reviews: true -> false".to_string());
        }

        let mut fork_pr_approval = Vec::new();
        if let Some(approval) = &self.fork_pr_approval
            && approval.approval_policy != "all_external_contributors"
        {
            fork_pr_approval.push(format!(
                "fork_pr_approval_policy: {} -> all_external_contributors",
                approval.approval_policy
            ));
        }

        let mut private_fork_workflows = Vec::new();
        if let Some(workflows) = &self.private_fork_workflows {
            if workflows.run_workflows_from_fork_pull_requests {
                private_fork_workflows
                    .push("run_workflows_from_fork_pull_requests: true -> false".to_string());
            }
            if workflows.send_write_tokens_to_workflows {
                private_fork_workflows
                    .push("send_write_tokens_to_workflows: true -> false".to_string());
            }
            if workflows.send_secrets_and_variables {
                private_fork_workflows
                    .push("send_secrets_and_variables: true -> false".to_string());
            }
            if workflows.require_approval_for_fork_pr_workflows {
                private_fork_workflows
                    .push("require_approval_for_fork_pr_workflows: true -> false".to_string());
            }
        }

        Drift {
            merge,
            workflow,
            fork_pr_approval,
            private_fork_workflows,
        }
    }

    /// Compare the current settings against the preferred values and return
    /// human-readable descriptions of every difference, across all three
    /// endpoint groups. An empty result means the repo already matches; see
    /// `drift` for the per-endpoint breakdown `planned_writes` acts on.
    fn deltas(&self) -> Vec<String> {
        self.drift().all()
    }
}

/// One GitHub write the apply step may need to issue. Kept as data -- rather
/// than issuing the `gh api` calls directly -- so "which writes does this
/// repo need" (`planned_writes`) is a pure function that unit tests can
/// assert on without a network, independent of the `gh api` invocations
/// `apply_settings` runs for each variant.
#[derive(Debug, PartialEq)]
enum SettingsWrite {
    MergeSettings,
    WorkflowPermissions,
    ForkPrApproval,
    PrivateForkWorkflows,
}

/// Decide which writes an apply should issue for `settings`, in the fixed
/// order `apply_settings` issues them: merge settings, then workflow
/// permissions, then fork-PR approval, then the private-repo fork-workflow
/// lockdown. At most one of the last two is ever applicable to a given repo
/// (see `LiveSettings`), so in practice at most three writes ever fire.
///
/// Without `--force`, a group's write is included iff that group actually
/// drifted (`Drift`'s per-group list is non-empty) -- each `gh api` call now
/// only fires for the endpoint that needs it, rather than every apply
/// re-asserting every endpoint regardless of what changed. With `--force`,
/// merge settings and workflow permissions are always included (re-asserted
/// even with no drift, which is the whole point of `--force`), but the
/// fork-PR-approval and private-fork-workflows writes are still included
/// only when applicable -- `fork_pr_approval.is_some()` /
/// `private_fork_workflows.is_some()` are the single sources of truth for
/// applicability (see `LiveSettings`), so this never attempts a write for a
/// repo whose visibility makes that endpoint 422, `--force` or not.
fn planned_writes(settings: &LiveSettings, force: bool) -> Vec<SettingsWrite> {
    let drift = settings.drift();
    let mut writes = Vec::new();
    if force || !drift.merge.is_empty() {
        writes.push(SettingsWrite::MergeSettings);
    }
    if force || !drift.workflow.is_empty() {
        writes.push(SettingsWrite::WorkflowPermissions);
    }
    if settings.fork_pr_approval.is_some() && (force || !drift.fork_pr_approval.is_empty()) {
        writes.push(SettingsWrite::ForkPrApproval);
    }
    if settings.private_fork_workflows.is_some()
        && (force || !drift.private_fork_workflows.is_empty())
    {
        writes.push(SettingsWrite::PrivateForkWorkflows);
    }
    writes
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

/// Fetch the repo's current settings from the GitHub API: the merge/branch
/// settings first, then Actions workflow token permissions, then exactly one
/// of the two fork-workflow endpoints depending on visibility -- the fork-PR
/// contributor approval policy for public repos, or the private-repo
/// fork-workflow lockdown for everything else. Each of those two endpoints
/// 422s on the visibility it doesn't apply to, so only the applicable one is
/// ever requested; the other field on `LiveSettings` ends up `None` rather
/// than an error.
fn get_settings(sh: &Shell, repo: &str) -> anyhow::Result<LiveSettings> {
    let repo_output = cmd!(sh, "gh api repos/{repo}").read()?;
    let repo_settings: RepoSettings = serde_json::from_str(&repo_output)?;

    let workflow_output = cmd!(sh, "gh api repos/{repo}/actions/permissions/workflow").read()?;
    let workflow: WorkflowPermissions = serde_json::from_str(&workflow_output)?;

    let (fork_pr_approval, private_fork_workflows) = if repo_settings.visibility == "public" {
        let output = cmd!(
            sh,
            "gh api repos/{repo}/actions/permissions/fork-pr-contributor-approval"
        )
        .read()?;
        (Some(serde_json::from_str(&output)?), None)
    } else {
        let output = cmd!(
            sh,
            "gh api repos/{repo}/actions/permissions/fork-pr-workflows-private-repos"
        )
        .read()?;
        (None, Some(serde_json::from_str(&output)?))
    };

    Ok(LiveSettings {
        repo: repo_settings,
        workflow,
        fork_pr_approval,
        private_fork_workflows,
    })
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
    check_and_apply_settings(
        repo,
        &settings,
        ApplyOptions {
            force,
            prompt,
            dry_run,
            yes,
        },
        confirm,
        || apply_settings(sh, repo, &settings, force),
    )
}

/// Drive the command-level apply decision after the live settings have been
/// loaded. Keeping the GitHub reads and writes outside this function is what
/// makes the safety properties testable: dry-runs and declined prompts must
/// not be able to reach a GitHub write call by accident.
fn check_and_apply_settings<C, A>(
    repo: &str,
    settings: &LiveSettings,
    options: ApplyOptions,
    mut confirm: C,
    mut apply: A,
) -> anyhow::Result<()>
where
    C: FnMut(&str) -> anyhow::Result<bool>,
    A: FnMut() -> anyhow::Result<()>,
{
    let deltas = settings.deltas();
    let decision = decide_apply(
        !deltas.is_empty(),
        options.force,
        options.prompt,
        options.dry_run,
        options.yes,
    );

    if matches!(decision, ApplyDecision::AlreadyConfigured) {
        info!("{} already configured correctly", repo);
        return Ok(());
    }

    if !deltas.is_empty() {
        info!("{} needs updates:", repo);
        for delta in &deltas {
            info!("  {}", delta);
        }
    }

    if matches!(decision, ApplyDecision::DryRun) {
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

    if matches!(decision, ApplyDecision::Confirm)
        && !confirm(&format!("Apply settings to {}?", repo))?
    {
        info!("Skipping {}", repo);
        return Ok(());
    }

    apply()
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
    let eligible = eligible_repos(repos);

    info!("Found {} eligible repositories", eligible.len());

    for repo in eligible {
        check_and_apply(sh, &repo.name_with_owner, force, true, dry_run, yes)?;
    }

    Ok(())
}

fn eligible_repos(repos: Vec<RepoListEntry>) -> Vec<RepoListEntry> {
    repos
        .into_iter()
        .filter(|r| !r.is_fork && !r.is_archived)
        .collect()
}

fn decide_apply(
    has_deltas: bool,
    force: bool,
    prompt: bool,
    dry_run: bool,
    yes: bool,
) -> ApplyDecision {
    if !has_deltas && !force {
        return ApplyDecision::AlreadyConfigured;
    }

    if dry_run {
        return ApplyDecision::DryRun;
    }

    if prompt && !yes {
        return ApplyDecision::Confirm;
    }

    ApplyDecision::Apply
}

/// Push the preferred settings to the repo via `gh api`, issuing one call
/// per entry in `planned_writes(settings, force)`.
///
/// Contract: writes happen in the fixed order merge settings, then workflow
/// permissions, then fork-PR approval, then the private-fork-workflows
/// lockdown (at most one of the last two ever fires for a given repo -- see
/// `LiveSettings`), and each is a synchronous `gh api` call that returns
/// before the next one starts -- there's no batching or concurrency to
/// reorder them. The sequence is *not* atomic: each `?`
/// stops on the first failure, and there's no rollback of writes that
/// already succeeded. A repo can therefore be left with, say, its merge
/// settings updated but its workflow permissions still stale if the
/// workflow-permissions call fails. That's fine because every run
/// recomputes `planned_writes` from freshly fetched settings -- rerunning
/// the command reconciles whatever's still outstanding without redoing
/// writes that already landed.
// NOTE: If you change the settings below, update LiveSettings::drift() to match!
fn apply_settings(
    sh: &Shell,
    repo: &str,
    settings: &LiveSettings,
    force: bool,
) -> anyhow::Result<()> {
    info!("Configuring {}...", repo);
    for write in planned_writes(settings, force) {
        match write {
            SettingsWrite::MergeSettings => {
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
            }
            SettingsWrite::WorkflowPermissions => {
                cmd!(
                    sh,
                    "gh api -X PUT repos/{repo}/actions/permissions/workflow
                        -f default_workflow_permissions=read
                        -F can_approve_pull_request_reviews=false"
                )
                .ignore_stdout()
                .run()?;
            }
            SettingsWrite::ForkPrApproval => {
                cmd!(
                    sh,
                    "gh api -X PUT repos/{repo}/actions/permissions/fork-pr-contributor-approval
                        -f approval_policy=all_external_contributors"
                )
                .ignore_stdout()
                .run()?;
            }
            SettingsWrite::PrivateForkWorkflows => {
                cmd!(
                    sh,
                    "gh api -X PUT repos/{repo}/actions/permissions/fork-pr-workflows-private-repos
                        -F run_workflows_from_fork_pull_requests=false
                        -F send_write_tokens_to_workflows=false
                        -F send_secrets_and_variables=false
                        -F require_approval_for_fork_pr_workflows=false"
                )
                .ignore_stdout()
                .run()?;
            }
        }
    }
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
    use super::{
        ApplyDecision, ApplyOptions, ForkPrApproval, LiveSettings, PrivateForkWorkflows,
        RepoListEntry, RepoSettings, SettingsWrite, WorkflowPermissions, decide_apply,
        eligible_repos, planned_writes,
    };
    use std::cell::Cell;

    /// A public repo whose merge/branch settings, workflow token
    /// permissions, and fork-PR approval policy all already match
    /// preferred: `deltas()` on this must be empty.
    fn preferred_settings() -> LiveSettings {
        LiveSettings {
            repo: RepoSettings {
                allow_merge_commit: false,
                allow_squash_merge: true,
                squash_merge_commit_title: "PR_TITLE".to_string(),
                squash_merge_commit_message: "PR_BODY".to_string(),
                allow_rebase_merge: false,
                delete_branch_on_merge: true,
                visibility: "public".to_string(),
            },
            workflow: WorkflowPermissions {
                default_workflow_permissions: "read".to_string(),
                can_approve_pull_request_reviews: false,
            },
            fork_pr_approval: Some(ForkPrApproval {
                approval_policy: "all_external_contributors".to_string(),
            }),
            private_fork_workflows: None,
        }
    }

    /// A private repo, otherwise identical in spirit to `preferred_settings`:
    /// merge/branch and workflow settings match preferred, `fork_pr_approval`
    /// is `None` because that endpoint isn't applicable to private repos, and
    /// `private_fork_workflows` is `Some` with all four fields already
    /// `false` -- its mirror-image applicable endpoint, already in the
    /// preferred state. `deltas()` on this must also be empty.
    fn private_preferred_settings() -> LiveSettings {
        let mut settings = preferred_settings();
        settings.repo.visibility = "private".to_string();
        settings.fork_pr_approval = None;
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: false,
            send_write_tokens_to_workflows: false,
            send_secrets_and_variables: false,
            require_approval_for_fork_pr_workflows: false,
        });
        settings
    }

    fn apply_options(force: bool, prompt: bool, dry_run: bool, yes: bool) -> ApplyOptions {
        ApplyOptions {
            force,
            prompt,
            dry_run,
            yes,
        }
    }

    #[test]
    fn deltas_is_empty_when_repo_matches_preferred_settings() {
        let settings = preferred_settings();
        assert!(settings.deltas().is_empty());
    }

    /// A private repo that matches on every applicable setting must report
    /// no deltas -- in particular, the absent fork-PR-approval read must not
    /// be mistaken for a mismatch.
    #[test]
    fn deltas_is_empty_for_private_repo_matching_applicable_settings() {
        let settings = private_preferred_settings();
        assert!(settings.deltas().is_empty());
    }

    #[test]
    fn deltas_reports_delete_branch_on_merge_when_disabled() {
        let mut settings = preferred_settings();
        settings.repo.delete_branch_on_merge = false;

        assert_eq!(
            settings.deltas(),
            vec!["delete_branch_on_merge: false -> true".to_string()]
        );
    }

    #[test]
    fn deltas_reports_allow_merge_commit_when_enabled() {
        let mut settings = preferred_settings();
        settings.repo.allow_merge_commit = true;
        assert_eq!(settings.deltas(), vec!["allow_merge_commit: true -> false"]);
    }

    /// The workflow-token-permissions mismatch (default not `read`) must
    /// surface as its own delta, sourced from the
    /// `actions/permissions/workflow` endpoint.
    #[test]
    fn deltas_reports_default_workflow_permissions_mismatch() {
        let mut settings = preferred_settings();
        settings.workflow.default_workflow_permissions = "write".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["default_workflow_permissions: write -> read"]
        );
    }

    /// The workflow-token `can_approve_pull_request_reviews` flag must be
    /// forced back to `false`: it's the repo-wide switch controlling
    /// whether *any* Actions workflow whose token has sufficient
    /// permissions may create or approve pull requests, not something
    /// specific to fork-PR runs (those are read-only regardless, via
    /// GitHub's separate fork-isolation rules -- see the module docs).
    /// Leaving it `true` widens what a workflow could do if it somehow
    /// obtained write-scoped token permissions.
    #[test]
    fn deltas_reports_can_approve_pull_request_reviews_when_enabled() {
        let mut settings = preferred_settings();
        settings.workflow.can_approve_pull_request_reviews = true;
        assert_eq!(
            settings.deltas(),
            vec!["can_approve_pull_request_reviews: true -> false"]
        );
    }

    /// Workflow-token permission drift must be caught on a *private* repo
    /// too, not only the public fixture every other workflow-drift test
    /// starts from -- `default_workflow_permissions` and
    /// `can_approve_pull_request_reviews` apply regardless of visibility
    /// (see `WorkflowPermissions`'s docs), so an implementation that only
    /// checked them for public repos would slip this test past every other
    /// workflow-permission test in this module. This also exercises the
    /// full command-level decision path end to end: a real apply (no
    /// `--force`, no prompt, no dry-run) must actually invoke the apply
    /// callback once a delta exists, private repo or not.
    #[test]
    fn check_and_apply_settings_applies_workflow_drift_on_private_repo() {
        let mut settings = private_preferred_settings();
        settings.workflow.default_workflow_permissions = "write".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["default_workflow_permissions: write -> read"]
        );

        let applied = Cell::new(false);
        super::check_and_apply_settings(
            "owner/repo",
            &settings,
            apply_options(false, false, false, false),
            |_| Ok(true),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(applied.get());
    }

    /// The fork-PR approval policy mismatch, sourced from the
    /// `actions/permissions/fork-pr-contributor-approval` endpoint, must
    /// surface as its own delta on a public repo where the setting is
    /// applicable.
    #[test]
    fn deltas_reports_fork_pr_approval_policy_mismatch() {
        let mut settings = preferred_settings();
        settings.fork_pr_approval = Some(ForkPrApproval {
            approval_policy: "first_time_contributors".to_string(),
        });
        assert_eq!(
            settings.deltas(),
            vec!["fork_pr_approval_policy: first_time_contributors -> all_external_contributors"]
        );
    }

    /// The `run_workflows_from_fork_pull_requests` mismatch, sourced from
    /// the `actions/permissions/fork-pr-workflows-private-repos` endpoint,
    /// must surface as its own delta on a private repo where the setting is
    /// applicable. This is the field that actually stops fork-PR workflows
    /// from running at all; the other three are only meaningful once this
    /// one is `true` (see `PrivateForkWorkflows`'s docs).
    #[test]
    fn deltas_reports_run_workflows_from_fork_pull_requests_mismatch() {
        let mut settings = private_preferred_settings();
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: true,
            send_write_tokens_to_workflows: false,
            send_secrets_and_variables: false,
            require_approval_for_fork_pr_workflows: false,
        });
        assert_eq!(
            settings.deltas(),
            vec!["run_workflows_from_fork_pull_requests: true -> false"]
        );
    }

    /// The `send_write_tokens_to_workflows` mismatch must surface as its own
    /// delta, independent of the other three private-fork-workflow fields.
    #[test]
    fn deltas_reports_send_write_tokens_to_workflows_mismatch() {
        let mut settings = private_preferred_settings();
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: false,
            send_write_tokens_to_workflows: true,
            send_secrets_and_variables: false,
            require_approval_for_fork_pr_workflows: false,
        });
        assert_eq!(
            settings.deltas(),
            vec!["send_write_tokens_to_workflows: true -> false"]
        );
    }

    /// The `send_secrets_and_variables` mismatch must surface as its own
    /// delta, independent of the other three private-fork-workflow fields.
    #[test]
    fn deltas_reports_send_secrets_and_variables_mismatch() {
        let mut settings = private_preferred_settings();
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: false,
            send_write_tokens_to_workflows: false,
            send_secrets_and_variables: true,
            require_approval_for_fork_pr_workflows: false,
        });
        assert_eq!(
            settings.deltas(),
            vec!["send_secrets_and_variables: true -> false"]
        );
    }

    /// The `require_approval_for_fork_pr_workflows` mismatch must surface as
    /// its own delta. `kd` never sets this field `true` (see the module
    /// docs), but a repo found with it `true` -- e.g. flipped by hand, or
    /// left over from before fork workflows were disabled -- must still be
    /// reported and corrected back to `false`.
    #[test]
    fn deltas_reports_require_approval_for_fork_pr_workflows_mismatch() {
        let mut settings = private_preferred_settings();
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: false,
            send_write_tokens_to_workflows: false,
            send_secrets_and_variables: false,
            require_approval_for_fork_pr_workflows: true,
        });
        assert_eq!(
            settings.deltas(),
            vec!["require_approval_for_fork_pr_workflows: true -> false"]
        );
    }

    /// With no drift and no `--force`, nothing needs writing: the plan must
    /// be empty rather than blindly re-asserting every endpoint. This is
    /// the core of the delta-driven plan -- the previous behavior issued
    /// every write unconditionally once *any* setting had drifted, which
    /// meant an already-correct endpoint got rewritten for no reason (and,
    /// worse, could exhaust an earlier write's error budget before a later,
    /// actually-needed write ran).
    #[test]
    fn planned_writes_is_empty_when_nothing_drifted_and_not_forced() {
        let settings = preferred_settings();
        assert_eq!(planned_writes(&settings, false), Vec::new());
    }

    /// Without `--force`, only the group that actually drifted should be
    /// written -- here, only the workflow-permissions endpoint, even though
    /// merge settings and fork-PR approval are both present and applicable.
    #[test]
    fn planned_writes_includes_only_the_drifted_group_without_force() {
        let mut settings = preferred_settings();
        settings.workflow.default_workflow_permissions = "write".to_string();
        assert_eq!(
            planned_writes(&settings, false),
            vec![SettingsWrite::WorkflowPermissions]
        );
    }

    /// `--force` re-asserts merge settings and workflow permissions even
    /// with no drift, and still includes fork-PR approval when it's
    /// applicable (public repo) -- exactly those three, in the fixed order
    /// `apply_settings` issues them, and critically *not*
    /// `PrivateForkWorkflows`: that endpoint doesn't apply to a public repo
    /// (`private_fork_workflows` is `None` on `preferred_settings()`), so
    /// the exact-match assertion here doubles as the "public plan never
    /// includes it" guarantee. Fork-PR approval specifically has to stay in
    /// a public repo's plan: omitting it would leave workflows triggered by
    /// external forks free to run unapproved, which is exactly the
    /// billing/execution risk this command exists to close.
    #[test]
    fn planned_writes_force_includes_all_applicable_writes_in_order() {
        let settings = preferred_settings();
        assert_eq!(
            planned_writes(&settings, true),
            vec![
                SettingsWrite::MergeSettings,
                SettingsWrite::WorkflowPermissions,
                SettingsWrite::ForkPrApproval
            ]
        );
    }

    /// Without `--force`, a private repo whose *only* drift is in the
    /// private-fork-workflows group must plan *only* that write -- merge
    /// settings and workflow permissions are untouched because neither
    /// drifted, mirroring `planned_writes_includes_only_the_drifted_group_without_force`
    /// but for the private-repo endpoint.
    #[test]
    fn planned_writes_includes_only_private_fork_workflows_when_that_group_alone_drifts() {
        let mut settings = private_preferred_settings();
        settings.private_fork_workflows = Some(PrivateForkWorkflows {
            run_workflows_from_fork_pull_requests: true,
            send_write_tokens_to_workflows: false,
            send_secrets_and_variables: false,
            require_approval_for_fork_pr_workflows: false,
        });
        assert_eq!(
            planned_writes(&settings, false),
            vec![SettingsWrite::PrivateForkWorkflows]
        );
    }

    /// `planned_writes` must never include `ForkPrApproval` for a private
    /// repo, regardless of what deltas exist -- the endpoint 422s there, so
    /// even a `--force` apply must not attempt it. `fork_pr_approval.is_some()`
    /// is the sole applicability signal (see `LiveSettings::fork_pr_approval`),
    /// so this also guards against a future refactor re-deriving applicability
    /// from `repo.visibility` and getting out of sync. Conversely, `--force`
    /// on a private repo *must* include `PrivateForkWorkflows` -- its mirror
    /// applicable endpoint -- even with no drift, since that's the entire
    /// point of `--force`.
    #[test]
    fn planned_writes_force_on_private_repo_excludes_approval_includes_workflows() {
        let settings = private_preferred_settings();
        assert_eq!(
            planned_writes(&settings, true),
            vec![
                SettingsWrite::MergeSettings,
                SettingsWrite::WorkflowPermissions,
                SettingsWrite::PrivateForkWorkflows,
            ]
        );
    }

    #[test]
    fn check_and_apply_settings_does_not_apply_when_already_configured() {
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &preferred_settings(),
            apply_options(false, false, false, false),
            |_| Ok(true),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(!applied.get());
    }

    #[test]
    fn check_and_apply_settings_does_not_apply_dry_run() {
        let mut settings = preferred_settings();
        settings.repo.allow_merge_commit = true;
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &settings,
            apply_options(false, false, true, false),
            |_| Ok(true),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(!applied.get());
    }

    #[test]
    fn check_and_apply_settings_skips_when_prompt_declined() {
        let mut settings = preferred_settings();
        settings.repo.allow_merge_commit = true;
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &settings,
            apply_options(false, true, false, false),
            |_| Ok(false),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(!applied.get());
    }

    #[test]
    fn check_and_apply_settings_applies_when_confirmed() {
        let mut settings = preferred_settings();
        settings.repo.allow_merge_commit = true;
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &settings,
            apply_options(false, true, false, false),
            |_| Ok(true),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(applied.get());
    }

    #[test]
    fn check_and_apply_settings_force_applies_without_deltas() {
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &preferred_settings(),
            apply_options(true, false, false, false),
            |_| Ok(true),
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(applied.get());
    }

    #[test]
    fn check_and_apply_settings_yes_applies_without_prompting() {
        let mut settings = preferred_settings();
        settings.repo.allow_merge_commit = true;
        let prompted = Cell::new(false);
        let applied = Cell::new(false);

        super::check_and_apply_settings(
            "owner/repo",
            &settings,
            apply_options(false, true, false, true),
            |_| {
                prompted.set(true);
                Ok(false)
            },
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(!prompted.get());
        assert!(applied.get());
    }

    #[test]
    fn deltas_reports_allow_squash_merge_when_disabled() {
        let mut settings = preferred_settings();
        settings.repo.allow_squash_merge = false;
        assert_eq!(settings.deltas(), vec!["allow_squash_merge: false -> true"]);
    }

    #[test]
    fn deltas_reports_squash_merge_commit_title_mismatch() {
        let mut settings = preferred_settings();
        settings.repo.squash_merge_commit_title = "COMMIT_OR_PR_TITLE".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["squash_merge_commit_title: COMMIT_OR_PR_TITLE -> PR_TITLE"]
        );
    }

    #[test]
    fn deltas_reports_squash_merge_commit_message_mismatch() {
        let mut settings = preferred_settings();
        settings.repo.squash_merge_commit_message = "BLANK".to_string();
        assert_eq!(
            settings.deltas(),
            vec!["squash_merge_commit_message: BLANK -> PR_BODY"]
        );
    }

    #[test]
    fn deltas_reports_allow_rebase_merge_when_enabled() {
        let mut settings = preferred_settings();
        settings.repo.allow_rebase_merge = true;
        assert_eq!(settings.deltas(), vec!["allow_rebase_merge: true -> false"]);
    }
    #[test]
    fn decide_apply_skips_when_repo_is_already_configured() {
        assert_eq!(
            decide_apply(false, false, false, false, false),
            ApplyDecision::AlreadyConfigured
        );
    }

    #[test]
    fn decide_apply_returns_dry_run_before_patch() {
        assert_eq!(
            decide_apply(true, false, false, true, false),
            ApplyDecision::DryRun
        );
    }

    #[test]
    fn decide_apply_forces_patch_even_without_deltas() {
        assert_eq!(
            decide_apply(false, true, false, false, false),
            ApplyDecision::Apply
        );
    }

    #[test]
    fn decide_apply_confirms_before_batch_patch() {
        assert_eq!(
            decide_apply(true, false, true, false, false),
            ApplyDecision::Confirm
        );
    }

    #[test]
    fn decide_apply_bypasses_confirmation_when_yes_is_set() {
        assert_eq!(
            decide_apply(true, false, true, false, true),
            ApplyDecision::Apply
        );
    }

    #[test]
    fn decide_apply_patches_changed_settings() {
        assert_eq!(
            decide_apply(true, false, false, false, false),
            ApplyDecision::Apply
        );
    }

    #[test]
    fn eligible_repos_excludes_forks_and_archives() {
        let repos = vec![
            RepoListEntry {
                name_with_owner: "owner/active".to_string(),
                is_fork: false,
                is_archived: false,
            },
            RepoListEntry {
                name_with_owner: "owner/fork".to_string(),
                is_fork: true,
                is_archived: false,
            },
            RepoListEntry {
                name_with_owner: "owner/archive".to_string(),
                is_fork: false,
                is_archived: true,
            },
        ];

        let names: Vec<_> = eligible_repos(repos)
            .into_iter()
            .map(|repo| repo.name_with_owner)
            .collect();

        assert_eq!(names, vec!["owner/active"]);
    }

    /// A minimal `repos/{repo}` body with `"visibility": "internal"` must
    /// parse the field correctly. This pins GitHub's wire field name and
    /// its three-valued domain (`public`/`private`/`internal`) that
    /// `get_settings` relies on to gate the fork-PR-approval read to
    /// exactly `visibility == "public"` -- a regression back to the
    /// boolean `private` field would silently treat Enterprise "internal"
    /// repos as public and call an endpoint that 422s for them.
    #[test]
    fn repo_settings_deserializes_visibility() {
        let json = r#"{
            "allow_merge_commit": false,
            "allow_squash_merge": true,
            "squash_merge_commit_title": "PR_TITLE",
            "squash_merge_commit_message": "PR_BODY",
            "allow_rebase_merge": false,
            "delete_branch_on_merge": true,
            "visibility": "internal"
        }"#;
        let settings: RepoSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.visibility, "internal");
    }

    /// A minimal `actions/permissions/workflow` body must deserialize into
    /// `WorkflowPermissions`. This pins GitHub's exact wire field names --
    /// `default_workflow_permissions` and `can_approve_pull_request_reviews`
    /// -- that delta detection depends on; if GitHub ever renamed or
    /// restructured this response, `serde` would silently leave these
    /// fields at their defaults instead of erroring, so this test is what
    /// makes that kind of schema drift fail locally instead of in
    /// production.
    #[test]
    fn workflow_permissions_deserializes() {
        let json = r#"{
            "default_workflow_permissions": "read",
            "can_approve_pull_request_reviews": false
        }"#;
        let permissions: WorkflowPermissions = serde_json::from_str(json).unwrap();
        assert_eq!(permissions.default_workflow_permissions, "read");
        assert!(!permissions.can_approve_pull_request_reviews);
    }

    /// A minimal `actions/permissions/fork-pr-contributor-approval` body
    /// must deserialize into `ForkPrApproval`. This protects the
    /// `approval_policy` wire field name specifically: it's the only signal
    /// `deltas()` uses to decide whether public-fork workflow approval is
    /// already correctly configured, so silent schema drift here would mean
    /// the delta check passes on a repo that's actually still exposed.
    #[test]
    fn fork_pr_approval_deserializes() {
        let json = r#"{"approval_policy": "all_external_contributors"}"#;
        let approval: ForkPrApproval = serde_json::from_str(json).unwrap();
        assert_eq!(approval.approval_policy, "all_external_contributors");
    }

    /// A minimal `actions/permissions/fork-pr-workflows-private-repos` body
    /// must deserialize into `PrivateForkWorkflows`. This pins GitHub's four
    /// wire field names for the private-repo endpoint, the mirror-image
    /// counterpart to `fork_pr_approval_deserializes` above -- silent schema
    /// drift here would mean `deltas()` silently stops catching a permissive
    /// private repo.
    #[test]
    fn private_fork_workflows_deserializes() {
        let json = r#"{
            "run_workflows_from_fork_pull_requests": false,
            "send_write_tokens_to_workflows": false,
            "send_secrets_and_variables": false,
            "require_approval_for_fork_pr_workflows": false
        }"#;
        let workflows: PrivateForkWorkflows = serde_json::from_str(json).unwrap();
        assert!(!workflows.run_workflows_from_fork_pull_requests);
        assert!(!workflows.send_write_tokens_to_workflows);
        assert!(!workflows.send_secrets_and_variables);
        assert!(!workflows.require_approval_for_fork_pr_workflows);
    }
}
