//! Create or update a GitHub repository ruleset that protects the default
//! branch. The ruleset enforces linear history (no merge commits), blocks
//! force-pushes, and optionally requires selected CI status checks to pass
//! before merging.
//!
//! Uses the GitHub Rulesets API (not the older Branch Protection API) because
//! rulesets are more flexible and can apply to `~DEFAULT_BRANCH` symbolically.

use super::{MainProtectArgs, resolve_repo};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use tracing::info;
use xshell::{Shell, cmd};

// ── GitHub Rulesets API types ──────────────────────────────────────────

/// Abbreviated ruleset listing used to find our ruleset by name.
#[derive(Deserialize)]
struct RulesetSummary {
    id: u64,
    name: String,
}

/// Full ruleset detail, needed to inspect whether an existing ruleset
/// actually protects the default branch with the required base rules.
#[derive(Deserialize)]
struct RulesetDetail {
    target: String,
    enforcement: String,
    conditions: Value,
    rules: Vec<RuleRaw>,
}

/// A single rule within a ruleset. The `parameters` value is rule-type
/// specific, so we keep it as raw JSON.
#[derive(Deserialize, Clone)]
struct RuleRaw {
    #[serde(rename = "type")]
    rule_type: String,
    parameters: Option<Value>,
}

/// Typed view of the parameters for a `required_status_checks` rule.
#[derive(Deserialize)]
struct StatusCheckParameters {
    required_status_checks: Vec<StatusCheckParam>,
}

/// A single required status check entry. `integration_id` is the GitHub
/// App that owns the check; we omit it when creating checks via the CLI
/// since GitHub fills it in automatically.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, Clone)]
struct StatusCheckParam {
    context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    integration_id: Option<i64>,
}

/// Subset of the check-runs API response, used to discover available
/// CI check names for the interactive picker.
#[derive(Deserialize)]
struct CheckRun {
    name: String,
    details_url: Option<String>,
}

/// Minimal workflow run metadata used to make GitHub Actions checks look
/// like the checks list in the GitHub UI without changing the stored
/// required-check context.
#[derive(Deserialize)]
struct WorkflowRun {
    name: Option<String>,
    event: String,
}

/// A single selectable status check. `context` is the value GitHub rulesets
/// store and enforce; `display_name` is only for the interactive picker.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AvailableCheck {
    context: String,
    display_name: String,
}

// ── Request payloads ──────────────────────────────────────────────────

#[derive(Serialize)]
struct CreateRulesetPayload {
    name: String,
    target: String,
    enforcement: String,
    conditions: Value,
    rules: Vec<RulePayload>,
}

#[derive(Serialize)]
struct UpdateRulesetPayload {
    enforcement: String,
    conditions: Value,
    rules: Vec<RulePayload>,
}

#[derive(Serialize)]
struct RulePayload {
    #[serde(rename = "type")]
    rule_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Value>,
}

const RULESET_NAME: &str = "main-protect";

/// The ruleset applies to the repo's default branch (usually `main`).
/// `~DEFAULT_BRANCH` is a GitHub symbolic ref that tracks renames.
fn conditions() -> Value {
    serde_json::json!({
        "ref_name": {
            "include": ["~DEFAULT_BRANCH"],
            "exclude": []
        }
    })
}

/// Build a `required_status_checks` rule payload from the selected checks.
fn make_status_checks_rule(checks: &[StatusCheckParam]) -> RulePayload {
    RulePayload {
        rule_type: "required_status_checks".to_string(),
        parameters: Some(serde_json::json!({
            "required_status_checks": checks,
            "strict_required_status_checks_policy": false,
        })),
    }
}

/// The non-negotiable rules every protected branch gets: linear history
/// (forces squash/rebase merges) and no force-pushes.
fn base_rules() -> Vec<RulePayload> {
    vec![
        RulePayload {
            rule_type: "required_linear_history".to_string(),
            parameters: None,
        },
        RulePayload {
            rule_type: "non_fast_forward".to_string(),
            parameters: None,
        },
    ]
}

/// Combine base rules with optional status checks into a full rule set.
fn rules_with_checks(checks: &[StatusCheckParam]) -> Vec<RulePayload> {
    let mut rules = base_rules();
    // GitHub rejects required_status_checks with an empty checks list (HTTP 422),
    // so we only include the rule when there are checks to enforce.
    if !checks.is_empty() {
        rules.push(make_status_checks_rule(checks));
    }
    rules
}

/// Look up our ruleset by name; returns its ID if it already exists.
fn find_ruleset(sh: &Shell, repo: &str) -> anyhow::Result<Option<u64>> {
    let output = cmd!(sh, "gh api repos/{repo}/rulesets").read()?;
    let rulesets: Vec<RulesetSummary> = serde_json::from_str(&output)?;
    Ok(rulesets
        .into_iter()
        .find(|r| r.name == RULESET_NAME)
        .map(|r| r.id))
}

/// Fetch the full detail of a ruleset so we can inspect its current state.
fn get_ruleset(sh: &Shell, repo: &str, id: u64) -> anyhow::Result<RulesetDetail> {
    let id_str = id.to_string();
    let output = cmd!(sh, "gh api repos/{repo}/rulesets/{id_str}").read()?;
    Ok(serde_json::from_str(&output)?)
}

/// Create a new ruleset with the base rules (no status checks yet).
/// Returns the newly created ruleset's ID.
fn create_ruleset(sh: &Shell, repo: &str) -> anyhow::Result<u64> {
    let payload = CreateRulesetPayload {
        name: RULESET_NAME.to_string(),
        target: "branch".to_string(),
        enforcement: "active".to_string(),
        conditions: conditions(),
        rules: base_rules(),
    };
    let body = serde_json::to_string(&payload)?;
    let output = cmd!(sh, "gh api -X POST repos/{repo}/rulesets --input -")
        .stdin(body)
        .read()?;
    let created: RulesetSummary = serde_json::from_str(&output)?;
    Ok(created.id)
}

/// Overwrite the ruleset with the full set of rules (base + checks).
/// This is a PUT, so it replaces everything — we always include all rules.
fn update_ruleset(
    sh: &Shell,
    repo: &str,
    id: u64,
    checks: &[StatusCheckParam],
) -> anyhow::Result<()> {
    let payload = UpdateRulesetPayload {
        enforcement: "active".to_string(),
        conditions: conditions(),
        rules: rules_with_checks(checks),
    };
    let body = serde_json::to_string(&payload)?;
    let id_str = id.to_string();
    cmd!(sh, "gh api -X PUT repos/{repo}/rulesets/{id_str} --input -")
        .stdin(body)
        .ignore_stdout()
        .run()?;
    Ok(())
}

/// Check whether a specific rule type is present in the ruleset.
fn has_rule(detail: &RulesetDetail, rule_type: &str) -> bool {
    detail.rules.iter().any(|r| r.rule_type == rule_type)
}

/// Determine whether the existing ruleset is missing any of the base
/// protections and needs to be updated to bring it into compliance.
fn needs_fix(detail: &RulesetDetail) -> bool {
    detail.target != "branch"
        || detail.enforcement != "active"
        || detail.conditions != conditions()
        || !has_rule(detail, "required_linear_history")
        || !has_rule(detail, "non_fast_forward")
}

/// Pull out the currently-configured required status checks from an
/// existing ruleset, so we can preserve them across updates.
fn extract_current_checks(detail: &RulesetDetail) -> anyhow::Result<Vec<StatusCheckParam>> {
    let Some(rule) = detail
        .rules
        .iter()
        .find(|rule| rule.rule_type == "required_status_checks")
    else {
        return Ok(Vec::new());
    };

    let parsed = serde_json::from_value::<StatusCheckParameters>(
        rule.parameters
            .clone()
            .context("required_status_checks rule is missing parameters")?,
    )
    .context("failed to parse required_status_checks parameters")?;

    Ok(parsed.required_status_checks)
}

/// Ask GitHub for the repo's default branch name (usually `main`).
fn get_default_branch(sh: &Shell, repo: &str) -> anyhow::Result<String> {
    let output = cmd!(sh, "gh api repos/{repo} --jq .default_branch").read()?;
    Ok(output.trim().to_string())
}

/// List the CI check runs that ran against a specific commit/ref.
fn get_check_runs_for_ref(sh: &Shell, repo: &str, git_ref: &str) -> anyhow::Result<Vec<CheckRun>> {
    let output = cmd!(
        sh,
        "gh api repos/{repo}/commits/{git_ref}/check-runs --jq .check_runs"
    )
    .read()?;
    Ok(serde_json::from_str(&output)?)
}

/// Find the head commit SHA from a recent merged PR, if any.
/// Used to discover PR-only checks that don't run on the default branch.
fn get_latest_merged_pr_sha(sh: &Shell, repo: &str) -> anyhow::Result<Option<String>> {
    let output = cmd!(
        sh,
        "gh pr list --repo {repo} --state merged --limit 1 --json headRefOid --jq .[0].headRefOid"
    )
    .read()?;
    let sha = output.trim();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha.to_string()))
    }
}

/// Parse a workflow run ID out of a GitHub Actions details URL.
fn parse_workflow_run_id(details_url: &str) -> Option<u64> {
    let mut parts = details_url.split('/');
    while let Some(part) = parts.next() {
        if part == "runs" {
            return parts.next()?.parse().ok();
        }
    }
    None
}

/// Build a display label that matches GitHub's checks UI more closely while
/// keeping the underlying ruleset context unchanged.
fn format_check_display_name(context: &str, workflow_run: Option<&WorkflowRun>) -> String {
    match workflow_run {
        Some(run) => match run.name.as_deref() {
            Some(workflow_name) if !workflow_name.is_empty() => {
                format!("{workflow_name} / {context} ({})", run.event)
            }
            _ => context.to_string(),
        },
        _ => context.to_string(),
    }
}

/// Fetch workflow-run metadata for each unique GitHub Actions run referenced
/// by the discovered check runs. Failures are ignored because the metadata is
/// only used to improve display labels.
fn get_workflow_runs(sh: &Shell, repo: &str, check_runs: &[CheckRun]) -> HashMap<u64, WorkflowRun> {
    let run_ids: HashSet<u64> = check_runs
        .iter()
        .filter_map(|check_run| check_run.details_url.as_deref())
        .filter_map(parse_workflow_run_id)
        .collect();

    let mut workflow_runs = HashMap::new();
    for run_id in run_ids {
        let run_id_str = run_id.to_string();
        let Ok(output) = cmd!(sh, "gh api repos/{repo}/actions/runs/{run_id_str}").read() else {
            continue;
        };
        let Ok(workflow_run) = serde_json::from_str::<WorkflowRun>(&output) else {
            continue;
        };
        workflow_runs.insert(run_id, workflow_run);
    }
    workflow_runs
}

/// Convert raw check runs into selectable check entries, preserving the
/// ruleset context while enriching the display label where possible.
fn build_available_checks(
    check_runs: Vec<CheckRun>,
    workflow_runs: &HashMap<u64, WorkflowRun>,
) -> Vec<AvailableCheck> {
    let mut checks_by_context: HashMap<String, AvailableCheck> = HashMap::new();

    for check_run in check_runs {
        let workflow_run = check_run
            .details_url
            .as_deref()
            .and_then(parse_workflow_run_id)
            .and_then(|run_id| workflow_runs.get(&run_id));
        let available = AvailableCheck {
            context: check_run.name.clone(),
            display_name: format_check_display_name(&check_run.name, workflow_run),
        };

        checks_by_context
            .entry(available.context.clone())
            .and_modify(|existing| {
                if existing.display_name == existing.context
                    && available.display_name != available.context
                {
                    existing.display_name = available.display_name.clone();
                }
            })
            .or_insert(available);
    }

    let mut checks: Vec<AvailableCheck> = checks_by_context.into_values().collect();
    checks.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    checks
}

/// Collect check names from both the default branch HEAD and the latest merged PR.
/// The default branch only has CI checks; PR-triggered checks (e.g. conventional-commit)
/// never run on main, so we need the PR's head commit to discover those.
fn get_available_checks(
    sh: &Shell,
    repo: &str,
    branch: &str,
) -> anyhow::Result<Vec<AvailableCheck>> {
    let mut check_runs = get_check_runs_for_ref(sh, repo, branch)?;

    if let Some(sha) = get_latest_merged_pr_sha(sh, repo)? {
        check_runs.extend(get_check_runs_for_ref(sh, repo, &sha)?);
    }

    let workflow_runs = get_workflow_runs(sh, repo, &check_runs);
    Ok(build_available_checks(check_runs, &workflow_runs))
}

/// Present an interactive menu of available CI checks. The user can
/// add/remove individual checks by number, select `all`/`none`, or
/// press Enter to leave things unchanged. Returns `None` if the user
/// chose to skip (empty input).
fn prompt_for_checks(
    available: &[AvailableCheck],
    current_blocking: &[StatusCheckParam],
) -> anyhow::Result<Option<Vec<StatusCheckParam>>> {
    let blocking_set: HashSet<&str> = current_blocking
        .iter()
        .map(|c| c.context.as_str())
        .collect();

    println!();
    for (i, check) in available.iter().enumerate() {
        let tag = if blocking_set.contains(check.context.as_str()) {
            "[BLOCKING]"
        } else {
            "[ ]"
        };
        println!("  {}. {} {}", i + 1, tag, check.display_name);
    }
    println!();

    print!("Blocking checks (e.g. 1,4,-7 to add 1,4 and remove 7; all; none; or empty to skip): ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    parse_check_selection(input, available, current_blocking)
}

/// Parse the status-check picker input without touching stdin/stdout.
///
/// Empty input means "leave the ruleset unchanged" and returns `None`.
/// `none` clears all required checks, `all` selects every discoverable
/// check plus any currently blocking check that was not rediscovered, and
/// comma-separated numbers add/remove entries relative to the current
/// blocking set. A leading `-` removes a check.
fn parse_check_selection(
    input: &str,
    available: &[AvailableCheck],
    current_blocking: &[StatusCheckParam],
) -> anyhow::Result<Option<Vec<StatusCheckParam>>> {
    if input.is_empty() {
        return Ok(None);
    }

    if input == "none" {
        return Ok(Some(Vec::new()));
    }

    if input == "all" {
        let result: HashSet<&str> = available
            .iter()
            .map(|check| check.context.as_str())
            .chain(current_blocking.iter().map(|check| check.context.as_str()))
            .collect();
        return Ok(Some(selected_checks_from_contexts(
            &result,
            available,
            current_blocking,
        )));
    }

    // Start from current blocking set, then apply additions/removals.
    // Comma-separated entries; a leading '-' means remove, otherwise add.
    let mut result: HashSet<&str> = current_blocking
        .iter()
        .map(|c| c.context.as_str())
        .collect();
    for part in input.split(',') {
        let part = part.trim();
        let (remove, num_str) = match part.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, part),
        };
        let num: usize = num_str.parse().context("invalid number in selection")?;
        if num == 0 || num > available.len() {
            bail!("number {} out of range (1-{})", num, available.len());
        }
        let name = available[num - 1].context.as_str();
        if remove {
            result.remove(name);
        } else {
            result.insert(name);
        }
    }

    Ok(Some(selected_checks_from_contexts(
        &result,
        available,
        current_blocking,
    )))
}

/// Build the ruleset check list from selected context names.
///
/// Undiscovered checks that are already blocking keep their original
/// `integration_id`; rediscovered checks intentionally omit it because
/// GitHub can fill that value in from the check context.
fn selected_checks_from_contexts(
    selected_contexts: &HashSet<&str>,
    available: &[AvailableCheck],
    current_blocking: &[StatusCheckParam],
) -> Vec<StatusCheckParam> {
    let available_contexts: HashSet<&str> = available
        .iter()
        .map(|check| check.context.as_str())
        .collect();
    let mut selected: Vec<StatusCheckParam> = current_blocking
        .iter()
        .filter(|check| {
            !available_contexts.contains(check.context.as_str())
                && selected_contexts.contains(check.context.as_str())
        })
        .cloned()
        .collect();

    selected.extend(
        available
            .iter()
            .filter(|check| selected_contexts.contains(check.context.as_str()))
            .map(|check| StatusCheckParam {
                context: check.context.clone(),
                integration_id: None,
            }),
    );

    selected
}

pub fn run(args: MainProtectArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;
    let repo = resolve_repo(args.repo.as_deref(), &std::env::current_dir()?)?;

    let ruleset_id = match find_ruleset(&sh, &repo)? {
        Some(id) => {
            info!("Found existing '{}' ruleset (id={})", RULESET_NAME, id);
            let detail = get_ruleset(&sh, &repo, id)?;
            let current_checks = extract_current_checks(&detail)?;
            if needs_fix(&detail) {
                info!("Fixing ruleset enforcement/rules...");
                update_ruleset(&sh, &repo, id, &current_checks)?;
            }
            id
        }
        None => {
            info!("Creating '{}' ruleset...", RULESET_NAME);
            let id = create_ruleset(&sh, &repo)?;
            info!("Created ruleset (id={})", id);
            id
        }
    };

    let default_branch = get_default_branch(&sh, &repo)?;
    let available_checks = get_available_checks(&sh, &repo, &default_branch)?;

    if available_checks.is_empty() {
        info!(
            "No check runs found on '{}'; skipping status check selection",
            default_branch
        );
        return Ok(());
    }

    let detail = get_ruleset(&sh, &repo, ruleset_id)?;
    let current_checks = extract_current_checks(&detail)?;

    if let Some(selected) = prompt_for_checks(&available_checks, &current_checks)? {
        update_ruleset(&sh, &repo, ruleset_id, &selected)?;
        info!(
            "Updated required status checks ({} blocking)",
            selected.len()
        );
    } else {
        info!("No changes to status checks");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_workflow_run_id_reads_actions_run_urls() {
        assert_eq!(
            parse_workflow_run_id(
                "https://github.com/example-owner/example-repo/actions/runs/123456789/job/1"
            ),
            Some(123456789)
        );
    }

    #[test]
    fn parse_workflow_run_id_rejects_non_actions_urls() {
        assert_eq!(
            parse_workflow_run_id("https://github.com/example-owner/example-repo/checks/123"),
            None
        );
    }

    #[test]
    fn format_check_display_name_uses_workflow_and_event() {
        let workflow_run = WorkflowRun {
            name: Some("CI".to_string()),
            event: "push".to_string(),
        };

        assert_eq!(
            format_check_display_name("clippy", Some(&workflow_run)),
            "CI / clippy (push)"
        );
    }

    #[test]
    fn rules_with_checks_omits_status_checks_when_empty() {
        let rules = rules_with_checks(&[]);
        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .all(|r| r.rule_type != "required_status_checks")
        );
    }

    #[test]
    fn rules_with_checks_includes_status_checks_when_present() {
        let checks = vec![StatusCheckParam {
            context: "ci/test".to_string(),
            integration_id: None,
        }];
        let rules = rules_with_checks(&checks);
        assert_eq!(rules.len(), 3);
        assert!(
            rules
                .iter()
                .any(|r| r.rule_type == "required_status_checks")
        );
    }

    fn ruleset_detail(enforcement: &str, rule_types: &[&str]) -> RulesetDetail {
        RulesetDetail {
            target: "branch".to_string(),
            enforcement: enforcement.to_string(),
            conditions: conditions(),
            rules: rule_types
                .iter()
                .map(|t| RuleRaw {
                    rule_type: t.to_string(),
                    parameters: None,
                })
                .collect(),
        }
    }

    #[test]
    fn needs_fix_false_when_all_base_rules_active() {
        let detail = ruleset_detail("active", &["required_linear_history", "non_fast_forward"]);
        assert!(!needs_fix(&detail));
    }

    #[test]
    fn needs_fix_true_when_enforcement_disabled() {
        let detail = ruleset_detail("disabled", &["required_linear_history", "non_fast_forward"]);
        assert!(needs_fix(&detail));
    }

    #[test]
    fn needs_fix_true_when_target_is_not_branch() {
        let mut detail = ruleset_detail("active", &["required_linear_history", "non_fast_forward"]);
        detail.target = "tag".to_string();

        assert!(needs_fix(&detail));
    }

    #[test]
    fn needs_fix_true_when_conditions_do_not_target_default_branch() {
        let mut detail = ruleset_detail("active", &["required_linear_history", "non_fast_forward"]);
        detail.conditions = serde_json::json!({
            "ref_name": {
                "include": ["refs/heads/release"],
                "exclude": []
            }
        });

        assert!(needs_fix(&detail));
    }

    #[test]
    fn needs_fix_true_when_linear_history_missing() {
        let detail = ruleset_detail("active", &["non_fast_forward"]);
        assert!(needs_fix(&detail));
    }

    #[test]
    fn needs_fix_true_when_non_fast_forward_missing() {
        let detail = ruleset_detail("active", &["required_linear_history"]);
        assert!(needs_fix(&detail));
    }

    #[test]
    fn extract_current_checks_returns_empty_when_no_status_check_rule() {
        let detail = ruleset_detail("active", &["required_linear_history"]);
        assert!(extract_current_checks(&detail).unwrap().is_empty());
    }

    #[test]
    fn extract_current_checks_returns_configured_checks() {
        let detail = RulesetDetail {
            target: "branch".to_string(),
            enforcement: "active".to_string(),
            conditions: conditions(),
            rules: vec![RuleRaw {
                rule_type: "required_status_checks".to_string(),
                parameters: Some(serde_json::json!({
                    "required_status_checks": [
                        {"context": "ci/test", "integration_id": 1},
                        {"context": "ci/lint"}
                    ]
                })),
            }],
        };
        let checks = extract_current_checks(&detail).unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].context, "ci/test");
        assert_eq!(checks[1].context, "ci/lint");
    }

    #[test]
    fn extract_current_checks_errors_when_parameters_are_missing() {
        let detail = RulesetDetail {
            target: "branch".to_string(),
            enforcement: "active".to_string(),
            conditions: conditions(),
            rules: vec![RuleRaw {
                rule_type: "required_status_checks".to_string(),
                parameters: None,
            }],
        };
        let err = extract_current_checks(&detail).unwrap_err();

        assert!(
            err.to_string()
                .contains("required_status_checks rule is missing parameters"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn extract_current_checks_errors_on_malformed_parameters() {
        let detail = RulesetDetail {
            target: "branch".to_string(),
            enforcement: "active".to_string(),
            conditions: conditions(),
            rules: vec![RuleRaw {
                rule_type: "required_status_checks".to_string(),
                parameters: Some(serde_json::json!({
                    "required_status_checks": "not a list"
                })),
            }],
        };
        let err = extract_current_checks(&detail).unwrap_err();

        assert!(
            err.to_string()
                .contains("failed to parse required_status_checks parameters"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn build_available_checks_prefers_richer_display_names() {
        let check_runs = vec![
            CheckRun {
                name: "clippy".to_string(),
                details_url: None,
            },
            CheckRun {
                name: "clippy".to_string(),
                details_url: Some(
                    "https://github.com/example-owner/example-repo/actions/runs/42/job/100"
                        .to_string(),
                ),
            },
            CheckRun {
                name: "fmt".to_string(),
                details_url: Some(
                    "https://github.com/example-owner/example-repo/actions/runs/99/job/101"
                        .to_string(),
                ),
            },
        ];
        let workflow_runs = HashMap::from([
            (
                42,
                WorkflowRun {
                    name: Some("CI".to_string()),
                    event: "push".to_string(),
                },
            ),
            (
                99,
                WorkflowRun {
                    name: Some("CI".to_string()),
                    event: "pull_request".to_string(),
                },
            ),
        ]);

        assert_eq!(
            build_available_checks(check_runs, &workflow_runs),
            vec![
                AvailableCheck {
                    context: "clippy".to_string(),
                    display_name: "CI / clippy (push)".to_string(),
                },
                AvailableCheck {
                    context: "fmt".to_string(),
                    display_name: "CI / fmt (pull_request)".to_string(),
                },
            ]
        );
    }

    fn available_check(context: &str) -> AvailableCheck {
        AvailableCheck {
            context: context.to_string(),
            display_name: context.to_string(),
        }
    }

    fn status_check(context: &str) -> StatusCheckParam {
        StatusCheckParam {
            context: context.to_string(),
            integration_id: None,
        }
    }

    fn status_check_with_integration(context: &str, integration_id: i64) -> StatusCheckParam {
        StatusCheckParam {
            context: context.to_string(),
            integration_id: Some(integration_id),
        }
    }

    #[test]
    fn parse_check_selection_empty_input_skips_update() {
        let available = vec![available_check("ci/test")];

        assert_eq!(parse_check_selection("", &available, &[]).unwrap(), None);
    }

    #[test]
    fn parse_check_selection_none_clears_blocking_checks() {
        let available = vec![available_check("ci/test")];

        assert!(
            parse_check_selection("none", &available, &[status_check("ci/test")])
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_check_selection_all_selects_every_available_check() {
        let available = vec![available_check("ci/test"), available_check("ci/lint")];
        let selected = parse_check_selection("all", &available, &[])
            .unwrap()
            .unwrap();

        assert_eq!(selected[0].context, "ci/test");
        assert_eq!(selected[1].context, "ci/lint");
    }

    #[test]
    fn parse_check_selection_all_preserves_undiscovered_blocking_checks() {
        let available = vec![available_check("ci/test"), available_check("ci/lint")];
        let current = vec![status_check_with_integration("external/deploy", 123)];
        let selected = parse_check_selection("all", &available, &current)
            .unwrap()
            .unwrap();

        assert_eq!(
            selected,
            vec![
                StatusCheckParam {
                    context: "external/deploy".to_string(),
                    integration_id: Some(123),
                },
                StatusCheckParam {
                    context: "ci/test".to_string(),
                    integration_id: None,
                },
                StatusCheckParam {
                    context: "ci/lint".to_string(),
                    integration_id: None,
                },
            ]
        );
    }

    #[test]
    fn parse_check_selection_adds_and_removes_by_number() {
        let available = vec![
            available_check("ci/test"),
            available_check("ci/lint"),
            available_check("ci/fmt"),
        ];
        let current = vec![status_check("ci/test"), status_check("ci/lint")];
        let selected = parse_check_selection("3,-1", &available, &current)
            .unwrap()
            .unwrap();
        let contexts: Vec<_> = selected.into_iter().map(|check| check.context).collect();

        assert_eq!(contexts, vec!["ci/lint", "ci/fmt"]);
    }

    #[test]
    fn parse_check_selection_preserves_undiscovered_blocking_checks() {
        let available = vec![available_check("ci/test"), available_check("ci/lint")];
        let current = vec![status_check("external/deploy"), status_check("ci/test")];
        let selected = parse_check_selection("2", &available, &current)
            .unwrap()
            .unwrap();
        let contexts: Vec<_> = selected.into_iter().map(|check| check.context).collect();

        assert_eq!(contexts, vec!["external/deploy", "ci/test", "ci/lint"]);
    }

    #[test]
    fn parse_check_selection_rejects_out_of_range_numbers() {
        let available = vec![available_check("ci/test")];
        let err = parse_check_selection("2", &available, &[]).unwrap_err();

        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_check_selection_rejects_zero() {
        let available = vec![available_check("ci/test")];
        let err = parse_check_selection("0", &available, &[]).unwrap_err();

        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_check_selection_rejects_invalid_tokens() {
        let available = vec![available_check("ci/test")];
        let err = parse_check_selection("wat", &available, &[]).unwrap_err();

        assert!(
            err.to_string().contains("invalid number"),
            "unexpected error: {}",
            err
        );
    }
}
