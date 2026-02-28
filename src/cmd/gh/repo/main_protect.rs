use super::{MainProtectArgs, resolve_repo};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};
use tracing::info;
use xshell::{Shell, cmd};

#[derive(Deserialize)]
struct RulesetSummary {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct RulesetDetail {
    enforcement: String,
    rules: Vec<RuleRaw>,
}

#[derive(Deserialize, Clone)]
struct RuleRaw {
    #[serde(rename = "type")]
    rule_type: String,
    parameters: Option<Value>,
}

#[derive(Deserialize)]
struct StatusCheckParameters {
    required_status_checks: Vec<StatusCheckParam>,
}

#[derive(Deserialize, Serialize, Clone)]
struct StatusCheckParam {
    context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    integration_id: Option<i64>,
}

#[derive(Deserialize)]
struct CheckRun {
    name: String,
}

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

fn conditions() -> Value {
    serde_json::json!({
        "ref_name": {
            "include": ["~DEFAULT_BRANCH"],
            "exclude": []
        }
    })
}

fn make_status_checks_rule(checks: &[StatusCheckParam]) -> RulePayload {
    RulePayload {
        rule_type: "required_status_checks".to_string(),
        parameters: Some(serde_json::json!({
            "required_status_checks": checks,
            "strict_required_status_checks_policy": false,
        })),
    }
}

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

fn rules_with_checks(checks: &[StatusCheckParam]) -> Vec<RulePayload> {
    let mut rules = base_rules();
    // GitHub rejects required_status_checks with an empty checks list (HTTP 422),
    // so we only include the rule when there are checks to enforce.
    if !checks.is_empty() {
        rules.push(make_status_checks_rule(checks));
    }
    rules
}

fn find_ruleset(sh: &Shell, repo: &str) -> anyhow::Result<Option<u64>> {
    let output = cmd!(sh, "gh api repos/{repo}/rulesets").read()?;
    let rulesets: Vec<RulesetSummary> = serde_json::from_str(&output)?;
    Ok(rulesets
        .into_iter()
        .find(|r| r.name == RULESET_NAME)
        .map(|r| r.id))
}

fn get_ruleset(sh: &Shell, repo: &str, id: u64) -> anyhow::Result<RulesetDetail> {
    let id_str = id.to_string();
    let output = cmd!(sh, "gh api repos/{repo}/rulesets/{id_str}").read()?;
    Ok(serde_json::from_str(&output)?)
}

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

fn has_rule(detail: &RulesetDetail, rule_type: &str) -> bool {
    detail.rules.iter().any(|r| r.rule_type == rule_type)
}

fn needs_fix(detail: &RulesetDetail) -> bool {
    detail.enforcement != "active"
        || !has_rule(detail, "required_linear_history")
        || !has_rule(detail, "non_fast_forward")
}

fn extract_current_checks(detail: &RulesetDetail) -> Vec<StatusCheckParam> {
    for rule in &detail.rules {
        if rule.rule_type == "required_status_checks"
            && let Some(params) = &rule.parameters
            && let Ok(parsed) = serde_json::from_value::<StatusCheckParameters>(params.clone())
        {
            return parsed.required_status_checks;
        }
    }
    Vec::new()
}

fn get_default_branch(sh: &Shell, repo: &str) -> anyhow::Result<String> {
    let output = cmd!(sh, "gh api repos/{repo} --jq .default_branch").read()?;
    Ok(output.trim().to_string())
}

fn get_check_names_for_ref(sh: &Shell, repo: &str, git_ref: &str) -> anyhow::Result<Vec<String>> {
    let output = cmd!(
        sh,
        "gh api repos/{repo}/commits/{git_ref}/check-runs --jq .check_runs"
    )
    .read()?;
    let check_runs: Vec<CheckRun> = serde_json::from_str(&output)?;
    Ok(check_runs.into_iter().map(|c| c.name).collect())
}

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

/// Collect check names from both the default branch HEAD and the latest merged PR.
/// The default branch only has CI checks; PR-triggered checks (e.g. conventional-commit)
/// never run on main, so we need the PR's head commit to discover those.
fn get_check_names(sh: &Shell, repo: &str, branch: &str) -> anyhow::Result<Vec<String>> {
    let mut names = get_check_names_for_ref(sh, repo, branch)?;

    if let Some(sha) = get_latest_merged_pr_sha(sh, repo)? {
        names.extend(get_check_names_for_ref(sh, repo, &sha)?);
    }

    names.sort();
    names.dedup();
    Ok(names)
}

fn prompt_for_checks(
    available: &[String],
    current_blocking: &[StatusCheckParam],
) -> anyhow::Result<Option<Vec<StatusCheckParam>>> {
    let blocking_set: std::collections::HashSet<&str> = current_blocking
        .iter()
        .map(|c| c.context.as_str())
        .collect();

    println!();
    for (i, name) in available.iter().enumerate() {
        let tag = if blocking_set.contains(name.as_str()) {
            "[BLOCKING]"
        } else {
            "[ ]"
        };
        println!("  {}. {} {}", i + 1, tag, name);
    }
    println!();

    print!("Blocking checks (e.g. 1,4,-7 to add 1,4 and remove 7; all; none; or empty to skip): ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return Ok(None);
    }

    if input == "none" {
        return Ok(Some(Vec::new()));
    }

    if input == "all" {
        return Ok(Some(
            available
                .iter()
                .map(|name| StatusCheckParam {
                    context: name.clone(),
                    integration_id: None,
                })
                .collect(),
        ));
    }

    // Start from current blocking set, then apply additions/removals.
    // Comma-separated entries; a leading '-' means remove, otherwise add.
    let mut result = blocking_set.clone();
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
        let name = available[num - 1].as_str();
        if remove {
            result.remove(name);
        } else {
            result.insert(name);
        }
    }

    let selected: Vec<StatusCheckParam> = available
        .iter()
        .filter(|name| result.contains(name.as_str()))
        .map(|name| StatusCheckParam {
            context: name.clone(),
            integration_id: None,
        })
        .collect();

    Ok(Some(selected))
}

pub fn run(args: MainProtectArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;
    let repo = resolve_repo(args.repo.as_deref(), &std::env::current_dir()?)?;

    let ruleset_id = match find_ruleset(&sh, &repo)? {
        Some(id) => {
            info!("Found existing '{}' ruleset (id={})", RULESET_NAME, id);
            let detail = get_ruleset(&sh, &repo, id)?;
            let current_checks = extract_current_checks(&detail);
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
    let check_names = get_check_names(&sh, &repo, &default_branch)?;

    if check_names.is_empty() {
        info!(
            "No check runs found on '{}'; skipping status check selection",
            default_branch
        );
        return Ok(());
    }

    let detail = get_ruleset(&sh, &repo, ruleset_id)?;
    let current_checks = extract_current_checks(&detail);

    if let Some(selected) = prompt_for_checks(&check_names, &current_checks)? {
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
