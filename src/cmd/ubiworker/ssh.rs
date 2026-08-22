//! `kd ubiworker ssh [ARGS...]`: connect to an owned ubiworker over
//! Tailscale SSH, resolving the target worker the same way `destroy` does.
//!
//! kd never speaks the ssh protocol itself. It resolves which worker is
//! meant, builds an argv, and then [`std::os::unix::process::CommandExt::exec`]s
//! straight into `tailscale ssh` — the kd process is replaced, not
//! spawned-and-waited-on, so the exit code, signals, and tty handling all
//! pass straight through exactly as if the user had typed `tailscale ssh
//! ...` themselves. That's also why there's no polling/retry here for a
//! worker that hasn't finished enrolling yet (mirroring `create`'s
//! no-polling stance, see module docs): a not-yet-reachable worker just
//! surfaces the connection error, unmodified.
//!
//! # Why `tailscale ssh` rather than plain `ssh`
//!
//! `tailscale ssh` is a thin wrapper that runs the system `ssh` binary, so
//! ssh's usual flags and remote-command syntax still work. What it adds is
//! the identity story this fleet needs: it collects the peers' host keys
//! as advertised through the Tailscale coordination server into a
//! Tailscale-managed known-hosts file, and runs `ssh` with that file as
//! `UserKnownHostsFile` under `StrictHostKeyChecking yes` (plus, where
//! needed, a `ProxyCommand` through tailscaled for transport). So the
//! worker's identity is checked against what the tailnet says it is, and
//! `~/.ssh/known_hosts` is never consulted or written.
//! Ordinary known-hosts pinning is a poor fit here — worker names are
//! reused, and every fresh VM boot mints a fresh OpenSSH host key, so
//! reconnecting to a recreated `ubiworker-foo` would hard-fail with
//! "REMOTE HOST IDENTIFICATION HAS CHANGED". An earlier version of this
//! command worked around that by disabling host-key checking outright;
//! delegating to `tailscale ssh` removes that bypass entirely while keeping
//! reconnection friction-free, which is why this command no longer offers a
//! plain-`ssh` mode at all. Anyone who wants unverified plain ssh can run
//! it by hand outside kd. Login authorization is the tailnet policy's `ssh`
//! section, not keys; see SPEC.md for the rule the tailnet needs.

use super::{SshArgs, VmRow, local_unix_user, normalize_worker_name, owned_workers, require_env};
use anyhow::{Context, bail};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use tracing::info;
use xshell::Shell;

pub fn run(args: SshArgs) -> anyhow::Result<()> {
    // Same fail-fast rationale as the other subcommands: `ubi` reads
    // UBI_TOKEN itself (needed here for the `ubi vm list` resolution
    // round-trip below), kd just surfaces a missing token as a clear error
    // up front instead of letting it fail deep inside `ubi`.
    require_env("UBI_TOKEN")?;
    let sh = Shell::new()?;

    let (name_token, ssh_args) = split_target(&args.args);
    // A name token is required to be UTF-8: the worker-name charset
    // (`has_valid_name_chars`) is ASCII-only, so a non-UTF-8 first token can
    // only be garbage, never a name `normalize_worker_name` would accept
    // anyway. Rejecting it here, with a message that says *why*, is clearer
    // than letting an opaque `Utf8Error`-shaped failure fall out of
    // `normalize_worker_name` (which takes `&str`).
    let name = name_token
        .map(|token| {
            token
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("worker name is not valid UTF-8: {token:?}"))
        })
        .transpose()?
        // Normalize before listing, not after — same reasoning as `destroy`:
        // an explicit name must be matched against the same prefixed form
        // `owned_workers` reports it under.
        .map(normalize_worker_name)
        .transpose()?;

    // The login account is the local username, mirroring what `create`
    // provisioned (see `local_unix_user` for the same-operator assumption).
    let unix_user = local_unix_user(&sh)?;
    let rows = owned_workers(&sh)?;
    let target = resolve_target(&rows, name.as_deref())?;

    info!("connecting to ubiworker '{}' as '{unix_user}'", target.name);

    // Built once and shared by the argv and the error message below, so the
    // destination kd reports on failure is by construction the one it
    // actually tried.
    let destination = format!("{unix_user}@{}", target.name);
    let ssh_argv = build_ssh_argv(&destination, ssh_args);
    // exec replaces this process image outright; on success this call
    // never returns. On failure it returns the io::Error that `execvp(2)`
    // produced (e.g. tailscale not found on PATH) — surface it with the
    // program and destination that was attempted, not the full argv:
    // forwarded ssh args can carry secrets (e.g. a `SendEnv`-adjacent value
    // or a credential embedded in a remote command), and joining them with
    // spaces would also lose their original argument boundaries.
    let err = std::process::Command::new(&ssh_argv[0])
        .args(&ssh_argv[1..])
        // Neither tailscale nor the ssh it wraps needs kd's own credentials,
        // and an ssh-config helper the user has configured (`ProxyCommand`,
        // `SendEnv`, etc.) would otherwise inherit them from this process's
        // environment by default.
        .env_remove("UBI_TOKEN")
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        // Same leak path as `UBI_DEBUG` in `create`: with this knob set,
        // `tailscale ssh` logs the complete OpenSSH argv it builds —
        // forwarded args included — which is exactly what the error
        // message above goes out of its way not to print.
        .env_remove("TS_DEBUG_SSH_EXEC")
        .exec();
    Err(err).with_context(|| format!("failed to exec tailscale ssh to '{destination}'"))
}

/// Split the raw trailing capture into an optional worker-name token and the
/// remaining ssh arguments: the first element is the name iff it does not
/// start with `-`.
///
/// This rule is sound — not just convenient — because of the worker-name
/// charset: [`super::has_valid_name_chars`] requires the first character to
/// be alphanumeric, so no legal worker name can start with `-`. That means
/// "first token starts with `-`" and "first token is not a worker name" are
/// exactly the same condition; the split can't misfire by mistaking a real
/// name for an ssh flag, or vice versa. A leading `-` token is therefore
/// unambiguously routed to `ssh` rather than treated as a name that would
/// fail `normalize_worker_name` a moment later.
///
/// Pure over its input (no I/O) so both branches — and the empty-slice case
/// — are directly unit-testable.
fn split_target(args: &[OsString]) -> (Option<&OsStr>, &[OsString]) {
    match args.split_first() {
        Some((first, rest)) if !first.as_encoded_bytes().starts_with(b"-") => {
            (Some(first.as_os_str()), rest)
        }
        _ => (None, args),
    }
}

/// Resolve the `ssh` target out of the already-fetched, already-owned
/// `rows`: the row matching `name` if given, or — if `name` is `None` —
/// the sole row, erroring on zero or multiple.
///
/// This is `destroy::resolve_targets`'s no-name/single-name path narrowed
/// to exactly one result, since `ssh` (unlike `destroy`) never targets more
/// than one worker at a time and has no `--all` equivalent. Kept as a
/// separate pure function (rather than reusing or generalizing
/// `destroy`'s) so each stays simple and independently testable, and so a
/// future change to one doesn't have to reason about the other's callers.
///
/// Pure over its inputs (no shelling out, no I/O) so every branch —
/// named-and-found, named-and-missing, and the zero/one/many cases of the
/// no-argument path — is unit-testable directly.
fn resolve_target<'a>(rows: &'a [VmRow], name: Option<&str>) -> anyhow::Result<&'a VmRow> {
    if let Some(name) = name {
        return rows
            .iter()
            .find(|row| row.name == name)
            .ok_or_else(|| anyhow::anyhow!("no ubiworker named '{name}' exists"));
    }

    match rows {
        [] => bail!("no ubiworkers exist"),
        [row] => Ok(row),
        multiple => {
            let names: Vec<&str> = multiple.iter().map(|w| w.name.as_str()).collect();
            // Unlike `destroy`'s equivalent message, this doesn't suggest
            // `--all`: `ssh` only ever connects to one worker, so there is
            // no bulk-target flag to point at.
            bail!(
                "multiple ubiworkers exist ({}); specify a name",
                names.join(", ")
            );
        }
    }
}

/// Build the full `tailscale ssh` argv (program name included, as element
/// 0) for connecting to `destination` (`user@host`), with `ssh_args`
/// forwarded verbatim after it.
///
/// The shape is fixed by `tailscale ssh`'s own usage, `tailscale ssh
/// [user@]<host> [args...]`: the destination must be the first non-flag
/// argument, and everything after it is handed to the underlying `ssh`
/// unchanged. That's why `ssh_args` is appended, never merged or
/// reordered — OpenSSH parses flags positioned after the destination just
/// fine and treats the first non-flag argument as the start of a remote
/// command, so both a forwarded flag (`-L 8080:localhost:80`) and a
/// forwarded remote command (`uptime`) work correctly in this order without
/// kd distinguishing the two cases. No host-key options are passed: the
/// host-key check is `tailscale ssh`'s job (see the module docs).
///
/// The caller passes a fully formed `user@host` even though the user is the
/// local username and `tailscale ssh` would default to exactly that: being
/// explicit keeps the argv self-describing in `ps`/error output and means
/// the destination doesn't silently change if `tailscale ssh`'s defaulting
/// rules ever do.
///
/// `OsString` end to end (not `String`): this is the same "forward
/// verbatim, including non-UTF-8 bytes" contract as `SshArgs::args` — see
/// its docs — and converting to `String` here would silently reintroduce
/// the restriction one layer down.
///
/// Pure and I/O-free (just vec assembly) so it's directly unit-testable
/// without actually invoking `tailscale`.
fn build_ssh_argv(destination: &str, ssh_args: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["tailscale".into(), "ssh".into(), destination.into()];
    argv.extend(ssh_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_row(name: &str) -> VmRow {
        VmRow {
            location: "us-east-a2".to_string(),
            name: name.to_string(),
            id: "id".to_string(),
            ip4: Some("1.2.3.4".to_string()),
        }
    }

    #[test]
    fn resolve_target_named_and_found() {
        let rows = vec![vm_row("ubiworker-a"), vm_row("ubiworker-b")];
        let target = resolve_target(&rows, Some("ubiworker-b")).unwrap();
        assert_eq!(target.name, "ubiworker-b");
    }

    /// A requested name that is a strict *prefix* of an existing worker's
    /// name must not match it. `resolve_target` compares with `==`, not
    /// `starts_with`, but that's exactly the kind of property that's cheap
    /// to break by a well-intentioned "make lookup more forgiving" edit —
    /// and a prefix-match regression here would silently connect the user to
    /// the wrong VM instead of erroring, which is worse than any of the
    /// error-shaped ways this function fails.
    #[test]
    fn resolve_target_rejects_prefix_match() {
        let rows = vec![vm_row("ubiworker-foobar")];
        let err = resolve_target(&rows, Some("ubiworker-foo")).unwrap_err();
        assert!(
            err.to_string().contains("ubiworker-foo"),
            "unexpected error: {err}"
        );
    }

    /// The error must name the worker that was looked for, not just say
    /// "not found" — that's what lets a typo be diagnosed from the error
    /// alone.
    #[test]
    fn resolve_target_named_and_missing_names_the_worker() {
        let rows = vec![vm_row("ubiworker-a")];
        let err = resolve_target(&rows, Some("ubiworker-missing")).unwrap_err();
        assert!(
            err.to_string().contains("ubiworker-missing"),
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
        let rows = vec![vm_row("ubiworker-a")];
        let target = resolve_target(&rows, None).unwrap();
        assert_eq!(target.name, "ubiworker-a");
    }

    /// Multiple owned workers with no explicit name must be an error —
    /// `ssh` should never guess which VM the user meant — and the error
    /// should list the candidates by name. Unlike `destroy`'s equivalent
    /// message, it must *not* suggest `--all`, since `ssh` has no such
    /// flag.
    #[test]
    fn resolve_target_no_arg_multiple_workers_errors_listing_names() {
        let rows = vec![vm_row("ubiworker-a"), vm_row("ubiworker-b")];
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
        assert!(
            !message.contains("--all"),
            "ssh has no --all flag, error should not suggest it: {message}"
        );
    }

    /// The argv must be `tailscale ssh <user>@<host>` and nothing more: in
    /// particular no `StrictHostKeyChecking=no`/`UserKnownHostsFile`-style
    /// options may creep back in. Those were the previous implementation's
    /// host-key bypass, and reintroducing them would silently defeat the
    /// host-key verification that delegating to `tailscale ssh` exists to
    /// provide.
    #[test]
    fn build_ssh_argv_no_extra_args() {
        let argv = build_ssh_argv("alice@ubiworker-foo", &[]);
        assert_eq!(argv, vec!["tailscale", "ssh", "alice@ubiworker-foo"]);
    }

    /// A forwarded flag (as opposed to a remote command) must survive
    /// appended verbatim after the destination, unreordered — `tailscale
    /// ssh` requires the destination first, and this is the port-forwarding
    /// use case (`kd ubiworker ssh name -L 8080:localhost:80`).
    #[test]
    fn build_ssh_argv_forwards_flag_args() {
        let ssh_args: Vec<OsString> = vec!["-L".into(), "8080:localhost:80".into()];
        let argv = build_ssh_argv("alice@ubiworker-foo", &ssh_args);
        assert_eq!(
            argv,
            vec![
                "tailscale",
                "ssh",
                "alice@ubiworker-foo",
                "-L",
                "8080:localhost:80",
            ]
        );
    }

    /// A forwarded remote command (as opposed to an ssh flag) must also
    /// survive appended verbatim — this is the `kd ubiworker ssh name
    /// uptime` use case, and it's what proves kd doesn't need to
    /// distinguish "flag" from "command" itself; OpenSSH does that once
    /// both land after the destination.
    #[test]
    fn build_ssh_argv_forwards_remote_command() {
        let ssh_args: Vec<OsString> = vec!["uptime".into()];
        let argv = build_ssh_argv("alice@ubiworker-foo", &ssh_args);
        assert_eq!(
            argv,
            vec!["tailscale", "ssh", "alice@ubiworker-foo", "uptime"]
        );
    }

    // ── split_target ─────────────────────────────────────────────────────
    // These pin the "first token is the name iff it doesn't start with `-`"
    // rule directly, independent of clap — see `split_target`'s docs for why
    // the rule is sound, and the CLI wiring tests in `cmd::tests` for
    // coverage of the same rule as clap actually parses it end to end.

    #[test]
    fn split_target_empty_is_no_name_no_args() {
        let args: Vec<OsString> = vec![];
        let (name, rest) = split_target(&args);
        assert_eq!(name, None);
        assert!(rest.is_empty());
    }

    /// A bare name with nothing following it: `kd ubiworker ssh myname`.
    #[test]
    fn split_target_single_non_flag_token_is_the_name() {
        let args: Vec<OsString> = vec!["myname".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("myname")));
        assert!(rest.is_empty());
    }

    /// `kd ubiworker ssh myname -v`: the name is consumed, and everything
    /// after it — including a token that collides with one of kd's own
    /// global flags — is left for `ssh`, not reinterpreted by kd. This is
    /// the direct regression test for bug B2.
    #[test]
    fn split_target_name_followed_by_flag_shaped_arg() {
        let args: Vec<OsString> = vec!["myname".into(), "-v".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("myname")));
        assert_eq!(rest, [OsString::from("-v")]);
    }

    /// `kd ubiworker ssh -- -L 8080:x` arrives here as `[-L, 8080:x]` (clap
    /// has already stripped the `--`): a leading hyphen means no name, and
    /// the flag must reach `ssh` intact. This is the direct regression test
    /// for bug B1.
    #[test]
    fn split_target_leading_flag_is_no_name() {
        let args: Vec<OsString> = vec!["-L".into(), "8080:x".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, None);
        assert_eq!(rest, [OsString::from("-L"), OsString::from("8080:x")]);
    }

    /// `kd ubiworker ssh -- uptime`: under the leading-hyphen rule, a bare
    /// word after `--` is a *name* (targeting `ubiworker-uptime`), not a
    /// remote command run on the sole worker. Pinning this here documents
    /// that intentional (if surprising) consequence of the split rule.
    #[test]
    fn split_target_word_after_separator_is_a_name_not_a_command() {
        let args: Vec<OsString> = vec!["uptime".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("uptime")));
        assert!(rest.is_empty());
    }
}
