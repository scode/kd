//! `kd ubiworker destroy`: tear down a ubiworker VM after confirmation.
//!
//! Resolving *which* VM to destroy and getting the user's go-ahead both
//! happen before any destructive call is made, so the only thing that can
//! go wrong in [`destroy_vm`] itself is the `ubi` call failing outright.

use super::{DestroyArgs, VmRow, normalize_worker_name, owned_workers, require_env};
use anyhow::{Context, anyhow, bail};
use std::io::{self, Write};
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
    let normalized_name = args
        .name
        .as_deref()
        .map(normalize_worker_name)
        .transpose()?;
    let rows = owned_workers(&sh)?;
    let target = resolve_target(&rows, normalized_name.as_deref())?;
    let name = target.name.clone();
    let id = target.id.clone();

    if !args.yes && !confirm_destroy(&name)? {
        bail!("aborted");
    }

    destroy_vm(&sh, &id)
}

/// Pick the row to destroy out of the already-fetched, already-owned
/// `rows`: an explicit `name` match, or — if `name` is `None` — the sole
/// row, erroring on zero or multiple.
///
/// Pure over its inputs (no shelling out, no I/O) so every branch is
/// unit-testable directly: explicit name found, explicit name not found,
/// and the zero/one/many cases of the no-argument path.
fn resolve_target<'a>(rows: &'a [VmRow], name: Option<&str>) -> anyhow::Result<&'a VmRow> {
    match name {
        Some(name) => rows
            .iter()
            .find(|row| row.name == name)
            .ok_or_else(|| anyhow!("no ubiworker named '{name}' exists")),
        None => match rows {
            [] => bail!("no ubiworkers exist"),
            [row] => Ok(row),
            multiple => {
                let names: Vec<&str> = multiple.iter().map(|w| w.name.as_str()).collect();
                bail!(
                    "multiple ubiworkers exist ({}); specify which one to destroy",
                    names.join(", ")
                );
            }
        },
    }
}

/// Prompt `Destroy <name>? [y/N] ` on stdout and read one line from stdin.
/// The accept/reject decision itself is delegated to [`parse_confirmation`]
/// so it's unit-testable without going through real stdin/stdout.
fn confirm_destroy(name: &str) -> anyhow::Result<bool> {
    print!("Destroy {name}? [y/N] ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(parse_confirmation(&input))
}

/// Decide whether one line of confirmation input means "yes".
///
/// Case-insensitive `y`/`yes` (after trimming surrounding whitespace,
/// including the trailing newline from `read_line`) accept; everything
/// else — including empty input — rejects, matching the `[y/N]` prompt's
/// default-to-no behavior.
fn parse_confirmation(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Destroy the VM via `ubi vm <id> destroy -f` — by immutable id, not by
/// name (`ubi vm` accepts either a `location/vm-name` or a bare `vm-id`).
///
/// Destroying by id closes a race that destroying by name would leave
/// open: [`confirm_destroy`] pauses for a human to type `y`, and a name is
/// reusable — nothing stops someone (or another script) from destroying the
/// confirmed VM and creating a *new* one under the same name during that
/// pause. If this function re-resolved the name to a VM at destroy time,
/// the force-destroy could land on that new, unrelated VM. Destroying by
/// the id captured back in [`run`] — at the same moment the name shown to
/// the user was read — pins the exact VM that was listed and confirmed.
/// This narrows the race window to list→destroy; it cannot be eliminated
/// entirely from a CLI with a confirmation pause in the middle.
///
/// Without `-f`, the `ubi` thin client does an interactive confirmation
/// round-trip: the server sends back a `ubi-confirm` header and the client
/// prompts and expects the exact VM name typed back on stdin. kd already
/// obtained explicit user confirmation in [`run`] above, so it passes
/// `-f`/`--force` ("do not require confirmation" per `ubi`'s own destroy
/// options) to keep this subprocess call non-interactive. `ubi`'s own
/// stdout (which prints something like "scheduled for destruction" on
/// success) is allowed straight through rather than replaced with kd's own
/// message.
fn destroy_vm(sh: &Shell, id: &str) -> anyhow::Result<()> {
    cmd!(sh, "ubi vm {id} destroy -f")
        .run()
        .context("ubi vm destroy failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_confirmation_accepts_y_variants() {
        assert!(parse_confirmation("y"));
        assert!(parse_confirmation("Y"));
        assert!(parse_confirmation("yes"));
        assert!(parse_confirmation("YES\n"));
        assert!(parse_confirmation("  yes  "));
    }

    /// The prompt is `[y/N]`, so anything other than an explicit yes —
    /// including just pressing enter — must default to "no".
    #[test]
    fn parse_confirmation_rejects_empty_and_no() {
        assert!(!parse_confirmation(""));
        assert!(!parse_confirmation("\n"));
        assert!(!parse_confirmation("n"));
        assert!(!parse_confirmation("no"));
        assert!(!parse_confirmation("maybe"));
    }

    fn vm_row(name: &str, id: &str) -> VmRow {
        VmRow {
            location: "us-east-a2".to_string(),
            name: name.to_string(),
            id: id.to_string(),
            ip4: Some("1.2.3.4".to_string()),
        }
    }

    #[test]
    fn resolve_target_explicit_name_found() {
        let rows = vec![vm_row("ubiworker-a", "id-a"), vm_row("ubiworker-b", "id-b")];
        let target = resolve_target(&rows, Some("ubiworker-b")).unwrap();
        assert_eq!(target.id, "id-b");
    }

    #[test]
    fn resolve_target_explicit_name_not_found() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let err = resolve_target(&rows, Some("ubiworker-missing")).unwrap_err();
        assert!(
            err.to_string()
                .contains("no ubiworker named 'ubiworker-missing' exists"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_target_no_arg_zero_workers_errors() {
        let rows: Vec<VmRow> = vec![];
        let err = resolve_target(&rows, None).unwrap_err();
        assert!(err.to_string().contains("no ubiworkers exist"));
    }

    #[test]
    fn resolve_target_no_arg_one_worker_is_chosen() {
        let rows = vec![vm_row("ubiworker-a", "id-a")];
        let target = resolve_target(&rows, None).unwrap();
        assert_eq!(target.id, "id-a");
    }

    /// Zero or multiple owned workers with no explicit name must both be
    /// errors — `destroy` should never guess which VM the user meant, and
    /// the error should name the candidates so the user can pick one.
    #[test]
    fn resolve_target_no_arg_multiple_workers_errors_listing_names() {
        let rows = vec![vm_row("ubiworker-a", "id-a"), vm_row("ubiworker-b", "id-b")];
        let err = resolve_target(&rows, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ubiworker-a"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("ubiworker-b"),
            "unexpected error: {message}"
        );
    }
}
