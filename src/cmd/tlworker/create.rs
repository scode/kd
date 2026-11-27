//! `kd tlworker create`: start a Tensorlake sandbox and enroll it into the
//! tailnet, exactly as ubiworkers and boatworkers enroll themselves.
//!
//! The flow mirrors the other fleets: mint a one-use Tailscale auth key,
//! render the shared first-boot script (Tailscale installer, `tailscale up
//! --ssh` with the worker name as hostname and its retry loop, then the
//! detached `kd-bootstrap` apt unit), and run it on the machine. Two things
//! differ, both because of what Tensorlake offers.
//!
//! # No first-boot hook: kd starts the script itself
//!
//! Tensorlake has nothing like Ubicloud's init script or boat's setup file.
//! Its create request does take an `entrypoint`, the command the sandbox
//! starts with, but kd does not use it. Instead `create` starts the
//! sandbox, then starts the script through Tensorlake's process API. The
//! process belongs to the sandbox, not to kd's connection, so by default
//! `create` returns right away and enrollment carries on in the background,
//! as on the other fleets. With `--wait`, `create` stays until the script
//! exits (a minute or two), relays its progress lines, and reports one clear
//! outcome: enrolled, or failed with the end of the script's output. The
//! `kd-bootstrap` unit runs detached afterwards either way.
//!
//! The image is `tensorlake/ubuntu-systemd`, and the script is prefixed with
//! a wait for systemd (see [`READINESS_PREFIX`]): Tailscale's installer
//! starts `tailscaled` as a systemd service and the bootstrap is a
//! `systemd-run` unit, so neither works until systemd is up.
//!
//! # Delivery: stdin into a root-only temp file
//!
//! The script holds the auth key. kd sends it on the process's stdin to
//! [`SUDO_WRAPPER`], which as root copies it into a `mktemp` file (mode
//! 0600, root-owned) and runs that with stdin from `/dev/null`; the first
//! line of the script deletes the file. So the key never sits in the argv
//! of kd or of any process kd starts, and spends a moment on the sandbox's
//! disk readable only by root. Two simpler routes were rejected: Tensorlake's
//! file API cannot set a file's mode, and `sh` reading the script straight
//! from stdin would let any command in it that reads stdin (an installer,
//! say) swallow the rest of the script.
//!
//! The key still reaches Tensorlake's API (as it reaches Ubicloud's and
//! boat's), and on the worker it appears in `tailscale up --auth-key=...`'s
//! argv while that runs, as on the other fleets.

use super::api::{Api, CreateRefused, ProcessInfo, load_credentials};
use super::list::render_shape;
use super::{
    CreateArgs, TAILSCALE_TAG, TS_API_CLIENT_ID, UNIX_USER, default_worker_name,
    normalize_worker_name,
};
use crate::cmd::worker_bootstrap::init_script::{
    BASE_PACKAGES, bootstrap_packages, redact_auth_key, render_init_script, validate_package_name,
};
use crate::cmd::worker_bootstrap::tailscale::{self, POLICY_EDITOR_URL, render_ssh_policy_rule};
use crate::cmd::worker_bootstrap::{TS_API_CLIENT_SECRET, require_envs};
use anyhow::{Context, bail};
use jiff::Zoned;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, info};
use xshell::Shell;

/// The command kd starts on the sandbox, as `sudo -n /bin/sh -c <this>`.
///
/// It reads the script from stdin into a fresh `mktemp` file (root-owned,
/// mode 0600) and runs it with stdin from `/dev/null`, so nothing the
/// script runs can read the rest of it. `-n` makes a missing sudo grant
/// fail instead of waiting for a password nobody will type. The script
/// deletes the file as its first step ([`READINESS_PREFIX`]).
const SUDO_WRAPPER: &str =
    r#"f=$(mktemp) || exit 1; cat >"$f" || exit 1; exec /bin/sh "$f" </dev/null"#;

/// What runs before the shared first-boot script: delete this file (it
/// holds the auth key), then wait for systemd.
///
/// PID 1 must be systemd, or nothing below works; the image is
/// `tensorlake/ubuntu-systemd` precisely for this. Right after boot,
/// `systemctl` cannot reach the manager yet and answers nothing (`--wait`
/// included; farhelm observed this on 2026-11-03), so a single read is not
/// enough: poll every 2 seconds, up to 90 times (3 minutes), until it says
/// `running` or `degraded`. `degraded` counts because a usable fresh
/// machine can have an unrelated failed unit.
///
/// The shared script's own `#!/bin/sh` line follows and is an ordinary
/// comment there; its `set -eu` applies from that point on.
const READINESS_PREFIX: &str = r#"#!/bin/sh
# kd tlworker: this file holds the auth key; delete it before anything else.
rm -f -- "$0"
echo "kd: waiting for systemd" >&2
pid1=$(cat /proc/1/comm 2>/dev/null)
if [ "$pid1" != systemd ]; then
    echo "kd: PID 1 is ${pid1:-unknown}, not systemd; tlworkers need the tensorlake/ubuntu-systemd image" >&2
    exit 1
fi
tries=0
while :; do
    state=$(systemctl is-system-running 2>/dev/null)
    case "$state" in running|degraded) break;; esac
    tries=$((tries + 1))
    if [ "$tries" -ge 90 ]; then
        echo "kd: systemd did not finish starting within 180 seconds (state: ${state:-no answer})" >&2
        exit 1
    fi
    sleep 2
done
echo "kd: systemd is up; installing tailscale and joining the tailnet" >&2
"#;

/// How long `--wait` waits for the script to exit: up to 3 minutes of
/// systemd wait, then the Tailscale install and up to five enrollment
/// attempts 30 seconds apart, with room to spare.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often `--wait` checks whether the script has exited (and reads its
/// output for progress lines; the whole output each time, which is bounded
/// by [`SCRIPT_TIMEOUT`] and small next to a sandbox's lifetime cost).
const SCRIPT_POLL: Duration = Duration::from_secs(2);

/// How many trailing lines of the script's output a failure shows.
const OUTPUT_TAIL_LINES: usize = 40;

/// Entry point for `kd tlworker create`. Every check that can fail without
/// spending anything (credentials, name, packages) runs before the auth key
/// is minted or a sandbox is started.
pub fn run(args: CreateArgs) -> anyhow::Result<()> {
    let creds = require_envs(&[TS_API_CLIENT_ID, TS_API_CLIENT_SECRET])?;
    let [client_id, client_secret]: [String; 2] = creds
        .try_into()
        .expect("require_envs returns one value per requested variable");
    let api = Api::new(load_credentials()?);

    let name = match &args.name {
        Some(n) => normalize_worker_name(n)?,
        None => default_worker_name(&Zoned::now()),
    };
    let packages = bootstrap_packages(BASE_PACKAGES, &args.pkgs);
    for pkg in &packages {
        validate_package_name(pkg)?;
    }
    let memory_mb = u64::from(args.memory) * 1024;
    let disk_mb = u64::from(args.disk) * 1024;

    info!("Minting tailscale auth key...");
    let agent = tailscale::build_agent();
    let access_token =
        tailscale::exchange_access_token(&agent, &client_id, &client_secret, TAILSCALE_TAG)?;
    let auth_key = tailscale::mint_auth_key(
        &agent,
        &access_token,
        TAILSCALE_TAG,
        &format!("kd tlworker {name}"),
    )?;
    let script = render_script(&auth_key, &name, &packages)?;
    // Never log the script verbatim: it embeds the minted auth key.
    debug!(
        "first-boot script:\n{}",
        redact_auth_key(&script, &auth_key)
    );

    info!(
        "Creating Tensorlake sandbox '{name}' ({}, {} GiB disk)...",
        render_shape(f64::from(args.cpus), memory_mb),
        args.disk
    );
    // A create that fails or times out may still have produced a sandbox,
    // so the error names the requested name, which is all kd knows then.
    // The exception is an outright refusal (a taken name, a size the plan
    // forbids): nothing was created, and after a 409 the hint would point at
    // the worker that already has the name.
    let id = match api.create_sandbox(&name, args.cpus, memory_mb, disk_mb) {
        Ok(id) => id,
        Err(err) if err.downcast_ref::<CreateRefused>().is_some() => return Err(err),
        Err(err) => return Err(err.context(maybe_leftover_hint(&name))),
    };

    info!("Sandbox {id} is running; starting its tailnet enrollment...");
    let pid = start_script(&api, &id, &script).with_context(|| leftover_hint(&name, &id))?;
    let enrollment = if args.wait {
        info!("Waiting for '{name}' to join the tailnet (usually a minute or two)...");
        let outcome = wait_for_script(&api, &id, pid, &auth_key)
            .with_context(|| leftover_hint(&name, &id))?;
        if let Some(failure) = describe_failure(outcome.ended.as_ref()) {
            bail!(
                "the first-boot script on '{name}' ({id}) {failure}; its output ended \
                 with:\n{}\n{}",
                redact_auth_key(&outcome.tail.join("\n"), &auth_key),
                leftover_hint(&name, &id)
            );
        }
        Enrollment::Done
    } else {
        Enrollment::Background { pid }
    };

    let sh = Shell::new()?;
    let tailscale_login = tailscale::local_login(&sh);
    println!(
        "{}",
        render_summary(&Summary {
            name: &name,
            id: &id,
            shape: &render_shape(f64::from(args.cpus), memory_mb),
            disk_gib: args.disk,
            packages: &packages,
            enrollment,
            tailscale_login: tailscale_login.as_deref(),
        })
    );
    Ok(())
}

/// The error suffix for a create request whose outcome kd does not know (a
/// timeout, a 5xx, an unreadable reply): a sandbox may exist and bill, and
/// this is how to find and remove it.
fn maybe_leftover_hint(name: &str) -> String {
    format!(
        "a sandbox named '{name}' may exist and bill anyway: check `kd tlworker list` and \
         remove it with `kd tlworker destroy {name}`"
    )
}

/// The error suffix for anything that fails once the sandbox is known to be
/// running: it exists and bills until destroyed.
fn leftover_hint(name: &str, id: &str) -> String {
    format!(
        "sandbox '{name}' ({id}) exists and bills until destroyed: remove it with \
         `kd tlworker destroy {name}`"
    )
}

/// The full script kd runs on the sandbox: [`READINESS_PREFIX`] followed by
/// the shared rendered first-boot script for this worker. Validation of the
/// key, name and packages is the renderer's.
fn render_script(auth_key: &str, name: &str, packages: &[String]) -> anyhow::Result<String> {
    Ok(format!(
        "{READINESS_PREFIX}{}",
        render_init_script(auth_key, name, packages)?
    ))
}

/// Whether `create` saw enrollment finish, which the summary reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enrollment {
    /// `--wait`: the script exited 0, so the worker is on the tailnet.
    Done,
    /// The default: the script was started and left running on the
    /// sandbox as process `pid`.
    Background { pid: u32 },
}

/// How the script run ended: its final process state, or `None` when it
/// was still running at [`SCRIPT_TIMEOUT`], plus the end of its output.
struct ScriptOutcome {
    ended: Option<ProcessInfo>,
    tail: Vec<String>,
}

/// How many polls in a row may fail before kd gives up waiting on the
/// script. One failed status read (a dropped connection, a 502 from the
/// proxy) says nothing about the script, which keeps running on the
/// sandbox either way, so it must not abort a create that is going fine.
const MAX_POLL_FAILURES: u32 = 5;

/// How often `--wait` says it is still waiting, when nothing else has been
/// logged in the meantime.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// The prefix of the progress lines the script writes for `--wait` to
/// relay (the readiness prefix's systemd lines, the shared script's
/// enrollment attempts). Everything else the script prints (the Tailscale
/// installer's apt output, mostly) is only shown on failure.
const PROGRESS_PREFIX: &str = "kd: ";

/// Start `script` on the sandbox under [`SUDO_WRAPPER`] and feed it on
/// stdin, returning the process's PID. The script then runs on its own: the
/// process belongs to the sandbox, not to kd's connection, so `create` may
/// return without waiting.
///
/// Both calls are single attempts: retrying either could run the script
/// twice.
fn start_script(api: &Api, sandbox_id: &str, script: &str) -> anyhow::Result<u32> {
    let pid = api.start_process(sandbox_id, "sudo", &["-n", "/bin/sh", "-c", SUDO_WRAPPER])?;
    api.feed_stdin(sandbox_id, pid, script.as_bytes())?;
    Ok(pid)
}

/// Wait for the script started as `pid` to exit, bounded by
/// [`SCRIPT_TIMEOUT`], logging its progress lines as they appear and a
/// heartbeat when nothing has been logged for [`HEARTBEAT`].
///
/// Status and output reads tolerate up to [`MAX_POLL_FAILURES`] failures in
/// a row; a failed output read only costs a progress line. The output tail
/// is kept for when the script failed or timed out, the only times it is
/// shown; a failure to read it is reported in place of the output rather
/// than hiding the script's own failure. A timed-out script is left
/// running: it may still finish enrolling, and the worker is destroyed by
/// hand either way.
fn wait_for_script(
    api: &Api,
    sandbox_id: &str,
    pid: u32,
    auth_key: &str,
) -> anyhow::Result<ScriptOutcome> {
    let started = Instant::now();
    let deadline = started + SCRIPT_TIMEOUT;
    let mut last_log = started;
    let mut relayed = HashSet::new();
    let mut failures = 0;
    let ended = loop {
        match api.process_output(sandbox_id, pid) {
            Ok(lines) => {
                for line in new_progress_lines(&lines, &mut relayed) {
                    info!("{}", redact_auth_key(line, auth_key));
                    last_log = Instant::now();
                }
            }
            // Only a progress line is at stake; the status read below is
            // what decides whether the wait goes on.
            Err(err) => debug!("reading the first-boot script's output failed: {err:#}"),
        }
        match api.process(sandbox_id, pid) {
            Ok(info) if info.status != "running" => break Some(info),
            Ok(_) => failures = 0,
            Err(err) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Err(err.context(format!(
                        "could not read the first-boot script's status {MAX_POLL_FAILURES} times \
                         in a row"
                    )));
                }
                debug!("reading the first-boot script's status failed, retrying: {err:#}");
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break None;
        }
        if now.duration_since(last_log) >= HEARTBEAT {
            info!(
                "still setting up the worker ({}s so far)...",
                now.duration_since(started).as_secs()
            );
            last_log = now;
        }
        std::thread::sleep(SCRIPT_POLL);
    };

    // One last read after the script ended: it relays the progress lines
    // printed between the loop's last output read and its status read, and
    // it is where the failure tail comes from.
    let output = api.process_output(sandbox_id, pid);
    if let Ok(lines) = &output {
        for line in new_progress_lines(lines, &mut relayed) {
            info!("{}", redact_auth_key(line, auth_key));
        }
    }
    let tail = if describe_failure(ended.as_ref()).is_some() {
        match output {
            Ok(lines) => tail_lines(lines, OUTPUT_TAIL_LINES),
            Err(err) => vec![format!("(could not read the output: {err:#})")],
        }
    } else {
        Vec::new()
    };
    Ok(ScriptOutcome { ended, tail })
}

/// The progress lines among `lines` not relayed before, in order, adding
/// them to `relayed`.
///
/// Deduplicated by content, not by position: Tensorlake returns a process's
/// whole output on every read, but not as an append-only list (observed:
/// the combined stdout/stderr view puts newer stdout lines ahead of older
/// stderr lines, so counting lines already seen repeats the progress
/// lines). Every progress line the script prints is distinct within a run
/// (each enrollment attempt carries its number), so content identifies it.
/// Pure, so the bookkeeping is unit-testable.
fn new_progress_lines<'a>(lines: &'a [String], relayed: &mut HashSet<String>) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|line| line.starts_with(PROGRESS_PREFIX))
        .filter(|line| relayed.insert((*line).clone()))
        .map(String::as_str)
        .collect()
}

/// `None` when the script exited 0; otherwise how it ended, as a phrase for
/// the error message ("exited with status 1", "was killed by signal 9",
/// "ran out of memory"). `None` for `ended` means it was still running at
/// [`SCRIPT_TIMEOUT`].
fn describe_failure(ended: Option<&ProcessInfo>) -> Option<String> {
    let Some(info) = ended else {
        return Some(format!(
            "was still running after {} minutes (it is left running and may yet finish)",
            SCRIPT_TIMEOUT.as_secs() / 60
        ));
    };
    match (info.status.as_str(), info.exit_code, info.signal) {
        ("exited", Some(0), _) => None,
        ("oom_killed", _, _) => Some("ran out of memory".to_string()),
        (_, _, Some(signal)) => Some(format!("was killed by signal {signal}")),
        (_, Some(code), _) => Some(format!("exited with status {code}")),
        (status, None, None) => Some(format!("ended as '{status}' with no exit status")),
    }
}

/// The last `n` lines, in order.
fn tail_lines(mut lines: Vec<String>, n: usize) -> Vec<String> {
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    lines
}

/// What the create summary reports.
struct Summary<'a> {
    name: &'a str,
    id: &'a str,
    shape: &'a str,
    disk_gib: u32,
    packages: &'a [String],
    enrollment: Enrollment,
    tailscale_login: Option<&'a str>,
}

/// The human-readable summary printed to stdout after a successful create,
/// in the shape of the other fleets'.
///
/// With `--wait`, enrollment has already happened when this prints, so the
/// summary says so; without it, the summary says where to check. The lifetime
/// line spells out suspension because it is the one way a tlworker drops
/// off the tailnet without anyone destroying it. The policy section is
/// printed every time, as for the other fleets: kd can neither install nor
/// detect the tailnet rules `kd tlworker ssh` needs.
fn render_summary(s: &Summary) -> String {
    [
        format!(
            "Created tlworker '{}' ({}, {}, {} GiB disk)",
            s.name, s.id, s.shape, s.disk_gib
        ),
        "  lifetime: runs until `kd tlworker destroy`; Tensorlake suspends it after its idle \
         timeout, set to the plan's maximum (24 hours on Usage Credits, unlimited on Pro). A \
         suspended worker is off the tailnet and kd does not resume it; after a long suspension \
         Tailscale may have removed its ephemeral node, so recreate it rather than resume it."
            .to_string(),
        format!("  unix user: {UNIX_USER}"),
        match s.enrollment {
            Enrollment::Done => format!(
                "  tailscale: enrolled as '{}' with Tailscale SSH (tag {TAILSCALE_TAG})",
                s.name
            ),
            // Without --wait kd knows only that the script started, so the
            // line says where to look rather than implying it finished.
            Enrollment::Background { pid } => format!(
                "  tailscale: enrolling as '{}' with Tailscale SSH (tag {TAILSCALE_TAG}) in the \
                 background, usually a minute or two: `tailscale status` lists it once it has \
                 joined, and `tl sbx ps {} {pid}` (with the tl CLI) shows whether the setup script is still running \
                 (use --wait to wait and see any failure)",
                s.name, s.id
            ),
        },
        format!(
            "  packages (async after enrollment, log /var/log/kd-bootstrap.log): {}",
            if s.packages.is_empty() {
                "none (dist-upgrade only)".to_string()
            } else {
                s.packages.join(", ")
            }
        ),
        format!("  connect: kd tlworker ssh {}", s.name),
        format!("  destroy: kd tlworker destroy {}", s.name),
        format!(
            "  for `kd tlworker ssh` (`tailscale ssh {UNIX_USER}@{}`), the tailnet policy file \
             needs:\n    \
             1. an ssh rule letting you log in to {TAILSCALE_TAG} as '{UNIX_USER}' — append this \
             object to the existing \"ssh\" array (create the array if there is none; a tag \
             such as tag:server also works as src):\n       {}\n    \
             2. an acl/grant that lets you reach {TAILSCALE_TAG} on port 22\n  \
             edit the policy file at {POLICY_EDITOR_URL}\n  \
             (admin console -> Access controls -> Edit file; UI path may be stale)",
            s.name,
            render_ssh_policy_rule(TAILSCALE_TAG, UNIX_USER, s.tailscale_login)
        ),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "tskey-auth-abc123-def456";

    /// The file must delete itself before anything that could fail (it
    /// holds the auth key), and the systemd checks must come before the
    /// shared script, which needs systemd from its first `hostnamectl`.
    #[test]
    fn render_script_deletes_itself_then_waits_for_systemd_then_enrolls() {
        let script = render_script(KEY, "tlworker-a", &[]).unwrap();
        let rm = script.find("rm -f -- \"$0\"").unwrap();
        let pid1 = script.find("/proc/1/comm").unwrap();
        let wait = script.find("systemctl is-system-running").unwrap();
        let enroll = script.find("tailscale up").unwrap();
        assert!(rm < pid1 && pid1 < wait && wait < enroll);
        // The rm is the first command after the shebang and comment.
        let first_command = script
            .lines()
            .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .unwrap();
        assert_eq!(first_command, "rm -f -- \"$0\"");
    }

    /// The wrapper must read the script into a fresh private file and run it
    /// with stdin from /dev/null, so nothing the script runs can consume the
    /// rest of it.
    #[test]
    fn sudo_wrapper_runs_a_private_copy_with_stdin_closed() {
        assert!(SUDO_WRAPPER.contains("mktemp"));
        assert!(SUDO_WRAPPER.contains("cat >\"$f\""));
        assert!(SUDO_WRAPPER.ends_with("exec /bin/sh \"$f\" </dev/null"));
    }

    fn info(status: &str, exit_code: Option<i32>, signal: Option<i32>) -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            status: status.to_string(),
            exit_code,
            signal,
        }
    }

    /// Only a clean exit 0 is success; every other ending is described,
    /// including one with no exit status at all.
    #[test]
    fn describe_failure_accepts_only_exit_zero() {
        assert_eq!(describe_failure(Some(&info("exited", Some(0), None))), None);
        assert_eq!(
            describe_failure(Some(&info("exited", Some(1), None))).unwrap(),
            "exited with status 1"
        );
        assert_eq!(
            describe_failure(Some(&info("signaled", None, Some(9)))).unwrap(),
            "was killed by signal 9"
        );
        assert_eq!(
            describe_failure(Some(&info("oom_killed", None, None))).unwrap(),
            "ran out of memory"
        );
        assert!(describe_failure(Some(&info("weird", None, None))).is_some());
        assert!(describe_failure(None).unwrap().contains("still running"));
    }

    /// Only `kd: ` lines are relayed, each exactly once although every read
    /// returns the whole output, and also when a later read shows older
    /// lines at a different position (what Tensorlake's combined output did
    /// in a live run).
    #[test]
    fn new_progress_lines_relays_each_marker_once() {
        let mut relayed = HashSet::new();
        let first: Vec<String> = ["kd: waiting for systemd", "apt noise"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            new_progress_lines(&first, &mut relayed),
            vec!["kd: waiting for systemd"]
        );
        // Newer stdout ahead of the older marker, plus a new marker.
        let second: Vec<String> = [
            "apt noise",
            "more apt noise",
            "kd: waiting for systemd",
            "kd: enrollment attempt 1 of 5",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            new_progress_lines(&second, &mut relayed),
            vec!["kd: enrollment attempt 1 of 5"]
        );
        assert!(new_progress_lines(&second, &mut relayed).is_empty());
    }

    /// Without `--wait` the summary must not claim enrollment finished, and
    /// must say where to check on it.
    #[test]
    fn render_summary_background_says_where_to_check() {
        let summary = render_summary(&Summary {
            name: "tlworker-a",
            id: "sbx1",
            shape: "4 vCPU/16 GiB",
            disk_gib: 100,
            packages: &[],
            enrollment: Enrollment::Background { pid: 42 },
            tailscale_login: None,
        });
        assert!(!summary.contains("enrolled as"), "{summary}");
        assert!(summary.contains("in the background"), "{summary}");
        assert!(summary.contains("tl sbx ps sbx1 42"), "{summary}");
        assert!(summary.contains("--wait"), "{summary}");
    }

    /// The script's own progress lines carry the prefix `--wait` relays.
    #[test]
    fn render_script_prints_progress_markers() {
        let script = render_script(KEY, "tlworker-a", &[]).unwrap();
        for marker in [
            "kd: waiting for systemd",
            "kd: systemd is up",
            "kd: enrollment attempt",
        ] {
            assert!(script.contains(marker), "{marker}");
        }
    }

    /// A failure shows the last lines, in order, and a short output whole.
    #[test]
    fn tail_lines_keeps_the_last_lines_in_order() {
        let lines: Vec<String> = (1..=5).map(|i| i.to_string()).collect();
        assert_eq!(tail_lines(lines.clone(), 2), vec!["4", "5"]);
        assert_eq!(tail_lines(lines, 10).len(), 5);
    }

    /// The summary names the fixed account and tag, and its policy rule is
    /// for `tl-user` on `tag:tensorlake`, the pair `kd tlworker ssh` uses.
    #[test]
    fn render_summary_names_tl_user_on_tensorlake_tag() {
        let pkgs = vec!["git".to_string()];
        let summary = render_summary(&Summary {
            name: "tlworker-a",
            id: "sbx1",
            shape: "4 vCPU/16 GiB",
            disk_gib: 100,
            packages: &pkgs,
            enrollment: Enrollment::Done,
            tailscale_login: Some("me@example.com"),
        });
        assert!(
            summary.contains("Created tlworker 'tlworker-a' (sbx1, 4 vCPU/16 GiB, 100 GiB disk)")
        );
        assert!(summary.contains("unix user: tl-user"));
        assert!(summary.contains("tag:tensorlake"));
        assert!(summary.contains("\"tl-user\""), "{summary}");
        assert!(summary.contains("connect: kd tlworker ssh tlworker-a"));
        assert!(summary.contains("git"));
    }
}
