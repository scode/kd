//! `kd boatworker create`: start a boat sandbox that enrolls itself into the
//! tailnet on first boot, exactly as a ubiworker does.
//!
//! The flow mirrors `kd ubiworker create`: mint a one-use Tailscale auth
//! key, render the first-boot script that installs Tailscale, joins the
//! tailnet with Tailscale SSH and then launches the detached `kd-bootstrap`
//! apt unit, and hand it to the provider. The script is the one ubiworkers
//! run, unchanged (see [`render_init_script`]); only its delivery differs.
//!
//! # Delivery: a boat setup file
//!
//! Boat runs a per-sandbox setup file (`boat new --setup-file`) once, in the
//! background, after the sandbox is ready. It runs it as the unprivileged
//! `user` account, with `bash <file>`, so the script is prefixed with a
//! small prelude that re-executes it under `sudo -n /bin/sh` and then
//! deletes the file (see [`SETUP_PRELUDE`]). The script reaches boat as a
//! file, not as a command-line argument, so the auth key never sits in kd's
//! or any child process's argv, an exposure `kd ubiworker create` accepts
//! for `ubi`. kd writes it to a private temp file (mode 0600) that is
//! removed when `create` returns or fails; an interrupt (Ctrl-C) while
//! `boat new` runs can leave it behind in the temp directory.
//!
//! # Shape
//!
//! Every boatworker is created with `--no-env --no-snapshots
//! --no-monitoring`: it never receives the boat account's stored secrets, and
//! boat keeps neither snapshots nor command history of it. By default it also
//! gets `--no-auto-stop` and runs (and bills) until `kd boatworker destroy`.
//! `--ttl` swaps that for `boat new --ttl <seconds>`. boat documents the
//! value only as the lifetime "before the sandbox is eligible for cleanup".
//! Observed once with boat CLI 1.0.97: a boatworker (snapshots off) whose
//! TTL was shortened with `boat extend` was gone, not stopped, about 15
//! seconds after its deadline, so kd calls it deleted. The machine type is
//! the other choice left to the caller (`--type`, default `large`).
//!
//! `create` returns when `boat new` reports the sandbox ready. Like
//! `kd ubiworker create`, it does not wait for tailnet enrollment or for the
//! package bootstrap.

use super::{
    CreateArgs, OwnedWorkers, TAILSCALE_TAG, TS_API_CLIENT_ID, Ttl, UNIX_USER, default_worker_name,
    normalize_worker_name, owned_workers,
};
use crate::cmd::worker_bootstrap::init_script::{
    BASE_PACKAGES, bootstrap_packages, redact_auth_key, render_init_script, validate_package_name,
};
use crate::cmd::worker_bootstrap::tailscale::{self, POLICY_EDITOR_URL, render_ssh_policy_rule};
use crate::cmd::worker_bootstrap::{TS_API_CLIENT_SECRET, require_envs};
use anyhow::{Context, bail};
use jiff::Zoned;
use serde::Deserialize;
use std::ffi::OsString;
use std::io::Write as _;
use tracing::{debug, info};
use xshell::{Shell, cmd};

/// Entry point for `kd boatworker create`. Every check that can fail without
/// spending anything (credentials, name, packages, name collisions) runs
/// before the auth key is minted or a sandbox is started.
pub fn run(args: CreateArgs) -> anyhow::Result<()> {
    let creds = require_envs(&[TS_API_CLIENT_ID, TS_API_CLIENT_SECRET])?;
    let [client_id, client_secret]: [String; 2] = creds
        .try_into()
        .expect("require_envs returns one value per requested variable");

    let sh = Shell::new()?;
    let name = match args.name {
        Some(n) => normalize_worker_name(&n)?,
        None => default_worker_name(&Zoned::now()),
    };
    let machine_type = args.machine_type.as_boat_type();

    let packages = bootstrap_packages(BASE_PACKAGES, &args.pkgs);
    for pkg in &packages {
        validate_package_name(pkg)?;
    }

    // Boat documents no uniqueness rule for sandbox names, so nothing stops
    // a second sandbox under a name already in use; `ssh` would then refuse
    // the name as ambiguous and `destroy` would take both. Refusing here
    // keeps kd from creating that situation itself.
    refuse_taken_name(&owned_workers(&sh)?, &name)?;

    info!("Minting tailscale auth key...");
    let agent = tailscale::build_agent();
    let access_token =
        tailscale::exchange_access_token(&agent, &client_id, &client_secret, TAILSCALE_TAG)?;
    let auth_key = tailscale::mint_auth_key(
        &agent,
        &access_token,
        TAILSCALE_TAG,
        &format!("kd boatworker {name}"),
    )?;

    let script = render_setup_script(&auth_key, &name, &packages)?;
    // Never log the script verbatim: it embeds the minted auth key.
    debug!("setup script:\n{}", redact_auth_key(&script, &auth_key));

    // `tempfile` creates the file with mode 0600 and deletes it when
    // `setup_file` drops, on success and error alike. The write goes
    // straight to the file (no buffer), so `boat` sees all of it.
    let mut setup_file =
        tempfile::NamedTempFile::new().context("failed to create the setup script temp file")?;
    setup_file
        .write_all(script.as_bytes())
        .context("failed to write the setup script temp file")?;

    info!("Creating boat sandbox '{name}' ({machine_type})...");
    let NewSandbox { id, deadline } = create_sandbox(
        &sh,
        &name,
        machine_type,
        args.ttl.as_ref(),
        setup_file.path(),
    )?;
    let lifetime = describe_lifetime(args.ttl.as_ref(), deadline, &Zoned::now());

    let tailscale_login = tailscale::local_login(&sh);
    println!(
        "{}",
        render_summary(
            &name,
            &id,
            machine_type,
            &lifetime,
            &packages,
            tailscale_login.as_deref()
        )
    );
    Ok(())
}

/// Fail if an owned sandbox already has `name`, naming its id.
///
/// Only what the listing shows can be checked; a cut-off listing has
/// already been warned about, and a collision beyond it goes undetected.
fn refuse_taken_name(owned: &OwnedWorkers, name: &str) -> anyhow::Result<()> {
    if let Some(existing) = owned.workers.iter().find(|w| w.name == name) {
        bail!(
            "a boatworker named '{name}' already exists ({}); destroy it first or pick another name",
            existing.id
        );
    }
    Ok(())
}

/// The re-exec prelude placed in front of the shared first-boot script.
///
/// Boat runs a setup file as `user` (passwordless sudo) with `bash
/// <relative path>` from `/home/user`, but the script assumes root, as
/// Ubicloud's init scripts run. Re-executing the file itself under `sudo -n
/// /bin/sh` gives it root and the same `/bin/sh` it runs under on Ubicloud,
/// and `sudo` keeps the working directory, so the relative `$0` still
/// resolves. `-n` makes sudo fail instead of waiting for a password that
/// nobody will type. The shared script's own `#!/bin/sh` line follows
/// and is an ordinary comment there.
///
/// Once running as root, the prelude deletes the file. Boat leaves setup
/// files on the sandbox, readable by any local account (observed mode
/// 0644), and this one holds the auth key: consumed if enrollment succeeds,
/// but still valid for up to an hour if it fails. `sh` reads the script
/// through the descriptor it already has open, so the rest of it still runs
/// after the unlink (verified on a boat sandbox).
const SETUP_PRELUDE: &str = "#!/bin/sh\n\
# kd boatworker: boat runs setup files as 'user'; the enrollment script below\n\
# needs root, so re-run this file under sudo, then delete it (it holds the key).\n\
if [ \"$(id -u)\" -ne 0 ]; then exec sudo -n /bin/sh \"$0\"; fi\n\
rm -f -- \"$0\"\n";

/// The full setup file: [`SETUP_PRELUDE`] followed by the shared rendered
/// first-boot script for this worker. Validation of the key, name and
/// packages is the renderer's (it refuses anything that would be unsafe to
/// interpolate).
fn render_setup_script(auth_key: &str, name: &str, packages: &[String]) -> anyhow::Result<String> {
    Ok(format!(
        "{SETUP_PRELUDE}{}",
        render_init_script(auth_key, name, packages)?
    ))
}

/// Start the sandbox with `boat new` and return its id and boat's deadline.
///
/// The flags are the fixed boatworker shape (see module docs). The child
/// gets the Tailscale OAuth pair removed from its environment: boat needs
/// neither, and the auth key minted with them reaches boat only through
/// the setup file.
///
/// `boat new --json` prints one JSON object per line as the sandbox comes
/// up and returns once it is ready; kd reads the id from those lines (see
/// [`parse_new_output`]). If `boat new` itself fails, the sandbox may still
/// have been created, so the error says to check `kd boatworker list`.
fn create_sandbox(
    sh: &Shell,
    name: &str,
    machine_type: &str,
    ttl: Option<&Ttl>,
    setup_file: &std::path::Path,
) -> anyhow::Result<NewSandbox> {
    let args = boat_new_args(name, machine_type, ttl, setup_file);
    let output = cmd!(sh, "boat {args...}")
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        .read()
        .with_context(|| {
            format!(
                "boat new failed; a sandbox named '{name}' may exist anyway, check \
                 `kd boatworker list`"
            )
        })?;
    parse_new_output(&output)
}

/// The `boat` argv (after the program name) that starts a boatworker.
///
/// Split out so a test pins the fixed flags. They are the part most costly
/// to lose silently: without `--no-env` every worker would receive the boat
/// account's stored secrets, and without either `--no-auto-stop` or `--ttl`
/// it would stop after boat's default hour. The two are alternatives: boat
/// documents `--ttl` as the lifetime before the sandbox is cleaned up and
/// `--no-auto-stop` as no automatic stop at all, so exactly one is passed.
fn boat_new_args(
    name: &str,
    machine_type: &str,
    ttl: Option<&Ttl>,
    setup_file: &std::path::Path,
) -> Vec<OsString> {
    let lifetime = match ttl {
        Some(ttl) => vec!["--ttl".to_owned(), ttl.seconds.to_string()],
        None => vec!["--no-auto-stop".to_owned()],
    };
    let mut args: Vec<OsString> = std::iter::once("new".to_owned())
        .chain(lifetime)
        .chain(
            [
                "--no-env",
                "--no-snapshots",
                "--no-monitoring",
                "--type",
                machine_type,
                "--name",
                name,
                "--setup-file",
            ]
            .map(str::to_owned),
        )
        .map(OsString::from)
        .collect();
    args.push(setup_file.as_os_str().to_owned());
    args.push("--json".into());
    args
}

/// One line of `boat new --json` output. Only the event name, the id and
/// the deadline matter to kd; boat adds further fields per event.
#[derive(Deserialize)]
struct NewEvent {
    event: String,
    id: Option<String>,
    /// When boat will clean the sandbox up, an RFC 3339 timestamp, on the
    /// `ready` event; `null` for a sandbox without a TTL.
    #[serde(rename = "archiveAfter")]
    archive_after: Option<String>,
}

/// What kd takes from `boat new --json`: the sandbox id, and boat's own
/// deadline for it when it has one.
#[derive(Debug, PartialEq)]
struct NewSandbox {
    id: String,
    /// `None` without `--ttl`, and also if boat's output lacks the field or
    /// holds something unparseable: the deadline only feeds the summary, so
    /// it must never fail a create that already has a running sandbox.
    deadline: Option<jiff::Timestamp>,
}

/// Find the new sandbox's id in `boat new --json` output and check that boat
/// reported it ready.
///
/// Observed shape (boat CLI 1.0.97): a `created` line carrying the id, any
/// number of `state` lines, then a `ready` line.
///
/// This only runs after `boat new` succeeded, so a sandbox exists and bills
/// until deleted. A parse problem must therefore never hide it: lines that
/// are not JSON events (a self-update notice, say) are skipped rather than
/// failing the whole create, and every error says how to find and remove
/// the sandbox. A missing `ready` line is still reported as an error rather
/// than a success summary.
fn parse_new_output(output: &str) -> anyhow::Result<NewSandbox> {
    let mut id = None;
    let mut ready = false;
    let mut deadline = None;
    for line in output.lines() {
        let Ok(event) = serde_json::from_str::<NewEvent>(line) else {
            continue;
        };
        if id.is_none() {
            id = event.id;
        }
        if let Some(at) = event.archive_after.as_deref() {
            deadline = at.parse().ok().or(deadline);
        }
        ready |= event.event == "ready";
    }
    let Some(id) = id else {
        bail!(
            "'boat new' succeeded but its output carried no sandbox id; the sandbox is likely \
             running: find it with `kd boatworker list` and remove it with `kd boatworker destroy`"
        );
    };
    if !ready {
        bail!(
            "'boat new' returned without reporting sandbox {id} ready; it may still be running: \
             check `kd boatworker list`, and remove it with `kd boatworker destroy` if unwanted"
        );
    }
    Ok(NewSandbox { id, deadline })
}

/// The summary's lifetime line: how long the worker runs, and for a
/// `--ttl` worker when boat deletes it, in `now`'s time zone.
///
/// The time is boat's own deadline (`archiveAfter` from `boat new`) when
/// boat reported one, since boat counts from its own creation of the
/// sandbox and may have waited for a machine first. Without it, the time is
/// estimated from `now` (just after `boat new` returned) and marked as an
/// estimate. It says "deletes" rather than "stops" because that is what was
/// observed (see the module docs), and a reader used to boat's snapshots
/// would otherwise expect to resume the worker.
fn describe_lifetime(ttl: Option<&Ttl>, deadline: Option<jiff::Timestamp>, now: &Zoned) -> String {
    let Some(ttl) = ttl else {
        return "runs until `kd boatworker destroy`".to_owned();
    };
    let local = |at: Zoned| at.strftime("%Y-%m-%d %H:%M %Z").to_string();
    let when = match deadline {
        Some(at) => format!(" (at {})", local(at.to_zoned(now.time_zone().clone()))),
        None => i64::try_from(ttl.seconds)
            .ok()
            .and_then(|secs| now.checked_add(jiff::SignedDuration::from_secs(secs)).ok())
            .map(|at| format!(" (around {}, estimated)", local(at)))
            .unwrap_or_default(),
    };
    format!("boat deletes it after {}{when}, keeping nothing", ttl.text)
}

/// The human-readable summary printed to stdout after a successful create,
/// in the shape of ubiworker's.
///
/// The policy section is printed every time, as ubiworker's is: kd can
/// neither install nor detect the tailnet rules a worker needs, and
/// `tailscale ssh` fails without them in ways that say nothing about policy.
/// Unlike ubiworker, the login user is boat's fixed `user`, and `kd
/// boatworker ssh` does not depend on any of this, since it goes through
/// boat.
fn render_summary(
    name: &str,
    id: &str,
    machine_type: &str,
    lifetime: &str,
    packages: &[String],
    tailscale_login: Option<&str>,
) -> String {
    [
        format!("Created boatworker '{name}' ({id}, {machine_type})"),
        format!("  lifetime: {lifetime}"),
        format!("  unix user: {UNIX_USER}"),
        // Enrollment and the package bootstrap both run after `create`
        // returns; these two lines say where to look instead of implying
        // either has finished.
        format!(
            "  first-boot setup (tailscale enrollment) runs in the background: \
             `boat info {id}` shows its setup status"
        ),
        format!(
            "  packages (async after enrollment, log /var/log/kd-bootstrap.log): {}",
            if packages.is_empty() {
                "none (dist-upgrade only)".to_string()
            } else {
                packages.join(", ")
            }
        ),
        format!("  tailscale tag: {TAILSCALE_TAG}"),
        format!("  connect: kd boatworker ssh {name}"),
        format!("  destroy: kd boatworker destroy {name}"),
        format!(
            "  for `tailscale ssh {UNIX_USER}@{name}`, the tailnet policy file needs:\n    \
             1. an ssh rule letting you log in to {TAILSCALE_TAG} as '{UNIX_USER}' — append this \
             object to the existing \"ssh\" array (create the array if there is none; a tag \
             such as tag:server also works as src):\n       {}\n    \
             2. an acl/grant that lets you reach {TAILSCALE_TAG} on port 22\n  \
             edit the policy file at {POLICY_EDITOR_URL}\n  \
             (admin console -> Access controls -> Edit file; UI path may be stale)",
            render_ssh_policy_rule(TAILSCALE_TAG, UNIX_USER, tailscale_login)
        ),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::boatworker::Worker;

    const KEY: &str = "tskey-auth-abc123-def456";

    // ── render_setup_script ──────────────────────────────────────────────

    /// The re-exec must be the first thing the file does after its shebang:
    /// everything below it (`set -eu`, `hostnamectl`, `systemctl`) assumes
    /// root, and boat starts the file as `user`. It must also re-exec the
    /// file itself (`$0`) under `sudo -n`, so a missing sudo grant fails
    /// rather than hangs on a password prompt. The very next step, as root,
    /// deletes the file, before the script does anything that could fail
    /// and leave it (and the auth key in it) lying around.
    #[test]
    fn render_setup_script_reexecs_under_sudo_then_deletes_itself() {
        let script = render_setup_script(KEY, "boatworker-a", &[]).unwrap();
        let lines: Vec<&str> = script
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(
            lines[0],
            r#"if [ "$(id -u)" -ne 0 ]; then exec sudo -n /bin/sh "$0"; fi"#
        );
        assert_eq!(lines[1], r#"rm -f -- "$0""#);
        assert_eq!(lines[2], "set -eu");
        assert!(script.starts_with("#!/bin/sh\n"), "{script}");
    }

    // ── boat_new_args ────────────────────────────────────────────────────

    /// Pins the fixed boatworker shape on the `boat new` command line: no
    /// auto-stop, none of the account's stored secrets, no snapshots, no
    /// monitoring, JSON output for `parse_new_output`, and the setup file
    /// passed by path (the script itself never appears in argv).
    #[test]
    fn boat_new_args_carry_the_fixed_shape() {
        let args = boat_new_args(
            "boatworker-a",
            "large",
            None,
            std::path::Path::new("/tmp/x"),
        );
        assert_eq!(
            args,
            [
                "new",
                "--no-auto-stop",
                "--no-env",
                "--no-snapshots",
                "--no-monitoring",
                "--type",
                "large",
                "--name",
                "boatworker-a",
                "--setup-file",
                "/tmp/x",
                "--json"
            ]
            .map(OsString::from)
        );
    }

    /// Everything after the prelude is the shared rendered script, byte
    /// for byte: the point of sharing it is that the two fleets enroll and
    /// bootstrap identically.
    #[test]
    fn render_setup_script_embeds_shared_script_unchanged() {
        let packages = vec!["cowsay".to_string()];
        let script = render_setup_script(KEY, "boatworker-a", &packages).unwrap();
        let inner = render_init_script(KEY, "boatworker-a", &packages).unwrap();
        assert_eq!(script, format!("{SETUP_PRELUDE}{inner}"));
    }

    /// The renderer's own refusals still apply through the wrapper, so an
    /// unsafe key cannot reach the file.
    #[test]
    fn render_setup_script_refuses_invalid_key() {
        assert!(render_setup_script("tskey-auth-x'y", "boatworker-a", &[]).is_err());
    }

    // ── parse_new_output ─────────────────────────────────────────────────

    /// Real `boat new --json` output (boat CLI 1.0.97), trimmed and with
    /// identifying values replaced.
    const NEW_FIXTURE: &str = r#"{"event":"created","hostedPorts":null,"id":"bx_aaaaaaaa","org":null,"team":null,"ttlSeconds":null}
{"event":"state","id":"bx_aaaaaaaa","state":"ready"}
{"archiveAfter":null,"commands":{"ssh":"boat ssh bx_aaaaaaaa"},"event":"ready","id":"bx_aaaaaaaa","state":"ready"}
"#;

    #[test]
    fn parse_new_output_returns_the_created_id() {
        assert_eq!(
            parse_new_output(NEW_FIXTURE).unwrap(),
            NewSandbox {
                id: "bx_aaaaaaaa".to_owned(),
                deadline: None
            }
        );
    }

    /// A line that is not a JSON event (a self-update notice, say) must not
    /// fail a create whose sandbox already exists; it is skipped.
    #[test]
    fn parse_new_output_skips_non_json_lines() {
        let output = format!("boat: updated to a newer version\n{NEW_FIXTURE}");
        assert_eq!(parse_new_output(&output).unwrap().id, "bx_aaaaaaaa");
    }

    /// Output that never says `ready` is not a success, and the error must
    /// carry the id and say how to remove the sandbox, which may be running
    /// and billing.
    #[test]
    fn parse_new_output_without_ready_names_the_id_and_destroy() {
        let output = r#"{"event":"created","id":"bx_aaaaaaaa"}"#;
        let msg = parse_new_output(output).unwrap_err().to_string();
        assert!(
            msg.contains("bx_aaaaaaaa") && msg.contains("kd boatworker destroy"),
            "{msg}"
        );
    }

    /// No id anywhere is an error rather than a summary for a sandbox kd
    /// cannot name, and it too points at `list` and `destroy`.
    #[test]
    fn parse_new_output_without_id_points_at_list_and_destroy() {
        for output in [r#"{"event":"ready"}"#, "Creating sandbox...\n"] {
            let msg = parse_new_output(output).unwrap_err().to_string();
            assert!(
                msg.contains("kd boatworker list") && msg.contains("kd boatworker destroy"),
                "{msg}"
            );
        }
    }

    // ── refuse_taken_name ────────────────────────────────────────────────

    /// A name an owned sandbox already has is refused with that sandbox's
    /// id; any other name passes.
    #[test]
    fn refuse_taken_name_only_refuses_existing_names() {
        let owned = OwnedWorkers {
            workers: vec![Worker {
                name: "boatworker-a".to_string(),
                id: "bx_a".to_string(),
                state: "idle".to_string(),
                machine_type: "large".to_string(),
            }],
            complete: true,
        };
        let msg = refuse_taken_name(&owned, "boatworker-a")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("bx_a"), "{msg}");
        assert!(refuse_taken_name(&owned, "boatworker-b").is_ok());
    }

    // ── render_summary ───────────────────────────────────────────────────

    /// The summary must point at the boatworker commands (not ubiworker's,
    /// which this was modeled on) and print a policy rule for `tag:boat` and
    /// boat's fixed `user` account, with the placeholder when the operator's
    /// login is unknown.
    #[test]
    fn render_summary_names_boatworker_commands_and_boat_rule() {
        let summary = render_summary("boatworker-a", "bx_a", "large", "LIFETIME", &[], None);
        assert!(summary.contains("\n  lifetime: LIFETIME\n"), "{summary}");
        assert!(summary.contains("connect: kd boatworker ssh boatworker-a"));
        assert!(summary.contains("destroy: kd boatworker destroy boatworker-a"));
        assert!(summary.contains("boat info bx_a"));
        assert!(summary.contains(
            r#"{ "action": "accept", "src": ["<your-tailscale-login>"], "dst": ["tag:boat"], "users": ["user"] }"#
        ));
        assert!(!summary.contains("ubiworker"), "{summary}");
    }

    /// `--ttl` replaces `--no-auto-stop` with boat's `--ttl <seconds>`, and
    /// only that: the rest of the shape (no stored secrets, no snapshots, no
    /// monitoring) must not change with it. Both flags at once would leave
    /// boat to decide which wins.
    #[test]
    fn boat_new_args_with_ttl_swap_only_the_lifetime_flag() {
        let ttl = crate::cmd::boatworker::parse_ttl("3h").unwrap();
        let with = boat_new_args(
            "boatworker-a",
            "large",
            Some(&ttl),
            std::path::Path::new("/tmp/x"),
        );
        let without = boat_new_args(
            "boatworker-a",
            "large",
            None,
            std::path::Path::new("/tmp/x"),
        );
        assert_eq!(&with[..3], ["new", "--ttl", "10800"].map(OsString::from));
        assert!(
            !with.contains(&OsString::from("--no-auto-stop")),
            "{with:?}"
        );
        assert_eq!(with[3..], without[2..]);
    }

    /// The lifetime line says plainly how the worker ends: with `--ttl`,
    /// deleted after the given time, around a local clock time; without it,
    /// only when destroyed.
    #[test]
    fn describe_lifetime_covers_both_kinds() {
        let now: Zoned = "2026-11-14T10:00:00[UTC]".parse().unwrap();
        assert_eq!(
            describe_lifetime(None, None, &now),
            "runs until `kd boatworker destroy`"
        );
        let ttl = crate::cmd::boatworker::parse_ttl("3h").unwrap();
        assert_eq!(
            describe_lifetime(Some(&ttl), None, &now),
            "boat deletes it after 3h (around 2026-11-14 13:00 UTC, estimated), keeping nothing"
        );
    }

    /// boat's own deadline wins over kd's estimate, shown in the local
    /// zone: boat counts from its creation of the sandbox, which can be
    /// well before `boat new` returns.
    #[test]
    fn describe_lifetime_prefers_boats_deadline() {
        let now: Zoned = "2026-11-14T10:00:00[America/New_York]".parse().unwrap();
        let ttl = crate::cmd::boatworker::parse_ttl("3h").unwrap();
        let deadline: jiff::Timestamp = "2026-11-14T17:58:00Z".parse().unwrap();
        assert_eq!(
            describe_lifetime(Some(&ttl), Some(deadline), &now),
            "boat deletes it after 3h (at 2026-11-14 12:58 EST), keeping nothing"
        );
    }

    /// The estimate is a calendar-aware sum in the local zone: across a
    /// daylight-saving change, 7d of seconds lands one wall-clock hour
    /// earlier, as the real deadline does.
    #[test]
    fn describe_lifetime_estimate_crosses_daylight_saving() {
        let now: Zoned = "2026-10-31T10:00:00[America/New_York]".parse().unwrap();
        let ttl = crate::cmd::boatworker::parse_ttl("7d").unwrap();
        assert_eq!(
            describe_lifetime(Some(&ttl), None, &now),
            "boat deletes it after 7d (around 2026-11-07 09:00 EST, estimated), keeping nothing"
        );
    }

    /// boat's `ready` event carries the deadline as `archiveAfter`; it is
    /// picked up when present, and a malformed value is ignored rather than
    /// failing a create whose sandbox already runs.
    #[test]
    fn parse_new_output_reads_boats_deadline() {
        let with = NEW_FIXTURE.replace(
            r#""archiveAfter":null"#,
            r#""archiveAfter":"2026-11-21T01:55:47.325Z""#,
        );
        assert_eq!(
            parse_new_output(&with).unwrap().deadline,
            Some("2026-11-21T01:55:47.325Z".parse().unwrap())
        );
        let junk = NEW_FIXTURE.replace(r#""archiveAfter":null"#, r#""archiveAfter":"soon""#);
        assert_eq!(parse_new_output(&junk).unwrap().deadline, None);
    }
}
