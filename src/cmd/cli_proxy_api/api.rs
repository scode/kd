//! CLIProxyAPI management API access: the three calls `manage-priorities`
//! needs, and parsing of their responses.
//!
//! Everything goes through the management API (`/v0/management`), the same
//! interface CLIProxyAPI's own web panel uses. Nothing here reads
//! CLIProxyAPI's files or container. Two of the calls are reads:
//!
//! - `GET /auth-files` lists credentials with routing state.
//! - `POST /api-call` makes CLIProxyAPI perform an outbound request with a
//!   credential's token substituted for `$TOKEN$`. kd uses it only for
//!   `GET https://api.anthropic.com/api/oauth/usage`, the request the panel's
//!   Quota Management page makes. For Claude credentials CLIProxyAPI uses
//!   the stored access token as-is (no refresh, checked in v7.3.17
//!   `resolveTokenForAuth`), so the lookup changes nothing on the proxy.
//!
//! The only write is `PATCH /auth-files/fields` with a new `priority`.
//!
//! `/usage-queue` is deliberately not used: reading it removes items, so it
//! must have exactly one consumer and kd should not become a second one.
//!
//! Parsing is split into pure functions over response text so the tests can
//! use literal JSON. Fields kd does not use are ignored, so additions in new
//! CLIProxyAPI or Anthropic releases do not break a run; missing required
//! fields are errors rather than defaults.

use super::plan::{Account, Cooldown, Usage, Window};
use anyhow::{Context, anyhow, bail};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// One request should never hang a periodic run. The usage lookup is the
/// slowest call: it normally takes well under a second, but CLIProxyAPI
/// waits up to 60 seconds on Anthropic, so kd waits a little longer and gets
/// CLIProxyAPI's own error instead of timing out first.
const HTTP_TIMEOUT: Duration = Duration::from_secs(75);

/// Anthropic's subscription usage endpoint. Undocumented and evolving (its
/// response carries internal codenames), so only the two windows kd needs
/// are parsed and everything else is ignored.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Client for one CLIProxyAPI instance. Holds the management key; neither
/// the key nor any request header is ever included in errors or logs.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    key: String,
}

impl Client {
    /// `base` is the server root, such as `http://127.0.0.1:8317`. It must
    /// be HTTPS or a loopback HTTP address: the management key would
    /// otherwise cross the network in the clear. Tunnels (SSH `-L`) keep the
    /// loopback form working from another machine.
    ///
    /// Proxy environment variables (`HTTP_PROXY`, `ALL_PROXY`, ...) are
    /// ignored on purpose. ureq honours them by default, with no loopback
    /// exemption, so a proxy set in the environment would receive the key in
    /// clear text, and CLIProxyAPI would then refuse the request anyway
    /// because it no longer comes from a local client.
    pub fn new(base: &str, key: String) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/').to_owned();
        check_base_url(&base)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .proxy(None)
            // Non-2xx responses carry CLIProxyAPI's explanation in the body
            // (bad auth_index, IP ban, upstream failure); keep it.
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self { agent, base, key })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v0/management{path}", self.base)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.key)
    }

    /// `GET /auth-files`: every credential CLIProxyAPI knows.
    pub fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        let result = self
            .agent
            .get(self.url("/auth-files"))
            .header("Authorization", self.auth())
            .call();
        parse_accounts(&finish("listing auth files", result)?)
    }

    /// Anthropic's view of one Claude credential's quota windows, fetched
    /// through CLIProxyAPI's `api-call` so the token never leaves the proxy.
    pub fn claude_usage(&self, auth_index: &str) -> anyhow::Result<Usage> {
        let request = json!({
            "auth_index": auth_index,
            "method": "GET",
            "url": USAGE_URL,
            "header": {
                "Authorization": "Bearer $TOKEN$",
                "anthropic-beta": "oauth-2025-04-20",
            },
        });
        let result = self
            .agent
            .post(self.url("/api-call"))
            .header("Authorization", self.auth())
            .send_json(&request);
        parse_usage_call(&finish("usage lookup", result)?)
    }

    /// `PATCH /auth-files/fields`: set one credential's routing priority.
    /// CLIProxyAPI updates the routing attribute before answering and then
    /// persists the value to the credential file; a failure to persist is
    /// not reported back (see SPEC_impl.md).
    pub fn set_priority(&self, name: &str, priority: i64) -> anyhow::Result<()> {
        let result = self
            .agent
            .patch(self.url("/auth-files/fields"))
            .header("Authorization", self.auth())
            .send_json(json!({ "name": name, "priority": priority }));
        finish(&format!("setting priority of {name}"), result)?;
        Ok(())
    }
}

/// Read a response body and turn a non-2xx status into an error that names
/// the operation, adds a hint for the statuses a user can act on, and
/// quotes the start of CLIProxyAPI's explanation. Request headers are never
/// part of it, so the key cannot leak through an error message; CLIProxyAPI
/// does not echo the key in its error bodies.
fn finish(
    context: &str,
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> anyhow::Result<String> {
    let mut response = result.map_err(|err| anyhow!("{context}: {err}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("{context}: reading response"))?;
    if !(200..300).contains(&status) {
        return Err(status_error(context, status, &body));
    }
    Ok(body)
}

fn status_error(context: &str, status: u16, body: &str) -> anyhow::Error {
    let hint = match status {
        401 => {
            " (the management key was rejected; check --key-file, and stop any periodic run until it is fixed: five failures ban the caller's address from the management API for 30 minutes)"
        }
        403 => {
            " (CLIProxyAPI refused the caller: remote management is off for non-loopback clients, or this address is banned for 30 minutes after repeated bad keys)"
        }
        404 => {
            " (management API not found: is remote-management.secret-key set, and is --url the CLIProxyAPI root?)"
        }
        _ => "",
    };
    let detail: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    if detail.is_empty() {
        anyhow!("{context}: HTTP {status}{hint}")
    } else {
        anyhow!("{context}: HTTP {status}{hint}: {detail}")
    }
}

/// Reject a base URL that would send the management key in the clear to
/// another machine. Plain HTTP is fine to loopback only. The URL is parsed
/// with a real URI parser, and any userinfo is refused, because
/// `http://localhost:8317@elsewhere/` names the host `elsewhere` however
/// loopback its first half looks.
fn check_base_url(base: &str) -> anyhow::Result<()> {
    let uri: ureq::http::Uri = base
        .parse()
        .with_context(|| format!("--url {base:?} is not a valid URL"))?;
    let authority = uri
        .authority()
        .with_context(|| format!("--url {base:?} has no host"))?;
    if authority.as_str().contains('@') {
        bail!("--url must not contain credentials");
    }
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("https") => Ok(()),
        Some("http") if matches!(host, "127.0.0.1" | "localhost" | "::1") => Ok(()),
        Some("http") => bail!(
            "refusing plain http to {host}: the management key would travel unencrypted; use https, or an SSH tunnel to 127.0.0.1"
        ),
        _ => bail!("--url must start with http:// or https://"),
    }
}

/// Wire shape of one `GET /auth-files` entry: only the fields kd reads.
#[derive(Deserialize)]
struct RawAccount {
    name: String,
    #[serde(default)]
    auth_index: String,
    // Required: CLIProxyAPI's reduced disk-only listing (served when its auth
    // manager is unavailable) has no provider, and treating every entry as
    // an unmanaged provider "" would look like a healthy empty pool.
    provider: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    unavailable: bool,
    #[serde(default)]
    status: String,
    #[serde(default)]
    status_message: String,
    #[serde(default)]
    next_retry_after: Option<String>,
    #[serde(default)]
    cooldowns: Option<Vec<RawCooldown>>,
    #[serde(default)]
    recent_requests: Option<Vec<RawBucket>>,
}

#[derive(Deserialize)]
struct RawCooldown {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    model_key: Option<String>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    retry_at: Option<String>,
    #[serde(default)]
    http_status: Option<u16>,
}

#[derive(Deserialize)]
struct RawBucket {
    #[serde(default)]
    success: u64,
    #[serde(default)]
    failed: u64,
}

#[derive(Deserialize)]
struct RawAccounts {
    files: Vec<RawAccount>,
}

/// Parse a `GET /auth-files` body.
///
/// Timestamps that fail to parse become `None` rather than failing the run:
/// they only feed flags and the log, never the plan. Go's zero time
/// (`0001-01-01T00:00:00Z`) parses fine and is always in the past, so it
/// never counts as an active block.
pub fn parse_accounts(body: &str) -> anyhow::Result<Vec<Account>> {
    let raw: RawAccounts = serde_json::from_str(body).context("parsing auth-files response")?;
    Ok(raw
        .files
        .into_iter()
        .map(|a| {
            let buckets = a.recent_requests.unwrap_or_default();
            Account {
                name: a.name,
                auth_index: a.auth_index,
                provider: a.provider,
                email: a.email.filter(|e| !e.is_empty()),
                priority: a.priority.unwrap_or(0),
                disabled: a.disabled,
                unavailable: a.unavailable,
                status: a.status,
                status_message: a.status_message,
                next_retry_after: a.next_retry_after.as_deref().and_then(parse_ts),
                cooldowns: a
                    .cooldowns
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| Cooldown {
                        scope: c.scope,
                        model_key: c.model_key.filter(|m| !m.is_empty()),
                        reason: c.reason,
                        retry_at: c.retry_at.as_deref().and_then(parse_ts),
                        http_status: c.http_status.filter(|s| *s != 0),
                    })
                    .collect(),
                recent_success: buckets.iter().map(|b| b.success).sum(),
                recent_failed: buckets.iter().map(|b| b.failed).sum(),
            }
        })
        .collect())
}

fn parse_ts(s: &str) -> Option<Timestamp> {
    s.parse().ok()
}

/// Wire shape of the `api-call` wrapper. `body` is the upstream response
/// body as a string.
#[derive(Deserialize)]
struct RawCall {
    status_code: u16,
    #[serde(default)]
    body: String,
}

/// Parse the `api-call` wrapper around Anthropic's usage response.
///
/// A non-200 upstream status or an unparseable body is an error, and so is
/// a missing `seven_day` window: without it there is no reset time to plan
/// by, and guessing would put the account in the wrong tier. A present
/// window whose `resets_at` is `null` is fine; it means the week has not
/// started. A missing `five_hour` window only weakens the flags, so it is
/// tolerated.
pub fn parse_usage_call(body: &str) -> anyhow::Result<Usage> {
    let call: RawCall = serde_json::from_str(body).context("parsing api-call response")?;
    if call.status_code != 200 {
        let snippet: String = call
            .body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();
        bail!(
            "usage endpoint returned HTTP {}: {snippet}",
            call.status_code
        );
    }
    let usage: Value = serde_json::from_str(&call.body).context("parsing usage body")?;
    let seven_day = usage
        .get("seven_day")
        .filter(|v| v.is_object())
        .context("usage response has no seven_day window")?;
    Ok(Usage {
        five_hour: usage
            .get("five_hour")
            .filter(|v| v.is_object())
            .map(parse_window)
            .transpose()?
            .unwrap_or_default(),
        seven_day: parse_window(seven_day)?,
    })
}

/// One `{utilization, resets_at}` window. A `resets_at` that is present but
/// not a timestamp is an error: that value decides the plan.
fn parse_window(window: &Value) -> anyhow::Result<Window> {
    let resets_at = match window.get("resets_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            s.parse::<Timestamp>()
                .with_context(|| format!("usage resets_at {s:?} is not a timestamp"))?,
        ),
        Some(other) => bail!("usage resets_at has unexpected value {other}"),
    };
    Ok(Window {
        utilization: window.get("utilization").and_then(Value::as_f64),
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback HTTP is how the command normally runs (on the box, or
    /// through an SSH tunnel); anything else over plain HTTP would expose
    /// the management key on the wire. Userinfo tricks that make a remote
    /// host look like loopback to naive string splitting must be refused.
    #[test]
    fn base_url_must_be_https_or_loopback() {
        for ok in [
            "http://127.0.0.1:8317",
            "http://localhost:8317/",
            "http://[::1]:8317",
            "https://proxy.example",
            "HTTP://127.0.0.1:8317",
        ] {
            assert!(check_base_url(ok.trim_end_matches('/')).is_ok(), "{ok}");
        }
        for bad in [
            "http://10.0.0.5:8317",
            "http://127.0.0.1.example:8317",
            "http://localhost:8317@evil.example",
            "http://127.0.0.1:1@evil.example/",
            "https://user:pw@proxy.example",
            "ftp://127.0.0.1",
            "127.0.0.1:8317",
            "http://",
        ] {
            assert!(check_base_url(bad).is_err(), "{bad}");
        }
    }

    /// Shape taken from a live v7.3.17 response (values made up), including
    /// the fields kd ignores, so a parse regression shows up here first.
    #[test]
    fn parses_live_shaped_auth_files() {
        let body = r#"{"files":[
          {"name":"claude-a.json","auth_index":"c81d","provider":"claude","email":"a@example.com",
           "priority":10,"disabled":false,"unavailable":false,"status":"active","status_message":"",
           "cooldowns":[],"recent_requests":[{"time":"21:40-21:50","success":3,"failed":1},{"time":"21:50-22:00","success":2,"failed":0}],
           "quota":{"signals":{}},"model_quotas":{},"account_type":"oauth","size":688},
          {"name":"claude-b.json","auth_index":"d1db","provider":"claude",
           "next_retry_after":"2026-10-13T00:00:00.5-07:00",
           "cooldowns":[{"scope":"auth","reason":"quota","retry_at":"2026-10-13T00:00:00-07:00","remaining_seconds":5,"http_status":429}]}
        ],"observed_at":"2026-09-24T23:37:23-07:00"}"#;
        let accounts = parse_accounts(body).unwrap();
        assert_eq!(accounts.len(), 2);
        let a = &accounts[0];
        assert_eq!(a.priority, 10);
        assert_eq!(a.email.as_deref(), Some("a@example.com"));
        assert_eq!((a.recent_success, a.recent_failed), (5, 1));
        let b = &accounts[1];
        assert_eq!(b.priority, 0, "missing priority means CLIProxyAPI's 0");
        assert_eq!(b.email, None);
        assert_eq!(b.cooldowns.len(), 1);
        assert_eq!(b.cooldowns[0].http_status, Some(429));
        assert_eq!(
            b.cooldowns[0].retry_at,
            Some("2026-10-13T07:00:00Z".parse().unwrap())
        );
        assert!(b.next_retry_after.is_some());
    }

    /// A malformed listing must fail the run rather than look like an empty
    /// pool, which would silently plan nothing. That includes CLIProxyAPI's
    /// reduced disk-only listing, whose entries carry no provider.
    #[test]
    fn auth_files_without_files_array_is_an_error() {
        assert!(parse_accounts(r#"{"observed_at":"x"}"#).is_err());
        assert!(parse_accounts("not json").is_err());
        assert!(parse_accounts(r#"{"files":[{"name":"a.json","type":"claude"}]}"#).is_err());
    }

    /// Errors must carry CLIProxyAPI's own explanation (an IP ban and a bad
    /// auth_index look identical by status alone) plus an actionable hint.
    #[test]
    fn status_errors_quote_the_body_and_hint() {
        let err = status_error(
            "usage lookup",
            403,
            "{\"error\":\"IP banned due to too many failed attempts\"}",
        )
        .to_string();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("banned for 30 minutes"), "{err}");
        assert!(
            err.contains("IP banned due to too many failed attempts"),
            "{err}"
        );
        let err = status_error("x", 500, &"y".repeat(1000)).to_string();
        assert!(err.len() < 300, "body is truncated: {}", err.len());
        assert_eq!(status_error("x", 502, "  ").to_string(), "x: HTTP 502");
    }

    fn call(status: u16, body: &str) -> String {
        json!({"status_code": status, "header": {}, "body": body}).to_string()
    }

    /// Shape from a live `oauth/usage` response, including the internal
    /// codename windows kd must ignore.
    #[test]
    fn parses_usage_windows_and_ignores_the_rest() {
        let body = r#"{"five_hour":{"utilization":0.0,"resets_at":"2026-09-25T09:20:00.391915+00:00"},
          "seven_day":{"utilization":23.0,"resets_at":"2026-09-27T16:00:00.391937+00:00"},
          "iguana_necktie":{"utilization":0.0,"resets_at":null,"limit_dollars":250},
          "extra_usage":{"is_enabled":false},"limits":[{"kind":"session"}]}"#;
        let usage = parse_usage_call(&call(200, body)).unwrap();
        assert_eq!(usage.seven_day.utilization, Some(23.0));
        assert_eq!(
            usage.seven_day.resets_at,
            Some("2026-09-27T16:00:00.391937Z".parse().unwrap())
        );
        assert_eq!(usage.five_hour.utilization, Some(0.0));
    }

    /// A week that has not started reports `resets_at: null`; that is a
    /// valid state (planned last), not an error.
    #[test]
    fn unstarted_weekly_window_has_no_reset() {
        let body = r#"{"seven_day":{"utilization":0.0,"resets_at":null}}"#;
        let usage = parse_usage_call(&call(200, body)).unwrap();
        assert_eq!(usage.seven_day.resets_at, None);
        assert_eq!(usage.five_hour, Window::default());
    }

    /// Anything that would force kd to guess an account's tier is an error:
    /// upstream failures, a missing weekly window, or a malformed reset.
    #[test]
    fn usage_failures_are_errors() {
        let err = parse_usage_call(&call(429, r#"{"error":"rate limited"}"#)).unwrap_err();
        assert!(err.to_string().contains("HTTP 429"), "{err}");
        assert!(parse_usage_call(&call(200, r#"{"five_hour":{}}"#)).is_err());
        assert!(parse_usage_call(&call(200, r#"{"seven_day":null}"#)).is_err());
        assert!(parse_usage_call(&call(200, r#"{"seven_day":{"resets_at":"soon"}}"#)).is_err());
        assert!(parse_usage_call(&call(200, r#"{"seven_day":{"resets_at":5}}"#)).is_err());
        assert!(parse_usage_call(&call(200, "<html>")).is_err());
        assert!(parse_usage_call("nope").is_err());
    }
}
