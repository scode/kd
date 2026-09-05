//! `kd devbox backup`: preflight the devbox, take a Hermes backup, pull it
//! to the controller.
//!
//! The sequence is SPEC_impl.md "Backup sequence". The shape worth knowing
//! before reading the code:
//!
//! - Everything on the controller is checked first (profile, the four agent
//!   credentials, the backup directory), so a laptop problem never leaves
//!   Hermes stopped on the devbox.
//! - The devbox preflight is read-only and its whole purpose is to put the
//!   "is anything in flight?" evidence in front of the user before the one
//!   yes/no question. Nothing is enumerated or waived item by item.
//! - Hermes is stopped for the backup and left stopped on success, so no
//!   state accumulates between the archive and the reinstall. `--keep-running`
//!   never touches Hermes at all and is how a rehearsal gets an archive.
//! - The archive is pulled streaming, hash-checked against `sha256sum` on the
//!   devbox, and deleted there afterwards: it contains `.hermes/.env`.

use super::{BackupArgs, confirm, hermes, home_dir, secrets, transport::Transport};
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Where `hermes backup` writes on the devbox before the pull. Home-relative
/// so the remote shell resolves it.
const REMOTE_ARCHIVE: &str = "hermes-backup-kd.zip";

pub fn run(args: BackupArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let profile = args.profile.load()?;

    // Controller preflight: fail before touching the devbox.
    let sources = secrets::resolve_all(&home)?;
    info!("{}", secrets::describe(&sources));
    std::fs::create_dir_all(&profile.backup_dir)
        .with_context(|| format!("failed to create {}", profile.backup_dir.display()))?;

    let t = Transport::ssh(format!("{}@{}", profile.user, profile.host));

    // Devbox preflight: read-only, printed as is, then one question.
    info!("running read-only preflight on {}", profile.host);
    let report = t.capture(PREFLIGHT_SCRIPT)?;
    print!("{}", report.stdout);
    if !report.stderr.trim().is_empty() {
        eprint!("{}", report.stderr);
    }
    if !report.success() {
        bail!("preflight script failed on {}", profile.host);
    }
    if !args.yes && !confirm("ok to proceed with the backup?")? {
        bail!("aborted");
    }

    if !args.keep_running {
        hermes::stop(&t)?;
    }
    let result = take_and_pull(
        &t,
        &profile.backup_dir,
        &profile.hostname,
        args.keep_running,
    );
    // Failure path: never leave the devbox stopped because of a bug here.
    if result.is_err()
        && !args.keep_running
        && let Err(e) = hermes::start(&t)
    {
        warn!("could not restart Hermes after the failed backup: {e:#}");
    }
    let archive = result?;

    let version = t
        .capture("hermes --version 2>/dev/null | head -n 1")
        .map(|c| c.stdout.trim().to_owned())
        .unwrap_or_default();
    println!();
    println!("archive: {}", archive.display());
    println!("source hermes: {version}");
    if args.keep_running {
        println!("Hermes was left running (--keep-running).");
    } else {
        println!(
            "Hermes is STOPPED on {}. `kd devbox resume` starts it again.",
            profile.host
        );
        println!();
        println!("{}", REINSTALL_CHECKLIST.trim_end());
    }
    Ok(())
}

/// Run `hermes backup` on the devbox, validate it, pull it into
/// `backup_dir`, verify the hash, remove the remote copy. Returns the local
/// path.
fn take_and_pull(
    t: &Transport,
    backup_dir: &Path,
    hostname: &str,
    keep_running: bool,
) -> anyhow::Result<PathBuf> {
    info!("running hermes backup on {}", t.destination);
    let out = t.capture(&format!("hermes backup -o \"$HOME\"/{REMOTE_ARCHIVE}"))?;
    if !out.stdout.trim().is_empty() {
        print!("{}", out.stdout);
    }
    if !out.success() {
        bail!(
            "hermes backup failed (exit {}):\n{}",
            out.status,
            out.stderr.trim_end()
        );
    }
    // A live backup cannot archive Unix sockets and Hermes says so; that is
    // the expected noise under --keep-running and a hard failure otherwise,
    // because a stopped Hermes has no sockets and "incomplete" then means
    // something real.
    let combined = format!("{}\n{}", out.stdout, out.stderr);
    if !keep_running && combined.to_ascii_lowercase().contains("incomplete") {
        bail!(
            "hermes backup reported an incomplete archive:\n{}",
            combined.trim_end()
        );
    }

    let remote_hash = t
        .capture(&format!(
            "test -s \"$HOME\"/{REMOTE_ARCHIVE} && sha256sum \"$HOME\"/{REMOTE_ARCHIVE} | cut -d' ' -f1"
        ))
        .context("failed to hash the archive on the devbox")?;
    if !remote_hash.success() {
        bail!("archive missing or empty on the devbox after hermes backup");
    }
    let remote_hash = remote_hash.stdout.trim().to_owned();

    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    let local = backup_dir.join(format!("hermes-{hostname}-{stamp}.zip"));
    info!("pulling archive to {}", local.display());
    t.pull_stdout(&format!("cat \"$HOME\"/{REMOTE_ARCHIVE}"), &local)?;

    let local_hash = sha256_file(&local)?;
    if local_hash != remote_hash {
        bail!(
            "archive hash mismatch after pull (devbox {remote_hash}, local {local_hash}); left {} for inspection",
            local.display()
        );
    }
    t.capture(&format!("rm -f \"$HOME\"/{REMOTE_ARCHIVE}"))?;
    Ok(local)
}

/// Lowercase hex SHA-256 of a file, matching `sha256sum`'s output format so
/// the two sides compare as strings.
fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Read-only. Every `pgrep` pattern uses the bracket form because this
/// script travels in the argv of the shell that runs it (see transport.rs).
const PREFLIGHT_SCRIPT: &str = r#"
echo "== agent processes"
pgrep -af '[c]odex|[c]laude|[o]pencode|[m]use|[h]ermes' || echo "(none)"
echo "== tmux sessions"
tmux ls 2>/dev/null || echo "(none)"
echo "== git repos with dirty or unpushed work"
found=0
for d in "$HOME"/git/*/; do
  [ -d "$d/.git" ] || continue
  dirty=$(git -C "$d" status --porcelain 2>/dev/null | head -n 1)
  unpushed=$(git -C "$d" log --branches --not --remotes --oneline 2>/dev/null | head -n 1)
  if [ -n "$dirty" ] || [ -n "$unpushed" ]; then
    found=1
    printf '%s:%s%s\n' "$(basename "$d")" "${dirty:+ dirty}" "${unpushed:+ unpushed}"
  fi
done
[ "$found" = 1 ] || echo "(none)"
"#;

const REINSTALL_CHECKLIST: &str = r#"
Next, reinstall the box by hand:
  1. In the provider's panel, reinstall with the newest Ubuntu LTS image.
  2. Add the profile's public key so the new box accepts the controller.
  3. When it is up, open the provider's console and run
       ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
     and keep that fingerprint; `kd devbox bootstrap` will ask you to confirm it.
Then: kd devbox bootstrap --profile <name>
"#;
