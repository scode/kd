//! `kd boatworker ssh [ARGS...]`: open a shell on an owned boatworker, or
//! run a command there, through `boat ssh`.
//!
//! kd resolves which worker is meant through the owned-worker listing, then
//! [`std::os::unix::process::CommandExt::exec`]s into `boat ssh <id> ...`,
//! replacing itself, so signals and tty handling behave as if `boat ssh`
//! had been typed by hand.
//!
//! # Why `boat ssh` and not `tailscale ssh`
//!
//! `kd ubiworker ssh` goes through Tailscale because that is what gives it
//! host-key verification for a fleet of reused names. Boat already solves
//! that problem: `boat ssh` looks up the sandbox's address and verifies its
//! host key against what boat's API reports, with a key pair of its own
//! (`~/.ssh/ascii_box_ed25519`). It works the moment the sandbox is ready,
//! whether or not the sandbox is on the tailnet, and needs no tailnet
//! policy grant on the machine running kd.
//!
//! # What does not carry over from `kd ubiworker ssh`
//!
//! `boat ssh` is not a thin wrapper over OpenSSH the way `tailscale ssh` is.
//! Everything after the sandbox id is a remote command whose argument
//! boundaries boat preserves; ssh's own flags (`-L`, `-D`, ...) cannot be
//! passed through it (`boat forward` covers port forwarding). The one flag
//! it takes in that position is its own `-t`, which allocates a terminal for
//! the command. And a remote command's non-zero exit status comes back as
//! exit status 1, with a JSON error line from boat on stdout, rather than as
//! the command's own status.
//!
//! `--` does not help either. After `--`, `boat ssh` joins everything into
//! one string for the underlying ssh client, so argument boundaries are lost
//! there, and a lone flag such as `-v` ends up as an ssh option rather than
//! a command. `kd ubiworker ssh -- -v`, which forwards `-v` to ssh, has no
//! working boatworker equivalent: `kd boatworker ssh -- -v` reaches boat as
//! `boat ssh <id> -v`, which boat refuses.

use super::{OwnedWorkers, SshArgs, Worker, owned_workers, with_prefix};
use anyhow::{Context, bail};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use tracing::info;
use xshell::Shell;

/// Entry point for `kd boatworker ssh`: resolve the worker, then replace
/// this process with `boat ssh`. Returns only if the exec itself fails.
pub fn run(args: SshArgs) -> anyhow::Result<()> {
    let sh = Shell::new()?;

    let (name_token, boat_args) = split_target(&args.args);
    // Names are compared against boat's names, which are UTF-8 strings, so
    // a non-UTF-8 first token can never match; saying so beats a confusing
    // "no boatworker named ..." with a mangled name in it.
    let name = name_token
        .map(|token| {
            token
                .to_str()
                .map(with_prefix)
                .ok_or_else(|| anyhow::anyhow!("worker name is not valid UTF-8: {token:?}"))
        })
        .transpose()?;

    let owned = owned_workers(&sh)?;
    let target = resolve_target(&owned, name.as_deref())?;
    info!("connecting to boatworker '{}' ({})", target.name, target.id);

    let argv = build_boat_ssh_argv(&target.id, boat_args);
    let err = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        // The Tailscale OAuth pair kd's worker commands use is often exported
        // in the same shell. boat needs neither, and an ssh-config helper
        // boat's ssh might run would otherwise inherit them from kd.
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        .exec();
    // Name the program and the target only, never the forwarded arguments:
    // they can carry secrets, and joining them would lose their boundaries.
    Err(err).with_context(|| {
        format!(
            "failed to exec boat ssh to boatworker '{}' ({})",
            target.name, target.id
        )
    })
}

/// Split the trailing capture into an optional worker-name token and the
/// arguments for `boat ssh`: the first element is the name iff it does not
/// start with `-`.
///
/// The rule is `kd ubiworker ssh`'s. It costs nothing here: every owned
/// worker's full name starts with `boatworker-`, so any worker can always
/// be named in that form, and a leading hyphen therefore always means "no
/// name, and this is for boat". Pure, so it is unit-tested directly.
fn split_target(args: &[OsString]) -> (Option<&OsStr>, &[OsString]) {
    match args.split_first() {
        Some((first, rest)) if !first.as_encoded_bytes().starts_with(b"-") => {
            (Some(first.as_os_str()), rest)
        }
        _ => (None, args),
    }
}

/// Resolve the one worker to connect to: the one named `name`, or with no
/// name the sole owned worker.
///
/// Boat documents no uniqueness rule for sandbox names, so a name can match
/// several sandboxes. `destroy` treats that as "all of them", but a
/// connection has to go to exactly one, and picking an arbitrary one would
/// be a silent wrong-host connection; listing the ids lets the user pick
/// with `boat ssh <id>` or destroy the extras.
///
/// The no-name form also refuses an incomplete listing, for the same reason
/// as `destroy`'s: "the sole worker" cannot be inferred from a listing that
/// was cut off. Pure, so each branch is unit-tested.
fn resolve_target<'a>(owned: &'a OwnedWorkers, name: Option<&str>) -> anyhow::Result<&'a Worker> {
    if name.is_none() && !owned.complete {
        bail!(
            "boat's sandbox listing was cut off, so kd cannot tell whether there is only one \
             boatworker; specify a name"
        );
    }
    let candidates: Vec<&Worker> = match name {
        Some(name) => owned.workers.iter().filter(|w| w.name == name).collect(),
        None => owned.workers.iter().collect(),
    };
    match (name, candidates.as_slice()) {
        (_, [worker]) => Ok(worker),
        (Some(name), []) => bail!("no boatworker named '{name}' exists"),
        (None, []) => bail!("no boatworkers exist"),
        (Some(name), several) => {
            let ids: Vec<&str> = several.iter().map(|w| w.id.as_str()).collect();
            bail!(
                "several sandboxes are named '{name}' ({}); connect with `boat ssh <id>` \
                 or destroy the extras",
                ids.join(", ")
            )
        }
        (None, several) => {
            let names: Vec<String> = several
                .iter()
                .map(|w| format!("{} ({})", w.name, w.id))
                .collect();
            bail!(
                "multiple boatworkers exist ({}); specify a name",
                names.join(", ")
            )
        }
    }
}

/// The full `boat ssh` argv, program name included: `boat ssh <id>` followed
/// by the forwarded arguments, verbatim and in order.
///
/// Nothing is inserted between the id and the forwarded arguments, `--` in
/// particular. That keeps boat's own `-t` reachable (`kd boatworker ssh foo
/// -t tmux attach`), and a `--` would buy nothing, because boat joins what
/// follows it into one string, so ssh flags still would not work.
fn build_boat_ssh_argv(id: &str, boat_args: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["boat".into(), "ssh".into(), id.into()];
    argv.extend(boat_args.iter().cloned());
    argv
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

    // ── resolve_target ───────────────────────────────────────────────────

    #[test]
    fn resolve_target_named_and_found() {
        let owned = complete(vec![
            worker("boatworker-a", "bx_a"),
            worker("boatworker-b", "bx_b"),
        ]);
        assert_eq!(
            resolve_target(&owned, Some("boatworker-b")).unwrap().id,
            "bx_b"
        );
    }

    /// Exact match only: a requested name that is a prefix of a real
    /// worker's name must not connect to it. A "forgiving" lookup here would
    /// turn a typo into a session on the wrong machine.
    #[test]
    fn resolve_target_rejects_prefix_match() {
        let owned = complete(vec![worker("boatworker-foobar", "bx_a")]);
        let msg = resolve_target(&owned, Some("boatworker-foo"))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("no boatworker named 'boatworker-foo'"),
            "{msg}"
        );
    }

    /// Two sandboxes with the requested name must be an error that lists
    /// both ids, never a connection to whichever came first.
    #[test]
    fn resolve_target_refuses_ambiguous_name_listing_ids() {
        let owned = complete(vec![
            worker("boatworker-a", "bx_1"),
            worker("boatworker-a", "bx_2"),
        ]);
        let msg = resolve_target(&owned, Some("boatworker-a"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("bx_1") && msg.contains("bx_2"), "{msg}");
    }

    /// With no name, exactly one owned worker is chosen; zero or several
    /// are errors, and unlike `destroy` the error does not suggest `--all`,
    /// which `ssh` does not have.
    #[test]
    fn resolve_target_no_name_needs_exactly_one_worker() {
        let one = complete(vec![worker("boatworker-a", "bx_a")]);
        assert_eq!(resolve_target(&one, None).unwrap().id, "bx_a");

        let msg = resolve_target(&complete(vec![]), None)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no boatworkers exist"), "{msg}");

        let two = complete(vec![
            worker("boatworker-a", "bx_a"),
            worker("boatworker-b", "bx_b"),
        ]);
        let msg = resolve_target(&two, None).unwrap_err().to_string();
        assert!(
            msg.contains("boatworker-a (bx_a)") && msg.contains("boatworker-b (bx_b)"),
            "{msg}"
        );
        assert!(!msg.contains("--all"), "{msg}");
    }

    /// A cut-off listing that happens to show one worker must not be read
    /// as "the sole worker"; a named lookup still works on what was seen.
    #[test]
    fn resolve_target_no_name_refuses_incomplete_listing() {
        let owned = OwnedWorkers {
            workers: vec![worker("boatworker-a", "bx_a")],
            complete: false,
        };
        let msg = resolve_target(&owned, None).unwrap_err().to_string();
        assert!(msg.contains("cut off"), "{msg}");
        assert_eq!(
            resolve_target(&owned, Some("boatworker-a")).unwrap().id,
            "bx_a"
        );
    }

    // ── build_boat_ssh_argv ──────────────────────────────────────────────

    /// With nothing forwarded, the argv is exactly `boat ssh <id>`: an
    /// interactive shell on the resolved sandbox, addressed by id.
    #[test]
    fn build_boat_ssh_argv_no_extra_args() {
        assert_eq!(
            build_boat_ssh_argv("bx_a", &[]),
            vec!["boat", "ssh", "bx_a"]
        );
    }

    /// Forwarded arguments follow the id verbatim and in order, with no
    /// `--` inserted, so boat's own `-t` stays reachable in front of a
    /// remote command (see the function docs for why).
    #[test]
    fn build_boat_ssh_argv_forwards_tty_flag_and_command_verbatim() {
        let args: Vec<OsString> = vec!["-t".into(), "tmux".into(), "attach".into()];
        assert_eq!(
            build_boat_ssh_argv("bx_a", &args),
            vec!["boat", "ssh", "bx_a", "-t", "tmux", "attach"]
        );
    }

    // ── split_target ─────────────────────────────────────────────────────

    #[test]
    fn split_target_empty_is_no_name_no_args() {
        let args: Vec<OsString> = vec![];
        assert_eq!(split_target(&args), (None, &args[..]));
    }

    /// `kd boatworker ssh foo uptime`: the name, then a remote command.
    #[test]
    fn split_target_name_then_command() {
        let args: Vec<OsString> = vec!["foo".into(), "uptime".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("foo")));
        assert_eq!(rest, [OsString::from("uptime")]);
    }

    /// `kd boatworker ssh -t htop`: a leading hyphen means no name, so the
    /// sole worker is targeted and everything goes to boat.
    #[test]
    fn split_target_leading_flag_is_no_name() {
        let args: Vec<OsString> = vec!["-t".into(), "htop".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, None);
        assert_eq!(rest, &args[..]);
    }
}
