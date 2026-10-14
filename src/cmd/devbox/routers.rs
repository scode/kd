//! Local subscription routers: codex-lb in front of Codex, CLIProxyAPI in
//! front of Claude Code.
//!
//! Every bootstrap installs both as Docker Compose services and points plain
//! `codex` and `claude` at them. A fresh box ends with healthy routers that
//! hold no accounts; the user logs accounts in afterwards through an SSH
//! tunnel (see [`login_help`]). Until then plain `codex` and `claude` fail:
//! the routers are the only way either reaches a model, because bootstrap
//! leaves no native login on the target (see SPEC_impl.md "Secrets"). kd's
//! own Codex runs bypass codex-lb with [`CODEX_NATIVE_OVERRIDE`] and the
//! login lent for the run (see [`super::codex_login`]).
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
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};
use tracing::info;

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
/// kd's agent phases and probe use it with the login lent for the run, so
/// they work before any codex-lb account exists, including on reruns where
/// config.toml already points at codex-lb. Verified 2026-09-24: the request
/// reports `provider: openai` and never reaches codex-lb.
pub const CODEX_NATIVE_OVERRIDE: &str = r#"-c 'model_provider="openai"'"#;

/// Install or converge both services, then point Claude and Codex at them.
/// Runs after the user-space phase, when Docker, jq and dotfiles are in
/// place.
///
/// Docker must be usable by the user; otherwise bootstrap fails. There used
/// to be a degraded path that skipped the routers and left the clients on
/// their copied native logins, but no native login is left on the target
/// any more, so a box without routers would have no working Codex or Claude
/// at all. Every step is safe to rerun.
pub fn install(t: &Transport) -> anyhow::Result<()> {
    let docker = t.capture("docker info >/dev/null")?;
    if !docker.success() {
        bail!(
            "Docker is not usable by the user on {} ({}), and the routers need it. \
             If the system phase just added the user to the docker group and your SSH \
             config multiplexes connections (ControlMaster), close the master connection \
             and rerun; a box without systemd cannot run the routers at all.",
            t.destination,
            docker.stderr.trim()
        );
    }
    info!("installing codex-lb and CLIProxyAPI on {}", t.destination);
    t.run(&services_script())
        .context("router service setup failed")?;
    t.run(CLAUDE_SETTINGS)
        .context("pointing Claude at CLIProxyAPI failed")?;
    wire_codex(t).context("pointing Codex at codex-lb failed")?;
    Ok(())
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
/// truncated file. Keep the previous config at `config.toml.kd-backup` with
/// mode 0600; staging the backup also avoids following an old backup symlink.
/// This does not serialize against a Codex process writing
/// the same file (project trust); interactive sessions must be closed during
/// bootstrap, as for Claude.
const INSTALL_CODEX_CONFIG: &str = r#"umask 077
config="$HOME/.codex/config.toml"
if [ -e "$config" ]; then
  backup=$(mktemp "$HOME/.codex/config.toml.backup.XXXXXXXX") || exit 1
  trap 'rm -f -- "$backup"' EXIT
  cat -- "$config" > "$backup" && chmod 600 "$backup" || exit 1
  mv -T -- "$backup" "$config.kd-backup" || exit 1
fi
mv -T -- "$config.kd" "$config""#;

/// Read Codex's config over the transport, merge the codex-lb provider, and
/// write it back only if it changed. The edit happens on the controller
/// because the target has no TOML writer. Config may contain user-supplied
/// headers or credentials, so neither the original nor the result is logged.
/// A changed existing file is backed up before replacement.
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

/// Route Codex requests and live model discovery through codex-lb.
/// Unrelated settings survive; the old file-catalog override is removed so
/// a frozen snapshot cannot supersede the router's current model metadata.
///
/// The provider's display name stays `openai` on purpose: with any other name
/// Codex stopped using remote compaction through codex-lb (observed during
/// the manual setup). The key, `codex-lb`, is what identifies the route; in
/// the TUI, `/status` shows the localhost URL. `model` and reasoning effort
/// are deliberately not touched, since those are the user's choice.
///
/// `requires_openai_auth` is false, although codex-lb's README calls `true`
/// required. It is required only for the Codex desktop app, which does not
/// run on these Linux boxes. With `true`, Codex demands a local ChatGPT login
/// even though codex-lb ignores the token it sends, and a dead login drops
/// the TUI to the sign-in screen. See [`REQUIRES_OPENAI_AUTH_COMMENT`].
///
/// The static actor-authorization header lets Codex expose native image
/// tools in this login-skipping mode. It is a capability marker, not a proxy
/// credential. See [`ACTOR_AUTHORIZATION_COMMENT`] for the verified setup.
///
/// Skipping local login also skips ordinary ChatGPT model discovery. Enable
/// the API-key discovery path with an explicit catalog URL and a nonsecret
/// bearer marker. LB's client authentication is disabled by default and it
/// uses its own accounts upstream; the marker satisfies Codex's discovery
/// gate, not upstream authentication. No model or context limits are pinned.
/// Existing command authentication enables discovery without that marker.
/// Header-only Authorization credentials require an explicit migration to
/// provider authentication; refuse to overwrite their runtime behavior.
///
/// Values kd replaces keep their surrounding whitespace and trailing
/// comments, so an already-wired file is a byte-for-byte no-op and reruns do
/// not rewrite it. Keys inside `[model_providers.codex-lb]` that kd does not
/// set survive, and the actor marker is merged into existing headers. A
/// different nonempty actor value is an error rather than a silent overwrite.
/// An inline
/// `model_providers = { ... }` (legal, and accepted by Codex) is converted to
/// a standard table; that changes its layout, not its meaning. A
/// `model_providers` or `codex-lb` entry that is not a table is an error.
fn codex_config(existing: Option<&str>) -> anyhow::Result<String> {
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .context("~/.codex/config.toml is not valid TOML")?;
    set_keeping_decor(doc.as_table_mut(), "model_provider", "codex-lb".into());
    doc.remove("model_catalog_json");
    let features = standard_table(
        doc.as_table_mut(),
        "features",
        false,
        "features in ~/.codex/config.toml is not a table",
    )?;
    if features
        .get("api_key_model_discovery")
        .and_then(Item::as_bool)
        != Some(true)
    {
        set_keeping_decor(features, "api_key_model_discovery", true.into());
        prepend_key_comment(
            features,
            "api_key_model_discovery",
            "# Discover current model metadata through the router without a local ChatGPT login.\n",
        );
    }
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
    let base_url = format!("http://127.0.0.1:{CODEX_LB_PORT}/backend-api/codex");
    set_keeping_decor(provider, "base_url", base_url.clone().into());
    set_keeping_decor(
        provider,
        "model_catalog_url",
        format!("{base_url}/models").into(),
    );
    set_codex_discovery_auth(provider)?;
    set_keeping_decor(provider, "wire_api", "responses".into());
    set_requires_openai_auth_false(provider);
    set_actor_authorization(provider)?;
    Ok(doc.to_string())
}

/// Supply discovery authentication without replacing router credentials.
/// Command authentication already enables discovery, and Codex rejects it
/// when combined with a bearer or env_key. Environment-key authentication
/// takes precedence over a bearer and needs no marker. Header-only
/// authentication does not enable API-key discovery; refuse that migration
/// before writing rather than letting the marker override an Authorization
/// credential at runtime.
fn set_codex_discovery_auth(provider: &mut Table) -> anyhow::Result<()> {
    if provider.contains_key("auth") {
        anyhow::ensure!(
            !provider.contains_key("experimental_bearer_token")
                && !provider.contains_key("env_key"),
            "codex-lb auth cannot be combined with experimental_bearer_token or env_key"
        );
        return Ok(());
    }
    // Router auth settings survive bootstrap. An existing bearer may be a
    // real client credential, and it already satisfies the discovery gate.
    let bearer_missing = provider
        .get("experimental_bearer_token")
        .map(|item| {
            item.as_str()
                .context("codex-lb experimental_bearer_token must be a string")
                .map(|value| value.trim().is_empty())
        })
        .transpose()?
        .unwrap_or(true);
    if let Some(item) = provider.get("env_key") {
        let name = item.as_str().context("codex-lb env_key must be a string")?;
        anyhow::ensure!(
            !name.trim().is_empty(),
            "codex-lb env_key must not be blank"
        );
        // Resolve this credential in Codex on the target, not in bootstrap's
        // controller environment. A configured env_key already enables discovery.
        return Ok(());
    }
    if bearer_missing {
        for name in ["http_headers", "env_http_headers"] {
            if let Some(item) = provider.get(name) {
                let headers = item
                    .as_table_like()
                    .with_context(|| format!("model_providers.codex-lb.{name} is not a table"))?;
                anyhow::ensure!(
                    !headers
                        .iter()
                        .any(|(key, _)| key.eq_ignore_ascii_case("authorization")),
                    "codex-lb Authorization header would be overridden by the discovery marker; configure provider authentication with experimental_bearer_token, env_key, or auth before rerunning bootstrap"
                );
            }
        }
        set_keeping_decor(provider, "experimental_bearer_token", "codex-lb".into());
        prepend_key_comment(
            provider,
            "experimental_bearer_token",
            "# Nonsecret discovery marker: LB ignores this bearer with client authentication disabled.\n",
        );
    }
    Ok(())
}

/// Codex checks this static header case-insensitively when deciding whether
/// a provider that skips local login can expose native image tools.
const ACTOR_AUTHORIZATION_HEADER: &str = "x-openai-actor-authorization";

/// Explain the marker in generated config so it is not removed as an unused
/// credential or replaced with a secret. The snippet records both client
/// eligibility gates and a fresh-session generation/edit probe.
const ACTOR_AUTHORIZATION_COMMENT: &str = "\
# Native image tools need this static capability marker when local login is skipped.
# The codex-lb value is not a secret and does not authenticate proxy requests.
# https://snippets.scode.org/s/codex-lb-native-imagegen-host-setup/
";

/// Merge the actor marker without discarding headers or their table layout.
///
/// Codex compares header names without regard to case and trims values for
/// eligibility. Keep an existing matching entry byte-for-byte, repair empty
/// strings, and reject other values without including them in the error.
/// Reject duplicate case variants too: they could send ambiguous HTTP headers
/// even though the TOML keys are distinct. Environment headers do not satisfy
/// Codex's eligibility predicate, so this always edits static `http_headers`.
fn set_actor_authorization(provider: &mut Table) -> anyhow::Result<()> {
    let item = provider
        .entry("http_headers")
        .or_insert_with(|| Item::Value(Value::InlineTable(InlineTable::new())));
    let headers = item
        .as_table_like_mut()
        .context("model_providers.codex-lb.http_headers is not a table")?;
    let matching: Vec<String> = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(ACTOR_AUTHORIZATION_HEADER))
        .map(|(key, _)| key.to_owned())
        .collect();
    if matching.len() > 1 {
        bail!(
            "codex-lb.http_headers has duplicate actor-authorization header names; resolve them before rerunning bootstrap"
        );
    }
    let key = matching
        .first()
        .map(String::as_str)
        .unwrap_or(ACTOR_AUTHORIZATION_HEADER);
    if let Some(old) = headers.get(key) {
        let value = old
            .as_str()
            .context("codex-lb actor-authorization header must be a string")?;
        match value.trim() {
            "codex-lb" => return Ok(()),
            "" => {}
            _ => bail!(
                "codex-lb has a different actor-authorization value; resolve it before rerunning bootstrap"
            ),
        }
    }
    set_keeping_decor(headers, key, "codex-lb".into());
    // Dotted tables serialize their leaf entries, not the parent key's
    // decoration. Put the explanation on the marker itself for tables;
    // inline maps need it outside the braces to keep their syntax valid.
    if let Some(headers) = provider
        .get_mut("http_headers")
        .and_then(Item::as_table_mut)
    {
        prepend_key_comment(headers, key, ACTOR_AUTHORIZATION_COMMENT);
    } else {
        prepend_key_comment(provider, "http_headers", ACTOR_AUTHORIZATION_COMMENT);
    }
    Ok(())
}

/// Add a signpost to an existing key without losing the user's leading
/// comments. Callers choose a key emitted by the table's representation:
/// a dotted table does not serialize decoration on its parent key.
fn prepend_key_comment(table: &mut Table, name: &str, comment: &str) {
    if let Some(mut key) = table.key_mut(name) {
        let previous = key
            .leaf_decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_owned();
        key.leaf_decor_mut()
            .set_prefix(format!("{previous}{comment}"));
    }
}

/// Comment kd puts above `requires_openai_auth` whenever it writes that key,
/// so a person reading the file does not "fix" it back to codex-lb's
/// documented `true`.
const REQUIRES_OPENAI_AUTH_COMMENT: &str = "\
# false on purpose (kd devbox bootstrap): codex-lb ignores Codex's own token,
# so no local ChatGPT login is needed; true makes a dead login block startup.
# Keep ~/.codex/auth.json absent too: Codex still loads one that exists and
# keeps trying to refresh it.
";

/// Set `requires_openai_auth = false`, leaving an already-false key exactly
/// as it is. Any other value is replaced together with its decor: the
/// trailing comment on a hand-written `true` is typically codex-lb's
/// "required for codex app", which would contradict the new value.
fn set_requires_openai_auth_false(provider: &mut Table) {
    const KEY: &str = "requires_openai_auth";
    if provider.get(KEY).and_then(Item::as_bool) == Some(false) {
        return;
    }
    provider.remove(KEY);
    provider.insert(KEY, Item::Value(false.into()));
    if let Some(mut key) = provider.key_mut(KEY) {
        key.leaf_decor_mut()
            .set_prefix(REQUIRES_OPENAI_AUTH_COMMENT);
    }
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
fn set_keeping_decor(table: &mut dyn TableLike, key: &str, new: Value) {
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
accounts in, plain `codex` and `claude` on the box fail: the box has no native
login for either, on purpose.

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
        assert_eq!(
            parsed["features"]["api_key_model_discovery"].as_bool(),
            Some(true)
        );
        assert!(!parsed.contains_key("model_catalog_json"));
        assert!(!parsed.contains_key("model_context_window"));
        let provider = &parsed["model_providers"]["codex-lb"];
        assert_eq!(provider["name"].as_str(), Some("openai"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:2455/backend-api/codex")
        );
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            provider["model_catalog_url"].as_str(),
            Some("http://127.0.0.1:2455/backend-api/codex/models")
        );
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some("codex-lb")
        );
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
        assert_eq!(
            provider["http_headers"][ACTOR_AUTHORIZATION_HEADER].as_str(),
            Some("codex-lb")
        );
        assert!(fresh.contains(REQUIRES_OPENAI_AUTH_COMMENT));
        assert!(fresh.contains(ACTOR_AUTHORIZATION_COMMENT));
        assert_eq!(codex_config(Some(&fresh)).unwrap(), fresh);

        let existing = r#"# my settings
model = "gpt-6-sol"
model_provider = "openai"
model_reasoning_effort = "high"
model_catalog_json = "/old-catalog.json"

[features]
api_key_model_discovery = false
other_feature = true

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
        assert!(!parsed.contains_key("model_catalog_json"));
        assert_eq!(
            parsed["features"]["api_key_model_discovery"].as_bool(),
            Some(true)
        );
        assert_eq!(parsed["features"]["other_feature"].as_bool(), Some(true));
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

        let compact = "model_provider=\"codex-lb\"\n[features]\napi_key_model_discovery=true\n[model_providers.codex-lb]\nname=\"openai\"\nbase_url=\"http://127.0.0.1:2455/backend-api/codex\"\nmodel_catalog_url=\"http://127.0.0.1:2455/backend-api/codex/models\"\nexperimental_bearer_token=\"codex-lb\"\nwire_api=\"responses\"\nrequires_openai_auth=false\nhttp_headers={x-openai-actor-authorization=\"codex-lb\"}\n";
        assert_eq!(codex_config(Some(compact)).unwrap(), compact);
    }

    /// Feature tables may be inline or implicit parents of nested settings.
    /// Enabling discovery must preserve those settings and converge on rerun;
    /// replacing the entire table would discard the user's other capabilities.
    #[test]
    fn codex_discovery_preserves_inline_and_nested_feature_settings() {
        for existing in [
            "features = { api_key_model_discovery = false, other_feature = true }\n",
            "[features.multi_agent_v2]\nmax_concurrent_threads_per_session = 20\n",
        ] {
            let merged = codex_config(Some(existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            assert_eq!(
                parsed["features"]["api_key_model_discovery"].as_bool(),
                Some(true)
            );
            if existing.starts_with("features =") {
                assert_eq!(parsed["features"]["other_feature"].as_bool(), Some(true));
            } else {
                assert_eq!(
                    parsed["features"]["multi_agent_v2"]["max_concurrent_threads_per_session"]
                        .as_integer(),
                    Some(20)
                );
            }
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
    }

    /// Router settings and accounts survive reruns, so a user may already
    /// have enabled client authentication. Its bearer must not become the
    /// dummy marker: either string satisfies Codex's discovery gate, but
    /// only the original credential can authenticate to that router.
    #[test]
    fn codex_discovery_preserves_existing_bearers_and_repairs_blank_values() {
        for token in ["existing-client-key", "", "  "] {
            let existing = format!(
                "[model_providers.codex-lb]\nexperimental_bearer_token = \"{token}\" # kept\n"
            );
            let merged = codex_config(Some(&existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            let expected = if token.trim().is_empty() {
                "codex-lb"
            } else {
                token
            };
            assert_eq!(
                parsed["model_providers"]["codex-lb"]["experimental_bearer_token"].as_str(),
                Some(expected)
            );
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
        let error = codex_config(Some(
            "[model_providers.codex-lb]\nexperimental_bearer_token = 1\n",
        ))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("experimental_bearer_token must be a string")
        );
    }

    /// Codex's command-auth path discovers models independently of the
    /// API-key gate. A marker would make an otherwise valid provider unloadable;
    /// preserve the command and reject existing conflicting auth fields.
    #[test]
    fn codex_discovery_preserves_command_auth_without_a_bearer() {
        let existing =
            "[model_providers.codex-lb.auth]\ncommand = \"router-token\"\nargs = [\"--client\"]\n";
        let merged = codex_config(Some(existing)).unwrap();
        let parsed: toml::Table = toml::from_str(&merged).unwrap();
        let provider = parsed["model_providers"]["codex-lb"].as_table().unwrap();
        assert!(!provider.contains_key("experimental_bearer_token"));
        assert_eq!(provider["auth"]["command"].as_str(), Some("router-token"));
        assert_eq!(provider["auth"]["args"][0].as_str(), Some("--client"));
        assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        for conflicting in [
            "experimental_bearer_token = \"\"",
            "env_key = \"ROUTER_KEY\"",
        ] {
            let config = format!("[model_providers.codex-lb]\n{conflicting}\n{existing}");
            let error = codex_config(Some(&config)).unwrap_err();
            assert!(error.to_string().contains("auth cannot be combined"));
        }
    }

    /// Static and environment-backed Authorization headers may be the only
    /// credentials for a retained router. Refuse to inject a marker over them,
    /// including case variants and both TOML table forms, without leaking values.
    #[test]
    fn codex_discovery_refuses_to_override_header_authentication() {
        for headers in [
            "http_headers = { Authorization = \"Bearer private-client-key\" }\n",
            "[model_providers.codex-lb.http_headers]\naUtHoRiZaTiOn = \"Bearer private-client-key\"\n",
            "env_http_headers = { AUTHORIZATION = \"PRIVATE_ROUTER_KEY\" }\n",
            "[model_providers.codex-lb.env_http_headers]\nauthorization = \"PRIVATE_ROUTER_KEY\"\n",
        ] {
            for bearer in ["", "experimental_bearer_token = \" \"\n"] {
                let config = format!("[model_providers.codex-lb]\n{bearer}{headers}");
                let error = codex_config(Some(&config)).unwrap_err().to_string();
                assert!(error.contains("Authorization header would be overridden"));
                assert!(!error.contains("private-client-key"));
                assert!(!error.contains("PRIVATE_ROUTER_KEY"));
            }
        }
    }

    /// An environment key already satisfies the discovery gate and takes
    /// precedence over Authorization headers. Preserve that working route
    /// without adding a marker or inspecting the controller's environment.
    #[test]
    fn codex_discovery_preserves_environment_key_authentication() {
        for headers in [
            "",
            "http_headers = { Authorization = \"Bearer private-client-key\" }\n",
            "env_http_headers = { Authorization = \"OTHER_ROUTER_KEY\" }\n",
        ] {
            let existing =
                format!("[model_providers.codex-lb]\nenv_key = \"ROUTER_KEY\" # kept\n{headers}");
            let merged = codex_config(Some(&existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            let provider = parsed["model_providers"]["codex-lb"].as_table().unwrap();
            assert_eq!(provider["env_key"].as_str(), Some("ROUTER_KEY"));
            assert!(!provider.contains_key("experimental_bearer_token"));
            for name in ["http_headers", "env_http_headers"] {
                let original: toml::Table = toml::from_str(&existing).unwrap();
                if let Some(value) = original["model_providers"]["codex-lb"].get(name) {
                    assert_eq!(provider[name]["Authorization"], value["Authorization"]);
                }
            }
            assert!(merged.contains("env_key = \"ROUTER_KEY\" # kept"));
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
        for (invalid, expected) in [
            ("1", "codex-lb env_key must be a string"),
            ("\"\"", "codex-lb env_key must not be blank"),
            ("\" \"", "codex-lb env_key must not be blank"),
        ] {
            let config = format!("[model_providers.codex-lb]\nenv_key = {invalid}\n");
            let error = codex_config(Some(&config)).unwrap_err();
            // Exact diagnostics establish which guard refused the value and
            // prevent the configuration value from being added to the error.
            assert_eq!(error.to_string(), expected);
        }
    }

    /// Existing headers can contain credentials and use either TOML table
    /// form. Adding image eligibility must preserve their values, comments
    /// and layout; reruns must not accumulate comments or duplicate keys.
    #[test]
    fn actor_marker_merges_into_inline_and_standard_header_tables() {
        for headers in [
            "http_headers = { X-Extra = \"kept\" } # headers\n",
            "[model_providers.codex-lb.http_headers]\n# user header\nX-Extra = \"kept\" # value\n",
        ] {
            let existing = format!("[model_providers.codex-lb]\n{headers}");
            let merged = codex_config(Some(&existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            let map = &parsed["model_providers"]["codex-lb"]["http_headers"];
            assert_eq!(map["X-Extra"].as_str(), Some("kept"));
            assert_eq!(map[ACTOR_AUTHORIZATION_HEADER].as_str(), Some("codex-lb"));
            assert!(merged.contains(ACTOR_AUTHORIZATION_COMMENT));
            if headers.starts_with('[') {
                assert!(merged.contains("# user header\nX-Extra = \"kept\" # value"));
            } else {
                assert!(merged.contains("http_headers = {"));
                assert!(merged.contains("# headers"));
            }
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
    }

    /// Dotted keys are legal TOML too. Their implicit header table omits
    /// parent key decoration, so the explanation must survive on a leaf.
    /// Parsing and idempotency alone miss a lost marker comment.
    #[test]
    fn actor_marker_preserves_dotted_header_definitions() {
        for existing in [
            "[model_providers.codex-lb]\nhttp_headers.X-Extra = \"kept\"\n",
            "[model_providers.codex-lb]\nhttp_headers . X-Extra = \"kept\"\n",
            "[model_providers.codex-lb]\n\"http_headers\".\"X-Extra\" = \"kept\"\n",
            "model_providers.codex-lb.http_headers = { X-Extra = \"kept\" }\n",
        ] {
            let merged = codex_config(Some(existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            let headers = &parsed["model_providers"]["codex-lb"]["http_headers"];
            assert!(merged.contains(ACTOR_AUTHORIZATION_COMMENT));
            assert_eq!(headers["X-Extra"].as_str(), Some("kept"));
            assert_eq!(
                headers[ACTOR_AUTHORIZATION_HEADER].as_str(),
                Some("codex-lb")
            );
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
    }

    /// Codex's actor predicate ignores header-name case and trims values.
    /// Matching markers must retain user spelling and comments; empty ones
    /// need repair without adding a second differently cased header.
    #[test]
    fn actor_marker_preserves_casing_and_repairs_empty_values() {
        for value in ["codex-lb", " codex-lb ", "", "  "] {
            let existing = format!(
                "[model_providers.codex-lb.http_headers]\nX-OpenAI-Actor-Authorization = \"{value}\" # mine\n"
            );
            let merged = codex_config(Some(&existing)).unwrap();
            let parsed: toml::Table = toml::from_str(&merged).unwrap();
            let map = parsed["model_providers"]["codex-lb"]["http_headers"]
                .as_table()
                .unwrap();
            assert_eq!(map.len(), 1);
            let expected = if value.trim().is_empty() {
                "codex-lb"
            } else {
                value
            };
            assert_eq!(map["X-OpenAI-Actor-Authorization"].as_str(), Some(expected));
            assert!(merged.contains("# mine"));
            if !value.trim().is_empty() {
                assert!(merged.contains(&existing));
            }
            assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
        }
    }

    /// Conflicting actor values can be user credentials. Refuse ambiguous
    /// header maps and invalid types without copying their contents into
    /// diagnostics; a provisioning rerun must not silently replace them.
    #[test]
    fn actor_marker_rejects_conflicts_duplicates_and_invalid_shapes() {
        for headers in [
            "http_headers = 1",
            "http_headers = { x-openai-actor-authorization = 1 }",
            "http_headers = { x-openai-actor-authorization = \"private-value\" }",
            "http_headers = { x-openai-actor-authorization = \"codex-lb\", X-OpenAI-Actor-Authorization = \"codex-lb\" }",
        ] {
            let existing = format!("[model_providers.codex-lb]\n{headers}\n");
            let error = codex_config(Some(&existing)).unwrap_err().to_string();
            assert!(!error.contains("private-value"));
        }
    }

    /// Execute the exact installer against a fixture home. Keeping HOME in
    /// the child environment avoids racing other tests' process settings.
    fn install_codex_config(home: &std::path::Path) -> std::process::Output {
        std::process::Command::new("bash")
            .args(["-c", INSTALL_CODEX_CONFIG])
            .env("HOME", home)
            .output()
            .unwrap()
    }

    /// A converged config replaces the live file only after preserving its
    /// previous bytes privately. Fresh installs need no backup, and a failed
    /// backup must leave the original file in place for recovery.
    #[test]
    fn codex_config_install_backs_up_existing_files_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join(".codex");
        fs::create_dir(&codex).unwrap();
        let config = codex.join("config.toml");
        let staged = codex.join("config.toml.kd");
        let backup = codex.join("config.toml.kd-backup");
        fs::write(&staged, "first").unwrap();
        assert!(install_codex_config(dir.path()).status.success());
        assert_eq!(fs::read_to_string(&config).unwrap(), "first");
        assert!(!backup.exists());
        fs::write(&staged, "second").unwrap();
        assert!(install_codex_config(dir.path()).status.success());
        assert_eq!(fs::read_to_string(&config).unwrap(), "second");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "first");
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::remove_file(&backup).unwrap();
        fs::create_dir(&backup).unwrap();
        // A nonempty destination directory forces the backup rename to
        // fail with -T, instead of moving the backup inside that directory.
        fs::write(backup.join("keep"), "unrelated").unwrap();
        fs::write(&staged, "third").unwrap();
        assert!(!install_codex_config(dir.path()).status.success());
        assert_eq!(fs::read_to_string(&config).unwrap(), "second");
    }

    /// Boxes wired before logins stopped being copied carry
    /// `requires_openai_auth = true`, often with codex-lb's "required for
    /// codex app" comment. A rerun must flip it, drop that contradicting
    /// comment, explain the new value, and then leave it alone.
    #[test]
    fn codex_config_merge_turns_off_requires_openai_auth() {
        let old = "[model_providers.codex-lb]\nname = \"openai\"\nrequires_openai_auth = true # required for codex app\n";
        let merged = codex_config(Some(old)).unwrap();
        let parsed: toml::Table = toml::from_str(&merged).unwrap();
        assert_eq!(
            parsed["model_providers"]["codex-lb"]["requires_openai_auth"].as_bool(),
            Some(false)
        );
        assert!(merged.contains(&format!(
            "{REQUIRES_OPENAI_AUTH_COMMENT}requires_openai_auth = false\n"
        )));
        assert!(!merged.contains("required for codex app"));
        assert_eq!(codex_config(Some(&merged)).unwrap(), merged);
    }

    /// Shapes kd cannot merge into without guessing are errors, not rewrites.
    #[test]
    fn codex_config_merge_rejects_unmergeable_shapes() {
        for bad in [
            "not = [valid",
            "model_providers = 1\n",
            "[model_providers]\ncodex-lb = 1\n",
            "features = 1\n",
        ] {
            assert!(codex_config(Some(bad)).is_err(), "{bad}");
        }
    }

    /// Controller-side tunnel scripts depend on these exact ports, and the
    /// login pages are the only way a fresh box gets working CLIs, so pin the
    /// rendered text. The native bypasses must not come back: with no login
    /// left on the box they would only fail.
    #[test]
    fn login_help_names_the_fixed_ports_and_login_pages() {
        let help = login_help("user@box");
        assert!(help.contains(
            "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:2455:127.0.0.1:2455 -L 127.0.0.1:1455:127.0.0.1:1455 -L 127.0.0.1:8317:127.0.0.1:8317 -L 127.0.0.1:54545:127.0.0.1:54545 user@box"
        ));
        assert!(help.contains("http://127.0.0.1:2455"));
        assert!(help.contains("http://127.0.0.1:8317/management.html"));
        // No native login is left on the box, so a bypass would only fail.
        assert!(!help.contains(CODEX_NATIVE_OVERRIDE));
    }
}
