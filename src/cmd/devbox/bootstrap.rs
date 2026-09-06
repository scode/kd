//! `kd devbox bootstrap`: configure an explicit SSH destination.
//!
//! The sequence is SPEC_impl.md "Bootstrap sequence"; this file is that list
//! in code, in order. The starting state is the premise of everything here:
//! a minimal Ubuntu that is up, has internet, and accepts SSH authentication
//! as `root` or as the user with passwordless sudo. From there kd owns only
//! what an agent cannot or must not do: the transport, the host-key decision,
//! the guard, creating the user, installing Codex, placing secrets, the
//! prompts, the reboot, and the probe. Two Codex runs on the box do the rest.
//!
//! Shared settings describe the environment. Only `--restore` selects
//! state to import; `--rehearsal` keeps that state's services stopped.
//! Tailscale enrollment is independent and explicitly requested.
//!
//! Every step is idempotent and there is no partial resume: the recovery for
//! any failure is "fix or ignore, then rerun the whole command".

use super::{
    BootstrapArgs, agent, confirm, home_dir, probe, profile, prompts, secrets,
    transport::Transport, wait_for_enter,
};
use anyhow::{Context, bail};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// What a run knows after the up-front phase.
struct Run {
    /// Transport as the intended user. Plain-SSH restores carry a temporary
    /// host-key pin; other runs retain the transport's normal verification.
    t: Transport,
    /// The unix user everything after seeding runs as.
    user: String,
    /// Keeps the per-run known-hosts file alive for the whole run.
    _known_hosts: Option<tempfile::NamedTempFile>,
}

/// Configure the destination in two agent phases. Resolve any restore
/// archive locally first; the source machine is never contacted by this run.
pub fn run(args: BootstrapArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let config = profile::load_config(&profile::config_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        &home,
    ))?;
    let plan = BootstrapPlan::resolve(&args, &config, &home)?;
    let rehearsal = args.rehearsal;

    // 1. Controller preflight: cheap, no connections, before any prompt.
    let sources = secrets::resolve_all(&home)?;
    info!("{}", secrets::describe(&sources));
    let public_key_path = profile::expand_tilde(&config.bootstrap.public_key, &home);
    let public_key = std::fs::read_to_string(&public_key_path)
        .with_context(|| format!("cannot read public key {}", public_key_path.display()))?;
    let hermes = plan.archive.is_some();
    let archive = &plan.archive;
    if let Some(archive) = archive {
        info!("archive to restore: {}", archive.display());
    }

    // 2. Select transport independently from restore and enrollment.
    let mut user_t = target_transport(&plan.destination, args.plain_ssh);
    let mut root_t = target_transport(&plan.root_destination, args.plain_ssh);
    // A restore may reuse an address with a stale key. Keep that key pin
    // isolated from the user's known_hosts; ordinary targets use SSH config.
    let known_hosts = if hermes
        && !rehearsal
        && matches!(user_t.backing, super::transport::Backing::Ssh { .. })
    {
        let file = confirm_fingerprint(&plan.destination)?;
        user_t = user_t.with_known_hosts_file(file.path());
        root_t = root_t.with_known_hosts_file(file.path());
        Some(file)
    } else {
        None
    };
    info!(
        "bootstrap against {} as {} via {}",
        plan.destination,
        plan.user,
        describe(&user_t)
    );
    // 3. Connect as the user; fall back to root for seeding.
    let seed_t = if user_t.capture("true")?.success() {
        info!("connected to {}", plan.destination);
        user_t.clone()
    } else {
        if rehearsal {
            bail!(
                "rehearsal requires an existing login with passwordless sudo on {}",
                plan.destination
            );
        }
        info!(
            "no login as {} yet; seeding as root on {}",
            plan.user, plan.destination
        );
        if !root_t.capture("true")?.success() {
            bail!(
                "cannot connect to {} as {} or root using SSH authentication",
                plan.destination,
                plan.user
            );
        }
        root_t
    };
    let run = Run {
        t: user_t,
        user: plan.user.clone(),
        _known_hosts: known_hosts,
    };
    let (mut run, seed_transport) = (run, seed_t);

    // 3. Guard, over the first connection.
    guard(&seed_transport, rehearsal || !hermes)?;

    // 4. Seed: the user, the key, sudo, a proven second connection, Codex.
    seed(&seed_transport, &run.t, &run.user, &public_key, rehearsal)?;
    let codex = sources
        .iter()
        .find(|s| s.cli == "codex")
        .context("codex credentials missing after preflight")?;
    install_codex(&run.t, codex)?;

    // 5. Token and Tailscale decisions, now that the box is reachable.
    let github_token = github_token(&run.t, rehearsal)?;
    if args.enroll_tailscale {
        tailscale_device_prompt(&run.t, &plan.hostname)?;
    }

    // 6. System phase, then the reboot if the upgrade asked for one.
    let system_report = agent::run_phase(
        &run.t,
        "system",
        &prompts::system_phase(&plan.hostname, &run.user),
    )?;
    reboot_if_required(&mut run)?;

    // 7. Secrets: the other three auth files, the archive, the token.
    place_secrets(
        &run.t,
        &sources,
        archive.as_deref(),
        github_token.as_deref(),
    )?;

    // 8. User-space phase.
    let user_report = agent::run_phase(
        &run.t,
        "user-space",
        &prompts::user_space_phase(
            &run.user,
            &config.bootstrap.repos,
            rehearsal,
            hermes,
            args.enroll_tailscale,
        ),
    )?;

    // 9. Tailscale, only when requested. The agent installed it; kd enrolls,
    // because the login URL has to reach this terminal.
    if args.enroll_tailscale {
        tailscale_up(&run.t)?;
    }

    // 10. Probe, then the agents' own reports. Exit 0 regardless.
    let expected_repos = expected_repo_count(&config.bootstrap.repos);
    let report = probe::run(
        &run.t,
        &probe::script(
            &plan.hostname,
            expected_repos,
            rehearsal,
            hermes,
            args.enroll_tailscale,
        ),
    )?;
    println!("\n== probe on {}\n{}", run.t.destination, report.trim_end());
    println!("\n== system phase report\n{}", system_report.trim_end());
    println!("\n== user-space phase report\n{}", user_report.trim_end());
    if rehearsal {
        println!(
            "\nrehearsal done. The target holds real credentials; destroy it when you are finished."
        );
    } else if run._known_hosts.is_some() {
        println!(
            "\nbootstrap done. If your own ~/.ssh/known_hosts has a stale key for {}, remove that entry with `ssh-keygen -R {}` (use [host]:port for a non-default port).",
            target_host(&plan.destination),
            target_host(&plan.destination)
        );
    } else {
        println!("\nbootstrap done. The target holds real credentials.");
    }
    Ok(())
}

/// Resolve identity and archive selection before credentials or SSH are
/// touched. A scratch run never resolves a profile or reads a backup dir.
struct BootstrapPlan {
    destination: String,
    root_destination: String,
    user: String,
    hostname: String,
    archive: Option<PathBuf>,
}

impl BootstrapPlan {
    /// Keep source archive identity separate from target identity. The
    /// shared login default applies even when a source uses another user.
    fn resolve(
        args: &BootstrapArgs,
        config: &profile::Profiles,
        home: &Path,
    ) -> anyhow::Result<Self> {
        let restore = args
            .restore
            .as_deref()
            .map(|name| config.resolve(name, home))
            .transpose()?;
        let hostname = args
            .hostname
            .as_deref()
            .or_else(|| restore.as_ref().map(|p| p.hostname.as_str()))
            .context("--hostname is required without --restore")?
            .to_owned();
        profile::validate_token("bootstrap", "hostname", &hostname)?;
        let target = args.target.strip_prefix("ssh://").unwrap_or(&args.target);
        let (user, host) = target
            .rsplit_once('@')
            .unwrap_or((&config.bootstrap.user, target));
        profile::validate_token("bootstrap", "user", user)?;
        if user == "root" {
            bail!("target user must be a non-root account; root is used automatically for seeding");
        }
        if host.is_empty()
            || host.starts_with('-')
            || host.chars().any(char::is_whitespace)
            || host.contains('@')
        {
            bail!("invalid SSH target");
        }
        let prefix = if args.target.starts_with("ssh://") {
            "ssh://"
        } else {
            ""
        };
        let archive = restore
            .as_ref()
            .map(|p| newest_archive(&p.backup_dir, &p.hostname))
            .transpose()?;
        Ok(Self {
            destination: format!("{prefix}{user}@{host}"),
            root_destination: format!("{prefix}root@{host}"),
            user: user.to_owned(),
            hostname,
            archive,
        })
    }
}

/// Everything secret except Codex's own login (placed during seeding) goes
/// over now, between the two agent runs, so the system phase never saw it.
/// The token is written only when one was collected, i.e. when `gh` on the
/// box was not already logged in, so a rerun never leaves a token file.
fn place_secrets(
    t: &Transport,
    sources: &[secrets::AuthSource],
    archive: Option<&Path>,
    github_token: Option<&str>,
) -> anyhow::Result<()> {
    info!("placing credentials on {}", t.destination);
    for s in sources.iter().filter(|s| s.cli != "codex") {
        t.push_secret(&s.contents, s.remote_relative)?;
    }
    if let Some(archive) = archive {
        info!("placing the Hermes archive on {}", t.destination);
        t.push_file(archive, prompts::HERMES_ARCHIVE_FILE)?;
    }
    if let Some(token) = github_token {
        t.push_secret(token.as_bytes(), prompts::GITHUB_TOKEN_FILE)?;
    }
    Ok(())
}

/// The manifest deduplicated with the always-cloned repos, by repo name,
/// which is what `~/git/*` will contain.
fn expected_repo_count(repos: &[String]) -> usize {
    let mut names: Vec<&str> = repos
        .iter()
        .map(String::as_str)
        .chain(prompts::ALWAYS_CLONED)
        .filter_map(|r| r.rsplit_once('/').map(|(_, n)| n))
        .collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

/// `tailscale up` blocks until the browser login completes or the timeout
/// expires; that is the whole wait. Output is streamed so the login URL
/// reaches the terminal. Hostname defaults to the OS hostname; no `--ssh`,
/// no tags, not ephemeral.
fn tailscale_up(t: &Transport) -> anyhow::Result<()> {
    // A rerun finds the node already enrolled, and `tailscale up` on an
    // enrolled node refuses unless every non-default flag from the original
    // enrollment is repeated, so it must not be run again.
    if tailnet_running(t)? {
        info!(
            "{} is already on the tailnet; skipping tailscale up",
            t.destination
        );
        return Ok(());
    }
    info!(
        "enrolling {} in the tailnet; open the login URL when it appears",
        t.destination
    );
    t.run("sudo -n tailscale up --timeout 10m")
        .context("tailscale enrollment failed or timed out")
}

/// Whether the box already has a running tailnet session. A missing
/// `tailscale` binary counts as not running.
fn tailnet_running(t: &Transport) -> anyhow::Result<bool> {
    let out = t.capture("tailscale status --json 2>/dev/null")?;
    Ok(out.success() && out.stdout.contains("\"BackendState\": \"Running\""))
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
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let mtime = metadata.modified()?;
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

/// Discover tailnet names without requiring a running local client.
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
/// every non-rehearsal plain-SSH restore; normal targets use SSH's own
/// verification and tailnet peers use Tailscale's identity.
fn confirm_fingerprint(destination: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    // ssh -G resolves aliases, URI ports and configured HostName without
    // connecting. Scan the same endpoint SSH will actually use.
    let config = Command::new("ssh").args(["-G", destination]).output()?;
    if !config.status.success() {
        bail!("cannot resolve SSH configuration for {destination}");
    }
    let config = String::from_utf8_lossy(&config.stdout);
    let value = |key: &str| config.lines().find_map(|line| line.strip_prefix(key));
    let host = value("hostname ").context("ssh -G returned no hostname")?;
    let port = value("port ").context("ssh -G returned no port")?;
    info!("scanning host key of {host}");
    let scan = Command::new("ssh-keyscan")
        .args(["-t", "ed25519", "-p", port, host])
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
fn guard(t: &Transport, refuse_live: bool) -> anyhow::Result<()> {
    let out = t.capture("pgrep -f '[h]ermes.*gateway' >/dev/null")?;
    if out.status == 1 {
        return Ok(());
    }
    if !out.success() {
        bail!(
            "cannot check for a live Hermes gateway on {} (exit {})",
            t.destination,
            out.status
        );
    }
    if refuse_live {
        bail!(
            "a Hermes gateway is running on {}; scratch bootstrap and restore rehearsals refuse a live instance",
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

/// Runs as root or as a sudoer; `__USER__` is the intended user. The key is
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
printf '%s ALL=(ALL) NOPASSWD:ALL\n' "$u" | $S tee "/etc/sudoers.d/90-kd-$u" >/dev/null
$S chmod 0440 "/etc/sudoers.d/90-kd-$u"
$S visudo -cf "/etc/sudoers.d/90-kd-$u" >/dev/null
"#;

/// Rehearsal seed: the login exists; make sure the shared key is authorized
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
    // Hidden entry on a terminal. Without one (a scripted run, a test with
    // stdin piped) the token is read as one plain line from stdin instead;
    // it still never touches argv or the log.
    let token = match rpassword::prompt_password("paste the token: ") {
        Ok(t) => t,
        Err(_) => {
            eprintln!("(no terminal; reading the token as one line from stdin)");
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .context("reading the token from stdin")?;
            line
        }
    };
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
    if tailnet_running(t)? {
        info!("{} is already on the tailnet", t.destination);
        return Ok(());
    }
    wait_for_enter(&format!(
        "if replacing an old Tailscale device named `{hostname}`, delete that stale device in the admin console. \
         For a new machine, ensure `{hostname}` is unused. Press Enter when ready."
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
    use clap::Parser;

    /// Exercise the actual clap contract without invoking any commands.
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: BootstrapArgs,
    }

    /// A shared environment needs no stateful profile or backup directory.
    /// The explicit target user wins over the shared default, including URIs.
    #[test]
    fn scratch_bootstrap_is_independent_of_profiles() {
        let config =
            profile::parse("[bootstrap]\nuser='scode'\npublic_key='~/key.pub'\nrepos=[]").unwrap();
        for (target, user, destination) in [
            ("worker", "scode", "scode@worker"),
            ("alice@worker", "alice", "alice@worker"),
            (
                "ssh://alice@localhost:2299",
                "alice",
                "ssh://alice@localhost:2299",
            ),
        ] {
            let cli =
                Cli::try_parse_from(["bootstrap", "--target", target, "--hostname", "worker"])
                    .unwrap();
            let plan = BootstrapPlan::resolve(&cli.args, &config, Path::new("/missing")).unwrap();
            assert_eq!(plan.user, user);
            assert_eq!(plan.destination, destination);
            assert!(plan.archive.is_none());
        }
    }

    /// Restoring to another hostname must still select the source profile's
    /// archive. A missing archive is a local failure, before SSH or auth.
    #[test]
    fn restore_uses_source_archive_and_optional_destination_hostname() {
        let dir = tempfile::tempdir().unwrap();
        let config = profile::parse("[bootstrap]\nuser='scode'\npublic_key='~/key.pub'\nrepos=[]\n[devbox.source]\nhost='old-host'\nhostname='old-name'\nbackup_dir='~'").unwrap();
        let cli = Cli::try_parse_from(["bootstrap", "--target", "new-host", "--restore", "source"])
            .unwrap();
        assert!(BootstrapPlan::resolve(&cli.args, &config, dir.path()).is_err());
        let archive = dir.path().join("hermes-old-name-20260906.zip");
        std::fs::write(&archive, b"archive").unwrap();
        std::fs::write(
            dir.path().join("hermes-other-20260907.zip"),
            b"wrong instance",
        )
        .unwrap();
        let plan = BootstrapPlan::resolve(&cli.args, &config, dir.path()).unwrap();
        assert_eq!(plan.hostname, "old-name");
        assert_eq!(plan.archive.as_ref(), Some(&archive));
        let mut args = cli.args;
        args.hostname = Some("new-name".into());
        let plan = BootstrapPlan::resolve(&args, &config, dir.path()).unwrap();
        assert_eq!(plan.hostname, "new-name");
        assert_eq!(plan.archive, Some(archive));
    }

    /// Invalid mode combinations must fail before credentials or a target
    /// can be touched. Old implicit-restore flags are intentionally removed.
    #[test]
    fn cli_requires_explicit_destination_and_safe_modes() {
        for argv in [
            vec!["bootstrap", "--hostname", "worker"],
            vec!["bootstrap", "--target", "worker"],
            vec![
                "bootstrap",
                "--target",
                "worker",
                "--hostname",
                "worker",
                "--rehearsal",
            ],
            vec![
                "bootstrap",
                "--target",
                "worker",
                "--restore",
                "source",
                "--rehearsal",
                "--enroll-tailscale",
            ],
            vec!["bootstrap", "--target", "worker", "--profile", "source"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err());
        }
        let config =
            profile::parse("[bootstrap]\nuser='scode'\npublic_key='key'\nrepos=[]").unwrap();
        for target in [
            "root@worker",
            "bad user@worker",
            "scode@",
            "-option",
            "worker\nother",
        ] {
            let args = BootstrapArgs {
                target: target.into(),
                restore: None,
                hostname: Some("worker".into()),
                plain_ssh: false,
                rehearsal: false,
                enroll_tailscale: false,
            };
            assert!(BootstrapPlan::resolve(&args, &config, Path::new("/missing")).is_err());
        }
    }

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
    fn target_host_parses_the_ssh_forms() {
        assert_eq!(target_host("scode@worker"), "worker");
        assert_eq!(target_host("worker"), "worker");
        assert_eq!(target_host("ssh://tl-user@localhost:2299"), "localhost");
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
