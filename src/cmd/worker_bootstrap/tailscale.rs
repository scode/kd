//! Everything Tailscale-specific about creating a worker: minting its auth
//! key, validating that key, reading the operator's own login, and rendering
//! the policy rule `create` prints.
//!
//! Minting is native Tailscale REST calls, no external tool involved, in two
//! hops:
//!
//! 1. Exchange an OAuth client ID/secret for a short-lived API access token
//!    (`POST /oauth/token`).
//! 2. Use that token to mint a one-use, ephemeral, preauthorized auth key
//!    carrying the caller's tag (`POST /tailnet/-/keys`).
//!
//! The tag is a parameter because each fleet has its own (`tag:ubicloud`
//! for ubiworkers, `tag:boat` for boatworkers). Separate tags keep the
//! fleets apart in tailnet policy, and let an OAuth client be limited to
//! minting keys for one fleet.
//!
//! The minted key's secret value (the `key` field, format `tskey-auth-...`)
//! is only ever present in the response to step 2 — Tailscale does not let
//! you retrieve it again later, so the caller must capture it immediately
//! and is responsible for never logging it. Both HTTP calls take their
//! credentials as parameters rather than reading the environment, which is
//! what makes this module unit-testable without env mutation and keeps the
//! secret's flow through the program visible in function signatures.

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::warn;
use xshell::{Shell, cmd};

// ── Local node identity ──────────────────────────────────────────────────
// Unlike everything below, this talks to the *local* tailscaled through the
// `tailscale` CLI, not to the REST API. It exists so `create`'s printed SSH
// policy rule can name the operator's own login instead of a tailnet-wide
// selector.

/// The Tailscale login (e.g. `alice@github`) this machine is signed in as,
/// or `None` if it can't be determined.
///
/// Used only to render a policy-rule *suggestion*, so every failure mode —
/// no `tailscale` on `PATH`, tailscaled not running, unparseable output, a
/// machine that is itself a tagged node (whose "user" is the synthetic
/// `tagged-devices` account and would make a nonsensical `src`) — is
/// downgraded to a warning and `None`, never an error: a missing
/// suggestion must not block creating a worker. The caller substitutes an
/// unmistakable placeholder in that case.
///
/// Credentials are stripped from the child for the same reason as every
/// other subprocess kd spawns: `tailscale status` needs none of them.
pub fn local_login(sh: &Shell) -> Option<String> {
    let output = match cmd!(sh, "tailscale status --json")
        .quiet()
        .ignore_stderr()
        .env_remove("UBI_TOKEN")
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        .read()
    {
        Ok(output) => output,
        Err(err) => {
            warn!(
                "could not determine local tailscale login ({err}); the printed policy rule will use a placeholder"
            );
            return None;
        }
    };
    match parse_status_login(&output) {
        Ok(login) => Some(login),
        Err(err) => {
            warn!(
                "could not determine local tailscale login ({err}); the printed policy rule will use a placeholder"
            );
            None
        }
    }
}

/// Pull the signed-in user's login out of `tailscale status --json`: the
/// node's own `Self.UserID` looked up in the top-level `User` map's
/// `LoginName`. Rejects the synthetic `tagged-devices` login, which is what
/// a tagged node reports and is not a usable policy `src`. Pure, so the
/// JSON shape is pinned by tests without a running tailscaled.
fn parse_status_login(json: &str) -> anyhow::Result<String> {
    let status: serde_json::Value =
        serde_json::from_str(json).context("tailscale status output is not JSON")?;
    let user_id = status
        .pointer("/Self/UserID")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("no Self.UserID in tailscale status"))?;
    let login = status
        .pointer(&format!("/User/{user_id}/LoginName"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("no User[{user_id}].LoginName in tailscale status"))?;
    if login.is_empty() || login == "tagged-devices" {
        bail!("local node is a tagged device, not signed in as a user");
    }
    Ok(login.to_string())
}

const TOKEN_URL: &str = "https://api.tailscale.com/api/v2/oauth/token";
const KEYS_URL: &str = "https://api.tailscale.com/api/v2/tailnet/-/keys";

/// Applied to both Tailscale API calls. ureq 3 sets no timeout of any kind
/// by default (connect, read, or overall), so without this a stalled DNS
/// lookup, TCP connect, or slow/stuck read would hang `create` forever
/// instead of failing with a normal, retryable error.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared [`ureq::Agent`] both Tailscale calls run through, so
/// [`HTTP_TIMEOUT`] is configured in exactly one place rather than per-call.
/// Constructed once by each worker command's `create::run` and threaded
/// down as a parameter — consistent with every other credential/config
/// value in this module, which arrives as a parameter rather than getting
/// read or constructed ambiently deep in the call stack.
pub fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into()
}

/// Response body of the OAuth client-credentials token exchange. Tailscale
/// returns additional fields (`token_type`, `expires_in`, ...); we only
/// need the bearer token itself.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Mirrors the Tailscale API's `capabilities.devices.create` shape exactly
/// — the field names and nesting here are load-bearing, not stylistic.
#[derive(Serialize)]
struct DeviceCreateCapabilities {
    reusable: bool,
    ephemeral: bool,
    preauthorized: bool,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct DeviceCapabilities {
    create: DeviceCreateCapabilities,
}

#[derive(Serialize)]
struct Capabilities {
    devices: DeviceCapabilities,
}

/// Request body for minting an auth key.
///
/// `expiry_seconds` bounds how long the minted key stays usable: it's
/// consumed once, minutes after minting, by `tailscale up` on first boot
/// (see [`mint_auth_key`]). If the provider's create call (`ubi vm
/// create`, `boat new`) fails after the key was minted but before the
/// machine ever boots, the key would otherwise sit valid
/// (Tailscale's default key lifetime is much longer) with no code path in
/// kd that revokes it. A 1-hour expiry bounds that exposure without kd
/// having to implement revoke-on-failure at all.
#[derive(Serialize)]
struct CreateKeyPayload {
    capabilities: Capabilities,
    #[serde(rename = "expirySeconds")]
    expiry_seconds: u64,
    description: String,
}

/// [`CreateKeyPayload::expiry_seconds`]'s value — see that field's docs.
const KEY_EXPIRY_SECONDS: u64 = 3600;

/// Response body of the auth-key creation call. `key` holds the secret
/// value and, per the module docs, is only ever returned here.
#[derive(Deserialize)]
struct CreateKeyResponse {
    key: String,
}

/// Exchange an OAuth client ID/secret pair for a short-lived API access
/// token. Takes the credentials as parameters (see module docs) rather than
/// reading `TS_API_CLIENT_ID`/`TS_API_CLIENT_SECRET` itself. `agent` is the
/// shared, timeout-configured client built by [`build_agent`] — passed in
/// rather than constructed here so both Tailscale calls run through the
/// same one.
///
/// `tag` plays no part in the exchange itself. It is the tag the caller is
/// about to mint a key for, and it is only used to name the tag the OAuth
/// client must own in the error hint for a 401/403.
pub fn exchange_access_token(
    agent: &ureq::Agent,
    client_id: &str,
    client_secret: &str,
    tag: &str,
) -> anyhow::Result<String> {
    let mut response = agent
        .post(TOKEN_URL)
        .send_form([
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .map_err(|err| tailscale_http_error("oauth token exchange", err, tag))?;

    let body = response
        .body_mut()
        .read_to_string()
        .context("failed to read tailscale token response body")?;
    parse_token_response(&body)
}

/// Pure parse step for the token exchange response, split out from
/// [`exchange_access_token`] so it's testable with a literal JSON string
/// and no network call.
///
/// Rejects an empty `access_token`: Tailscale's schema doesn't distinguish
/// "empty" from "absent" for us, and an empty bearer token would otherwise
/// sail through here only to fail confusingly on the next HTTP call.
fn parse_token_response(body: &str) -> anyhow::Result<String> {
    let parsed: TokenResponse =
        serde_json::from_str(body).context("failed to parse tailscale token response")?;
    if parsed.access_token.is_empty() {
        bail!("tailscale token response had an empty access_token");
    }
    Ok(parsed.access_token)
}

/// Mint a one-use tailscale auth key: not reusable, ephemeral, preauthorized
/// (no manual approval step on the tailnet admin console), tagged `tag`,
/// and bounded to [`KEY_EXPIRY_SECONDS`] (see [`CreateKeyPayload`]).
/// `description` is the key's human-readable label, purely for auditing in
/// the tailscale admin console (callers use `kd <group> <worker name>`).
/// `agent` is the same shared client passed to [`exchange_access_token`].
pub fn mint_auth_key(
    agent: &ureq::Agent,
    access_token: &str,
    tag: &str,
    description: &str,
) -> anyhow::Result<String> {
    let payload = CreateKeyPayload {
        capabilities: Capabilities {
            devices: DeviceCapabilities {
                create: DeviceCreateCapabilities {
                    reusable: false,
                    ephemeral: true,
                    preauthorized: true,
                    tags: vec![tag.to_string()],
                },
            },
        },
        expiry_seconds: KEY_EXPIRY_SECONDS,
        description: description.to_string(),
    };

    let mut response = agent
        .post(KEYS_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .send_json(&payload)
        .map_err(|err| tailscale_http_error("auth key creation", err, tag))?;

    let body = response
        .body_mut()
        .read_to_string()
        .context("failed to read tailscale auth key response body")?;
    parse_create_key_response(&body)
}

/// Pure parse-and-validate step for the auth-key creation response, split
/// out from [`mint_auth_key`] so it's testable with literal JSON and no
/// network call.
///
/// Runs the full [`is_valid_auth_key`] check (prefix plus whole-string
/// charset), not just the `tskey-auth-` prefix: this is the first point a
/// minted key value exists in the program, and
/// [`render_init_script`](super::init_script::render_init_script)
/// downstream trusts a key that passes here to be safe to interpolate
/// unescaped inside a single-quoted shell argument. An invalid key must fail
/// here, not confusingly later, deep inside the init script on the VM.
fn parse_create_key_response(body: &str) -> anyhow::Result<String> {
    let parsed: CreateKeyResponse =
        serde_json::from_str(body).context("failed to parse tailscale auth key response")?;
    if !is_valid_auth_key(&parsed.key) {
        bail!(
            "tailscale returned an auth key that failed validation (expected a 'tskey-auth-' \
             prefix followed by a non-empty [A-Za-z0-9-] remainder)"
        );
    }
    Ok(parsed.key)
}

/// Map a `ureq` error to an `anyhow::Error` carrying the HTTP status (if
/// any) and a hint scoped to what that status actually implies. Deliberately
/// never includes request/response bodies or the credentials themselves —
/// only the status code — so this can't accidentally leak the client secret
/// or a minted auth key into logs or error output.
///
/// The hint is status-class-aware because a single "check your credentials"
/// message stopped being true for every status: 401/403 really do mean a
/// credentials/scope/tag problem, but suggesting that for a 429 or a 5xx
/// sends the reader chasing the wrong fix. Anything outside these classes
/// gets the bare status with no speculative hint at all, rather than a
/// guess that might be wrong.
fn tailscale_http_error(context: &str, err: ureq::Error, tag: &str) -> anyhow::Error {
    match err {
        ureq::Error::StatusCode(code @ (401 | 403)) => anyhow!(
            "{context} failed: tailscale API returned HTTP {code}; check \
             TS_API_CLIENT_ID/TS_API_CLIENT_SECRET and that the OAuth client has the auth_keys \
             scope and owns {tag}"
        ),
        ureq::Error::StatusCode(429) => {
            anyhow!("{context} failed: tailscale API returned HTTP 429 (rate limited); retry later")
        }
        ureq::Error::StatusCode(code) if (500..600).contains(&code) => anyhow!(
            "{context} failed: tailscale API returned HTTP {code} (server error); retry later"
        ),
        ureq::Error::StatusCode(code) => {
            anyhow!("{context} failed: tailscale API returned HTTP {code}")
        }
        other => anyhow!("{context} failed: {other}"),
    }
}

// ── Auth-key validation ──────────────────────────────────────────────────
// Shared by `parse_create_key_response` (the first place a minted key value
// exists at all) and `init_script::render_init_script` (which interpolates
// it unescaped into a single-quoted shell argument). One predicate, checked
// in both places, so the two call sites can't drift.

/// Full charset/shape validation for a tailscale auth key: the
/// `tskey-auth-` prefix, a non-empty remainder, and — over the *whole*
/// string, prefix included — nothing outside `[A-Za-z0-9-]`.
///
/// This must stay a full-string check, not just a prefix check: the reason
/// this predicate exists at all is to justify `render_init_script`
/// interpolating the key unescaped inside a single-quoted shell argument,
/// and a prefix-only check would let a `'` anywhere after the prefix slip
/// through into that interpolation.
pub(crate) fn is_valid_auth_key(key: &str) -> bool {
    const PREFIX: &str = "tskey-auth-";
    key.strip_prefix(PREFIX)
        .is_some_and(|rest| !rest.is_empty())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

// ── Tailnet policy rule ──────────────────────────────────────────────────
// `create` prints the one prerequisite kd can neither set up nor detect: an
// `ssh` rule in the tailnet policy that lets the operator log in to the new
// worker. Both worker commands print it, each for its own tag and Unix user.

/// Where the tailnet policy file is edited, for the create summary's
/// "go add the ssh rule" pointer. A console URL is the most precise
/// instruction kd can give, but it is also the kind of detail Tailscale can
/// move without notice — hence the "may be stale" wording next to its use.
pub(crate) const POLICY_EDITOR_URL: &str = "https://login.tailscale.com/admin/acls/file";

/// Placeholder `src` for the printed policy rule when the operator's own
/// Tailscale login couldn't be resolved (see [`local_login`]).
/// Deliberately unmistakable and unpasteable — the alternative of
/// defaulting to `autogroup:member` would quietly grant every tailnet
/// member a shell as the operator on every worker.
const LOGIN_PLACEHOLDER: &str = "<your-tailscale-login>";

/// The tailnet policy `ssh` rule object that lets the operator log in to
/// workers carrying `tag` as `unix_user` over Tailscale SSH, rendered as a
/// HuJSON object.
///
/// `src` is the operator's own Tailscale login when known, so the rule
/// grants exactly the person who just created the worker and nobody else.
/// kd never suggests `autogroup:member`: on a shared tailnet that selector
/// is every member (including invited users on shared destinations), and a
/// copy-pasted security example tends to become production policy
/// unchanged. Operators who genuinely want tailnet-wide access can widen
/// it themselves. Kept separate from each command's `render_summary` so
/// the snippet is unit-testable as the one line most likely to drift from
/// what Tailscale actually accepts.
///
/// `tag` is the rule's `dst`. ubiworker passes `tag:ubicloud` and the
/// operator's local user; boatworker passes `tag:boat` and boat's fixed
/// `user` account.
pub(crate) fn render_ssh_policy_rule(
    tag: &str,
    unix_user: &str,
    tailscale_login: Option<&str>,
) -> String {
    let src = tailscale_login.unwrap_or(LOGIN_PLACEHOLDER);
    format!(
        r#"{{ "action": "accept", "src": ["{src}"], "dst": ["{tag}"], "users": ["{unix_user}"] }}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_response_extracts_access_token() {
        let body = r#"{"access_token": "abc123", "token_type": "bearer", "expires_in": 3600}"#;
        assert_eq!(parse_token_response(body).unwrap(), "abc123");
    }

    #[test]
    fn parse_token_response_errors_when_field_missing() {
        assert!(parse_token_response(r#"{"token_type": "bearer"}"#).is_err());
    }

    /// An empty bearer token is syntactically present but useless — it must
    /// be rejected here rather than surface as a mystifying 401 on the next
    /// HTTP call.
    #[test]
    fn parse_token_response_rejects_empty_access_token() {
        let err = parse_token_response(r#"{"access_token": ""}"#).unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn parse_create_key_response_accepts_valid_key() {
        let body = r#"{"id": "k123", "key": "tskey-auth-xxxxx-yyyyy"}"#;
        assert_eq!(
            parse_create_key_response(body).unwrap(),
            "tskey-auth-xxxxx-yyyyy"
        );
    }

    #[test]
    fn parse_create_key_response_errors_when_key_field_missing() {
        let err = parse_create_key_response(r#"{"id": "k123"}"#).unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to parse tailscale auth key response"),
            "unexpected error: {err}"
        );
    }

    /// A `key` value without the `tskey-auth-` prefix means the API
    /// response shape isn't what we expect (or has changed); this must be
    /// rejected rather than fed into an init script as if it were valid.
    #[test]
    fn parse_create_key_response_rejects_non_tskey_prefix() {
        let body = r#"{"key": "not-a-real-key"}"#;
        let err = parse_create_key_response(body).unwrap_err();
        assert!(
            err.to_string().contains("tskey-auth-"),
            "unexpected error: {err}"
        );
    }

    /// Regression coverage for FX6: `parse_create_key_response` used to
    /// only check the prefix, so a key with a `'` or newline anywhere after
    /// it would have sailed through — and `init_script::render_init_script`
    /// interpolates the key unescaped inside a single-quoted shell
    /// argument, which a smuggled `'` would break out of.
    #[test]
    fn parse_create_key_response_rejects_embedded_quote() {
        let body = r#"{"key": "tskey-auth-xx'xx"}"#;
        assert!(parse_create_key_response(body).is_err());
    }

    #[test]
    fn parse_create_key_response_rejects_embedded_newline() {
        let body = "{\"key\": \"tskey-auth-xx\\nxx\"}";
        assert!(parse_create_key_response(body).is_err());
    }

    #[test]
    fn parse_create_key_response_rejects_prefix_only() {
        let body = r#"{"key": "tskey-auth-"}"#;
        assert!(parse_create_key_response(body).is_err());
    }

    /// 401/403 are the one status class where "check your credentials" is
    /// actually the right hint, and it must name the caller's tag: the OAuth
    /// client has to own the tag being minted, and with more than one fleet
    /// a hint naming some other tag would send the reader to check the wrong
    /// thing.
    #[test]
    fn tailscale_http_error_gives_credentials_hint_for_401_and_403() {
        for code in [401, 403] {
            let err = tailscale_http_error("op", ureq::Error::StatusCode(code), "tag:example");
            let message = err.to_string();
            assert!(
                message.contains("TS_API_CLIENT_ID") && message.contains("owns tag:example"),
                "unexpected error for {code}: {message}"
            );
        }
    }

    #[test]
    fn tailscale_http_error_gives_retry_hint_for_429() {
        let err = tailscale_http_error("op", ureq::Error::StatusCode(429), "tag:example");
        let message = err.to_string();
        assert!(
            message.contains("rate limited"),
            "unexpected error: {message}"
        );
        assert!(
            !message.contains("TS_API_CLIENT_ID"),
            "429 should not suggest a credentials problem: {message}"
        );
    }

    #[test]
    fn tailscale_http_error_gives_retry_hint_for_5xx() {
        let err = tailscale_http_error("op", ureq::Error::StatusCode(503), "tag:example");
        let message = err.to_string();
        assert!(
            message.contains("server error"),
            "unexpected error: {message}"
        );
        assert!(
            !message.contains("TS_API_CLIENT_ID"),
            "5xx should not suggest a credentials problem: {message}"
        );
    }

    /// Anything outside the classes above (e.g. a 404) gets the bare status
    /// with no speculative hint — guessing wrong is worse than saying
    /// nothing.
    #[test]
    fn tailscale_http_error_gives_no_hint_for_other_codes() {
        let err = tailscale_http_error("op", ureq::Error::StatusCode(404), "tag:example");
        let message = err.to_string();
        assert!(message.contains("404"), "unexpected error: {message}");
        assert!(
            !message.contains("TS_API_CLIENT_ID")
                && !message.contains("rate limited")
                && !message.contains("server error"),
            "404 should not carry a speculative hint: {message}"
        );
    }

    // ── parse_status_login ───────────────────────────────────────────────

    /// Pins the `tailscale status --json` shape the login lookup depends on:
    /// `Self.UserID` is a number, and the `User` map is keyed by that id
    /// as a *string*. Getting the key type wrong would silently yield
    /// "not found" for every node, and the policy rule would always fall
    /// back to the placeholder without anyone noticing.
    #[test]
    fn parse_status_login_resolves_self_user() {
        let json = r#"{
            "Self": {"UserID": 1001},
            "User": {
                "1002": {"LoginName": "tagged-devices"},
                "1001": {"LoginName": "alice@github"}
            }
        }"#;
        assert_eq!(parse_status_login(json).unwrap(), "alice@github");
    }

    /// A tagged node reports the synthetic `tagged-devices` login, which
    /// is not a valid policy `src`; it must be treated as "unknown" so the
    /// caller prints a placeholder rather than an unusable rule.
    #[test]
    fn parse_status_login_rejects_tagged_devices() {
        let json = r#"{"Self": {"UserID": 1}, "User": {"1": {"LoginName": "tagged-devices"}}}"#;
        assert!(parse_status_login(json).is_err());
    }

    #[test]
    fn parse_status_login_errors_on_missing_fields_or_garbage() {
        assert!(parse_status_login("not json").is_err());
        assert!(parse_status_login(r#"{"Self": {}}"#).is_err());
        assert!(parse_status_login(r#"{"Self": {"UserID": 7}, "User": {}}"#).is_err());
    }

    #[test]
    fn is_valid_auth_key_accepts_well_formed_key() {
        assert!(is_valid_auth_key("tskey-auth-xxxxx-yyyyy"));
    }

    #[test]
    fn is_valid_auth_key_rejects_prefix_only() {
        assert!(!is_valid_auth_key("tskey-auth-"));
    }

    #[test]
    fn is_valid_auth_key_rejects_missing_prefix() {
        assert!(!is_valid_auth_key("xxxxx-yyyyy"));
    }

    /// The whole point of this predicate is to keep a `'` out of a value
    /// that gets interpolated inside a single-quoted shell argument, so a
    /// key that smuggles one in after an otherwise-valid prefix must fail.
    #[test]
    fn is_valid_auth_key_rejects_embedded_quote() {
        assert!(!is_valid_auth_key("tskey-auth-xx'xx"));
    }

    /// The rule's `src` must never default to a tailnet-wide selector: when
    /// the operator's login is unknown, the output must be an obvious
    /// placeholder the reader has to replace, not `autogroup:member`.
    #[test]
    fn render_ssh_policy_rule_never_defaults_to_autogroup_member() {
        let rule = render_ssh_policy_rule("tag:example", "alice", None);
        assert_eq!(
            rule,
            r#"{ "action": "accept", "src": ["<your-tailscale-login>"], "dst": ["tag:example"], "users": ["alice"] }"#
        );
        assert!(!rule.contains("autogroup"), "unexpected rule: {rule}");
    }
}
