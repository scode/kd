//! `kd devbox backup`: preflight the devbox, take a Hermes backup, pull it
//! to the controller.
//!
//! The sequence is SPEC_impl.md "Backup sequence". The shape worth knowing
//! before reading the code:
//!
//! - Controller preflight checks the profile and archive directory. Agent
//!   credentials belong to bootstrap and are not needed for a backup.
//! - The devbox preflight is read-only and its whole purpose is to put the
//!   "is anything in flight?" evidence in front of the user before the one
//!   yes/no question. Nothing is enumerated or waived item by item.
//! - Service state is never changed, including on failure. Suspend first
//!   for a migration; a live snapshot may omit Unix sockets.
//! - The archive is pulled streaming, hash-checked against `sha256sum` on the
//!   devbox, and deleted there afterwards: it contains `.hermes/.env`.

use super::{BackupArgs, confirm, transport::Transport};
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::info;

/// Where `hermes backup` writes on the devbox before the pull. Home-relative
/// so the remote shell resolves it.
const REMOTE_ARCHIVE: &str = "hermes-backup-kd.zip";

/// Export and verify an archive without taking ownership of service state.
/// Failures leave suspension decisions with the operator, just like success.
pub fn run(args: BackupArgs) -> anyhow::Result<()> {
    let profile = args.profile.load()?;

    // Controller preflight: fail before touching the devbox.
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

    let archive = take_and_pull(&t, &profile.backup_dir, &profile.hostname)?;

    let version = t
        .capture("hermes --version 2>/dev/null | head -n 1")
        .map(|c| c.stdout.trim().to_owned())
        .unwrap_or_default();
    println!();
    println!("archive: {}", archive.display());
    println!("source hermes: {version}");
    println!("Service state was left unchanged. Suspend before backup when moving the instance.");
    Ok(())
}

/// Run `hermes backup` on the devbox, validate it, pull it into
/// `backup_dir`, verify the hash, remove the remote copy. Returns the local
/// path.
fn take_and_pull(t: &Transport, backup_dir: &Path, hostname: &str) -> anyhow::Result<PathBuf> {
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
    // Never silently accept an archive Hermes calls incomplete. A live
    // backup may fail here; suspend and retry for a migration-quality copy.
    let combined = format!("{}\n{}", out.stdout, out.stderr);
    if combined.to_ascii_lowercase().contains("incomplete") {
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
