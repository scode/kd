//! `kd tlworker ssh [ARGS...]`: connect to an owned tlworker over Tailscale
//! SSH, resolving the target worker the same way `destroy` does.
//!
//! This is `kd ubiworker ssh` with Tensorlake's listing and account: kd
//! resolves which worker is meant, builds a `tailscale ssh tl-user@<name>`
//! argv, and execs into it, so exit status, signals and the terminal pass
//! straight through. The reasons for `tailscale ssh` over plain `ssh` (host
//! keys checked against the tailnet, no `~/.ssh/known_hosts` churn for
//! reused names) are the ones `ubiworker::ssh` documents, and they apply
//! unchanged: `create` enrolls every tlworker with Tailscale SSH using its
//! sandbox name as the tailnet hostname, and this command relies on that. A
//! `tlworker-` sandbox made by hand that never enrolled, or one renamed after
//! enrolling, is listed but not reachable here.
//!
//! The one Tensorlake-specific wrinkle is suspension. A tlworker that is not
//! running is off the tailnet, and kd does not resume it (tlworkers are
//! created with auto-resume off, so traffic does not wake them either). kd
//! warns when the target is not running and connects anyway, leaving the
//! connection error to say the rest. Even a resumed worker may not come back
//! on the tailnet: it enrolled as an ephemeral node, and Tailscale removes
//! ephemeral nodes that stay offline.

use super::api::{Api, load_credentials};
use super::{SshArgs, UNIX_USER, Worker, owned_workers, with_prefix};
use anyhow::{Context, bail};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use tracing::{info, warn};

/// Entry point for `kd tlworker ssh`.
pub fn run(args: SshArgs) -> anyhow::Result<()> {
    let api = Api::new(load_credentials()?);

    let (name_token, ssh_args) = split_target(&args.args);
    // Tensorlake names are ASCII, so a non-UTF-8 first token cannot name a
    // worker; say so rather than report it as merely not found.
    let name = name_token
        .map(|token| {
            token
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("worker name is not valid UTF-8: {token:?}"))
        })
        .transpose()?
        .map(with_prefix);

    let workers = owned_workers(&api)?;
    let target = resolve_target(&workers, name.as_deref())?;

    if let Some(warning) = not_running_warning(target) {
        warn!("{warning}");
    }
    info!("connecting to tlworker '{}' as '{UNIX_USER}'", target.name);

    // Built once and shared by the argv and the error message, so the
    // destination kd reports on failure is the one it actually tried.
    let destination = format!("{UNIX_USER}@{}", target.name);
    let ssh_argv = build_ssh_argv(&destination, ssh_args);
    // On success exec never returns. On failure the error names the program
    // and destination only, never the forwarded arguments, which can carry
    // secrets and would lose their boundaries if joined into one string.
    let err = std::process::Command::new(&ssh_argv[0])
        .args(&ssh_argv[1..])
        // Neither tailscale nor the ssh it wraps needs kd's credentials, and
        // an ssh-config helper (`ProxyCommand`, `SendEnv`) would otherwise
        // inherit them from this process.
        .env_remove(super::api::API_KEY_VAR)
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        // With this set, `tailscale ssh` logs the full OpenSSH argv,
        // forwarded arguments included, which the error above avoids
        // printing; see `ubiworker::ssh`.
        .env_remove("TS_DEBUG_SSH_EXEC")
        .exec();
    Err(err).with_context(|| format!("failed to exec tailscale ssh to '{destination}'"))
}

/// The warning for a target that is not running, or `None` when it is.
///
/// Only a suspended sandbox gets the resume hint: `tl sbx resume` acts on
/// suspended sandboxes alone, and for a pending one, or a state Tensorlake
/// adds later, the hint would be wrong. The ephemeral-node caveat is there
/// because resuming does not guarantee the worker is back on the tailnet.
fn not_running_warning(worker: &Worker) -> Option<String> {
    match worker.status.as_str() {
        "running" => None,
        "suspended" => Some(format!(
            "tlworker '{}' is suspended, so it is off the tailnet and this connection will likely \
             fail; resume it with `tl sbx resume {}` (it enrolled as an ephemeral node, so after \
             a long suspension Tailscale may have removed it and it needs recreating)",
            worker.name, worker.id
        )),
        status => Some(format!(
            "tlworker '{}' is {status}, not running, so this connection will likely fail",
            worker.name
        )),
    }
}

/// Split the raw trailing capture into an optional worker-name token and the
/// remaining ssh arguments: the first element is the name iff it does not
/// start with `-`.
///
/// Sound for the same reason as ubiworker's split: a Tensorlake sandbox name
/// must start with a lowercase letter, so no worker name starts with `-`,
/// and "starts with `-`" is exactly "is not a worker name". Pure, so both
/// branches are unit-testable.
fn split_target(args: &[OsString]) -> (Option<&OsStr>, &[OsString]) {
    match args.split_first() {
        Some((first, rest)) if !first.as_encoded_bytes().starts_with(b"-") => {
            (Some(first.as_os_str()), rest)
        }
        _ => (None, args),
    }
}

/// Resolve the one worker `ssh` connects to: the one named `name` (already
/// prefixed), or with no name the sole owned worker, erroring on zero or
/// several. The listing is complete (see `Api::list_sandboxes`), so the
/// no-name form is a claim about the whole project.
fn resolve_target<'a>(workers: &'a [Worker], name: Option<&str>) -> anyhow::Result<&'a Worker> {
    if let Some(name) = name {
        return workers
            .iter()
            .find(|worker| worker.name == name)
            .ok_or_else(|| anyhow::anyhow!("no tlworker named '{name}' exists"));
    }
    match workers {
        [] => bail!("no tlworkers exist"),
        [worker] => Ok(worker),
        multiple => bail!(
            "multiple tlworkers exist ({}); specify a name",
            multiple
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The `tailscale ssh` argv, program included: `tailscale ssh
/// <user>@<host>` with `ssh_args` appended verbatim. No host-key options:
/// checking the host key is `tailscale ssh`'s job (see `ubiworker::ssh`).
fn build_ssh_argv(destination: &str, ssh_args: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["tailscale".into(), "ssh".into(), destination.into()];
    argv.extend(ssh_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A running worker named `name`, with an ID derived from the name so
    /// tests can tell workers apart by either.
    fn worker(name: &str) -> Worker {
        Worker {
            name: name.to_string(),
            id: format!("id-{name}"),
            status: "running".to_string(),
            cpus: 4.0,
            memory_mb: 16384,
        }
    }

    /// A requested name matches exactly, never as a prefix: a prefix match
    /// would quietly connect to the wrong worker.
    #[test]
    fn resolve_target_matches_whole_names_only() {
        let workers = vec![worker("tlworker-foobar"), worker("tlworker-b")];
        assert_eq!(
            resolve_target(&workers, Some("tlworker-b")).unwrap().name,
            "tlworker-b"
        );
        let msg = resolve_target(&workers, Some("tlworker-foo"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("tlworker-foo"), "{msg}");
    }

    /// With no name, exactly one worker is chosen; zero or several are
    /// errors, and the several case lists them without suggesting `--all`,
    /// which `ssh` does not have.
    #[test]
    fn resolve_target_no_name_needs_exactly_one_worker() {
        assert_eq!(
            resolve_target(&[worker("tlworker-a")], None).unwrap().name,
            "tlworker-a"
        );
        assert!(resolve_target(&[], None).is_err());
        let msg = resolve_target(&[worker("tlworker-a"), worker("tlworker-b")], None)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("tlworker-a") && msg.contains("tlworker-b"),
            "{msg}"
        );
        assert!(!msg.contains("--all"), "{msg}");
    }

    /// The account is Tensorlake's `tl-user`, the destination comes first,
    /// and forwarded arguments follow unchanged, with no host-key options
    /// added.
    #[test]
    fn build_ssh_argv_connects_as_tl_user_and_forwards_args() {
        let destination = format!("{UNIX_USER}@tlworker-a");
        let args: Vec<OsString> = vec!["-L".into(), "8080:localhost:80".into()];
        assert_eq!(
            build_ssh_argv(&destination, &args),
            vec![
                "tailscale",
                "ssh",
                "tl-user@tlworker-a",
                "-L",
                "8080:localhost:80"
            ]
        );
    }

    /// The first token is the name unless it starts with `-`; a leading
    /// flag means no name and everything is forwarded.
    #[test]
    fn split_target_reads_leading_non_flag_as_name() {
        let args: Vec<OsString> = vec!["a".into(), "-v".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("a")));
        assert_eq!(rest, [OsString::from("-v")]);

        let args: Vec<OsString> = vec!["-L".into(), "x".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, None);
        assert_eq!(rest.len(), 2);

        let (name, rest) = split_target(&[]);
        assert_eq!(name, None);
        assert!(rest.is_empty());
    }

    /// `kd tlworker ssh -- uptime` reaches here as `[uptime]`: under the
    /// leading-hyphen rule a bare word is a worker name, not a remote
    /// command for the sole worker, exactly as for ubiworkers.
    #[test]
    fn split_target_word_after_separator_is_a_name() {
        let args: Vec<OsString> = vec!["uptime".into()];
        let (name, rest) = split_target(&args);
        assert_eq!(name, Some(OsStr::new("uptime")));
        assert!(rest.is_empty());
    }

    /// Only a suspended worker gets the resume hint; a running one gets no
    /// warning, and any other state a generic one.
    #[test]
    fn not_running_warning_names_resume_only_for_suspended() {
        let mut w = worker("tlworker-a");
        assert_eq!(not_running_warning(&w), None);
        w.status = "suspended".into();
        assert!(not_running_warning(&w).unwrap().contains("tl sbx resume"));
        w.status = "pending".into();
        let msg = not_running_warning(&w).unwrap();
        assert!(msg.contains("pending") && !msg.contains("resume"), "{msg}");
    }
}
