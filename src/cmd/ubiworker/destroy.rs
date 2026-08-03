//! `kd ubiworker destroy`: tear down one or more owned ubiworker VMs, or
//! every owned one via `--all`.
//!
//! There is deliberately no confirmation prompt (see `SPEC.md`). This used
//! to be an interactive `Destroy <name>? [y/N]` step; it was removed as a
//! deliberate call, not an oversight, because this is a single-operator
//! tool and the prompt cost more in friction than it protected: the actual
//! guard is [`resolve_targets`] only ever selecting from the
//! already-filtered owned-worker listing — by exact name when names are
//! given, all of it with `--all`, or the sole owned worker when neither —
//! and a human retyping a name they just typed doesn't catch anything that
//! check doesn't already catch.
//!
//! Resolution is fail-closed across the whole request, though: `run`
//! collects every requested name and resolves all of them against one
//! listing *before* destroying anything (see [`resolve_targets`]). A typo
//! in one of several names must not turn into a half-executed bulk destroy
//! of the names that did resolve. Execution after that point is *not*
//! atomic — targets are destroyed one at a time by id, and a failure
//! partway through leaves everything destroyed so far destroyed, with no
//! rollback. The error names the worker that failed; re-running is the
//! recovery for whatever's left.

use super::{DestroyArgs, VmRow, normalize_worker_name, owned_workers, require_env};
use anyhow::{Context, bail};
use std::collections::HashSet;
use tracing::info;
use xshell::{Shell, cmd};

pub fn run(args: DestroyArgs) -> anyhow::Result<()> {
    // Same fail-fast rationale as `create`: `ubi` reads UBI_TOKEN itself,
    // kd just surfaces a missing token as a clear error up front.
    require_env("UBI_TOKEN")?;
    let sh = Shell::new()?;

    // Normalize before listing, not after: an explicit name must be
    // matched against the same prefixed form `owned_workers` would report
    // it under, regardless of whether the user typed the `ubiworker-`
    // prefix themselves.
    let names: Vec<String> = args
        .names
        .iter()
        .map(|name| normalize_worker_name(name))
        .collect::<anyhow::Result<_>>()?;

    let rows = owned_workers(&sh)?;
    let targets = resolve_targets(&rows, &names, args.all)?;

    if targets.is_empty() {
        // The only way to reach this: `--all` against zero owned workers.
        // `resolve_targets` treats every other empty-result shape (no-arg
        // destroy with zero rows) as a hard error instead — see its docs.
        // Erroring here too would make scripted "destroy everything, if
        // anything exists" cleanup annoying to write.
        info!("no ubiworkers to destroy");
        return Ok(());
    }

    for target in targets {
        destroy_vm(&sh, &target.id)
            .with_context(|| format!("failed to destroy ubiworker '{}'", target.name))?;
        // One line per target so a bulk `--all`/multi-name destroy narrates
        // its progress instead of going silent until the end. "Scheduled",
        // not "destroyed": destruction is asynchronous on Ubicloud's side
        // (see SPEC.md), and this log must not claim a completed state
        // transition that hasn't happened yet.
        info!(
            "scheduled destruction of ubiworker '{}' ({})",
            target.name, target.id
        );
    }

    Ok(())
}

/// Resolve a `destroy` invocation's targets out of the already-fetched,
/// already-owned `rows`: every row (if `all`), the rows matching `names`
/// (already normalized by the caller), or — if `names` is empty and `all`
/// is false — the sole row, erroring on zero or multiple.
///
/// Resolution is fail-closed: every name in `names` is looked up before
/// this function returns anything, and if even one has no match, the error
/// lists every missing name and this returns nothing at all rather than the
/// subset that did resolve. Duplicate names (after normalization) are
/// deduped so a name given twice destroys its VM once, not twice.
///
/// Callers are expected to have already enforced (via clap's
/// `conflicts_with`) that `names` and `all` aren't both set; this function
/// doesn't re-check that itself, and treats `all` as taking priority if it
/// somehow is.
///
/// Pure over its inputs (no shelling out, no I/O) so every branch is
/// unit-testable directly: `--all` with rows/with none, all-names-found,
/// one-of-several-missing, duplicate names, and the zero/one/many cases of
/// the no-argument path.
fn resolve_targets<'a>(
    rows: &'a [VmRow],
    names: &[String],
    all: bool,
) -> anyhow::Result<Vec<&'a VmRow>> {
    if all {
        return Ok(rows.iter().collect());
    }

    if names.is_empty() {
        return match rows {
            [] => bail!("no ubiworkers exist"),
            [row] => Ok(vec![row]),
            multiple => {
                let names: Vec<&str> = multiple.iter().map(|w| w.name.as_str()).collect();
                bail!(
                    "multiple ubiworkers exist ({}); specify a name or --all",
                    names.join(", ")
                );
            }
        };
    }

    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        // First occurrence wins; later repeats of the same normalized name
        // are silently skipped rather than queuing the same VM twice.
        if !seen.insert(name.as_str()) {
            continue;
        }
        match rows.iter().find(|row| row.name == *name) {
            Some(row) => targets.push(row),
            None => missing.push(name.as_str()),
        }
    }

    if !missing.is_empty() {
        // Fail closed: report every missing name and destroy nothing, even
        // though some of `names` did resolve. A typo in one of several
        // names must not result in a half-executed bulk destroy.
        bail!(
            "no ubiworker(s) named {} exist; destroying nothing",
            missing.join(", ")
        );
    }

    Ok(targets)
}

/// Destroy the VM via `ubi vm <id> destroy -f` — by immutable id, not by
/// name (`ubi vm` accepts either a `location/vm-name` or a bare `vm-id`).
///
/// Destroying by id pins the exact VM [`resolve_targets`] identified from
/// the listing in [`run`], closing a name-reuse race that destroying by
/// name would leave open: a name is reusable, and nothing stops another
/// process from destroying a matching VM and creating a *new* one under the
/// same name between kd's `ubi vm list` call and this destroy call —
/// especially with several targets destroyed in sequence, where later
/// targets sit further in time from the original listing. Destroying by the
/// id captured at resolution time means a race like that targets the stale
/// id — which cannot resolve to the replacement VM — rather than whichever
/// unrelated VM currently holds the name. (How `ubi` reports the stale id
/// is Ubicloud's business; the guarantee here is only about which VM the
/// request can land on.)
///
/// Without `-f`, the `ubi` thin client does an interactive confirmation
/// round-trip: the server sends back a `ubi-confirm` header and the client
/// prompts and expects the exact VM name typed back on stdin. kd passes
/// `-f`/`--force` ("do not require confirmation" per `ubi`'s own destroy
/// options) to bypass that — see the module docs for why kd's own
/// listing-based resolution is treated as sufficient guard without a
/// second, redundant confirmation. `ubi`'s own stdout (which prints
/// something like "scheduled for destruction" on success) is allowed
/// straight through rather than replaced with kd's own message.
fn destroy_vm(sh: &Shell, id: &str) -> anyhow::Result<()> {
    cmd!(sh, "ubi vm {id} destroy -f")
        .run()
        .context("ubi vm destroy failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_row(name: &str, id: &str) -> VmRow {
        VmRow {
            location: "us-east-a2".to_string(),
            name: name.to_string(),
            id: id.to_string(),
            ip4: Some("1.2.3.4".to_string()),
        }
    }

    #[test]
    fn resolve_targets_all_with_rows_returns_every_row() {
        let rows = vec![vm_row("ubiworker-a", "id-a"), vm_row("ubiworker-b", "id-b")];
        let targets = resolve_targets(&rows, &[], true).unwrap();
        // Assert the actual ids, not just the count — a result of the same
        // row twice has the right length and the wrong meaning.
        let ids: Vec<&str> = targets.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["id-a", "id-b"]);
    }

    /// `--all` against an empty owned-worker listing must succeed with an
    /// empty result, not error — `run` turns this into the "no ubiworkers
    /// to destroy" no-op so scripted cleanup doesn't need to special-case
    /// it. (The account may still hold unrelated VMs; only owned workers
    /// are ever in `rows`.)
    #[test]
    fn resolve_targets_all_with_no_rows_returns_empty() {
        let rows: Vec<VmRow> = vec![];
        let targets = resolve_targets(&rows, &[], true).unwrap();
        assert!(targets.is_empty());
    }

    #[test]
    fn resolve_targets_multiple_names_all_found() {
        let rows = vec![
            vm_row("ubiworker-a", "id-a"),
            vm_row("ubiworker-b", "id-b"),
            vm_row("ubiworker-c", "id-c"),
        ];
        let names = vec!["ubiworker-a".to_string(), "ubiworker-c".to_string()];
        let targets = resolve_targets(&rows, &names, false).unwrap();
        let ids: Vec<&str> = targets.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["id-a", "id-c"]);
    }

    /// One typo among several requested names must fail the *whole*
    /// request and name only the missing one(s) — not silently destroy the
    /// names that did resolve.
    #[test]
    fn resolve_targets_one_of_several_missing_errors_and_destroys_nothing() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let names = vec!["ubiworker-a".to_string(), "ubiworker-missing".to_string()];
        let err = resolve_targets(&rows, &names, false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ubiworker-missing"),
            "unexpected error: {message}"
        );
        assert!(
            !message.contains("ubiworker-a"),
            "error should only name the missing worker, not the one that resolved: {message}"
        );
    }

    /// The fail-closed error promises to name *every* missing worker, not
    /// just the first one hit — an implementation that stops at the first
    /// miss would send the user through one error-fix-retry loop per typo.
    #[test]
    fn resolve_targets_several_missing_names_all_reported() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let names = vec![
            "ubiworker-miss1".to_string(),
            "ubiworker-a".to_string(),
            "ubiworker-miss2".to_string(),
        ];
        let err = resolve_targets(&rows, &names, false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ubiworker-miss1") && message.contains("ubiworker-miss2"),
            "error should name every missing worker: {message}"
        );
    }

    #[test]
    fn resolve_targets_duplicate_names_deduped() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let names = vec!["ubiworker-a".to_string(), "ubiworker-a".to_string()];
        let targets = resolve_targets(&rows, &names, false).unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn resolve_targets_no_arg_zero_workers_errors() {
        let rows: Vec<VmRow> = vec![];
        let err = resolve_targets(&rows, &[], false).unwrap_err();
        assert!(err.to_string().contains("no ubiworkers exist"));
    }

    #[test]
    fn resolve_targets_no_arg_one_worker_is_chosen() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let targets = resolve_targets(&rows, &[], false).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "id-a");
    }

    /// Multiple owned workers with no explicit name/`--all` must be an
    /// error — `destroy` should never guess which VM the user meant, and
    /// the error should name the candidates and suggest `--all` so the user
    /// can pick. (The zero-worker case is specified by the test above.)
    #[test]
    fn resolve_targets_no_arg_multiple_workers_errors_listing_names() {
        let rows = vec![vm_row("ubiworker-a", "id-a"), vm_row("ubiworker-b", "id-b")];
        let err = resolve_targets(&rows, &[], false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ubiworker-a"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("ubiworker-b"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("--all"),
            "error should suggest --all as an alternative: {message}"
        );
    }
}
