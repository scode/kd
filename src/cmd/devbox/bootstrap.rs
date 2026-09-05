//! `kd devbox bootstrap`: rebuild a fresh box from a profile.
//!
//! The sequence is SPEC_impl.md "Bootstrap sequence"; this file is that list
//! in code, in order. The starting state is the premise of everything here:
//! a minimal Ubuntu that is up, has internet, and accepts the profile's key
//! as `root` or as the user with passwordless sudo. From there kd owns only
//! what an agent cannot or must not do: the transport, the host-key decision,
//! the guard, creating the user, installing Codex, placing secrets, the
//! prompts, the reboot, and the probe. Two Codex runs on the box do the rest.
//!
//! Two modes, decided by `--target`. Without it, a real run against the
//! profile host, with a fingerprint prompt and a per-run known-hosts file
//! (the user's own `known_hosts` still holds the pre-reinstall key and is
//! never edited). With it, a rehearsal: no prompts, the target's user is the
//! user, Hermes is installed but never started, Tailscale is skipped.
//!
//! Every step is idempotent and there is no partial resume: the recovery for
//! any failure is "fix or ignore, then rerun the whole command".

use super::{
    BootstrapArgs, agent, confirm, home_dir, prompts, secrets, transport::Transport, wait_for_enter,
};
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// What a run knows after the up-front phase.
struct Run {
    /// Transport to the box as the user. On a real run this is built after
    /// the fingerprint prompt and carries the per-run known-hosts file.
    t: Transport,
    /// The unix user everything after seeding runs as.
    user: String,
    /// Keeps the per-run known-hosts file alive for the whole run.
    _known_hosts: Option<tempfile::NamedTempFile>,
}

pub fn run(args: BootstrapArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let profile = args.profile.load()?;
    let rehearsal = args.target.is_some();

    // 1. Controller preflight: cheap, no connections, before any prompt.
    let sources = secrets::resolve_all(&home)?;
    info!("{}", secrets::describe(&sources));
    let public_key = std::fs::read_to_string(&profile.public_key)
        .with_context(|| format!("cannot read public key {}", profile.public_key.display()))?;
    let archive = newest_archive(&profile.backup_dir, &profile.hostname)?;
    info!("archive to restore: {}", archive.display());

    // 2. Real run only: fingerprint prompt, then the per-run known-hosts file.
    let (mut run, seed_transport) = match &args.target {
        Some(target) => {
            let t = target_transport(target, args.plain_ssh);
            let user = target_user(target);
            info!("rehearsal against {target} as {user} via {}", describe(&t));
            let run = Run {
                t: t.clone(),
                user,
                _known_hosts: None,
            };
            (run, t)
        }
        None => {
            let known_hosts = confirm_fingerprint(&profile.host)?;
            let user_t = Transport::ssh(format!("{}@{}", profile.user, profile.host))
                .with_known_hosts_file(known_hosts.path());
            let root_t = Transport::ssh(format!("root@{}", profile.host))
                .with_known_hosts_file(known_hosts.path());
            // 3. Connect as the user; fall back to root for seeding.
            let seed_t = if user_t.capture("true")?.success() {
                info!("connected to {} as {}", profile.host, profile.user);
                user_t.clone()
            } else {
                info!(
                    "no login as {} yet; seeding as root on {}",
                    profile.user, profile.host
                );
                if !root_t.capture("true")?.success() {
                    bail!(
                        "cannot connect to {} as {} or root with the profile key",
                        profile.host,
                        profile.user
                    );
                }
                root_t
            };
            let run = Run {
                t: user_t,
                user: profile.user.clone(),
                _known_hosts: Some(known_hosts),
            };
            (run, seed_t)
        }
    };

    // 3. Guard, over the first connection.
    guard(&seed_transport, rehearsal)?;

    // 4. Seed: the user, the key, sudo, a proven second connection, Codex.
    seed(&seed_transport, &run.t, &run.user, &public_key, rehearsal)?;
    let codex = sources
        .iter()
        .find(|s| s.cli == "codex")
        .context("codex credentials missing after preflight")?;
    install_codex(&run.t, codex)?;

    // 5. Token and Tailscale decisions, now that the box is reachable.
    let github_token = github_token(&run.t, rehearsal)?;
    if !rehearsal {
        tailscale_device_prompt(&run.t, &profile.hostname)?;
    }

    // 6. System phase, then the reboot if the upgrade asked for one.
    let system_report = agent::run_phase(
        &run.t,
        "system",
        &prompts::system_phase(&profile.hostname, &run.user),
    )?;
    reboot_if_required(&mut run)?;

    let _ = (github_token, archive, system_report);
    bail!(
        "bootstrap: the system phase is done; secrets, user-space phase, and probe are not implemented yet"
    )
}

// ── Up-front pieces ────────────────────────────────────────────────────

/// Newest `hermes-<hostname>-*.zip` by mtime in `backup_dir`. The hostname
/// filter is what lets two profiles share one directory.
fn newest_archive(backup_dir: &Path, hostname: &str) -> anyhow::Result<PathBuf> {
    let prefix = format!("hermes-{hostname}-");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(backup_dir)
        .with_context(|| format!("cannot read backup dir {}", backup_dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(&prefix) && name.ends_with(".zip")) {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, entry.path()));
        }
    }
    best.map(|(_, p)| p).with_context(|| {
        format!(
            "no {prefix}*.zip in {}; run `kd devbox backup` first",
            backup_dir.display()
        )
    })
}

/// `tailscale ssh` when the target host is a tailnet peer, plain ssh
/// otherwise or when forced. A missing `tailscale` binary counts as "not a
/// peer".
fn target_transport(target: &str, plain_ssh: bool) -> Transport {
    if !plain_ssh && is_tailnet_peer(target_host(target)) {
        Transport::tailscale(target)
    } else {
        Transport::ssh(target)
    }
}

fn describe(t: &Transport) -> &'static str {
    match t.backing {
        super::transport::Backing::Ssh { .. } => "plain ssh",
        super::transport::Backing::TailscaleSsh => "tailscale ssh",
    }
}

/// `user@host` -> `host`; `host` -> `host`. A `ssh://` URI keeps its host.
fn target_host(target: &str) -> &str {
    let rest = target.strip_prefix("ssh://").unwrap_or(target);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    rest.split_once(':').map_or(rest, |(h, _)| h)
}

/// The login name in a `--target`, or the controller's own username when
/// the target names only a host (ssh's default). Used for messages and for
/// the seed script's idea of "the user".
fn target_user(target: &str) -> String {
    let rest = target.strip_prefix("ssh://").unwrap_or(target);
    match rest.rsplit_once('@') {
        Some((u, _)) => u.to_owned(),
        None => std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
    }
}

fn is_tailnet_peer(host: &str) -> bool {
    let Ok(output) = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    peer_names(&String::from_utf8_lossy(&output.stdout))
        .iter()
        .any(|n| n == host)
}

/// Every name a peer answers to in `tailscale status --json`: `HostName`
/// and `DNSName` (with its trailing dot removed, and also its first label,
/// which is what MagicDNS short names resolve). Pure, for the unit test.
fn peer_names(status_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(status_json) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if let Some(peers) = v.get("Peer").and_then(|p| p.as_object()) {
        for peer in peers.values() {
            if let Some(h) = peer.get("HostName").and_then(|h| h.as_str()) {
                names.push(h.to_owned());
            }
            if let Some(d) = peer.get("DNSName").and_then(|d| d.as_str()) {
                let full = d.trim_end_matches('.');
                names.push(full.to_owned());
                if let Some((short, _)) = full.split_once('.') {
                    names.push(short.to_owned());
                }
            }
        }
    }
    names
}

/// Scan the host's Ed25519 key, show its fingerprint in both forms, and ask.
/// Returns the per-run known-hosts file holding exactly that key. Asked on
/// every real run; there is nothing to "already satisfy".
fn confirm_fingerprint(host: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    info!("scanning host key of {host}");
    let scan = Command::new("ssh-keyscan")
        .args(["-t", "ed25519", host])
        .output()
        .context("failed to run ssh-keyscan")?;
    let line = String::from_utf8_lossy(&scan.stdout)
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("ssh-keyscan returned no ed25519 key for {host}"))?;
    let mut file = tempfile::Builder::new()
        .prefix("kd-devbox-known-hosts-")
        .tempfile()
        .context("cannot create the per-run known-hosts file")?;
    use std::io::Write;
    writeln!(file, "{line}")?;
    file.flush()?;
    let path = file.path().to_str().context("temp path is not UTF-8")?;
    let fp = |extra: &[&str]| -> String {
        let out = Command::new("ssh-keygen")
            .args(extra)
            .args(["-lf", path])
            .output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    eprintln!("host key of {host}:");
    eprintln!("  {}", fp(&[]));
    eprintln!("  {}", fp(&["-E", "md5"]));
    if !confirm("does this match the fingerprint shown in the box's console?")? {
        bail!("host key not confirmed; nothing was changed");
    }
    Ok(file)
}

// ── Guard and seed ─────────────────────────────────────────────────────

/// A running Hermes gateway means a live devbox that was not reinstalled.
/// Running, not enabled, and no directory checks: reruns must work.
fn guard(t: &Transport, rehearsal: bool) -> anyhow::Result<()> {
    let out = t.capture("pgrep -f '[h]ermes.*gateway' >/dev/null")?;
    if !out.success() {
        return Ok(());
    }
    if rehearsal {
        bail!(
            "a Hermes gateway is running on {}; a rehearsal target must never have one",
            t.destination
        );
    }
    if !confirm(&format!(
        "a Hermes gateway is running on {}. Continue anyway?",
        t.destination
    ))? {
        bail!("stopped: Hermes gateway running on the target");
    }
    Ok(())
}

/// Create the user if missing, authorize the key, grant passwordless sudo,
/// then prove a connection as that user with working sudo. Idempotent. The
/// public key arrives on stdin so it never needs quoting.
fn seed(
    seed_t: &Transport,
    user_t: &Transport,
    user: &str,
    public_key: &str,
    rehearsal: bool,
) -> anyhow::Result<()> {
    info!("seeding user {user} on {}", seed_t.destination);
    let script = if rehearsal {
        // The target user exists and has sudo; only the key may be missing.
        REHEARSAL_SEED_SCRIPT.to_owned()
    } else {
        SEED_SCRIPT.replace("__USER__", user)
    };
    seed_t.run_with_stdin(&script, public_key.trim().as_bytes())?;
    let proof = user_t.capture("sudo -n true && id -un")?;
    if !proof.success() {
        bail!(
            "seeding did not produce a working login for {user} on {} with passwordless sudo:\n{}",
            user_t.destination,
            proof.stderr.trim_end()
        );
    }
    info!(
        "login as {} with passwordless sudo proven",
        proof.stdout.trim()
    );
    Ok(())
}

/// Runs as root or as a sudoer; `__USER__` is the profile user. The key is
/// read from stdin once and appended only if absent.
const SEED_SCRIPT: &str = r#"
set -eu
u=__USER__
key=$(cat)
if [ "$(id -u)" = 0 ]; then S=""; else S="sudo -n"; fi
if ! id -u "$u" >/dev/null 2>&1; then
  $S useradd -m -s /bin/bash "$u"
fi
home=$(getent passwd "$u" | cut -d: -f6)
$S install -d -m 0700 -o "$u" -g "$u" "$home/.ssh"
$S touch "$home/.ssh/authorized_keys"
$S chmod 0600 "$home/.ssh/authorized_keys"
$S chown "$u:$u" "$home/.ssh/authorized_keys"
if ! $S grep -qF "$key" "$home/.ssh/authorized_keys"; then
  printf '%s\n' "$key" | $S tee -a "$home/.ssh/authorized_keys" >/dev/null
fi
printf '%s ALL=(ALL) NOPASSWD:ALL\n' "$u" | $S install -m 0440 /dev/stdin "/etc/sudoers.d/90-kd-$u"
$S visudo -cf "/etc/sudoers.d/90-kd-$u" >/dev/null
"#;

/// Rehearsal seed: the login exists; make sure the profile key is authorized
/// too and that sudo is passwordless.
const REHEARSAL_SEED_SCRIPT: &str = r#"
set -eu
key=$(cat)
sudo -n true
install -d -m 0700 "$HOME/.ssh"
touch "$HOME/.ssh/authorized_keys"
chmod 0600 "$HOME/.ssh/authorized_keys"
grep -qF "$key" "$HOME/.ssh/authorized_keys" || printf '%s\n' "$key" >> "$HOME/.ssh/authorized_keys"
"#;

/// The one installer Rust owns (see SPEC_impl.md): fetch the Codex release
/// tarballs for this architecture straight from GitHub's `latest/download`
/// redirect, so no API call and no JSON parsing, and place its auth file.
///
/// Two binaries, not one: since Codex 0.153 the command runner lives in a
/// separate `codex-code-mode-host` executable that `codex` looks for next
/// to itself, and the `code_mode_host` feature is on by default and fails
/// closed. A `codex` without its host can run no commands at all, which is
/// exactly how the first rehearsal failed.
fn install_codex(t: &Transport, auth: &secrets::AuthSource) -> anyhow::Result<()> {
    info!("installing Codex on {}", t.destination);
    t.run(CODEX_INSTALL_SCRIPT)?;
    t.push_secret(&auth.contents, auth.remote_relative)?;
    Ok(())
}

const CODEX_INSTALL_SCRIPT: &str = r#"
set -eu
mkdir -p "$HOME/.local/bin"
base="https://github.com/openai/codex/releases/latest/download"
arch=$(uname -m)
fetch() {
  # $1 = release binary name, also the installed name.
  if [ -x "$HOME/.local/bin/$1" ]; then return 0; fi
  tmp=$(mktemp -d)
  curl -fsSL "$base/$1-$arch-unknown-linux-musl.tar.gz" | tar -xz -C "$tmp"
  bin=$(find "$tmp" -type f | head -n 1)
  install -m 0755 "$bin" "$HOME/.local/bin/$1"
  rm -rf "$tmp"
}
fetch codex
fetch codex-code-mode-host
"$HOME/.local/bin/codex" --version
"#;

// ── Prompts that need the box ──────────────────────────────────────────

/// A token only when `gh` on the box is not already logged in. Real run:
/// hidden prompt after opening the prefilled token page. Rehearsal: the
/// controller's own `gh auth token`.
fn github_token(t: &Transport, rehearsal: bool) -> anyhow::Result<Option<String>> {
    if t.capture("gh auth status >/dev/null 2>&1")?.success() {
        info!("gh is already authenticated on {}", t.destination);
        return Ok(None);
    }
    if rehearsal {
        let out = Command::new("gh")
            .args(["auth", "token"])
            .output()
            .context("failed to run gh auth token on the controller")?;
        if !output_ok(&out) {
            bail!("gh auth token failed on the controller; log in with `gh auth login` first");
        }
        return Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()));
    }
    eprintln!("GitHub needs a new classic token. Create one here (no expiry):");
    eprintln!("  {GITHUB_TOKEN_URL}");
    let token = rpassword::prompt_password("paste the token: ").context("reading the token")?;
    if token.trim().is_empty() {
        bail!("no token entered");
    }
    Ok(Some(token.trim().to_owned()))
}

/// Prefilled classic-token page: the four scopes the devbox's `gh` had, and
/// a description. Expiry is chosen on the page.
const GITHUB_TOKEN_URL: &str = "https://github.com/settings/tokens/new?description=KD+devbox&scopes=repo%2Cworkflow%2Cread%3Aorg%2Cgist";

fn output_ok(out: &std::process::Output) -> bool {
    out.status.success() && !out.stdout.is_empty()
}

/// The stale device for the same hostname must go before `tailscale up`, or
/// the new node becomes `<hostname>-1`. Skipped when the box is already on
/// the tailnet (a rerun).
fn tailscale_device_prompt(t: &Transport, hostname: &str) -> anyhow::Result<()> {
    let out = t.capture("tailscale status --json 2>/dev/null")?;
    if out.success() && out.stdout.contains("\"BackendState\": \"Running\"") {
        info!("{} is already on the tailnet", t.destination);
        return Ok(());
    }
    wait_for_enter(&format!(
        "delete the old device named `{hostname}` in the Tailscale admin console now, \
         otherwise the new node will be named `{hostname}-1`. Press Enter when done."
    ))
}

// ── Reboot ─────────────────────────────────────────────────────────────

/// Reboot when the upgrade asks for it and the box can (no systemd means no
/// reboot, and the warning says so). Polls ssh for up to ten minutes; the
/// host key survives a reboot so the same known-hosts file serves.
fn reboot_if_required(run: &mut Run) -> anyhow::Result<()> {
    let t = &run.t;
    if !t.capture("test -f /var/run/reboot-required")?.success() {
        info!("no reboot required");
        return Ok(());
    }
    if !t.capture("command -v systemctl >/dev/null")?.success() {
        warn!("reboot required but this box has no systemd; skipping the reboot");
        return Ok(());
    }
    info!("rebooting {}", t.destination);
    // The ssh session drops mid-command; its exit status means nothing.
    let _ = t.capture("sudo -n systemctl reboot");
    let deadline = Instant::now() + Duration::from_secs(600);
    std::thread::sleep(Duration::from_secs(10));
    while Instant::now() < deadline {
        if t.capture("true").map(|c| c.success()).unwrap_or(false) {
            info!("{} is back", t.destination);
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(10));
    }
    bail!(
        "{} did not come back within ten minutes of the reboot",
        t.destination
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer answers to its HostName, its full MagicDNS name, and the short
    /// first label; the trailing dot Tailscale prints must not leak in.
    #[test]
    fn peer_names_cover_every_way_a_target_is_written() {
        let json = r#"{"BackendState":"Running","Peer":{"k":{"HostName":"worker","DNSName":"worker.tail1234.ts.net.","Online":true}}}"#;
        let names = peer_names(json);
        assert!(names.contains(&"worker".to_owned()));
        assert!(names.contains(&"worker.tail1234.ts.net".to_owned()));
        assert!(!names.iter().any(|n| n.ends_with('.')));
        assert!(peer_names("not json").is_empty());
    }

    #[test]
    fn target_host_and_user_parse_the_ssh_forms() {
        assert_eq!(target_host("scode@worker"), "worker");
        assert_eq!(target_host("worker"), "worker");
        assert_eq!(target_host("ssh://tl-user@localhost:2299"), "localhost");
        assert_eq!(target_user("scode@worker"), "scode");
        assert_eq!(target_user("ssh://tl-user@localhost:2299"), "tl-user");
    }

    /// The seed script's only template slot is the username; the profile
    /// validation guarantees it is a plain token, so no quoting is needed.
    #[test]
    fn seed_script_substitutes_user_only() {
        let s = SEED_SCRIPT.replace("__USER__", "scode");
        assert!(s.contains("u=scode\n"));
        assert!(!s.contains("__USER__"));
    }
}
