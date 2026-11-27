//! Tensorlake sandbox workers: the Tensorlake counterpart of `kd ubiworker`
//! and `kd boatworker`. A tlworker is a disposable Tensorlake sandbox that kd
//! creates, lists, connects to and destroys with the same rules as the other
//! two fleets.
//!
//! kd drives Tensorlake through its REST API ([`api`]), not the `tl` CLI,
//! but it borrows the CLI's login when no API key is set (see the
//! credentials section of [`api`]).
//!
//! # Ownership convention
//!
//! A sandbox is a "tlworker" iff its Tensorlake name starts with
//! [`NAME_PREFIX`]. That is the whole test, as for boatworkers: Tensorlake
//! has no locations or labels to serve as a second condition. Unnamed
//! (ephemeral) sandboxes are never owned. The test applies within the
//! project the credentials select, which for the `tl` login is whichever
//! project `tl login` last picked.
//!
//! Every action resolves by name but acts on the sandbox ID, for the same
//! reason `kd ubiworker destroy` acts on Ubicloud's VM id: the ID is what
//! the listing returned, and acting on it means acting on exactly the
//! sandbox kd looked at.

pub mod api;
pub mod destroy;
pub mod list;
pub mod ssh;

use clap::{Args, Subcommand};
use tracing::debug;

/// Prefix marking a Tensorlake sandbox as a kd-managed tlworker (see module
/// docs on the ownership convention).
pub(crate) const NAME_PREFIX: &str = "tlworker-";

/// The Unix account on every Tensorlake sandbox. Tensorlake's image
/// provisions it (with passwordless sudo) and offers no way to choose
/// another, so unlike ubiworker's account it does not follow the operator's
/// local username. kd does not create an account of its own either.
pub(crate) const UNIX_USER: &str = "tl-user";

/// `kd tlworker ssh [ARGS...]` arguments.
///
/// A single trailing capture rather than a name positional plus a separate
/// argument vector, for the reason `kd ubiworker ssh` documents on its own
/// `SshArgs`: with two positionals, clap's global `-v`/`-q` would steal a
/// flag-shaped token meant for ssh once the name was filled.
/// `ssh::split_target` then reads the first element as an optional worker
/// name. `OsString` so arguments reach `tailscale ssh` byte for byte.
#[derive(Args)]
pub struct SshArgs {
    /// Optionally a worker name, then arguments forwarded verbatim to
    /// `tailscale ssh` after the destination: a remote command, or ssh's own
    /// flags.
    ///
    /// The first element is the worker name unless it starts with `-`. A
    /// leading `-v`, `-q` or `-h` (or `--verbose`, `--quiet`, `--help`) is
    /// claimed by kd itself, as anywhere else on the command line; use `--`
    /// to force it through to ssh.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "NAME_OR_SSH_ARG"
    )]
    pub args: Vec<std::ffi::OsString>,
}

/// `kd tlworker destroy [NAME...] [--all]` arguments.
#[derive(Args)]
pub struct DestroyArgs {
    /// Worker name(s) to destroy. If omitted (and `--all` isn't given),
    /// exactly one owned tlworker must exist and it is targeted
    /// automatically.
    pub names: Vec<String>,

    /// Destroy every owned tlworker. A no-op (not an error) if none exist,
    /// so scripted cleanup doesn't have to special-case an empty listing.
    #[arg(short, long, conflicts_with = "names")]
    pub all: bool,
}

/// `kd tlworker` subcommands. Each one's doc comment is its `--help` text.
#[derive(Subcommand)]
pub enum Commands {
    /// Terminate one or more tlworker sandboxes and delete their snapshots
    ///
    /// Takes any number of names, or --all to destroy every owned tlworker.
    /// With neither, targets the sole existing tlworker (an error if there
    /// are zero or more than one). There is no confirmation prompt (see
    /// SPEC.md); resolution is fail-closed, so a name that doesn't match
    /// destroys nothing rather than partially executing. After terminating,
    /// deletes every snapshot taken from the terminated sandboxes, since
    /// Tensorlake keeps (and bills) them otherwise.
    Destroy(DestroyArgs),
    /// List existing tlworker sandboxes
    ///
    /// Prints one tab-separated line per tlworker: name, sandbox ID,
    /// CPUs/memory, and Tensorlake's status. Suspended and pending
    /// sandboxes are included; terminated ones are not.
    List,
    /// Connect to an owned tlworker over Tailscale SSH
    ///
    /// With no name, targets the sole existing tlworker, like `destroy`.
    /// The first argument is the worker name unless it starts with `-`;
    /// everything after it goes to `tailscale ssh tl-user@<name>` unchanged
    /// (a remote command, or ssh flags such as `-L`). kd execs into
    /// `tailscale ssh`, replacing itself. A worker that is not running
    /// (suspended, say) is off the tailnet; kd warns and connects anyway.
    Ssh(SshArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Destroy(args) => destroy::run(args),
            Commands::List => list::run(),
            Commands::Ssh(args) => ssh::run(args),
        }
    }
}

/// Add the `tlworker-` ownership prefix to a user-supplied name if it's
/// missing, so `foo` and `tlworker-foo` name the same worker.
///
/// This is all a name used for lookup goes through. It is deliberately not
/// validated: a lookup only matches names already in the listing, so an
/// owned sandbox made by hand stays reachable by the name `list` shows.
pub(crate) fn with_prefix(name: &str) -> String {
    if name.starts_with(NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{NAME_PREFIX}{name}")
    }
}

// ── Worker listing ───────────────────────────────────────────────────────
// Shared by `list` and, as they are added, by every subcommand that
// resolves a name.

/// An owned tlworker: an [`api::Sandbox`] whose name passed the ownership
/// test, with the name unwrapped so callers never re-check the `Option`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Worker {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) cpus: f64,
    pub(crate) memory_mb: u64,
}

/// The ownership test itself (see module docs), plus unwrapping the name.
///
/// A terminated sandbox is dropped too. Tensorlake's listing already leaves
/// those out, so this is a backstop: a terminated worker is gone, and
/// `destroy` acting on it again, or `ssh` picking it as the sole worker,
/// would only produce a confusing error.
fn owned(sandbox: api::Sandbox) -> Option<Worker> {
    let name = sandbox.name.filter(|name| name.starts_with(NAME_PREFIX))?;
    if sandbox.status == "terminated" {
        return None;
    }
    Some(Worker {
        name,
        id: sandbox.id,
        status: sandbox.status,
        cpus: sandbox.resources.cpus,
        memory_mb: sandbox.resources.memory_mb,
    })
}

/// List every sandbox in the project and filter down to the ones kd owns.
pub(crate) fn owned_workers(api: &api::Api) -> anyhow::Result<Vec<Worker>> {
    let sandboxes = api.list_sandboxes()?;
    let total = sandboxes.len();
    let workers: Vec<Worker> = sandboxes.into_iter().filter_map(owned).collect();
    // Same -v aid as the other fleets: a worker "missing" from `list` is
    // almost always a sandbox whose name lacks the prefix, and the gap
    // between the two counts shows that without hand-running `tl sbx ls`.
    debug!(
        "tensorlake: {} sandboxes total, {} owned tlworkers",
        total,
        workers.len()
    );
    Ok(workers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: Option<&str>, status: &str) -> api::Sandbox {
        api::Sandbox {
            id: "sbx".to_string(),
            name: name.map(str::to_string),
            status: status.to_string(),
            resources: api::Resources {
                cpus: 4.0,
                memory_mb: 16384,
            },
        }
    }

    /// A bare name and its prefixed form must name the same worker, which is
    /// what lets `destroy foo` find `tlworker-foo`.
    #[test]
    fn with_prefix_adds_prefix_only_when_missing() {
        assert_eq!(with_prefix("foo"), "tlworker-foo");
        assert_eq!(with_prefix("tlworker-foo"), "tlworker-foo");
    }

    /// Ownership is the prefix at the start of the name and nothing else:
    /// an unnamed sandbox, an unrelated name, and a name that merely
    /// contains the prefix later on are all someone else's.
    #[test]
    fn owned_requires_the_prefix_at_the_start() {
        assert!(owned(sandbox(Some("tlworker-a"), "running")).is_some());
        assert!(owned(sandbox(None, "running")).is_none());
        assert!(owned(sandbox(Some("farhelm-host-a"), "running")).is_none());
        assert!(owned(sandbox(Some("my-tlworker-a"), "running")).is_none());
    }

    /// Suspended and pending workers are still kd's (they still bill, or
    /// will); only a terminated one is dropped.
    #[test]
    fn owned_keeps_every_state_but_terminated() {
        for status in ["running", "suspended", "pending", "resume_failed"] {
            assert!(
                owned(sandbox(Some("tlworker-a"), status)).is_some(),
                "{status}"
            );
        }
        assert!(owned(sandbox(Some("tlworker-a"), "terminated")).is_none());
    }
}
