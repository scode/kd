//! `kd boatworker destroy`: delete one or more owned boatworker sandboxes,
//! or every owned one via `--all`.
//!
//! The semantics are `kd ubiworker destroy`'s, carried over on purpose: no
//! confirmation prompt (the guard is that targets only ever come from the
//! owned-worker listing), fail-closed resolution of every requested name
//! before anything is deleted, deduplication of names that normalize to the
//! same worker, and sequential, non-atomic execution afterwards. See that
//! module's docs and SPEC.md for the reasoning; it applies unchanged.
//!
//! Two boat-specific differences. Boat documents no uniqueness rule for
//! sandbox names, so one name can match several sandboxes, and all of them
//! are then targets. And boat's listing can be cut off at one page, in which
//! case the no-name form refuses rather than guess that the one worker it
//! can see is the only one.

use super::{DestroyArgs, OwnedWorkers, Worker, owned_workers, with_prefix};
use anyhow::{Context, bail};
use std::collections::HashSet;
use tracing::info;
use xshell::{Shell, cmd};

/// Entry point for `kd boatworker destroy`: resolve every target against
/// one listing, then delete them one at a time.
pub fn run(args: DestroyArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;

    // Prefix before listing so an explicit name is matched in the same form
    // the listing reports, whether or not the user typed `boatworker-`.
    let names: Vec<String> = args.names.iter().map(|name| with_prefix(name)).collect();

    let owned = owned_workers(&sh)?;
    let targets = resolve_targets(&owned, &names, args.all)?;

    if targets.is_empty() {
        // Only reachable through `--all` with zero owned workers, which is a
        // deliberate no-op success; see `resolve_targets`.
        info!("no boatworkers to destroy");
        return Ok(());
    }

    for target in targets {
        delete_sandbox(&sh, &target.id)
            .with_context(|| format!("failed to destroy boatworker '{}'", target.name))?;
        info!(
            "requested deletion of boatworker '{}' ({})",
            target.name, target.id
        );
    }
    Ok(())
}

/// Resolve a `destroy` invocation's targets from the owned-worker listing:
/// every worker with `all`, the workers matching `names` (already
/// prefixed), or with neither the sole worker.
///
/// Fail-closed: every name is looked up before anything is returned, and one
/// unmatched name fails the whole request with every missing name listed.
/// A typo in one of several names must not become a half-executed bulk
/// delete. Repeated names resolve once.
///
/// The no-name form errors on zero or several workers, and also when the
/// listing is incomplete: "the sole worker" is a claim about what is absent
/// from the listing, which an incomplete listing cannot support. `--all`
/// and explicit names only act on what was seen, so they proceed (the
/// listing already warned).
///
/// This is `kd ubiworker destroy`'s resolution over boat's worker type.
/// Pure, so each branch is unit-testable.
fn resolve_targets<'a>(
    owned: &'a OwnedWorkers,
    names: &[String],
    all: bool,
) -> anyhow::Result<Vec<&'a Worker>> {
    let workers = owned.workers.as_slice();
    if all {
        return Ok(workers.iter().collect());
    }

    if names.is_empty() {
        if !owned.complete {
            bail!(
                "boat's sandbox listing was cut off, so kd cannot tell whether there is only \
                 one boatworker; specify a name or --all"
            );
        }
        return match workers {
            [] => bail!("no boatworkers exist"),
            [worker] => Ok(vec![worker]),
            multiple => bail!(
                "multiple boatworkers exist ({}); specify a name or --all",
                describe(multiple)
            ),
        };
    }

    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        // Every sandbox carrying the name is a target, since the name the
        // user gave covers all of them (see module docs on duplicates).
        let matches: Vec<&Worker> = workers.iter().filter(|w| w.name == *name).collect();
        if matches.is_empty() {
            missing.push(name.as_str());
        }
        targets.extend(matches);
    }

    if !missing.is_empty() {
        bail!(
            "no boatworker(s) named {} exist; destroying nothing",
            missing.join(", ")
        );
    }
    Ok(targets)
}

/// `name (id), ...` for error messages. The id is there because two
/// sandboxes can share a name, and a list reading `boatworker-a,
/// boatworker-a` would not say which is which.
fn describe(workers: &[Worker]) -> String {
    workers
        .iter()
        .map(|w| format!("{} ({})", w.name, w.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Delete one sandbox by its immutable id with `boat delete <id> --yes`.
///
/// By id, not name, so a name reused between kd's listing and this call can
/// only ever point the request at the sandbox the listing resolved. `--yes`
/// skips boat's interactive confirmation for the reason the module docs
/// give. boat's own output passes straight through.
fn delete_sandbox(sh: &Shell, id: &str) -> anyhow::Result<()> {
    cmd!(sh, "boat delete {id} --yes")
        .run()
        .context("boat delete failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(name: &str, id: &str) -> Worker {
        Worker {
            name: name.to_string(),
            id: id.to_string(),
            state: "idle".to_string(),
            machine_type: "large".to_string(),
        }
    }

    fn complete(workers: Vec<Worker>) -> OwnedWorkers {
        OwnedWorkers {
            workers,
            complete: true,
        }
    }

    fn ids(targets: &[&Worker]) -> Vec<String> {
        targets.iter().map(|w| w.id.clone()).collect()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// `--all` returns every owned worker, in listing order.
    #[test]
    fn resolve_targets_all_returns_every_worker() {
        let owned = complete(vec![
            worker("boatworker-a", "bx_a"),
            worker("boatworker-b", "bx_b"),
        ]);
        let targets = resolve_targets(&owned, &[], true).unwrap();
        assert_eq!(ids(&targets), vec!["bx_a", "bx_b"]);
    }

    /// `--all` with nothing owned is an empty success, which `run` turns
    /// into a no-op, so scripted cleanup needs no special case.
    #[test]
    fn resolve_targets_all_with_none_is_empty() {
        let owned = complete(vec![]);
        assert!(resolve_targets(&owned, &[], true).unwrap().is_empty());
    }

    /// Several names that all resolve return their workers in request order.
    #[test]
    fn resolve_targets_several_names_all_found() {
        let owned = complete(vec![
            worker("boatworker-a", "bx_a"),
            worker("boatworker-b", "bx_b"),
            worker("boatworker-c", "bx_c"),
        ]);
        let targets =
            resolve_targets(&owned, &names(&["boatworker-c", "boatworker-a"]), false).unwrap();
        assert_eq!(ids(&targets), vec!["bx_c", "bx_a"]);
    }

    /// One unmatched name among several fails the whole request, names only
    /// the missing ones, and returns nothing to delete.
    #[test]
    fn resolve_targets_one_missing_name_fails_closed() {
        let owned = complete(vec![worker("boatworker-a", "bx_a")]);
        let requested = names(&["boatworker-a", "boatworker-miss1", "boatworker-miss2"]);
        let msg = resolve_targets(&owned, &requested, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("boatworker-miss1") && msg.contains("boatworker-miss2"),
            "{msg}"
        );
        assert!(!msg.contains("boatworker-a"), "{msg}");
    }

    /// A name given twice deletes its sandbox once.
    #[test]
    fn resolve_targets_dedupes_repeated_names() {
        let owned = complete(vec![worker("boatworker-a", "bx_a")]);
        let requested = names(&["boatworker-a", "boatworker-a"]);
        assert_eq!(
            ids(&resolve_targets(&owned, &requested, false).unwrap()),
            vec!["bx_a"]
        );
    }

    /// Boat documents no uniqueness rule for names, so a name can match
    /// more than one sandbox; the request covers all of them rather than an
    /// arbitrary one.
    #[test]
    fn resolve_targets_name_matching_several_sandboxes_targets_all() {
        let owned = complete(vec![
            worker("boatworker-a", "bx_1"),
            worker("boatworker-a", "bx_2"),
        ]);
        assert_eq!(
            ids(&resolve_targets(&owned, &names(&["boatworker-a"]), false).unwrap()),
            vec!["bx_1", "bx_2"]
        );
    }

    /// With no names, exactly one owned worker is chosen, and zero or
    /// several are errors; the several case identifies the candidates by
    /// name and id and points at `--all`.
    #[test]
    fn resolve_targets_no_names_needs_exactly_one_worker() {
        let one = complete(vec![worker("boatworker-a", "bx_a")]);
        assert_eq!(
            ids(&resolve_targets(&one, &[], false).unwrap()),
            vec!["bx_a"]
        );

        let none = complete(vec![]);
        let msg = resolve_targets(&none, &[], false).unwrap_err().to_string();
        assert!(msg.contains("no boatworkers exist"), "{msg}");

        let two = complete(vec![
            worker("boatworker-a", "bx_1"),
            worker("boatworker-a", "bx_2"),
        ]);
        let msg = resolve_targets(&two, &[], false).unwrap_err().to_string();
        assert!(
            msg.contains("boatworker-a (bx_1)")
                && msg.contains("boatworker-a (bx_2)")
                && msg.contains("--all"),
            "{msg}"
        );
    }

    /// A truncated listing showing a single worker must not be read as "the
    /// sole worker": the one kd would delete might not be the only one.
    #[test]
    fn resolve_targets_no_names_refuses_incomplete_listing() {
        let owned = OwnedWorkers {
            workers: vec![worker("boatworker-a", "bx_a")],
            complete: false,
        };
        let msg = resolve_targets(&owned, &[], false).unwrap_err().to_string();
        assert!(msg.contains("cut off"), "{msg}");
    }
}
