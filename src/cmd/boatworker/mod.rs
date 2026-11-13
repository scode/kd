//! boat.dev worker VM lifecycle: the boat counterpart of `kd ubiworker`.
//! Boat calls its VMs sandboxes; a boatworker is a disposable sandbox that kd
//! lists, connects to and destroys with the same rules as a ubiworker.
//!
//! kd drives boat entirely through the `boat` CLI, which keeps its own login
//! (`boat login` / `boat onboard`), so unlike ubiworker there is no API
//! token for kd to preflight: a signed-out `boat` fails at its first call
//! with boat's own error, and kd surfaces that unchanged.
//!
//! # Ownership convention
//!
//! A sandbox is a "boatworker" iff its boat `name` starts with
//! [`NAME_PREFIX`]. That is the whole test. ubiworker's second condition, a
//! fixed Ubicloud location, has no boat equivalent because boat has no
//! locations. Sandboxes boat creates without a name (the CLI's default) are
//! never owned.
//!
//! The test applies within whatever account scope the `boat` CLI is in: the
//! personal account, unless `BOAT_ORG` or `boat org switch` selects an
//! organization. In an organization, boat's listing can include sandboxes
//! other members shared, and a shared one named `boatworker-...` counts as
//! owned too. Each record does say how the caller may use it (an `access`
//! field, `owner` for one's own), but its other values are undocumented, so
//! kd does not filter on it rather than risk losing track of its own
//! workers. kd is a single-operator tool, and the personal scope is the one
//! it is built for.
//!
//! Every action resolves by name but acts on the immutable `bx_...` id, for
//! the same reason `kd ubiworker destroy` acts on Ubicloud's VM id: a name
//! can be reused between kd's listing and its action, an id cannot.

pub mod destroy;
pub mod list;
pub mod ssh;

use anyhow::Context;
use clap::{Args, Subcommand};
use serde::Deserialize;
use tracing::{debug, warn};
use xshell::{Shell, cmd};

/// Prefix marking a boat sandbox as a kd-managed boatworker (see module docs
/// on the ownership convention).
pub(crate) const NAME_PREFIX: &str = "boatworker-";

/// `kd boatworker destroy [NAME...] [--all]` arguments.
#[derive(Args)]
pub struct DestroyArgs {
    /// Worker name(s) to destroy. If omitted (and `--all` isn't given),
    /// exactly one owned boatworker must exist and it is targeted
    /// automatically.
    pub names: Vec<String>,

    /// Destroy every owned boatworker. A no-op (not an error) if none
    /// exist, so scripted cleanup doesn't have to special-case an empty
    /// owned-worker listing.
    #[arg(short, long, conflicts_with = "names")]
    pub all: bool,
}

/// `kd boatworker ssh [ARGS...]` arguments.
///
/// A single trailing capture rather than a name positional plus a separate
/// argument vector, for the reason `kd ubiworker ssh` documents on its own
/// `SshArgs`: with two positionals, clap's global `-v`/`-q` would steal a
/// flag-shaped token meant for the remote side once the name was filled.
/// `ssh::split_target` then reads the first element as an optional worker
/// name. `OsString` so arguments reach `boat ssh` byte for byte.
#[derive(Args)]
pub struct SshArgs {
    /// Optionally a worker name, then arguments forwarded verbatim to
    /// `boat ssh` after the sandbox id: a remote command, optionally preceded
    /// by boat's own `-t` to give it a terminal.
    ///
    /// The first element is the worker name unless it starts with `-`. A
    /// leading `-v`, `-q` or `-h` (or `--verbose`, `--quiet`, `--help`) is
    /// claimed by kd itself, as anywhere else on the command line. ssh's own
    /// flags such as `-L` cannot be passed through `boat ssh`; use `boat
    /// forward` for port forwarding.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "NAME_OR_BOAT_SSH_ARG"
    )]
    pub args: Vec<std::ffi::OsString>,
}

/// `kd boatworker` subcommands. Each one's doc comment is its `--help` text.
#[derive(Subcommand)]
pub enum Commands {
    /// Delete one or more boatworker sandboxes
    ///
    /// Takes any number of names, or --all to delete every owned
    /// boatworker. With neither, targets the sole existing boatworker (an
    /// error if there are zero or more than one). There is no confirmation
    /// prompt (see SPEC.md); resolution is fail-closed, so a name that
    /// doesn't match deletes nothing rather than partially executing.
    Destroy(DestroyArgs),
    /// List existing boatworker sandboxes
    List,
    /// Open a shell on an owned boatworker, or run a command there, via boat ssh
    ///
    /// With no name, targets the sole existing boatworker, like `destroy`.
    /// The first argument is the worker name unless it starts with `-`;
    /// everything after it goes to `boat ssh <id>` unchanged: a remote
    /// command, optionally preceded by `-t` for a terminal. kd execs into
    /// `boat ssh`, replacing itself. ssh flags such as `-L` do not pass
    /// through boat (use `boat forward`), and a failing remote command exits
    /// with status 1 rather than its own status (see SPEC.md).
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

/// Add the `boatworker-` ownership prefix to a user-supplied name if it's
/// missing, so `foo` and `boatworker-foo` name the same worker.
///
/// This is all a name used for lookup goes through. It is deliberately not
/// validated: boat accepts nearly any sandbox name, so an owned sandbox made
/// by hand can carry a name kd would never have chosen, and `destroy` and
/// `ssh` must still be able to find it by the name `list` shows.
pub(crate) fn with_prefix(name: &str) -> String {
    if name.starts_with(NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{NAME_PREFIX}{name}")
    }
}

// ── Worker listing ───────────────────────────────────────────────────────
// Shared by `list` and by every subcommand that resolves a name.

/// One sandbox from `boat list --all --json`, reduced to the fields kd uses.
/// Boat's records carry several dozen more; serde ignores them, so a field
/// boat adds or drops elsewhere in the record cannot break parsing.
///
/// `name` is optional because boat only names a sandbox when asked to
/// (`boat new --name`). An unnamed sandbox is never a boatworker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Sandbox {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) state: String,
    #[serde(rename = "type")]
    pub(crate) machine_type: String,
}

/// The top level of `boat list --json`: one page of sandboxes plus the
/// paging information.
#[derive(Debug, Deserialize)]
struct SandboxPage {
    sandboxes: Vec<Sandbox>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

/// Boat's paging information. Only `hasMore` matters to kd: it is how kd
/// knows the listing it is working from is incomplete (see
/// [`owned_workers`]).
#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasMore")]
    has_more: bool,
}

/// An owned boatworker: a [`Sandbox`] whose name passed the ownership test,
/// with the name unwrapped so callers never re-check the `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worker {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) machine_type: String,
}

/// The owned workers kd could see, and whether that is all of them.
///
/// `complete` is false when boat said there were more sandboxes than its
/// one page held. Callers that would otherwise infer something from what is
/// *absent* (the no-name "sole worker" forms of `destroy` and `ssh`) must
/// refuse in that case, since the worker they would pick might not be the
/// only one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedWorkers {
    pub(crate) workers: Vec<Worker>,
    pub(crate) complete: bool,
}

/// Parse `boat list --json` output, failing on anything that is not the
/// expected shape.
///
/// Fail-closed for the same reason as ubiworker's `parse_vm_list`: a
/// listing kd only half understood must not turn into an empty or partial
/// list of workers, which `destroy` would then act on.
fn parse_sandbox_page(json: &str) -> anyhow::Result<SandboxPage> {
    serde_json::from_str(json).context("failed to parse 'boat list --json' output")
}

/// The ownership test itself (see module docs), plus unwrapping the name.
fn owned(sandbox: Sandbox) -> Option<Worker> {
    let name = sandbox.name.filter(|name| name.starts_with(NAME_PREFIX))?;
    Some(Worker {
        name,
        id: sandbox.id,
        state: sandbox.state,
        machine_type: sandbox.machine_type,
    })
}

/// Reduce a parsed page to the owned workers on it.
fn owned_workers_from(page: SandboxPage) -> OwnedWorkers {
    OwnedWorkers {
        workers: page.sandboxes.into_iter().filter_map(owned).collect(),
        complete: !page.page_info.has_more,
    }
}

/// List every sandbox in the account, in every state, and filter down to
/// the ones kd owns.
///
/// `--all` because boat's default listing shows only running sandboxes. A
/// boatworker that is still provisioning, or that went to `error`, is still
/// kd's, still listed by `kd boatworker list`, and still something `destroy`
/// must be able to find.
///
/// This reads one page. Boat's page holds 200 sandboxes, and the CLI has no
/// cursor flag to read further. ubiworker accepts the same single-page
/// limit for Ubicloud's listing; the difference here is that boat says when
/// there is more, so kd warns instead of truncating silently, and reports
/// the listing as incomplete (see [`OwnedWorkers`]).
pub(crate) fn owned_workers(sh: &Shell) -> anyhow::Result<OwnedWorkers> {
    let output = cmd!(sh, "boat list --all --json")
        .read()
        .context("failed to list boat sandboxes")?;
    let page = parse_sandbox_page(&output)?;
    let total = page.sandboxes.len();
    let owned = owned_workers_from(page);
    if !owned.complete {
        warn!(
            "boat listed only the first {total} sandboxes in this account and has more; \
             boatworkers beyond that page are not visible to kd"
        );
    }
    // Same -v aid as ubiworker: a worker "missing" from `list` is almost
    // always a sandbox whose name lacks the prefix, and the gap between the
    // two counts is what shows that without hand-running `boat list`.
    debug!(
        "boat list: {} sandboxes total, {} owned boatworkers",
        total,
        owned.workers.len()
    );
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare name and its prefixed form must name the same worker, which is
    /// what lets `destroy foo` find `boatworker-foo`.
    #[test]
    fn with_prefix_adds_prefix_only_when_missing() {
        assert_eq!(with_prefix("foo"), "boatworker-foo");
        assert_eq!(with_prefix("boatworker-foo"), "boatworker-foo");
    }

    /// A lookup name is not validated: an owned sandbox made by hand with a
    /// name kd would never choose must still be reachable by that name.
    #[test]
    fn with_prefix_keeps_unusual_names_intact() {
        assert_eq!(with_prefix("My_Box"), "boatworker-My_Box");
    }

    /// A trimmed copy of real `boat list --all --json` output (boat CLI
    /// 1.0.97), with a few of the fields kd ignores left in to show they
    /// don't matter, and identifying values replaced.
    const LIST_FIXTURE: &str = r#"{
      "pageInfo": {"hasMore": false, "limit": 200, "nextCursor": null},
      "sandboxes": [
        {"access": "owner", "id": "bx_aaaaaaaa", "name": "boatworker-dev1", "state": "idle",
         "type": "large", "vcpu": 8, "memoryGB": 16, "setupStatus": "done", "labels": {}},
        {"access": "owner", "id": "bx_bbbbbbbb", "name": "something-else", "state": "ready",
         "type": "default"},
        {"access": "owner", "id": "bx_cccccccc", "name": null, "state": "error", "type": "small"}
      ]
    }"#;

    /// The ownership filter keeps only prefixed names and drops both an
    /// unrelated name and an unnamed sandbox; the kept worker carries the
    /// fields `list` prints, and a page without `hasMore` is complete.
    #[test]
    fn owned_workers_from_filters_real_listing_by_prefix() {
        let owned = owned_workers_from(parse_sandbox_page(LIST_FIXTURE).unwrap());
        assert_eq!(
            owned,
            OwnedWorkers {
                workers: vec![Worker {
                    name: "boatworker-dev1".to_string(),
                    id: "bx_aaaaaaaa".to_string(),
                    state: "idle".to_string(),
                    machine_type: "large".to_string(),
                }],
                complete: true,
            }
        );
    }

    /// `hasMore` must surface as an incomplete listing, which is what makes
    /// the no-name forms refuse instead of guessing.
    #[test]
    fn owned_workers_from_marks_truncated_page_incomplete() {
        let json = r#"{"pageInfo": {"hasMore": true}, "sandboxes": []}"#;
        assert!(!owned_workers_from(parse_sandbox_page(json).unwrap()).complete);
    }

    /// The prefix test is a prefix test, not a substring test: a name that
    /// merely contains `boatworker-` somewhere else is not owned.
    #[test]
    fn owned_requires_the_prefix_at_the_start() {
        let sandbox = Sandbox {
            id: "bx_x".to_string(),
            name: Some("my-boatworker-1".to_string()),
            state: "idle".to_string(),
            machine_type: "large".to_string(),
        };
        assert_eq!(owned(sandbox), None);
    }

    /// Output kd doesn't understand (not JSON, or JSON missing the fields it
    /// needs) must be an error, never an empty listing: an empty listing
    /// would read as "no workers" and let `destroy --all` succeed as a
    /// no-op, or make a no-argument `destroy` report that none exist.
    #[test]
    fn parse_sandbox_page_rejects_unexpected_shapes() {
        for bad in [
            "not json",
            r#"{"sandboxes": []}"#,
            r#"{"pageInfo": {"hasMore": false}}"#,
            r#"{"pageInfo": {"hasMore": false}, "sandboxes": [{"name": "boatworker-x"}]}"#,
        ] {
            assert!(parse_sandbox_page(bad).is_err(), "{bad:?} should fail");
        }
    }
}
