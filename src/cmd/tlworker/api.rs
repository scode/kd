//! The slice of Tensorlake's REST API that `kd tlworker` uses, and where its
//! credentials come from.
//!
//! kd talks to Tensorlake over HTTP rather than through the `tl` CLI: the
//! lifecycle calls (`https://api.tensorlake.ai`) return JSON that kd can
//! parse fail-closed, and they take settings `tl sbx create` does not expose.
//! The calls are synchronous `ureq` requests, the same HTTP stack the
//! Tailscale key minting in `worker_bootstrap` uses, with no async runtime.
//!
//! # Credentials
//!
//! Two sources, in this order (see [`load_credentials`]):
//!
//! - `TENSORLAKE_API_KEY`: a project-scoped API key from the Tensorlake
//!   dashboard. This is Tensorlake's documented way to call the API, and the
//!   key carries its own project, so nothing else is sent with it.
//! - The `tl` CLI's own login, read from `~/.config/tensorlake/credentials.toml`:
//!   a personal access token plus the organization and project `tl login`
//!   selected, sent as `X-Forwarded-Organization-Id` and
//!   `X-Forwarded-Project-Id`. NOTE: neither the file format nor those
//!   headers are documented by Tensorlake. They are what `tl` 0.5.125 uses
//!   (the header names come from its binary), and a `tl` update may change
//!   them without notice. When that happens the API key is the stable way
//!   around it.
//!
//! The secret (key or token) never reaches a log line, an error message or a
//! process's argv: [`Credentials`] has no `Debug`, and error text taken from
//! a response body is redacted before it is shown.
//!
//! # Replies
//!
//! Replies are decoded leniently where leniency is safe: unknown fields are
//! ignored (Tensorlake adds fields without notice) and a status string kd
//! does not know is kept as text rather than failing the decode. Missing
//! required fields and malformed JSON still fail, since a listing kd only
//! half understood must not turn into a partial list that `destroy` acts on.

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Tensorlake's lifecycle API. The credentials file keys its login by this
/// same URL, which is why it is also the TOML table name kd looks up.
const API_BASE: &str = "https://api.tensorlake.ai";

/// Where `tl` keeps its login, relative to the home directory.
const CLI_CREDENTIALS_PATH: &str = ".config/tensorlake/credentials.toml";

/// The environment variable holding a Tensorlake API key. The same name the
/// `tl` CLI and Tensorlake's SDKs read.
pub(crate) const API_KEY_VAR: &str = "TENSORLAKE_API_KEY";

/// The bound on an ordinary lifecycle call (list, terminate, snapshot
/// calls). ureq 3 sets no timeout by default, and a stalled connection would
/// otherwise hang the command forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The most response body kd reads. Listings are a page of sandboxes, far
/// below this; the cap only stops a misbehaving server from filling memory.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// The most characters of a server's error text kd repeats to the user.
const MAX_ERROR_CHARS: usize = 300;

/// The upper bound on pages in one listing. Tensorlake pages hold up to 100
/// sandboxes, so this is far past any fleet kd is built for; reaching it
/// means the cursor is not advancing, and kd fails rather than loop forever.
const MAX_PAGES: usize = 1000;

/// The remedy every credential failure points at. Both routes are named
/// because either one fixes it, and which is right depends on the machine.
const CREDENTIALS_REMEDY: &str = "authenticate the tl CLI with `tl login`, or set \
     TENSORLAKE_API_KEY to an API key created in the Tensorlake dashboard \
     (https://cloud.tensorlake.ai, under the project's API keys)";

// ── Credentials ──────────────────────────────────────────────────────────

/// How kd authenticates to Tensorlake. Deliberately neither `Debug` nor
/// `Display`, so the secret cannot reach a log line by accident.
pub(crate) enum Credentials {
    /// A project-scoped API key from `TENSORLAKE_API_KEY`.
    ApiKey(String),
    /// The `tl` CLI's login: a personal access token plus the organization
    /// and project it acts in.
    CliLogin {
        token: String,
        organization: String,
        project: String,
    },
}

impl Credentials {
    /// The secret itself, for the `Authorization` header and for redacting
    /// error text. Nothing else should call this.
    fn secret(&self) -> &str {
        match self {
            Credentials::ApiKey(key) => key,
            Credentials::CliLogin { token, .. } => token,
        }
    }

    /// Where the credentials came from, for error messages that need to say
    /// which login Tensorlake rejected.
    fn source(&self) -> &'static str {
        match self {
            Credentials::ApiKey(_) => "the API key in TENSORLAKE_API_KEY",
            Credentials::CliLogin { .. } => "the tl CLI's login",
        }
    }

    /// The headers every request carries: the bearer token, plus the
    /// organization and project for a CLI login (an API key carries its own
    /// project and gets no extra headers).
    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![("Authorization", format!("Bearer {}", self.secret()))];
        if let Credentials::CliLogin {
            organization,
            project,
            ..
        } = self
        {
            headers.push(("X-Forwarded-Organization-Id", organization.clone()));
            headers.push(("X-Forwarded-Project-Id", project.clone()));
        }
        headers
    }
}

/// Load credentials from the real environment and home directory.
///
/// Reading `HOME` is the one ambient lookup; everything else goes through
/// [`resolve_credentials`], which takes both inputs as parameters so tests
/// never touch the process environment.
pub(crate) fn load_credentials() -> anyhow::Result<Credentials> {
    // An empty HOME counts as unset; joining onto "" would read the
    // credentials file relative to the current directory instead.
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    resolve_credentials(std::env::var(API_KEY_VAR).ok(), home.as_deref(), |path| {
        std::fs::read_to_string(path)
    })
}

/// Pick the credentials to use: the API key when set and non-empty, else the
/// `tl` CLI's login file under `home`, else an error naming both remedies.
///
/// An empty `TENSORLAKE_API_KEY` counts as unset, the same rule
/// `worker_bootstrap::require_envs` applies, since an `export FOO=` left in a
/// shell profile should not shadow a working `tl` login.
///
/// This runs before any network call, so a machine with neither source fails
/// at once with the fix, rather than with an HTTP 401 from Tensorlake.
fn resolve_credentials(
    api_key: Option<String>,
    home: Option<&Path>,
    read: impl FnOnce(&Path) -> std::io::Result<String>,
) -> anyhow::Result<Credentials> {
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        return Ok(Credentials::ApiKey(key));
    }
    let Some(home) = home else {
        bail!(
            "no Tensorlake credentials (TENSORLAKE_API_KEY is unset and HOME is unset, so the tl CLI's login cannot be found); {CREDENTIALS_REMEDY}"
        );
    };
    let path = home.join(CLI_CREDENTIALS_PATH);
    let text = match read(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "no Tensorlake credentials (TENSORLAKE_API_KEY is unset and {} does not exist); \
             {CREDENTIALS_REMEDY}",
            path.display()
        ),
        Err(err) => {
            return Err(anyhow!(err)).with_context(|| {
                format!(
                    "failed to read the tl CLI's login from {}; {CREDENTIALS_REMEDY}",
                    path.display()
                )
            });
        }
    };
    parse_cli_credentials(&text)
        .with_context(|| format!("{}: {CREDENTIALS_REMEDY}", path.display()))
}

/// Parse the `tl` CLI's credentials file: a TOML table keyed by the API URL
/// holding `token`, `organization` and `project`.
///
/// Every field must be a non-empty string. A login missing its organization
/// or project would make Tensorlake answer for some other scope, or refuse,
/// and either is worse than saying here what is missing.
fn parse_cli_credentials(text: &str) -> anyhow::Result<Credentials> {
    let doc: toml::Table = toml::from_str(text).context("not a valid TOML file")?;
    let login = doc
        .get(API_BASE)
        .and_then(|v| v.as_table())
        .ok_or_else(|| anyhow!("no login for {API_BASE} (has `tl login` been run?)"))?;
    let field = |name: &str| -> anyhow::Result<String> {
        match login.get(name).and_then(|v| v.as_str()) {
            Some(value) if !value.is_empty() => Ok(value.to_string()),
            _ => bail!("the login for {API_BASE} has no `{name}`"),
        }
    };
    Ok(Credentials::CliLogin {
        token: field("token")?,
        organization: field("organization")?,
        project: field("project")?,
    })
}

// ── Reply types ──────────────────────────────────────────────────────────

/// One sandbox from `GET /sandboxes`, reduced to the fields kd uses.
///
/// `name` is optional because Tensorlake only names a sandbox when asked;
/// an unnamed sandbox is never a tlworker. `status` is kept as text so a
/// state Tensorlake adds later shows up in `list` instead of failing it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Sandbox {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    pub(crate) status: String,
    pub(crate) resources: Resources,
}

/// A sandbox's CPU and memory allocation, as Tensorlake reports it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Resources {
    pub(crate) cpus: f64,
    pub(crate) memory_mb: u64,
}

/// One page of `GET /sandboxes`. `next_cursor` is absent or null on the
/// last page.
#[derive(Debug, Deserialize)]
struct SandboxPage {
    sandboxes: Vec<Sandbox>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Decide what follows a listing page: `Some(cursor)` to fetch the next
/// one, `None` when this was the last, or an error when paging cannot be
/// trusted to end.
///
/// The OpenAPI document does not say how the last page is marked, so both
/// a missing (or empty) cursor and a page with no sandboxes end the
/// listing. A cursor equal to the one just used would fetch the same page
/// forever; that is refused rather than returned as a listing that might be
/// incomplete.
fn next_page(current: Option<&str>, page: &SandboxPage) -> anyhow::Result<Option<String>> {
    let next = match page.next_cursor.as_deref() {
        Some(next) if !next.is_empty() && !page.sandboxes.is_empty() => next,
        _ => return Ok(None),
    };
    if current == Some(next) {
        bail!("Tensorlake's sandbox listing returned the same cursor twice");
    }
    Ok(Some(next.to_string()))
}

/// One snapshot from `GET /snapshots`, reduced to what `destroy` needs.
///
/// `sandbox_id` is the sandbox the snapshot was taken from. Tensorlake
/// snapshots a named sandbox when it suspends on idle, and those snapshots
/// outlive the sandbox and bill storage monthly, so `destroy` deletes every
/// snapshot whose source is a sandbox it terminated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) sandbox_id: Option<String>,
}

/// `GET /snapshots`. A `next_cursor` is not expected (see
/// [`Api::list_snapshots`]) and is only read to refuse it.
#[derive(Debug, Deserialize)]
struct SnapshotList {
    snapshots: Vec<Snapshot>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Parse `GET /snapshots`, refusing a reply that says there is more.
fn parse_snapshot_list(body: &str) -> anyhow::Result<Vec<Snapshot>> {
    let list: SnapshotList =
        serde_json::from_str(body).context("failed to parse Tensorlake's snapshot listing")?;
    if list.next_cursor.is_some_and(|c| !c.is_empty()) {
        bail!(
            "Tensorlake's snapshot listing is paginated now, which kd does not handle; \
             refusing to act on a possibly partial list"
        );
    }
    Ok(list.snapshots)
}

/// `GET /sandboxes/{id}`, reduced to the status `destroy` waits on.
#[derive(Debug, Deserialize)]
struct SandboxStatus {
    status: String,
}

/// Check that an ID taken from a Tensorlake reply is safe to put in a URL
/// path as is. Tensorlake's IDs are alphanumeric; anything outside
/// `[A-Za-z0-9_-]` (a `/`, `?` or `..`) would change which resource the
/// request names, so it is refused rather than encoded.
fn path_segment(id: &str) -> anyhow::Result<&str> {
    let safe = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !safe {
        bail!("Tensorlake returned an ID kd will not put in a URL: {id:?}");
    }
    Ok(id)
}

/// The outcome of a delete that succeeded: the thing was removed now, or it
/// was already gone (HTTP 404).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gone {
    Now,
    Already,
}

// ── Client ───────────────────────────────────────────────────────────────

/// A Tensorlake API client bound to one set of credentials.
pub(crate) struct Api {
    agent: ureq::Agent,
    credentials: Credentials,
}

impl Api {
    /// Build a client. Non-2xx statuses are returned as responses rather
    /// than errors, so kd can read Tensorlake's error text (a plan limit, a
    /// taken name) and show it; ureq's default would discard the body.
    pub(crate) fn new(credentials: Credentials) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(CALL_TIMEOUT))
            .build()
            .into();
        Api { agent, credentials }
    }

    /// Every sandbox in the current project that has not terminated,
    /// following the listing's cursor to the end.
    ///
    /// Reading every page is what lets the no-name forms of `destroy` and
    /// `ssh` trust that the one tlworker they see is the only one; there is
    /// no incomplete-listing case to warn about, unlike boatworker's.
    pub(crate) fn list_sandboxes(&self) -> anyhow::Result<Vec<Sandbox>> {
        let mut sandboxes = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut request = self.authorized(self.agent.get(format!("{API_BASE}/sandboxes")));
            if let Some(cursor) = &cursor {
                request = request.query("cursor", cursor);
            }
            let response = request
                .call()
                .map_err(|err| self.transport_error("listing sandboxes", err))?;
            let body = self.read_success(response, "listing sandboxes")?;
            let page: SandboxPage = serde_json::from_str(&body)
                .context("failed to parse Tensorlake's sandbox listing")?;
            let step = next_page(cursor.as_deref(), &page)?;
            sandboxes.extend(page.sandboxes);
            match step {
                Some(next) => cursor = Some(next),
                None => return Ok(sandboxes),
            }
        }
        bail!("Tensorlake's sandbox listing did not end after {MAX_PAGES} pages")
    }

    /// Terminate a sandbox by ID (`DELETE /sandboxes/{id}`).
    ///
    /// Tensorlake documents this as idempotent for an already-terminated
    /// sandbox. A 404 (the sandbox is gone from the project altogether) is
    /// reported as [`Gone::Already`] rather than an error: the worker went
    /// away between kd's listing and this call, which is the outcome the
    /// caller wanted anyway.
    pub(crate) fn terminate_sandbox(&self, id: &str) -> anyhow::Result<Gone> {
        let url = format!("{API_BASE}/sandboxes/{}", path_segment(id)?);
        self.delete(
            self.authorized(self.agent.delete(url)),
            "terminating the sandbox",
        )
    }

    /// A sandbox's current status (`GET /sandboxes/{id}`), or `None` when
    /// Tensorlake no longer knows it (404), which for a sandbox kd just
    /// terminated means it is gone.
    pub(crate) fn sandbox_status(&self, id: &str) -> anyhow::Result<Option<String>> {
        let url = format!("{API_BASE}/sandboxes/{}", path_segment(id)?);
        let response = self
            .authorized(self.agent.get(url))
            .call()
            .map_err(|err| self.transport_error("reading the sandbox", err))?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let body = self.read_success(response, "reading the sandbox")?;
        let status: SandboxStatus =
            serde_json::from_str(&body).context("failed to parse Tensorlake's sandbox record")?;
        Ok(Some(status.status))
    }

    /// Every snapshot in the project (`GET /snapshots`).
    ///
    /// The listing had no cursor when checked (November 2026), so one reply
    /// is the whole project. If a reply ever carries a cursor, kd refuses
    /// rather than act on what may be a partial list: `destroy` uses this to
    /// find a sandbox's leftover snapshots, and a silently missed one keeps
    /// billing storage.
    pub(crate) fn list_snapshots(&self) -> anyhow::Result<Vec<Snapshot>> {
        let response = self
            .authorized(self.agent.get(format!("{API_BASE}/snapshots")))
            .call()
            .map_err(|err| self.transport_error("listing snapshots", err))?;
        let body = self.read_success(response, "listing snapshots")?;
        parse_snapshot_list(&body)
    }

    /// Delete a snapshot by ID (`DELETE /snapshots/{id}`). A 404 means it is
    /// already gone, reported as [`Gone::Already`].
    pub(crate) fn delete_snapshot(&self, id: &str) -> anyhow::Result<Gone> {
        let url = format!("{API_BASE}/snapshots/{}", path_segment(id)?);
        self.delete(
            self.authorized(self.agent.delete(url)),
            "deleting the snapshot",
        )
    }

    /// Send a `DELETE`, treating 404 as already gone and any other non-2xx
    /// as an error.
    fn delete(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
        what: &str,
    ) -> anyhow::Result<Gone> {
        let response = request
            .call()
            .map_err(|err| self.transport_error(what, err))?;
        if response.status().as_u16() == 404 {
            return Ok(Gone::Already);
        }
        self.read_success(response, what)?;
        Ok(Gone::Now)
    }

    /// Attach the credential headers to a request.
    fn authorized<B>(&self, mut request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        for (name, value) in self.credentials.headers() {
            request = request.header(name, value);
        }
        request
    }

    /// Turn a response into its body, or an error that carries the status
    /// and Tensorlake's (redacted, shortened) explanation.
    ///
    /// 401 and 403 get the credential remedy, naming which login was used.
    /// Usually that is the fix; a 403 can also mean valid credentials
    /// without access to that project, which the server's text then says.
    fn read_success(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
        what: &str,
    ) -> anyhow::Result<String> {
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .with_context(|| format!("{what}: failed to read Tensorlake's reply"))?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let message = clean_message(&body, self.credentials.secret());
        let detail = if message.is_empty() {
            String::new()
        } else {
            format!(": {message}")
        };
        if status == 401 || status == 403 {
            bail!(
                "{what} failed: Tensorlake rejected {} (HTTP {status}){detail}; {CREDENTIALS_REMEDY}",
                self.credentials.source()
            );
        }
        bail!("{what} failed: Tensorlake answered HTTP {status}{detail}")
    }

    /// A failure before any HTTP status arrived (DNS, connect, TLS,
    /// timeout). ureq's error text names the URL and cause, never headers,
    /// so it carries no secret; it is redacted anyway as a backstop.
    fn transport_error(&self, what: &str, err: ureq::Error) -> anyhow::Error {
        anyhow!(
            "{what} failed: {}",
            clean_message(&err.to_string(), self.credentials.secret())
        )
    }
}

/// Make server-supplied text safe and short enough to show: the secret
/// redacted should a server ever echo it, whitespace runs collapsed to one
/// space (so a multi-line body stays on one line), control characters
/// dropped, and the result cut at [`MAX_ERROR_CHARS`].
fn clean_message(raw: &str, secret: &str) -> String {
    let redacted = if secret.is_empty() {
        raw.to_string()
    } else {
        raw.replace(secret, "<redacted>")
    };
    let collapsed = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── credential resolution ────────────────────────────────────────────

    const CLI_FILE: &str = r#"["https://api.tensorlake.ai"]
organization = "org_example"
project = "project_example"
token = "tl_pat_example"
"#;

    fn read_ok(text: &'static str) -> impl FnOnce(&Path) -> std::io::Result<String> {
        move |_| Ok(text.to_string())
    }

    fn read_missing(_: &Path) -> std::io::Result<String> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    /// An API key wins over a CLI login when both exist, matching the `tl`
    /// CLI's own precedence, and it is sent with no organization or project
    /// headers because the key carries its project.
    #[test]
    fn api_key_takes_precedence_and_sends_only_bearer() {
        let creds = resolve_credentials(
            Some("tl_apiKey_x".into()),
            Some(Path::new("/h")),
            read_ok(CLI_FILE),
        )
        .unwrap();
        assert!(matches!(creds, Credentials::ApiKey(ref k) if k == "tl_apiKey_x"));
        assert_eq!(
            creds.headers(),
            vec![("Authorization", "Bearer tl_apiKey_x".to_string())]
        );
    }

    /// An empty `TENSORLAKE_API_KEY` must not shadow a working CLI login.
    #[test]
    fn empty_api_key_falls_back_to_cli_login() {
        let creds = resolve_credentials(
            Some(String::new()),
            Some(Path::new("/h")),
            read_ok(CLI_FILE),
        )
        .unwrap();
        assert!(matches!(creds, Credentials::CliLogin { .. }));
    }

    /// The CLI login is read from the file `tl login` writes, under the
    /// home directory, and sent as the token plus the two scope headers
    /// `tl` itself sends.
    #[test]
    fn cli_login_reads_tl_file_and_sends_scope_headers() {
        let mut seen = None;
        let creds = resolve_credentials(None, Some(Path::new("/home/x")), |path| {
            seen = Some(path.to_path_buf());
            Ok(CLI_FILE.to_string())
        })
        .unwrap();
        assert_eq!(
            seen.unwrap(),
            Path::new("/home/x/.config/tensorlake/credentials.toml")
        );
        assert_eq!(
            creds.headers(),
            vec![
                ("Authorization", "Bearer tl_pat_example".to_string()),
                ("X-Forwarded-Organization-Id", "org_example".to_string()),
                ("X-Forwarded-Project-Id", "project_example".to_string()),
            ]
        );
    }

    /// With neither source, the error tells the user both ways to fix it,
    /// which is the whole point of failing before any network call.
    #[test]
    fn no_credentials_names_both_remedies() {
        let msg = resolve_credentials(None, Some(Path::new("/h")), read_missing)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("tl login"), "{msg}");
        assert!(msg.contains("TENSORLAKE_API_KEY"), "{msg}");
    }

    /// A file that exists but lacks a field is reported as such (with the
    /// remedy), never sent half-filled to Tensorlake.
    #[test]
    fn cli_file_missing_a_field_is_an_error_naming_it() {
        let text = "[\"https://api.tensorlake.ai\"]\ntoken = \"t\"\norganization = \"o\"\n";
        let msg = format!(
            "{:#}",
            resolve_credentials(None, Some(Path::new("/h")), |_| Ok(text.to_string()))
                .err()
                .unwrap()
        );
        assert!(msg.contains("`project`"), "{msg}");
        assert!(msg.contains("tl login"), "{msg}");
    }

    /// A login for some other API URL (a staging endpoint, say) is not the
    /// production login and must not be used as one.
    #[test]
    fn cli_file_without_production_table_is_an_error() {
        let text =
            "[\"https://staging.example\"]\ntoken = \"t\"\norganization = \"o\"\nproject = \"p\"\n";
        assert!(parse_cli_credentials(text).is_err());
    }

    // ── replies ──────────────────────────────────────────────────────────

    /// A trimmed `GET /sandboxes` reply (observed shape, November 2026), with
    /// some of the fields kd ignores left in to show they do not matter, an
    /// unnamed sandbox, and a status kd has never heard of.
    const PAGE_FIXTURE: &str = r#"{
      "counts": {"running": 1},
      "prev_cursor": null,
      "next_cursor": "abc",
      "sandboxes": [
        {"id": "sbx1", "name": "tlworker-a", "status": "running", "namespace": "n",
         "resources": {"cpus": 4.0, "memory_mb": 16384, "disk_mb": 102400},
         "auto_resume": false, "timeout_secs": 86400},
        {"id": "sbx2", "name": null, "status": "brand_new_state",
         "resources": {"cpus": 1.0, "memory_mb": 1024, "disk_mb": 10240}}
      ]
    }"#;

    /// The page decodes with unknown fields ignored and an unknown status
    /// kept as text, and the cursor comes through for the next request.
    #[test]
    fn sandbox_page_decodes_leniently() {
        let page: SandboxPage = serde_json::from_str(PAGE_FIXTURE).unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("abc"));
        assert_eq!(page.sandboxes.len(), 2);
        assert_eq!(page.sandboxes[0].name.as_deref(), Some("tlworker-a"));
        assert_eq!(page.sandboxes[0].resources.memory_mb, 16384);
        assert_eq!(page.sandboxes[1].name, None);
        assert_eq!(page.sandboxes[1].status, "brand_new_state");
    }

    fn page(next_cursor: Option<&str>, sandboxes: usize) -> SandboxPage {
        let sandbox = Sandbox {
            id: "s".into(),
            name: None,
            status: "running".into(),
            resources: Resources {
                cpus: 1.0,
                memory_mb: 1024,
            },
        };
        SandboxPage {
            sandboxes: vec![sandbox; sandboxes],
            next_cursor: next_cursor.map(str::to_string),
        }
    }

    /// Paging ends on a missing or empty cursor and on an empty page, since
    /// Tensorlake does not document which marks the end; it continues with a
    /// new cursor; and a cursor that does not move is refused rather than
    /// fetched forever.
    #[test]
    fn next_page_ends_continues_or_refuses() {
        assert_eq!(next_page(None, &page(None, 3)).unwrap(), None);
        assert_eq!(next_page(None, &page(Some(""), 3)).unwrap(), None);
        assert_eq!(next_page(None, &page(Some("c1"), 0)).unwrap(), None);
        assert_eq!(
            next_page(None, &page(Some("c1"), 3)).unwrap().as_deref(),
            Some("c1")
        );
        assert!(next_page(Some("c1"), &page(Some("c1"), 3)).is_err());
    }

    /// A sandbox record without an id cannot be acted on, so the whole
    /// listing fails rather than silently dropping it.
    #[test]
    fn sandbox_without_id_fails_the_page() {
        let json = r#"{"sandboxes": [{"name": "tlworker-a", "status": "running",
                       "resources": {"cpus": 1.0, "memory_mb": 1024}}]}"#;
        assert!(serde_json::from_str::<SandboxPage>(json).is_err());
    }

    /// The observed `GET /snapshots` shape (November 2026), trimmed: kd
    /// keeps the ID and source sandbox and ignores the rest, and a snapshot
    /// with no source sandbox decodes too.
    #[test]
    fn snapshot_list_decodes_source_sandbox() {
        let body = r#"{"snapshots": [
            {"id": "snap1", "sandbox_id": "sbx1", "status": "completed", "size_bytes": 1},
            {"id": "snap2", "status": "completed"}
        ]}"#;
        let snapshots = parse_snapshot_list(body).unwrap();
        assert_eq!(snapshots[0].sandbox_id.as_deref(), Some("sbx1"));
        assert_eq!(snapshots[1].sandbox_id, None);
    }

    /// A paginated snapshot listing would mean kd sees only part of it, and
    /// a missed snapshot keeps billing; refuse instead of sweeping a part.
    #[test]
    fn snapshot_list_with_cursor_is_refused() {
        let body = r#"{"snapshots": [], "next_cursor": "more"}"#;
        assert!(parse_snapshot_list(body).is_err());
        let body = r#"{"snapshots": [], "next_cursor": null}"#;
        assert!(parse_snapshot_list(body).is_ok());
    }

    /// IDs go into URL paths unencoded, so anything that could change which
    /// resource a request names is refused.
    #[test]
    fn path_segment_accepts_ids_and_refuses_path_syntax() {
        assert_eq!(path_segment("abc-DEF_123").unwrap(), "abc-DEF_123");
        for bad in ["", "a/b", "..", "a?b", "a b", "a%2f"] {
            assert!(path_segment(bad).is_err(), "{bad:?}");
        }
    }

    // ── error text ───────────────────────────────────────────────────────

    /// A server that echoes the token back must not get it printed, and a
    /// multi-line body comes out as one bounded line.
    #[test]
    fn clean_message_redacts_secret_and_flattens() {
        let msg = clean_message("bad token tl_pat_secret\n\n  try again", "tl_pat_secret");
        assert_eq!(msg, "bad token <redacted> try again");
        let long = "x".repeat(1000);
        assert_eq!(clean_message(&long, "s").len(), MAX_ERROR_CHARS);
    }
}
