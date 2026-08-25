//! End-to-end checks that `kd gh repo apply-preferred-settings` reads and
//! writes exactly the `gh api` endpoints it should, for exactly the repos
//! and settings it should.
//!
//! Everything in `apply_preferred_settings.rs`'s unit tests works against
//! hand-built `LiveSettings` values -- they prove the *decision* logic
//! (`deltas`, `drift`, `planned_writes`, `check_and_apply_settings`) is
//! correct, but nothing there proves `get_settings`/`apply_settings`
//! actually call the endpoints those decisions assume, with the right
//! HTTP verbs, paths, and fields. That's the gap these tests close: they
//! run the real `kd` binary against a fake `gh`, and inspect exactly what
//! `gh` was asked to do.
//!
//! Mechanism: spawn the built `kd` binary as a child with a private `PATH`
//! containing a fake `gh` shell script (following `tests/ubiworker_ssh_exec.rs`).
//! Only the child's environment is touched -- the test process's own env is
//! never mutated (see CLAUDE.md). The fake `gh` logs every invocation's
//! space-joined argv to a file and answers the GET endpoints `get_settings`
//! reads with canned JSON; any `-X PUT`/`-X PATCH` write is accepted
//! unconditionally (logged, exit 0) since correctness of the *arguments* to
//! those writes is what the tests assert on afterward, not something the
//! fake needs to validate itself. Anything the fake doesn't recognize is a
//! test failure (exit 99), which is also how a private-repo test proves the
//! public-only fork-approval endpoint is never requested, and a public-repo
//! test proves the private-only fork-workflows endpoint is never requested
//! -- see `fake_gh`'s docs for the mirror-image `fork_json`/
//! `private_fork_json` pair that drives this.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Write an executable shell script at `dir/name`.
fn fake_bin(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Build a fake `gh` that answers `get_settings`'s GET requests with canned
/// JSON and accepts any write unconditionally.
///
/// `fork_json` and `private_fork_json` are the mirror-image applicability
/// pair described in `LiveSettings`: exactly one should be `Some` per test
/// (public repo -> `fork_json`; non-public repo -> `private_fork_json`).
/// Whichever one is `None` models the endpoint GitHub 422s on for that
/// visibility, where `get_settings` must never issue the GET at all: the
/// fake treats that request as a test failure (exit 99) rather than quietly
/// answering it, so a regression that reads the inapplicable endpoint shows
/// up as a hard failure instead of a silently-wrong delta.
///
/// Every invocation, read or write, is appended to `argv_log` as one
/// space-joined `"$*"` line, so tests can assert on argv substrings without
/// caring about `gh`'s own quoting conventions.
fn fake_gh(
    dir: &Path,
    argv_log: &Path,
    repo: &str,
    repo_json: &str,
    workflow_json: &str,
    fork_json: Option<&str>,
    private_fork_json: Option<&str>,
) {
    let repo_path = format!("repos/{repo}");
    let workflow_path = format!("repos/{repo}/actions/permissions/workflow");
    let fork_path = format!("repos/{repo}/actions/permissions/fork-pr-contributor-approval");
    let private_fork_path =
        format!("repos/{repo}/actions/permissions/fork-pr-workflows-private-repos");

    let fork_arm = match fork_json {
        Some(json) => format!("  'api {fork_path}') printf '%s\\n' '{json}' ;;\n"),
        None => format!(
            "  'api {fork_path}') echo 'fork-pr-contributor-approval must not be read for a non-public repo' >&2; exit 99 ;;\n"
        ),
    };
    let private_fork_arm = match private_fork_json {
        Some(json) => format!("  'api {private_fork_path}') printf '%s\\n' '{json}' ;;\n"),
        None => format!(
            "  'api {private_fork_path}') echo 'fork-pr-workflows-private-repos must not be read for a public repo' >&2; exit 99 ;;\n"
        ),
    };

    // `printf`/`echo`/`case` are dash builtins, unlike `cat`: PATH is
    // deliberately just this one directory (see `setup`), so any external
    // command the script reached for -- `cat` included -- would fail with
    // "not found" rather than falling through to the real one on the host.
    let body = format!(
        r#"printf '%s\n' "$*" >> '{log}'

# Any write (-X PATCH / -X PUT) succeeds unconditionally: the exact fields
# each write carries are asserted by the test reading the argv log
# afterward, not by this fake rejecting a bad one up front.
if [ "$2" = "-X" ]; then
    exit 0
fi

case "$*" in
  'api {repo_path}') printf '%s\n' '{repo_json}' ;;
  'api {workflow_path}') printf '%s\n' '{workflow_json}' ;;
{fork_arm}{private_fork_arm}  *) echo "unexpected gh call: $*" >&2; exit 99 ;;
esac
"#,
        log = argv_log.display(),
    );

    fake_bin(dir, "gh", &body);
}

/// Set up a temp dir with a fake `gh` on a private `PATH`, ready to run
/// `kd gh repo apply-preferred-settings` against it. Returns the temp dir
/// (kept alive for the caller) and the argv-log path.
fn setup(
    repo: &str,
    repo_json: &str,
    workflow_json: &str,
    fork_json: Option<&str>,
    private_fork_json: Option<&str>,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let argv_log = dir.path().join("gh-argv");
    fs::write(&argv_log, "").unwrap();

    fake_gh(
        &bin,
        &argv_log,
        repo,
        repo_json,
        workflow_json,
        fork_json,
        private_fork_json,
    );

    (dir, argv_log)
}

/// Run `kd gh repo apply-preferred-settings <repo> [extra_args...]` with the
/// fake `gh` from `setup` shadowing the real one, returning captured output.
fn run_kd(dir: &tempfile::TempDir, repo: &str, extra_args: &[&str]) -> Output {
    let bin = dir.path().join("bin");
    Command::new(env!("CARGO_BIN_EXE_kd"))
        .args(["gh", "repo", "apply-preferred-settings", repo])
        .args(extra_args)
        // Child-only environment: a private PATH so the fake gh shadows the
        // real one. No credentials are needed -- this command shells out to
        // `gh` directly rather than preflighting anything itself.
        .env_clear()
        .env("PATH", bin)
        .output()
        .unwrap()
}

/// Preferred repo-settings JSON body, with `visibility` swappable per test.
fn repo_json(visibility: &str) -> String {
    format!(
        r#"{{
            "allow_merge_commit": false,
            "allow_squash_merge": true,
            "squash_merge_commit_title": "PR_TITLE",
            "squash_merge_commit_message": "PR_BODY",
            "allow_rebase_merge": false,
            "delete_branch_on_merge": true,
            "visibility": "{visibility}"
        }}"#
    )
}

const WORKFLOW_PREFERRED: &str =
    r#"{"default_workflow_permissions": "read", "can_approve_pull_request_reviews": false}"#;
const WORKFLOW_DRIFTED: &str =
    r#"{"default_workflow_permissions": "write", "can_approve_pull_request_reviews": false}"#;
const FORK_PREFERRED: &str = r#"{"approval_policy": "all_external_contributors"}"#;
const FORK_DRIFTED: &str = r#"{"approval_policy": "first_time_contributors"}"#;
const PRIVATE_FORK_WORKFLOWS_PREFERRED: &str = r#"{
    "run_workflows_from_fork_pull_requests": false,
    "send_write_tokens_to_workflows": false,
    "send_secrets_and_variables": false,
    "require_approval_for_fork_pr_workflows": false
}"#;
const PRIVATE_FORK_WORKFLOWS_DRIFTED: &str = r#"{
    "run_workflows_from_fork_pull_requests": true,
    "send_write_tokens_to_workflows": false,
    "send_secrets_and_variables": false,
    "require_approval_for_fork_pr_workflows": false
}"#;

/// F18: a private repo's `--dry-run` must read the repo-settings,
/// workflow-permissions, and private-fork-workflows endpoints but must
/// never even request the public-only fork-PR-approval endpoint -- unlike
/// the other settings, that one isn't merely "not written" for a private
/// repo, it's not applicable at all, and the real GitHub API 422s if you
/// try. The fake `gh` fails the test outright if that path is requested,
/// which is a stronger guarantee than asserting its *absence* from the log
/// after the fact would give alone.
#[test]
fn private_repo_dry_run_never_reads_fork_pr_approval() {
    let repo = "owner/private-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("private"),
        WORKFLOW_PREFERRED,
        None,
        Some(PRIVATE_FORK_WORKFLOWS_PREFERRED),
    );

    let output = run_kd(&dir, repo, &["--dry-run"]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    assert!(
        log.contains("api repos/owner/private-repo"),
        "repo GET missing: {log}"
    );
    assert!(
        log.contains("api repos/owner/private-repo/actions/permissions/workflow"),
        "workflow GET missing: {log}"
    );
    assert!(
        log.contains(
            "api repos/owner/private-repo/actions/permissions/fork-pr-workflows-private-repos"
        ),
        "private-fork-workflows GET missing: {log}"
    );
    assert!(
        !log.contains("fork-pr-contributor-approval"),
        "fork-approval endpoint must never be requested for a private repo: {log}"
    );
}

/// F19: a public repo's `--dry-run` with only workflow-permission drift
/// must still read the workflow-permissions endpoint (proving the read
/// actually happens at the command boundary, not just in unit tests
/// against a hand-built `LiveSettings`) and must report the drift and the
/// dry-run message on stderr -- and, critically, must not issue any write.
/// `setup`'s `private_fork_json: None` also means the fake fails the test
/// outright if `get_settings` mistakenly requests the private-repo
/// fork-workflows endpoint for this public repo.
#[test]
fn public_repo_dry_run_reports_workflow_drift_without_writing() {
    let repo = "owner/public-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("public"),
        WORKFLOW_DRIFTED,
        Some(FORK_PREFERRED),
        None,
    );

    let output = run_kd(&dir, repo, &["--dry-run"]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}",
        output.status
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    assert!(
        log.contains("api repos/owner/public-repo/actions/permissions/workflow"),
        "workflow GET missing: {log}"
    );
    assert!(
        !log.contains("-X "),
        "dry-run must not issue any write: {log}"
    );

    // tracing logs to stderr by default (see src/main.rs); INFO is the
    // default level, so no -v flag is needed to see these.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("default_workflow_permissions: write -> read"),
        "missing delta line in stderr: {stderr}"
    );
    assert!(
        stderr.contains("Dry run: would apply"),
        "missing dry-run message in stderr: {stderr}"
    );
}

/// F20 + F1: a public repo with *only* workflow-permission drift, applied
/// for real, must issue exactly one write -- the workflow-permissions PUT,
/// with the exact fields `apply_settings` is supposed to send -- and must
/// NOT touch the merge-settings or fork-approval endpoints. Before the
/// delta-driven plan (F1), every apply blindly re-asserted merge settings
/// and workflow permissions regardless of what actually drifted; this test
/// pins the fix at the process boundary, not just in the `planned_writes`
/// unit tests.
#[test]
fn public_repo_apply_writes_only_the_drifted_workflow_endpoint() {
    let repo = "owner/public-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("public"),
        WORKFLOW_DRIFTED,
        Some(FORK_PREFERRED),
        None,
    );

    let output = run_kd(&dir, repo, &[]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    let writes: Vec<&str> = log.lines().filter(|line| line.contains("-X ")).collect();
    assert_eq!(
        writes,
        vec![
            "api -X PUT repos/owner/public-repo/actions/permissions/workflow -f default_workflow_permissions=read -F can_approve_pull_request_reviews=false"
        ],
        "expected exactly one workflow-permissions PUT and nothing else: {log}"
    );
}

/// F21: a public repo with *only* fork-PR-approval drift, applied for
/// real, must issue exactly one write -- the fork-approval PUT with the
/// exact policy field -- and touch nothing else. This is the write
/// counterpart to F20: together they pin the endpoint, verb, and field
/// name for both of the security-relevant Actions settings, not just the
/// pre-existing merge-settings write.
#[test]
fn public_repo_apply_writes_only_the_drifted_fork_approval_endpoint() {
    let repo = "owner/public-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("public"),
        WORKFLOW_PREFERRED,
        Some(FORK_DRIFTED),
        None,
    );

    let output = run_kd(&dir, repo, &[]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    let writes: Vec<&str> = log.lines().filter(|line| line.contains("-X ")).collect();
    assert_eq!(
        writes,
        vec![
            "api -X PUT repos/owner/public-repo/actions/permissions/fork-pr-contributor-approval -f approval_policy=all_external_contributors"
        ],
        "expected exactly one fork-approval PUT and nothing else: {log}"
    );
}

/// A public repo that already matches every preferred setting, applied
/// with `--force`, must still issue all three writes -- merge settings,
/// workflow permissions, and fork approval -- in that fixed order, even
/// though nothing drifted. `--force`'s whole purpose is to re-assert
/// configuration unconditionally (e.g. after a suspected out-of-band
/// change), so a delta-driven plan that skipped a group just because it
/// matched would silently defeat the flag.
#[test]
fn public_repo_force_apply_issues_all_writes_in_order_even_without_drift() {
    let repo = "owner/public-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("public"),
        WORKFLOW_PREFERRED,
        Some(FORK_PREFERRED),
        None,
    );

    let output = run_kd(&dir, repo, &["--force"]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    let writes: Vec<&str> = log.lines().filter(|line| line.contains("-X ")).collect();
    assert_eq!(
        writes,
        vec![
            "api -X PATCH repos/owner/public-repo -F allow_merge_commit=false -F allow_squash_merge=true -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=PR_BODY -F allow_rebase_merge=false -F delete_branch_on_merge=true",
            "api -X PUT repos/owner/public-repo/actions/permissions/workflow -f default_workflow_permissions=read -F can_approve_pull_request_reviews=false",
            "api -X PUT repos/owner/public-repo/actions/permissions/fork-pr-contributor-approval -f approval_policy=all_external_contributors",
        ],
        "expected merge PATCH, then workflow PUT, then fork-approval PUT, in that order: {log}"
    );
}

/// A private repo with *only* private-fork-workflows drift
/// (`run_workflows_from_fork_pull_requests: true`), applied for real, must
/// issue exactly one write -- the private-fork-workflows PUT with all four
/// fields forced to `false` -- and touch nothing else. This is the
/// private-repo counterpart to `public_repo_apply_writes_only_the_drifted_fork_approval_endpoint`:
/// together they pin the write for whichever of the two mirror-image
/// fork-workflow endpoints applies to a given repo's visibility.
/// `setup`'s `fork_json: None` also means the fake fails the test outright
/// if `get_settings` mistakenly requests the public-only fork-approval
/// endpoint for this private repo.
#[test]
fn private_repo_apply_writes_only_the_drifted_private_fork_workflows_endpoint() {
    let repo = "owner/private-repo";
    let (dir, argv_log) = setup(
        repo,
        &repo_json("private"),
        WORKFLOW_PREFERRED,
        None,
        Some(PRIVATE_FORK_WORKFLOWS_DRIFTED),
    );

    let output = run_kd(&dir, repo, &[]);
    assert!(
        output.status.success(),
        "kd exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&argv_log).unwrap();
    let writes: Vec<&str> = log.lines().filter(|line| line.contains("-X ")).collect();
    assert_eq!(
        writes,
        vec![
            "api -X PUT repos/owner/private-repo/actions/permissions/fork-pr-workflows-private-repos -F run_workflows_from_fork_pull_requests=false -F send_write_tokens_to_workflows=false -F send_secrets_and_variables=false -F require_approval_for_fork_pr_workflows=false"
        ],
        "expected exactly one private-fork-workflows PUT and nothing else: {log}"
    );
}
