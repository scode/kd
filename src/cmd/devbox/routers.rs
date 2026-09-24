//! Local subscription routers: codex-lb in front of Codex, CLIProxyAPI in
//! front of Claude Code.
//!
//! Every bootstrap installs both as Docker Compose services and points plain
//! `codex` and `claude` at them. A fresh box ends with healthy routers that
//! hold no accounts; the user logs accounts in afterwards through an SSH
//! tunnel (see [`login_help`]). Until then the default CLIs fail, which is
//! why kd's own agent runs and probe requests bypass the routers explicitly
//! ([`CODEX_NATIVE_OVERRIDE`], [`CLAUDE_NATIVE_SETTINGS`]).
//!
//! This is deterministic Rust-owned setup rather than an agent prompt item
//! because the router layout is a security contract, not a preference: both
//! services use host networking and bind 127.0.0.1 only. Docker's `ports:`
//! publishing (what both upstreams document) binds every interface and
//! bypasses UFW, which would put the proxies on the public internet. It also
//! generates CLIProxyAPI's keys, and kd keeps secrets out of prompts.
//!
//! Ownership: kd owns and rewrites both compose files on every run, so a pin
//! bump in kd plus a rerun is the upgrade path. Everything the services
//! accumulate is left alone: the named volumes (accounts, settings, request
//! history) and CLIProxyAPI's `config.yaml`, which its management UI edits
//! in place. Nothing here is backed up; a new box means logging in again.

use super::transport::Transport;
use anyhow::{Context, bail};
use toml_edit::{DocumentMut, Item, Table, Value};
use tracing::{info, warn};

/// codex-lb's dashboard and proxy. Codex, SSH tunnels and the probe all use
/// this exact port; it is fixed so controller-side tunnel scripts can be too.
pub const CODEX_LB_PORT: u16 = 2455;
/// codex-lb's ChatGPT OAuth callback, listening only while a login runs.
pub const CODEX_LB_OAUTH_PORT: u16 = 1455;
/// CLIProxyAPI's proxy, management API and management UI.
pub const CLIPROXY_PORT: u16 = 8317;
/// CLIProxyAPI's Claude OAuth callback, listening only while a login runs.
/// It binds every interface. UFW's default deny keeps it off the public
/// interface (host networking does not bypass UFW), but bootstrap's firewall
/// allows all traffic in on `tailscale0`, so tailnet peers can reach it
/// during a login, and a host without UFW (no systemd) exposes it publicly
/// for that window.
pub const CLIPROXY_CLAUDE_OAUTH_PORT: u16 = 54545;

/// `codex` config override that selects the built-in provider for one run.
/// kd's agent phases and probe use it so they work before any codex-lb
/// account exists, including on reruns where config.toml already points at
/// codex-lb. Verified 2026-09-24: the request reports `provider: openai` and
/// never reaches codex-lb.
pub const CODEX_NATIVE_OVERRIDE: &str = r#"-c 'model_provider="openai"'"#;

/// `claude --settings` value that routes one run past CLIProxyAPI to the
/// native login. `env -u` is not enough, because settings.json `env` wins
/// over the process environment; emptying both variables through a higher
/// precedence settings layer does work (verified 2026-09-24, zero requests
/// reached the proxy). `claude auth status` also needs it: with the proxy
/// token set it reports `loggedIn: true` via `oauth_token` even when the
/// copied claude.ai login is broken.
pub const CLAUDE_NATIVE_SETTINGS: &str =
    r#"{"env":{"ANTHROPIC_BASE_URL":"","ANTHROPIC_AUTH_TOKEN":""}}"#;

/// Install or converge both services, then point Claude and Codex at them.
/// Runs after the user-space phase, when Docker, jq and dotfiles are in
/// place. Returns whether the routers were installed.
///
/// A target whose Docker daemon is not usable (no systemd, so the system
/// phase skipped starting it; or a session that does not have the docker
/// group yet) skips the routers with a warning instead of failing: bootstrap
/// supports such degraded targets, and the probe's router checks then report
/// the gap. The clients are not rewired in that case, because pointing them
/// at routers that do not exist would break the native CLIs for nothing.
/// Once Docker works, every other failure aborts bootstrap; every step is
/// safe to rerun.
pub fn install(t: &Transport) -> anyhow::Result<bool> {
    let docker = t.capture("docker info >/dev/null")?;
    if !docker.success() {
        warn!(
            "Docker is not usable on {} ({}); skipping codex-lb and CLIProxyAPI, clients stay native",
            t.destination,
            docker.stderr.trim()
        );
        return Ok(false);
    }
    info!("installing codex-lb and CLIProxyAPI on {}", t.destination);
    t.run(&services_script())
        .context("router service setup failed")?;
    t.run(CLAUDE_SETTINGS)
        .context("pointing Claude at CLIProxyAPI failed")?;
    wire_codex(t).context("pointing Codex at codex-lb failed")?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// codex-lb's compose file. The explicit `--host 127.0.0.1` matters: do not
/// rely on the image's listener default. Telemetry is off before first start.
const CODEX_LB_COMPOSE: &str = r#"# Written by kd devbox bootstrap; rewritten on every run. See kd's SPEC_impl.md.
# Host networking so local clients and SSH-forwarded requests reach codex-lb
# as loopback traffic; the listener is pinned to 127.0.0.1 because Docker port
# publishing would bind every interface and bypass UFW.
services:
  codex-lb:
    image: ghcr.io/soju06/codex-lb:1.24.0@sha256:ba5598aaa70f7acf74a958139037604621ec2582a7fb5fe1e870f08bece807d6
    container_name: codex-lb
    restart: unless-stopped
    network_mode: host
    command: [python, -m, app.cli, --host, 127.0.0.1, --port, "2455"]
    environment:
      CODEX_LB_DATA_DIR: /var/lib/codex-lb
      CODEX_LB_OAUTH_CALLBACK_HOST: 127.0.0.1
      CODEX_LB_TELEMETRY_ENABLED: "false"
    volumes:
      - codex-lb-data:/var/lib/codex-lb
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "3"
volumes:
  # Created by kd before compose runs; external so `docker compose down -v`
  # cannot delete account logins and history.
  codex-lb-data:
    name: codex-lb-data
    external: true
"#;

/// CLIProxyAPI's compose file. The listener address lives in config.yaml,
/// not here. `WRITABLE_PATH` puts logs and the downloaded management panel in
/// the volume next to config and auths; `TZ` overrides the image's hardcoded
/// Asia/Shanghai with the timezone bootstrap gives every devbox.
const CLIPROXY_COMPOSE: &str = r#"# Written by kd devbox bootstrap; rewritten on every run. See kd's SPEC_impl.md.
# Host networking with the listener pinned to 127.0.0.1 in config.yaml; Docker
# port publishing would bind every interface and bypass UFW. The image is
# pinned by digest because upstream releases almost daily.
services:
  cliproxy:
    image: eceasy/cli-proxy-api:v7.3.17@sha256:a1dffb9c2300099039d9e2dd3dbf6396a72b798c891fe170940d2d8106222a8c
    container_name: cliproxy
    restart: unless-stopped
    network_mode: host
    command: [./CLIProxyAPI, --config, /data/config.yaml]
    environment:
      WRITABLE_PATH: /data
      TZ: America/Los_Angeles
    volumes:
      - cliproxy-data:/data
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "3"
volumes:
  # Created by kd before compose runs; external so `docker compose down -v`
  # cannot delete the Claude OAuth credentials it holds.
  cliproxy-data:
    name: cliproxy-data
    external: true
"#;

/// CLIProxyAPI's initial config.yaml, written into the volume only when none
/// exists. Only settings that differ from upstream defaults, or must be
/// explicit, are set. `session-affinity` is the important one: upstream
/// defaults it off, which rotates accounts per request and throws away
/// Claude's per-account prompt cache on every turn. The management key is
/// hashed by CLIProxyAPI on first start; the plaintext stays in secrets.env.
const CLIPROXY_CONFIG: &str = r#"# Seeded once by kd devbox bootstrap; the management UI edits this file afterwards.
host: "127.0.0.1"
port: 8317
remote-management:
  allow-remote: false
  secret-key: "@MANAGEMENT_KEY@"
  disable-auto-update-panel: true
auth-dir: "/data/auths"
api-keys:
  - "@CLIENT_KEY@"
usage-statistics-enabled: true
routing:
  strategy: "round-robin"
  session-affinity: true
"#;

/// Runs inside a throwaway CLIProxyAPI container with the volume mounted at
/// `$1`, reading the rendered config on stdin. Seeds config.yaml only when
/// the volume has none, so a config edited in the management UI is never
/// replaced. `$2` is `fresh` when the keys were generated during this run:
/// an existing config then holds other keys, which would leave the proxy
/// rejecting the client key kd is about to wire into Claude, so it fails
/// (exit 3) instead. Must not contain single quotes; the script embeds it in
/// a single-quoted argument.
const SEED_IN_VOLUME: &str = r#"dir=$1
if [ -e "$dir/config.yaml" ]; then
  cat >/dev/null
  if [ "$2" = fresh ]; then
    echo "cliproxy-data already has a config.yaml, but ~/.config/cliproxy/secrets.env is missing. Restore that file, or run docker volume rm cliproxy-data to start over (this deletes the Claude logins)." >&2
    exit 3
  fi
else
  umask 077 && mkdir -p "$dir/auths" && cat > "$dir/config.yaml.kd" && mv "$dir/config.yaml.kd" "$dir/config.yaml"
fi"#;

/// Render the service setup script. Keys are generated on the target and
/// never leave it. They travel only through shell builtins (parameter
/// expansion, `printf`) and pipes, never argv, so other local users cannot
/// read them from the process list.
///
/// `secrets.env` and the seeded config.yaml belong together. New keys are
/// written to secrets.env only after the seed step accepted them, so a
/// refused seed (see [`SEED_IN_VOLUME`]) leaves nothing behind that a rerun
/// could mistake for the matching keys.
fn services_script() -> String {
    format!(
        r#"set -o pipefail
umask 077
codex_lb="$HOME/.config/codex-lb"
cliproxy="$HOME/.config/cliproxy"
mkdir -p "$codex_lb" "$cliproxy" && chmod 700 "$codex_lb" "$cliproxy" || exit 1
cat > "$codex_lb/compose.yaml" <<'KD_EOF' || exit 1
{CODEX_LB_COMPOSE}KD_EOF
cat > "$cliproxy/compose.yaml" <<'KD_EOF' || exit 1
{CLIPROXY_COMPOSE}KD_EOF
chmod 600 "$codex_lb/compose.yaml" "$cliproxy/compose.yaml" || exit 1

# Random hex from the kernel; openssl is not guaranteed on a minimal image.
random_hex() {{ head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }}
secrets="$cliproxy/secrets.env"
if [ -e "$secrets" ]; then
  fresh=
  client=$(sed -n 's/^CLIPROXY_CLIENT_KEY=//p' "$secrets") || exit 1
  management=$(sed -n 's/^CLIPROXY_MANAGEMENT_KEY=//p' "$secrets") || exit 1
  if [ -z "$client" ] || [ -z "$management" ]; then
    printf 'missing keys in %s\n' "$secrets" >&2
    exit 1
  fi
else
  fresh=fresh
  client=$(random_hex) && management=$(random_hex) || exit 1
  [ "${{#client}}" = 48 ] && [ "${{#management}}" = 48 ] || exit 1
  client=sk-cpa-$client
fi

for volume in codex-lb-data cliproxy-data; do
  docker volume inspect "$volume" >/dev/null 2>&1 || docker volume create "$volume" >/dev/null || exit 1
done

docker compose -f "$cliproxy/compose.yaml" pull --quiet || exit 1
image=$(docker compose -f "$cliproxy/compose.yaml" config --images) || exit 1
config=$(cat <<'KD_EOF'
{CLIPROXY_CONFIG}KD_EOF
)
config=${{config//@CLIENT_KEY@/$client}}
config=${{config//@MANAGEMENT_KEY@/$management}}
printf '%s\n' "$config" | docker run --rm -i -v cliproxy-data:/data --entrypoint sh "$image" -c \
  '{SEED_IN_VOLUME}' sh /data "$fresh" || exit 1
if [ -n "$fresh" ]; then
  printf 'CLIPROXY_CLIENT_KEY=%s\nCLIPROXY_MANAGEMENT_KEY=%s\n' "$client" "$management" > "$secrets.kd" || exit 1
  mv -- "$secrets.kd" "$secrets" || exit 1
fi
chmod 600 "$secrets" || exit 1

docker compose -f "$codex_lb/compose.yaml" up -d || exit 1
docker compose -f "$cliproxy/compose.yaml" up -d || exit 1
"#
    )
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

/// Point plain `claude` at CLIProxyAPI by merging two `env` keys into
/// `~/.claude/settings.json`, creating the file when missing. settings.json
/// rather than a shell profile, because it applies however Claude is
/// launched (tmux, scripts, other agents). The client key sits there as a
/// literal, so the file ends up mode 0600; it only works against the
/// loopback proxy.
///
/// Every other key is preserved. The dotfiles installer merges only its own
/// paths into the same file and keeps both `env` and the file mode, so the
/// two do not fight over it. Symlinked, invalid, or non-object files and a
/// non-object `env` fail rather than being replaced. An already-wired file
/// is left byte-for-byte unchanged. The key reaches jq through the
/// environment, not argv.
pub const CLAUDE_SETTINGS: &str = r#"
set -o pipefail
KD_CLIPROXY_KEY=$(sed -n 's/^CLIPROXY_CLIENT_KEY=//p' "$HOME/.config/cliproxy/secrets.env") || exit 1
[ -n "$KD_CLIPROXY_KEY" ] || exit 1
export KD_CLIPROXY_KEY
url=http://127.0.0.1:8317
settings="$HOME/.claude/settings.json"
if [ -L "$settings" ]; then
  printf 'refusing to replace symlink: ~/.claude/settings.json\n' >&2
  exit 1
fi
umask 077
mkdir -p "$HOME/.claude" || exit 1
if [ -e "$settings" ]; then
  jq -e 'type == "object"' "$settings" >/dev/null || exit 1
  if jq -e --arg url "$url" '.env.ANTHROPIC_BASE_URL == $url and .env.ANTHROPIC_AUTH_TOKEN == $ENV.KD_CLIPROXY_KEY' "$settings" >/dev/null; then
    chmod 600 "$settings" || exit 1
    exit 0
  fi
fi
staged=$(mktemp "$HOME/.claude/settings.json.kd.XXXXXXXX") || exit 1
trap 'rm -f -- "$staged"' EXIT
if [ -e "$settings" ]; then
  cat "$settings" > "$staged" || exit 1
else
  printf '{}\n' > "$staged" || exit 1
fi
updated=$(jq --arg url "$url" '.env = ((.env // {}) + {ANTHROPIC_BASE_URL: $url, ANTHROPIC_AUTH_TOKEN: $ENV.KD_CLIPROXY_KEY})' "$staged") || exit 1
printf '%s\n' "$updated" > "$staged" || exit 1
mv -- "$staged" "$settings" || exit 1
"#;

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// Print `~/.codex/config.toml` after a sentinel line, or signal its absence
/// with exit 4. A symlink (exit 3) is refused, like the Claude merge refuses
/// one: it points at config something else manages. The sentinel exists
/// because the transport runs `bash -lc`, and anything a login profile
/// prints to stdout would otherwise become part of the "file" kd writes back.
const READ_CODEX_CONFIG: &str = r#"f="$HOME/.codex/config.toml"
if [ -L "$f" ]; then exit 3; elif [ -e "$f" ]; then printf '%s\n' KD-CODEX-CONFIG-BEGIN && cat "$f"; else exit 4; fi"#;

/// Line [`READ_CODEX_CONFIG`] prints before the file's bytes.
const CODEX_CONFIG_SENTINEL: &str = "KD-CODEX-CONFIG-BEGIN\n";

/// Replace the pushed config in one rename, so a reader never sees a
/// truncated file. This does not serialize against a Codex process writing
/// the same file (project trust); interactive sessions must be closed during
/// bootstrap, as for Claude.
const INSTALL_CODEX_CONFIG: &str =
    r#"mv -- "$HOME/.codex/config.toml.kd" "$HOME/.codex/config.toml""#;

/// Read Codex's config over the transport, merge the codex-lb provider, and
/// write it back only if it changed. The edit happens on the controller
/// because the target has no TOML writer; the file holds no secrets.
fn wire_codex(t: &Transport) -> anyhow::Result<()> {
    let captured = t.capture(READ_CODEX_CONFIG)?;
    let existing = match captured.status {
        0 => Some(
            captured
                .stdout
                .split_once(CODEX_CONFIG_SENTINEL)
                .map(|(_, file)| file.to_owned())
                .context("reading ~/.codex/config.toml returned no sentinel")?,
        ),
        3 => bail!("refusing to replace symlink: ~/.codex/config.toml"),
        4 => None,
        status => bail!(
            "reading ~/.codex/config.toml exited with {status}: {}",
            captured.stderr.trim()
        ),
    };
    let updated = codex_config(existing.as_deref())?;
    if existing.as_deref() == Some(updated.as_str()) {
        return Ok(());
    }
    t.push_secret(updated.as_bytes(), ".codex/config.toml.kd")?;
    t.run(INSTALL_CODEX_CONFIG)
}

/// Make codex-lb the default provider in a Codex config.toml, preserving
/// everything else.
///
/// The provider's display name stays `openai` on purpose: with any other name
/// Codex stopped using remote compaction through codex-lb (observed during
/// the manual setup). The key, `codex-lb`, is what identifies the route; in
/// the TUI, `/status` shows the localhost URL. `model` and reasoning effort
/// are deliberately not touched, since those are the user's choice.
///
/// Values kd replaces keep their surrounding whitespace and trailing
/// comments, so an already-wired file is a byte-for-byte no-op and reruns do
/// not rewrite it. Keys inside `[model_providers.codex-lb]` that kd does not
/// set survive, so hand additions such as extra headers are kept. An inline
/// `model_providers = { ... }` (legal, and accepted by Codex) is converted to
/// a standard table; that changes its layout, not its meaning. A
/// `model_providers` or `codex-lb` entry that is not a table is an error.
fn codex_config(existing: Option<&str>) -> anyhow::Result<String> {
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .context("~/.codex/config.toml is not valid TOML")?;
    set_keeping_decor(doc.as_table_mut(), "model_provider", "codex-lb".into());
    let providers = standard_table(
        doc.as_table_mut(),
        "model_providers",
        true,
        "model_providers in ~/.codex/config.toml is not a table",
    )?;
    let provider = standard_table(
        providers,
        "codex-lb",
        false,
        "model_providers.codex-lb in ~/.codex/config.toml is not a table",
    )?;
    set_keeping_decor(provider, "name", "openai".into());
    set_keeping_decor(
        provider,
        "base_url",
        format!("http://127.0.0.1:{CODEX_LB_PORT}/backend-api/codex").into(),
    );
    set_keeping_decor(provider, "wire_api", "responses".into());
    set_keeping_decor(provider, "requires_openai_auth", true.into());
    Ok(doc.to_string())
}

/// Get `key` as a standard `[table]`, creating it when missing and
/// converting an inline table in place. `implicit` suppresses the header of
/// a table that only exists to hold subtables.
fn standard_table<'a>(
    parent: &'a mut Table,
    key: &str,
    implicit: bool,
    error: &'static str,
) -> anyhow::Result<&'a mut Table> {
    let item = parent.entry(key).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(implicit);
        Item::Table(table)
    });
    if let Some(inline) = item.as_inline_table().cloned() {
        *item = Item::Table(inline.into_table());
    }
    item.as_table_mut().context(error)
}

/// Set a scalar, reusing the old value's decor (spacing and trailing
/// comment) when there is one. Plain `table[key] = value(..)` resets the
/// decor, which drops comments and makes an unchanged file differ.
fn set_keeping_decor(table: &mut Table, key: &str, new: Value) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = new;
            *old.decor_mut() = decor;
        }
        None => {
            table.insert(key, Item::Value(new));
        }
    }
}

// ---------------------------------------------------------------------------
// Login instructions
// ---------------------------------------------------------------------------

/// What bootstrap prints at the end: the one tunnel both logins need and
/// where to go through it. The ports are the fixed constants above, never
/// chosen per run, so a controller-side script can hardcode the same
/// command. `destination` is the SSH destination bootstrap used.
pub fn login_help(destination: &str) -> String {
    let forward = |port: u16| format!("-L 127.0.0.1:{port}:127.0.0.1:{port}");
    format!(
        "\
Routers: codex-lb and CLIProxyAPI are running with no accounts. Until you log
accounts in, plain `codex` and `claude` on the box fail. The native bypasses
still work: `codex {CODEX_NATIVE_OVERRIDE} ...` and
`claude --settings '{CLAUDE_NATIVE_SETTINGS}' ...`.

From this machine, keep this tunnel open while logging in (fixed ports):

  ssh -N -o ExitOnForwardFailure=yes {} {} {} {} {destination}

- Codex: open http://127.0.0.1:{CODEX_LB_PORT}, Add account, complete ChatGPT
  OAuth. Repeat per subscription.
- Claude: open http://127.0.0.1:{CLIPROXY_PORT}/management.html. The management
  key is on the box: sed -n 's/^CLIPROXY_MANAGEMENT_KEY=//p' ~/.config/cliproxy/secrets.env
  Add a Claude OAuth account under OAuth Login. Repeat per subscription.",
        forward(CODEX_LB_PORT),
        forward(CODEX_LB_OAUTH_PORT),
        forward(CLIPROXY_PORT),
        forward(CLIPROXY_CLAUDE_OAUTH_PORT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::{Command, Output};

    /// The compose files and seeded config are what make the routers
    /// loopback-only. Their ports must also agree with the constants the
    /// tunnel, probe and client wiring use, since nothing else ties them.
    #[test]
    fn service_definitions_stay_loopback_only_and_match_the_fixed_ports() {
        for compose in [CODEX_LB_COMPOSE, CLIPROXY_COMPOSE] {
            assert!(compose.contains("network_mode: host"));
            assert!(!compose.contains("ports:"), "{compose}");
            assert!(compose.contains("@sha256:"));
            assert!(compose.contains("external: true"));
        }
        assert!(
            CODEX_LB_COMPOSE.contains(&format!("--host, 127.0.0.1, --port, \"{CODEX_LB_PORT}\""))
        );
        assert!(CLIPROXY_CONFIG.contains("host: \"127.0.0.1\""));
        assert!(CLIPROXY_CONFIG.contains(&format!("port: {CLIPROXY_PORT}")));
        assert!(CLIPROXY_CONFIG.contains("session-affinity: true"));
        assert!(CLAUDE_SETTINGS.contains(&format!("url=http://127.0.0.1:{CLIPROXY_PORT}\n")));
    }

    /// Run the real service script under bash with a stub `docker` that logs
    /// its arguments. `docker run` executes the real [`SEED_IN_VOLUME`]
    /// snippet it was handed, against `$HOME/volume` standing in for the
    /// mounted volume, so the in-container logic is tested rather than
    /// reimplemented in the stub.
    fn run_services(home: &Path) -> Output {
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        fs::write(
            &docker,
            r#"#!/bin/bash
printf '%s\n' "$*" >> "$HOME/docker.log"
case "$1 $2" in
  "volume inspect") [ -e "$HOME/volume-$3" ] ;;
  "volume create") touch "$HOME/volume-$3" ;;
  "run --rm") mkdir -p "$HOME/volume" && sh -c "${@: -4:1}" sh "$HOME/volume" "${@: -1}" ;;
  "compose -f") [ "$4" = config ] && echo image:pinned; exit 0 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        Command::new("bash")
            .args(["-c", &services_script()])
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    /// First run creates private keys, volumes and a config seeded with those
    /// keys; a rerun must keep the keys (they are baked into the config
    /// inside the volume) and must not reseed, while still starting both
    /// services. Keys never appear in any docker argv.
    #[test]
    fn services_generate_keys_once_and_never_reseed() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_services(dir.path());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let secrets_path = dir.path().join(".config/cliproxy/secrets.env");
        let secrets = fs::read_to_string(&secrets_path).unwrap();
        let key = |name: &str| {
            secrets
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .unwrap()
                .to_owned()
        };
        let (client, management) = (key("CLIPROXY_CLIENT_KEY"), key("CLIPROXY_MANAGEMENT_KEY"));
        assert!(
            client.starts_with("sk-cpa-") && client.len() == 55,
            "{client}"
        );
        assert_eq!(management.len(), 48);
        for path in [
            &secrets_path,
            &dir.path().join(".config/cliproxy/compose.yaml"),
            &dir.path().join(".config/codex-lb/compose.yaml"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let seeded_path = dir.path().join("volume/config.yaml");
        let seeded = fs::read_to_string(&seeded_path).unwrap();
        assert_eq!(
            fs::metadata(&seeded_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(dir.path().join("volume/auths").is_dir());
        assert!(seeded.contains(&format!("  - \"{client}\"")));
        assert!(seeded.contains(&format!("secret-key: \"{management}\"")));
        assert!(!seeded.contains('@'));
        assert!(dir.path().join("volume-codex-lb-data").exists());
        assert!(dir.path().join("volume-cliproxy-data").exists());

        let out = run_services(dir.path());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read_to_string(&secrets_path).unwrap(), secrets);
        assert_eq!(fs::read_to_string(&seeded_path).unwrap(), seeded);
        let log = fs::read_to_string(dir.path().join("docker.log")).unwrap();
        assert_eq!(log.matches("volume create").count(), 2);
        assert_eq!(log.matches("up -d").count(), 4);
        assert!(!log.contains(&client) && !log.contains(&management));
    }

    /// Losing secrets.env while the volume keeps its config must not produce
    /// fresh keys the proxy has never heard of: that would wire a client key
    /// into Claude that always gets a 401, and every rerun would repeat it.
    /// The run fails instead and leaves no new secrets.env behind.
    #[test]
    fn services_refuse_new_keys_when_the_volume_already_has_a_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_services(dir.path()).status.success());
        let secrets = dir.path().join(".config/cliproxy/secrets.env");
        let seeded = fs::read_to_string(dir.path().join("volume/config.yaml")).unwrap();
        fs::remove_file(&secrets).unwrap();
        let out = run_services(dir.path());
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("secrets.env is missing"));
        assert!(!secrets.exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("volume/config.yaml")).unwrap(),
            seeded
        );
    }

    /// The seed snippet is embedded in a single-quoted shell argument.
    #[test]
    fn seed_snippet_fits_in_single_quotes() {
        assert!(!SEED_IN_VOLUME.contains('\''));
    }

    /// Run the real Claude settings merge with bash and jq in an isolated
    /// home that already holds a CLIProxyAPI secrets file.
    fn run_claude(home: &Path) -> Output {
        let secrets = home.join(".config/cliproxy/secrets.env");
        fs::create_dir_all(secrets.parent().unwrap()).unwrap();
        fs::write(
            &secrets,
            "CLIPROXY_CLIENT_KEY=sk-cpa-test\nCLIPROXY_MANAGEMENT_KEY=m\n",
        )
        .unwrap();
        Command::new("bash")
            .args(["-c", CLAUDE_SETTINGS])
            .env("HOME", home)
            .output()
            .unwrap()
    }

    /// The merge must work on a fresh box where Claude never wrote settings,
    /// keep every existing setting including other env vars, lock the file
    /// down because it now holds a key, and leave a wired file untouched.
    #[test]
    fn claude_settings_merge_creates_preserves_and_is_idempotent() {
        for initial in [
            None,
            Some(r#"{"theme":"dark","env":{"FOO":"bar"},"permissions":{"allow":["x"]}}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join(".claude/settings.json");
            if let Some(initial) = initial {
                fs::create_dir_all(settings.parent().unwrap()).unwrap();
                fs::write(&settings, initial).unwrap();
                fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
            }
            let out = run_claude(dir.path());
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let bytes = fs::read(&settings).unwrap();
            let mut expected: serde_json::Value =
                serde_json::from_str(initial.unwrap_or("{}")).unwrap();
            expected["env"]["ANTHROPIC_BASE_URL"] = "http://127.0.0.1:8317".into();
            expected["env"]["ANTHROPIC_AUTH_TOKEN"] = "sk-cpa-test".into();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                expected
            );
            assert_eq!(
                fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(run_claude(dir.path()).status.success());
            assert_eq!(fs::read(&settings).unwrap(), bytes);
        }
    }

    /// Settings that something else manages, or that kd cannot merge into
    /// safely, must fail loudly and stay exactly as they were.
    #[test]
    fn claude_settings_merge_refuses_unsafe_files_without_replacing_them() {
        for initial in ["{", "[]", r#"{"env":"not an object"}"#] {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join(".claude/settings.json");
            fs::create_dir_all(settings.parent().unwrap()).unwrap();
            fs::write(&settings, initial).unwrap();
            assert!(!run_claude(dir.path()).status.success(), "{initial}");
            assert_eq!(fs::read_to_string(&settings).unwrap(), initial);
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("managed.json");
        fs::write(&target, "{}").unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        symlink(&target, dir.path().join(".claude/settings.json")).unwrap();
        assert!(!run_claude(dir.path()).status.success());
        assert_eq!(fs::read_to_string(target).unwrap(), "{}");
    }

    /// A missing config gets exactly the provider wiring; an existing one
    /// keeps its model, comments, unrelated tables and hand-added provider
    /// keys; a second pass is a no-op so reruns never rewrite the file.
    #[test]
    fn codex_config_merge_preserves_user_settings_and_is_idempotent() {
        let fresh = codex_config(None).unwrap();
        let parsed: toml::Table = toml::from_str(&fresh).unwrap();
        assert_eq!(parsed["model_provider"].as_str(), Some("codex-lb"));
        let provider = &parsed["model_providers"]["codex-lb"];
        assert_eq!(provider["name"].as_str(), Some("openai"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:2455/backend-api/codex")
        );
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
        assert_eq!(codex_config(Some(&fresh)).unwrap(), fresh);

        let existing = r#"# my settings
model = "gpt-6-sol"
model_provider = "openai"
model_reasoning_effort = "high"

[projects."/home/u/git/kd"]
trust_level = "trusted"

[model_providers.codex-lb]
name = "old"
http_headers = { "X-Extra" = "1" }
"#;
        let merged = codex_config(Some(existing)).unwrap();
        assert!(
            merged.starts_with(
                "# my settings\nmodel = \"gpt-6-sol\"\nmodel_provider = \"codex-lb\"\n"
            )
        );
        let parsed: toml::Table = toml::from_str(&merged).unwrap();
        assert_eq!(parsed["model_reasoning_effort"].as_str(), Some("high"));
        assert_eq!(
            parsed["projects"]["/home/u/git/kd"]["trust_level"].as_str(),
            Some("trusted")
        );
        let provider = &parsed["model_providers"]["codex-lb"];
        assert_eq!(provider["name"].as_str(), Some("openai"));
        assert_eq!(provider["http_headers"]["X-Extra"].as_str(), Some("1"));
        assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
    }

    /// Inline provider tables are legal Codex config and must be merged, not
    /// rejected; existing trailing comments and compact spacing must survive,
    /// which is also what keeps a wired file a byte-identical no-op.
    #[test]
    fn codex_config_merge_handles_inline_tables_and_keeps_value_decor() {
        let inline = "model_providers = { other = { name = \"x\" } }\n";
        let merged = codex_config(Some(inline)).unwrap();
        let parsed: toml::Table = toml::from_str(&merged).unwrap();
        assert_eq!(
            parsed["model_providers"]["other"]["name"].as_str(),
            Some("x")
        );
        assert_eq!(
            parsed["model_providers"]["codex-lb"]["name"].as_str(),
            Some("openai")
        );
        assert_eq!(codex_config(Some(&merged)).unwrap(), merged);

        let commented = "model_provider = \"openai\" # mine\n";
        assert!(
            codex_config(Some(commented))
                .unwrap()
                .starts_with("model_provider = \"codex-lb\" # mine\n")
        );

        let compact = "model_provider=\"codex-lb\"\n[model_providers.codex-lb]\nname=\"openai\"\nbase_url=\"http://127.0.0.1:2455/backend-api/codex\"\nwire_api=\"responses\"\nrequires_openai_auth=true\n";
        assert_eq!(codex_config(Some(compact)).unwrap(), compact);
    }

    /// Shapes kd cannot merge into without guessing are errors, not rewrites.
    #[test]
    fn codex_config_merge_rejects_unmergeable_shapes() {
        for bad in [
            "not = [valid",
            "model_providers = 1\n",
            "[model_providers]\ncodex-lb = 1\n",
        ] {
            assert!(codex_config(Some(bad)).is_err(), "{bad}");
        }
    }

    /// Controller-side tunnel scripts depend on these exact ports and on the
    /// bypass commands being spelled out, so pin the rendered text.
    #[test]
    fn login_help_names_the_fixed_ports_and_bypasses() {
        let help = login_help("user@box");
        assert!(help.contains(
            "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:2455:127.0.0.1:2455 -L 127.0.0.1:1455:127.0.0.1:1455 -L 127.0.0.1:8317:127.0.0.1:8317 -L 127.0.0.1:54545:127.0.0.1:54545 user@box"
        ));
        assert!(help.contains("http://127.0.0.1:2455"));
        assert!(help.contains("http://127.0.0.1:8317/management.html"));
        assert!(help.contains(CODEX_NATIVE_OVERRIDE));
        assert!(help.contains(CLAUDE_NATIVE_SETTINGS));
    }
}
