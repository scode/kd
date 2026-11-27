//! `kd tlworker destroy`: terminate one or more owned tlworker sandboxes, or
//! every owned one via `--all`, and delete the snapshots they leave behind.
//!
//! The semantics are `kd boatworker destroy`'s, carried over on purpose: no
//! confirmation prompt (targets only ever come from the owned-worker
//! listing), fail-closed resolution of every requested name before anything
//! is terminated, deduplication of names that normalize to the same worker,
//! and every target attempted even after one fails. See SPEC.md for the
//! reasoning; it applies unchanged.
//!
//! Two Tensorlake-specific differences. Tensorlake refuses a second sandbox
//! with a name already in use (HTTP 409), so a name matches at most one
//! worker and boatworker's several-sandboxes-per-name handling has no
//! counterpart here. And terminating a sandbox does not remove its
//! snapshots: Tensorlake snapshots a named sandbox when it suspends on idle,
//! and those snapshots outlive the sandbox and bill storage monthly. So once
//! the terminations are done, and each sandbox is confirmed gone, `destroy`
//! lists the project's snapshots and deletes every one whose source is a
//! sandbox it just terminated, and no other.

use super::api::{Api, Gone, load_credentials};
use super::{DestroyArgs, Worker, owned_workers, with_prefix};
use anyhow::bail;
use std::collections::HashSet;
use tracing::{error, info};

/// Entry point for `kd tlworker destroy`: resolve every target against one
/// listing, terminate them one at a time, then sweep their snapshots.
pub fn run(args: DestroyArgs) -> anyhow::Result<()> {
    let api = Api::new(load_credentials()?);

    // Prefix before listing so an explicit name is matched in the same form
    // the listing reports, whether or not the user typed `tlworker-`.
    let names: Vec<String> = args.names.iter().map(|name| with_prefix(name)).collect();

    let workers = owned_workers(&api)?;
    let targets = resolve_targets(&workers, &names, args.all)?;

    if targets.is_empty() {
        // Only reachable through `--all` with zero owned workers, which is a
        // deliberate no-op success; see `resolve_targets`.
        info!("no tlworkers to destroy");
        return Ok(());
    }

    // Every target is attempted even after a failure, as for boatworkers:
    // stopping at the first error would leave the remaining targets running
    // and billing. A worker that failed here is still listed, so a rerun of
    // `destroy` picks it up.
    let mut not_destroyed = Vec::new();
    let mut terminating = Vec::new();
    for target in targets {
        match api.terminate_sandbox(&target.id) {
            Ok(gone) => {
                let how = match gone {
                    Gone::Now => "terminating",
                    Gone::Already => "already gone:",
                };
                info!("{how} tlworker '{}' ({})", target.name, target.id);
                terminating.push(target);
            }
            Err(err) => {
                error!(
                    "failed to destroy tlworker '{}' ({}): {err:#}",
                    target.name, target.id
                );
                not_destroyed.push(format!("{} ({})", target.name, target.id));
            }
        }
    }

    // The termination request returns before the sandbox is gone, and a
    // worker caught suspending or snapshotting may still be writing a
    // snapshot. Sweeping before it finishes could miss that snapshot and
    // leave it billing, so wait for each one first. One that does not finish
    // in time is reported and its snapshots are left for a rerun: it is
    // still listed, so `destroy` finds it again.
    let mut terminated = Vec::new();
    for worker in terminating {
        match wait_until_terminated(&api, worker) {
            Ok(()) => terminated.push(worker),
            Err(err) => {
                error!("{err:#}");
                not_destroyed.push(format!("{} ({})", worker.name, worker.id));
            }
        }
    }

    // Snapshots of a sandbox whose termination failed or did not finish are
    // left alone: that sandbox still exists, and its latest snapshot may be
    // its suspended state.
    let leftover = if terminated.is_empty() {
        Vec::new()
    } else {
        sweep_snapshots(&api, &terminated)
    };

    if not_destroyed.is_empty() && leftover.is_empty() {
        return Ok(());
    }
    let mut problems = Vec::new();
    if !not_destroyed.is_empty() {
        problems.push(format!(
            "not destroyed (rerun destroy for these): {}",
            not_destroyed.join(", ")
        ));
    }
    if !leftover.is_empty() {
        // These sandboxes are terminated and no longer listed, so rerunning
        // `destroy` cannot find them; the remedy is by snapshot ID.
        problems.push(format!(
            "terminated, but snapshots left behind (delete them with `tl sbx checkpoint rm \
             <snapshot-id>`): {}",
            leftover.join(", ")
        ));
    }
    bail!("{}", problems.join("; "))
}

/// How long `destroy` waits for one terminated sandbox to be gone before
/// giving up on sweeping its snapshots, and how often it checks. Farhelm
/// waits the same 30 seconds at the same interval before its own sweep.
const TERMINATION_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const TERMINATION_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Wait until Tensorlake reports `worker`'s sandbox `terminated` or no
/// longer knows it (404), bounded by [`TERMINATION_WAIT`].
fn wait_until_terminated(api: &Api, worker: &Worker) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + TERMINATION_WAIT;
    loop {
        let status = api.sandbox_status(&worker.id)?;
        match status.as_deref() {
            None | Some("terminated") => {
                info!("terminated tlworker '{}' ({})", worker.name, worker.id);
                return Ok(());
            }
            Some(status) if std::time::Instant::now() >= deadline => bail!(
                "tlworker '{}' ({}) was still {status} {} seconds after its termination was \
                 requested, so its snapshots were not swept",
                worker.name,
                worker.id,
                TERMINATION_WAIT.as_secs()
            ),
            Some(_) => std::thread::sleep(TERMINATION_POLL),
        }
    }
}

/// Delete every snapshot taken from one of `terminated`, returning a
/// description of each snapshot that could not be deleted, or of the
/// workers whose snapshots could not be looked for at all.
///
/// Matching is by source sandbox ID only, against sandboxes this run
/// terminated. A snapshot with no source, or a source kd did not just
/// destroy, is never touched, however its sandbox was named: it may be a
/// template someone keeps on purpose.
fn sweep_snapshots(api: &Api, terminated: &[&Worker]) -> Vec<String> {
    let snapshots = match api.list_snapshots() {
        Ok(snapshots) => snapshots,
        Err(err) => {
            error!("failed to list snapshots, so none were deleted: {err:#}");
            return vec![format!(
                "any snapshots of {} (the snapshot listing failed; find them with `tl sbx \
                 checkpoint ls`)",
                describe(terminated)
            )];
        }
    };
    let mut failed = Vec::new();
    for (snapshot, worker) in snapshots_of(&snapshots, terminated) {
        match api.delete_snapshot(&snapshot) {
            Ok(_) => info!(
                "deleted snapshot {snapshot} of tlworker '{}' ({})",
                worker.name, worker.id
            ),
            Err(err) => {
                error!(
                    "failed to delete snapshot {snapshot} of tlworker '{}' ({}): {err:#}",
                    worker.name, worker.id
                );
                failed.push(format!(
                    "snapshot {snapshot} of {} ({})",
                    worker.name, worker.id
                ));
            }
        }
    }
    failed
}

/// Pair each snapshot taken from one of `workers` with its worker. Pure, so
/// the "only snapshots of what was terminated" rule is unit-testable.
fn snapshots_of<'a>(
    snapshots: &[super::api::Snapshot],
    workers: &[&'a Worker],
) -> Vec<(String, &'a Worker)> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let source = snapshot.sandbox_id.as_deref()?;
            let worker = workers.iter().find(|w| w.id == source)?;
            Some((snapshot.id.clone(), *worker))
        })
        .collect()
}

/// Resolve a `destroy` invocation's targets from the owned-worker listing:
/// every worker with `all`, the workers matching `names` (already
/// prefixed), or with neither the sole worker.
///
/// Fail-closed: every name is looked up before anything is returned, and one
/// unmatched name fails the whole request with every missing name listed. A
/// typo in one of several names must not become a half-executed bulk
/// termination. Repeated names resolve once.
///
/// The no-name form errors on zero or several workers. The listing is read
/// to the end (see `Api::list_sandboxes`), so "the sole worker" is always a
/// claim about the whole project. Pure, so each branch is unit-testable.
fn resolve_targets<'a>(
    workers: &'a [Worker],
    names: &[String],
    all: bool,
) -> anyhow::Result<Vec<&'a Worker>> {
    if all {
        return Ok(workers.iter().collect());
    }

    if names.is_empty() {
        return match workers {
            [] => bail!("no tlworkers exist"),
            [worker] => Ok(vec![worker]),
            multiple => bail!(
                "multiple tlworkers exist ({}); specify a name or --all",
                describe(&multiple.iter().collect::<Vec<_>>())
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
        match workers.iter().find(|w| w.name == *name) {
            Some(worker) => targets.push(worker),
            None => missing.push(name.as_str()),
        }
    }

    if !missing.is_empty() {
        bail!(
            "no tlworker(s) named {} exist; destroying nothing",
            missing.join(", ")
        );
    }
    Ok(targets)
}

/// `name (id), ...` for error messages about several workers.
fn describe(workers: &[&Worker]) -> String {
    workers
        .iter()
        .map(|w| format!("{} ({})", w.name, w.id))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::tlworker::api::Snapshot;

    fn worker(name: &str, id: &str) -> Worker {
        Worker {
            name: name.to_string(),
            id: id.to_string(),
            status: "running".to_string(),
            cpus: 4.0,
            memory_mb: 16384,
        }
    }

    fn ids(targets: &[&Worker]) -> Vec<String> {
        targets.iter().map(|w| w.id.clone()).collect()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// `--all` returns every owned worker, and with none it is an empty
    /// success, which `run` turns into a no-op so scripted cleanup needs no
    /// special case.
    #[test]
    fn resolve_targets_all_returns_every_worker_or_nothing() {
        let workers = vec![worker("tlworker-a", "a"), worker("tlworker-b", "b")];
        assert_eq!(
            ids(&resolve_targets(&workers, &[], true).unwrap()),
            vec!["a", "b"]
        );
        assert!(resolve_targets(&[], &[], true).unwrap().is_empty());
    }

    /// One unmatched name among several fails the whole request, names only
    /// the missing ones, and returns nothing to terminate.
    #[test]
    fn resolve_targets_one_missing_name_fails_closed() {
        let workers = vec![worker("tlworker-a", "a")];
        let requested = names(&["tlworker-a", "tlworker-miss1", "tlworker-miss2"]);
        let msg = resolve_targets(&workers, &requested, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("tlworker-miss1") && msg.contains("tlworker-miss2"),
            "{msg}"
        );
        assert!(!msg.contains("tlworker-a,"), "{msg}");
    }

    /// Names resolve in request order, and a name given twice terminates
    /// its sandbox once.
    #[test]
    fn resolve_targets_keeps_order_and_dedupes() {
        let workers = vec![worker("tlworker-a", "a"), worker("tlworker-b", "b")];
        let requested = names(&["tlworker-b", "tlworker-a", "tlworker-b"]);
        assert_eq!(
            ids(&resolve_targets(&workers, &requested, false).unwrap()),
            vec!["b", "a"]
        );
    }

    /// With no names, exactly one owned worker is chosen; zero or several
    /// are errors, and the several case lists them and points at `--all`.
    #[test]
    fn resolve_targets_no_names_needs_exactly_one_worker() {
        let one = vec![worker("tlworker-a", "a")];
        assert_eq!(ids(&resolve_targets(&one, &[], false).unwrap()), vec!["a"]);

        let msg = resolve_targets(&[], &[], false).unwrap_err().to_string();
        assert!(msg.contains("no tlworkers exist"), "{msg}");

        let two = vec![worker("tlworker-a", "a"), worker("tlworker-b", "b")];
        let msg = resolve_targets(&two, &[], false).unwrap_err().to_string();
        assert!(
            msg.contains("tlworker-a") && msg.contains("tlworker-b") && msg.contains("--all"),
            "{msg}"
        );
    }

    /// Only snapshots whose source is a terminated worker are swept. A
    /// snapshot of some other sandbox, and one with no recorded source, are
    /// left alone, since kd cannot attribute them to a worker it destroyed.
    #[test]
    fn snapshots_of_matches_only_terminated_sources() {
        let a = worker("tlworker-a", "sbx-a");
        let snapshots = vec![
            Snapshot {
                id: "s1".into(),
                sandbox_id: Some("sbx-a".into()),
            },
            Snapshot {
                id: "s2".into(),
                sandbox_id: Some("sbx-other".into()),
            },
            Snapshot {
                id: "s3".into(),
                sandbox_id: None,
            },
            Snapshot {
                id: "s4".into(),
                sandbox_id: Some("sbx-a".into()),
            },
        ];
        let matched: Vec<String> = snapshots_of(&snapshots, &[&a])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(matched, vec!["s1", "s4"]);
    }
}
