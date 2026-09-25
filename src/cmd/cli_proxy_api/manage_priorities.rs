//! `kd cli-proxy-api manage-priorities`: order CLIProxyAPI's Claude
//! accounts by weekly reset time so quota about to expire is spent first.
//!
//! One run is: list accounts, look up each managed account's quota windows,
//! plan priorities (see [`super::plan`]), print a report, append one JSON
//! line to the log, and, only with `--apply`, write the changed priorities
//! and read them back. It is stateless: every run starts from what the proxy
//! and Anthropic report now, so a crashed or skipped run is harmless and
//! the next one converges.
//!
//! Fail-safe rule: if any managed account's usage lookup fails, nothing is
//! written. A partial picture could promote the wrong account, and leaving
//! the previous priorities in place costs at most one interval.

use super::api::Client;
use super::plan::{self, Account, Change, Flag, Observed, Plan, Usage, Window};
use anyhow::{Context, bail};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// The operations a run needs from CLIProxyAPI. [`Client`] implements it
/// over HTTP; the orchestration tests use an in-memory fake so every
/// failure path can be driven without a server.
pub trait Management {
    fn list_accounts(&self) -> anyhow::Result<Vec<Account>>;
    fn claude_usage(&self, auth_index: &str) -> anyhow::Result<Usage>;
    fn set_priority(&self, name: &str, priority: i64) -> anyhow::Result<()>;
}

impl Management for Client {
    fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        Client::list_accounts(self)
    }
    fn claude_usage(&self, auth_index: &str) -> anyhow::Result<Usage> {
        Client::claude_usage(self, auth_index)
    }
    fn set_priority(&self, name: &str, priority: i64) -> anyhow::Result<()> {
        Client::set_priority(self, name, priority)
    }
}

/// One account's row in the report and the log.
#[derive(Debug)]
pub struct Row {
    pub observed: Observed,
    /// Why the usage lookup failed, for a managed account. Any such error
    /// blocks all writes for the run.
    pub usage_error: Option<String>,
    pub flags: Vec<Flag>,
}

/// Everything one run learned and did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub rows: Vec<Row>,
    /// `None` when planning was not possible (listing or a lookup failed).
    pub plan: Option<Plan>,
    /// Changes actually written, in order. Empty in a dry run.
    pub applied: Vec<Change>,
    /// The reason the run failed, if it did. A failed run exits nonzero
    /// after the report and log are written.
    pub error: Option<String>,
}

/// Run the whole observe, plan, and optionally apply cycle against `api`.
///
/// With `apply`, changes are written one at a time; the first write error
/// stops the rest (earlier writes stay, and the next run finishes the job).
/// After writing, the listing is read again and every planned priority is
/// checked, so "applied" in the report means CLIProxyAPI reports the new
/// value, not merely that the PATCH returned 200.
pub fn execute(api: &dyn Management, now: Timestamp, apply: bool) -> Outcome {
    let mut outcome = Outcome::default();
    let accounts = match api.list_accounts() {
        Ok(accounts) => accounts,
        Err(err) => {
            outcome.error = Some(format!("{err:#}"));
            return outcome;
        }
    };

    for account in accounts {
        let (usage, usage_error) = if plan::is_managed(&account) {
            match api.claude_usage(&account.auth_index) {
                Ok(usage) => (Some(usage), None),
                Err(err) => (None, Some(format!("{err:#}"))),
            }
        } else {
            (None, None)
        };
        let observed = Observed { account, usage };
        let flags = plan::flags(&observed, now);
        outcome.rows.push(Row {
            observed,
            usage_error,
            flags,
        });
    }

    // Named, because one enabled account whose login died (a revoked
    // refresh token answers every lookup with 401) freezes the whole pool
    // until someone disables or re-logs that account; whoever reads this
    // needs to know which one.
    let failed: Vec<&str> = outcome
        .rows
        .iter()
        .filter(|r| r.usage_error.is_some())
        .map(|r| r.observed.account.name.as_str())
        .collect();
    if !failed.is_empty() {
        outcome.error = Some(format!(
            "usage lookup failed for {}; no priorities changed",
            failed.join(", ")
        ));
        return outcome;
    }

    // Nothing to manage is not a success: every account disabled, or a
    // listing whose shape kd no longer understands, must reach the watcher
    // as a failed run rather than "already in order".
    if !outcome
        .rows
        .iter()
        .any(|r| plan::is_managed(&r.observed.account))
    {
        outcome.error = Some("no enabled Claude accounts found; nothing to manage".to_owned());
        return outcome;
    }

    let observed: Vec<Observed> = outcome.rows.iter().map(|r| r.observed.clone()).collect();
    let plan = plan::plan(&observed);
    if apply && !plan.changes.is_empty() {
        for change in &plan.changes {
            if let Err(err) = api.set_priority(&change.name, change.to) {
                outcome.error = Some(format!("{err:#}"));
                break;
            }
            outcome.applied.push(change.clone());
        }
        if outcome.error.is_none() {
            outcome.error = verify(api, &plan).err().map(|e| format!("{e:#}"));
        }
    }
    outcome.plan = Some(plan);
    outcome
}

/// Re-read the listing and confirm every managed account now carries its
/// planned priority.
fn verify(api: &dyn Management, plan: &Plan) -> anyhow::Result<()> {
    let accounts = api
        .list_accounts()
        .context("re-reading auth files to verify")?;
    for (name, target) in &plan.targets {
        match accounts.iter().find(|a| &a.name == name) {
            Some(a) if a.priority == *target => {}
            Some(a) => bail!("{name}: priority is {} after writing {target}", a.priority),
            None => bail!("{name}: missing from auth files after writing"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Command entry point
// ---------------------------------------------------------------------------

/// Arguments already resolved to concrete values; see
/// [`super::ManagePrioritiesArgs`] for the user-facing flags.
#[derive(Debug)]
pub struct Settings {
    pub url: String,
    pub key_file: PathBuf,
    pub log_file: PathBuf,
    pub apply: bool,
}

/// Run one cycle, print the report, append the log line, and turn a failed
/// run into a nonzero exit. The report and log are written even for a
/// failed run, including one that never reached the proxy (unreadable key
/// file, rejected `--url`): a watcher needs the evidence most exactly then,
/// and an empty log would look like a timer that stopped firing.
pub fn run(settings: Settings) -> anyhow::Result<()> {
    let now = Timestamp::now();
    let client = read_key_file(&settings.key_file).and_then(|key| Client::new(&settings.url, key));
    let outcome = match client {
        Ok(client) => execute(&client, now, settings.apply),
        Err(err) => Outcome {
            error: Some(format!("{err:#}")),
            ..Outcome::default()
        },
    };

    // A closed stdout (a watcher that stopped reading) must not abort the
    // run before the log line is written; print! would panic on EPIPE.
    let _ = std::io::stdout()
        .write_all(render(&outcome, settings.apply, now, &TimeZone::system()).as_bytes());
    for row in &outcome.rows {
        for flag in row.flags.iter().filter(|f| f.is_warning()) {
            warn!("{}: {}", row.observed.account.name, flag.as_str());
        }
    }
    let line = log_line(&outcome, settings.apply, now, &loggable_url(&settings.url));
    append_log(&settings.log_file, &line)
        .with_context(|| format!("writing log {}", settings.log_file.display()))?;
    info!("logged to {}", settings.log_file.display());

    match outcome.error {
        Some(error) => bail!(error),
        None => Ok(()),
    }
}

/// The URL as it may appear in the log. A URL with userinfo is refused by
/// the client, but it would still carry whatever credentials were typed
/// into it, so the log gets a placeholder instead.
fn loggable_url(url: &str) -> String {
    if url.contains('@') {
        "<rejected: contains credentials>".to_owned()
    } else {
        url.to_owned()
    }
}

/// Where the log goes by default: `$XDG_STATE_HOME/kd/`, falling back to
/// `~/.local/state/kd/` as the XDG spec says. A relative or empty
/// `XDG_STATE_HOME` is ignored, also per the spec. Values are passed in
/// rather than read here so tests do not touch the process environment.
pub fn default_log_file(xdg_state_home: Option<&OsStr>, home: &Path) -> PathBuf {
    let base = match xdg_state_home {
        Some(dir) if Path::new(dir).is_absolute() => PathBuf::from(dir),
        _ => home.join(".local").join("state"),
    };
    base.join("kd").join("cli-proxy-api-priorities.jsonl")
}

/// Where kd devbox bootstrap keeps CLIProxyAPI's generated keys.
pub fn default_key_file(home: &Path) -> PathBuf {
    home.join(".config").join("cliproxy").join("secrets.env")
}

fn read_key_file(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading key file {}", path.display()))?;
    parse_key_file(&text).with_context(|| format!("key file {}", path.display()))
}

/// Accept either bootstrap's `secrets.env` (the key is the value of
/// `CLIPROXY_MANAGEMENT_KEY=`) or a file holding only the key, which is
/// what a copied key on another machine usually looks like. Anything else
/// is an error, so a wrong file is never sent as a key.
pub fn parse_key_file(text: &str) -> anyhow::Result<String> {
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("CLIPROXY_MANAGEMENT_KEY=") {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            if value.is_empty() {
                bail!("CLIPROXY_MANAGEMENT_KEY is empty");
            }
            return Ok(value.to_owned());
        }
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match lines.as_slice() {
        [key] if !key.contains('=') && !key.contains(char::is_whitespace) => Ok((*key).to_owned()),
        _ => bail!("expected CLIPROXY_MANAGEMENT_KEY=<key> or a single line holding only the key"),
    }
}

// ---------------------------------------------------------------------------
// Report and log
// ---------------------------------------------------------------------------

/// Human-readable report: one line per account in planned order (unmanaged
/// accounts last), then the outcome. Times are shown in `tz` with the hours
/// remaining, since "resets in 31h" is what a reader actually wants.
pub fn render(outcome: &Outcome, apply: bool, now: Timestamp, tz: &TimeZone) -> String {
    let mut out = String::new();
    let mut rows: Vec<&Row> = outcome.rows.iter().collect();
    let target = |r: &Row| {
        outcome
            .plan
            .as_ref()
            .and_then(|p| p.target(&r.observed.account.name))
    };
    rows.sort_by_key(|r| {
        (
            std::cmp::Reverse(target(r).unwrap_or(i64::MIN)),
            r.observed.account.name.clone(),
        )
    });
    for row in rows {
        let account = &row.observed.account;
        let priority = match target(row) {
            Some(t) if t != account.priority => format!("{} -> {t}", account.priority),
            Some(t) => format!("{t}"),
            None => format!("{} (unmanaged)", account.priority),
        };
        let who = account.email.as_deref().unwrap_or(&account.name);
        out.push_str(&format!("{priority:<16} {who} [{}]\n", account.name));
        if let Some(usage) = &row.observed.usage {
            out.push_str(&format!(
                "                 7d {}  5h {}\n",
                window_text(usage.seven_day, now, tz),
                window_text(usage.five_hour, now, tz)
            ));
        }
        if let Some(error) = &row.usage_error {
            out.push_str(&format!("                 usage lookup failed: {error}\n"));
        }
        if !row.flags.is_empty() {
            let names: Vec<&str> = row.flags.iter().map(|f| f.as_str()).collect();
            out.push_str(&format!("                 flags: {}\n", names.join(", ")));
        }
    }
    let changes = outcome.plan.as_ref().map_or(0, |p| p.changes.len());
    let summary = match (&outcome.error, apply) {
        (Some(error), _) => format!("error: {error}"),
        (None, false) if changes == 0 => "dry run: priorities already in order".to_owned(),
        (None, false) => {
            format!(
                "dry run: would change {changes} priority value(s); rerun with --apply to write"
            )
        }
        (None, true) if changes == 0 => "priorities already in order; nothing written".to_owned(),
        (None, true) => format!("applied and verified {} change(s)", outcome.applied.len()),
    };
    out.push_str(&summary);
    out.push('\n');
    out
}

fn window_text(window: Window, now: Timestamp, tz: &TimeZone) -> String {
    let used = window
        .utilization
        .map_or("?".to_owned(), |u| format!("{u:.0}%"));
    let resets = match window.resets_at {
        None => "not started".to_owned(),
        Some(ts) => {
            let hours = (ts.as_second() - now.as_second()) as f64 / 3600.0;
            format!(
                "resets {} (in {hours:.1}h)",
                ts.to_zoned(tz.clone()).strftime("%a %Y-%m-%d %H:%M %Z")
            )
        }
    };
    format!("{used} used, {resets}")
}

/// One JSON object per run. It records what CLIProxyAPI and Anthropic
/// reported and what kd decided, which is what a later reader needs to
/// judge the policy or spot a stuck cooldown across runs. It holds account
/// names and emails, never keys or tokens.
pub fn log_line(outcome: &Outcome, apply: bool, now: Timestamp, url: &str) -> Value {
    let accounts: Vec<Value> = outcome
        .rows
        .iter()
        .map(|row| {
            let a = &row.observed.account;
            let window = |w: &Window| {
                json!({
                    "utilization": w.utilization,
                    "resets_at": w.resets_at.map(|t| t.to_string()),
                })
            };
            json!({
                "name": a.name,
                "email": a.email,
                "provider": a.provider,
                "managed": plan::is_managed(a),
                "disabled": a.disabled,
                "unavailable": a.unavailable,
                "status": a.status,
                "status_message": a.status_message,
                "priority_before": a.priority,
                "priority_planned": outcome.plan.as_ref().and_then(|p| p.target(&a.name)),
                "five_hour": row.observed.usage.as_ref().map(|u| window(&u.five_hour)),
                "seven_day": row.observed.usage.as_ref().map(|u| window(&u.seven_day)),
                "next_retry_after": a.next_retry_after.map(|t| t.to_string()),
                "cooldowns": a.cooldowns.iter().map(|c| json!({
                    "scope": c.scope,
                    "model_key": c.model_key,
                    "reason": c.reason,
                    "retry_at": c.retry_at.map(|t| t.to_string()),
                    "http_status": c.http_status,
                })).collect::<Vec<_>>(),
                "recent_success": a.recent_success,
                "recent_failed": a.recent_failed,
                "usage_error": row.usage_error,
                "flags": row.flags.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let change = |c: &Change| json!({"name": c.name, "from": c.from, "to": c.to});
    json!({
        "time": now.to_string(),
        "mode": if apply { "apply" } else { "dry-run" },
        "url": url,
        "accounts": accounts,
        "planned_changes": outcome.plan.as_ref().map(|p| p.changes.iter().map(change).collect::<Vec<_>>()),
        "applied_changes": outcome.applied.iter().map(change).collect::<Vec<_>>(),
        "error": outcome.error,
    })
}

/// Append one line, creating the file 0600 and its directory 0700 when
/// missing: the log names accounts and their usage. An existing file keeps
/// its mode. Each run writes one complete line with a single `write`, so a
/// reader never sees a half line from a finished run.
fn append_log(path: &Path, line: &Value) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let mut text = line.to_string();
    text.push('\n');
    file.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn account(name: &str, priority: i64) -> Account {
        Account {
            name: name.to_owned(),
            auth_index: format!("idx-{name}"),
            provider: "claude".to_owned(),
            email: Some(format!("{name}@example.com")),
            priority,
            disabled: false,
            unavailable: false,
            status: "active".to_owned(),
            status_message: String::new(),
            next_retry_after: None,
            cooldowns: Vec::new(),
            recent_success: 0,
            recent_failed: 0,
        }
    }

    fn usage(reset: &str) -> Usage {
        Usage {
            five_hour: Window {
                utilization: Some(5.0),
                resets_at: Some(ts("2026-09-25T12:00:00Z")),
            },
            seven_day: Window {
                utilization: Some(30.0),
                resets_at: Some(ts(reset)),
            },
        }
    }

    /// In-memory CLIProxyAPI: a mutable account list, canned usage per
    /// auth index (an `Err` string simulates a failed lookup), and a record
    /// of every write. `ignore_writes` simulates a PATCH that returns 200
    /// but does not take effect, which verification must catch.
    #[derive(Default)]
    struct Fake {
        accounts: RefCell<Vec<Account>>,
        usage: HashMap<String, Result<Usage, String>>,
        writes: RefCell<Vec<(String, i64)>>,
        fail_list: bool,
        ignore_writes: bool,
    }

    impl Management for Fake {
        fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
            if self.fail_list {
                bail!("HTTP 401");
            }
            Ok(self.accounts.borrow().clone())
        }
        fn claude_usage(&self, auth_index: &str) -> anyhow::Result<Usage> {
            match self.usage.get(auth_index) {
                Some(Ok(u)) => Ok(*u),
                Some(Err(e)) => bail!("{e}"),
                None => bail!("unexpected lookup for {auth_index}"),
            }
        }
        fn set_priority(&self, name: &str, priority: i64) -> anyhow::Result<()> {
            self.writes.borrow_mut().push((name.to_owned(), priority));
            if !self.ignore_writes {
                for a in self.accounts.borrow_mut().iter_mut() {
                    if a.name == name {
                        a.priority = priority;
                    }
                }
            }
            Ok(())
        }
    }

    fn fake_pool() -> Fake {
        let mut disabled = account("off", 3);
        disabled.disabled = true;
        Fake {
            accounts: RefCell::new(vec![account("late", 20), account("soon", 10), disabled]),
            usage: HashMap::from([
                ("idx-late".to_owned(), Ok(usage("2026-09-30T00:00:00Z"))),
                ("idx-soon".to_owned(), Ok(usage("2026-09-26T00:00:00Z"))),
            ]),
            ..Fake::default()
        }
    }

    const NOW: &str = "2026-09-25T08:00:00Z";

    /// Dry run is the default and the mode used against live proxies during
    /// testing, so it must never write, while still reporting the plan.
    #[test]
    fn dry_run_plans_but_never_writes() {
        let fake = fake_pool();
        let outcome = execute(&fake, ts(NOW), false);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.writes.borrow().is_empty());
        assert_eq!(outcome.plan.unwrap().changes.len(), 2);
        assert!(outcome.applied.is_empty());
    }

    /// Apply writes exactly the planned changes, leaves the disabled
    /// account alone (it was never even looked up), and verifies by
    /// re-reading.
    #[test]
    fn apply_writes_planned_changes_and_verifies() {
        let fake = fake_pool();
        let outcome = execute(&fake, ts(NOW), true);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            *fake.writes.borrow(),
            vec![("soon".to_owned(), 20), ("late".to_owned(), 10)]
        );
        assert_eq!(outcome.applied.len(), 2);
        let again = execute(&fake, ts(NOW), true);
        assert!(again.plan.unwrap().changes.is_empty());
        assert_eq!(
            fake.writes.borrow().len(),
            2,
            "a settled pool writes nothing"
        );
    }

    /// The fail-safe rule: one failed usage lookup means zero writes, an
    /// error for the run, and the failure visible in the row.
    #[test]
    fn any_failed_lookup_blocks_all_writes() {
        let mut fake = fake_pool();
        fake.usage.insert(
            "idx-late".to_owned(),
            Err("usage endpoint returned HTTP 429".to_owned()),
        );
        let outcome = execute(&fake, ts(NOW), true);
        assert!(fake.writes.borrow().is_empty());
        assert!(outcome.plan.is_none());
        assert_eq!(
            outcome.error.as_deref(),
            Some("usage lookup failed for late; no priorities changed")
        );
        assert!(
            outcome
                .rows
                .iter()
                .any(|r| r.usage_error.as_deref() == Some("usage endpoint returned HTTP 429"))
        );
    }

    /// A pool with nothing to manage (all disabled) fails the run instead of
    /// reporting "already in order".
    #[test]
    fn no_managed_accounts_is_an_error() {
        let mut off = account("off", 1);
        off.disabled = true;
        let fake = Fake {
            accounts: RefCell::new(vec![off]),
            ..Fake::default()
        };
        let outcome = execute(&fake, ts(NOW), true);
        assert!(
            outcome
                .error
                .unwrap()
                .contains("no enabled Claude accounts")
        );
        assert!(fake.writes.borrow().is_empty());
    }

    /// Apply-mode summaries: what a timer's output says after a real write,
    /// a no-op, and a failure.
    #[test]
    fn apply_mode_summaries() {
        let now = ts(NOW);
        let fake = fake_pool();
        let done = execute(&fake, now, true);
        assert!(
            render(&done, true, now, &TimeZone::UTC)
                .ends_with("applied and verified 2 change(s)\n")
        );
        let noop = execute(&fake, now, true);
        assert!(
            render(&noop, true, now, &TimeZone::UTC)
                .ends_with("priorities already in order; nothing written\n")
        );
        let failed = Outcome {
            error: Some("boom".to_owned()),
            ..Outcome::default()
        };
        assert_eq!(render(&failed, true, now, &TimeZone::UTC), "error: boom\n");
    }

    /// A listing failure (bad key, proxy down) is an error with no rows.
    #[test]
    fn listing_failure_is_reported() {
        let fake = Fake {
            fail_list: true,
            ..Fake::default()
        };
        let outcome = execute(&fake, ts(NOW), true);
        assert!(outcome.rows.is_empty());
        assert_eq!(outcome.error.as_deref(), Some("HTTP 401"));
    }

    /// A write that returns success but does not take effect must not be
    /// reported as applied-and-verified.
    #[test]
    fn verification_catches_writes_that_did_not_stick() {
        let fake = Fake {
            ignore_writes: true,
            ..fake_pool()
        };
        let outcome = execute(&fake, ts(NOW), true);
        let error = outcome.error.unwrap();
        assert!(error.contains("after writing"), "{error}");
    }

    /// The report shows transitions, usage, and a dry-run hint; the log
    /// line carries the same decision in machine-readable form.
    #[test]
    fn report_and_log_describe_the_decision() {
        let fake = fake_pool();
        let now = ts(NOW);
        let outcome = execute(&fake, now, false);
        let text = render(&outcome, false, now, &TimeZone::UTC);
        assert!(text.contains("10 -> 20"), "{text}");
        assert!(text.contains("3 (unmanaged)"), "{text}");
        assert!(
            text.contains("30% used, resets Sat 2026-09-26 00:00 UTC (in 16.0h)"),
            "{text}"
        );
        assert!(text.ends_with("rerun with --apply to write\n"), "{text}");
        let soon = text.find("soon@").unwrap();
        let late = text.find("late@").unwrap();
        assert!(soon < late, "planned order first: {text}");

        let line = log_line(&outcome, false, now, "http://127.0.0.1:8317");
        assert_eq!(line["mode"], "dry-run");
        assert_eq!(line["planned_changes"][0]["name"], "soon");
        let soon_row = line["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "soon")
            .unwrap();
        assert_eq!(soon_row["priority_planned"], 20);
        assert_eq!(soon_row["seven_day"]["resets_at"], "2026-09-26T00:00:00Z");
    }

    /// A run that cannot even reach the proxy still leaves a log line, so a
    /// watcher can tell a broken setup from a timer that stopped. A URL
    /// carrying credentials must not be copied into the log.
    #[test]
    fn setup_failures_are_logged_and_fail_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let log_file = dir.path().join("log.jsonl");
        let key_file = dir.path().join("key");
        std::fs::write(&key_file, "k\n").unwrap();
        for (url, key_file, expected) in [
            (
                "http://127.0.0.1:8317",
                dir.path().join("missing"),
                "reading key file",
            ),
            (
                "http://u:pw@10.0.0.1:8317",
                key_file.clone(),
                "must not contain credentials",
            ),
        ] {
            let result = run(Settings {
                url: url.to_owned(),
                key_file,
                log_file: log_file.clone(),
                apply: false,
            });
            assert!(result.unwrap_err().to_string().contains(expected));
        }
        let text = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0]["error"]
                .as_str()
                .unwrap()
                .contains("reading key file")
        );
        assert_eq!(lines[1]["url"], "<rejected: contains credentials>");
        assert!(!text.contains("pw@"));
    }

    /// Both the env-style file bootstrap writes and a bare copied key work;
    /// anything ambiguous is refused rather than sent as a key.
    #[test]
    fn key_file_formats() {
        assert_eq!(
            parse_key_file("CLIPROXY_CLIENT_KEY=sk-cpa-x\nCLIPROXY_MANAGEMENT_KEY=abc123\n")
                .unwrap(),
            "abc123"
        );
        assert_eq!(
            parse_key_file("CLIPROXY_MANAGEMENT_KEY=\"q\"\n").unwrap(),
            "q"
        );
        assert_eq!(parse_key_file("\n  abc123  \n").unwrap(), "abc123");
        for bad in [
            "",
            "CLIPROXY_MANAGEMENT_KEY=\n",
            "CLIPROXY_CLIENT_KEY=sk-cpa-x\n",
            "a\nb\n",
            "two words",
        ] {
            assert!(parse_key_file(bad).is_err(), "{bad:?}");
        }
    }

    /// XDG handling matches the spec: absolute `XDG_STATE_HOME` wins,
    /// empty or relative falls back to `~/.local/state`.
    #[test]
    fn default_log_file_follows_xdg() {
        let home = Path::new("/home/me");
        assert_eq!(
            default_log_file(Some(OsStr::new("/state")), home),
            PathBuf::from("/state/kd/cli-proxy-api-priorities.jsonl")
        );
        for fallback in [None, Some(OsStr::new("")), Some(OsStr::new("rel"))] {
            assert_eq!(
                default_log_file(fallback, home),
                PathBuf::from("/home/me/.local/state/kd/cli-proxy-api-priorities.jsonl")
            );
        }
    }

    /// The log holds account names and usage, so a new file and directory
    /// must be private; each run adds exactly one parseable line.
    #[test]
    fn log_is_private_and_one_line_per_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/kd/log.jsonl");
        append_log(&path, &json!({"run": 1})).unwrap();
        append_log(&path, &json!({"run": 2})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let runs: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(runs, vec![json!({"run": 1}), json!({"run": 2})]);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    // -----------------------------------------------------------------------
    // The real HTTP client against a local stub server
    // -----------------------------------------------------------------------

    /// One request as the stub saw it.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// Minimal HTTP/1.1 stub on 127.0.0.1: one request per connection
    /// (`Connection: close`), answers from `respond`, records every request.
    /// It exists to pin the wire format [`Client`] sends (paths, methods,
    /// auth header, JSON bodies), which the in-memory fake cannot see.
    fn stub_server(
        respond: impl Fn(&Seen) -> (u16, String) + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        use std::io::{BufRead, BufReader, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let mut length = 0;
                let mut authorization = None;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => authorization = Some(value.trim().to_owned()),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let req = Seen {
                    method,
                    path,
                    authorization,
                    body: String::from_utf8(body).unwrap(),
                };
                let (status, text) = respond(&req);
                log.lock().unwrap().push(req);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (base, seen)
    }

    const AUTH_FILES: &str = r#"{"files":[
      {"name":"late.json","auth_index":"i-late","provider":"claude","priority":20},
      {"name":"soon.json","auth_index":"i-soon","provider":"claude","priority":10}]}"#;

    fn usage_call(reset: &str) -> String {
        let body = json!({"seven_day": {"utilization": 30.0, "resets_at": reset}}).to_string();
        json!({"status_code": 200, "header": {}, "body": body}).to_string()
    }

    /// A dry run over real HTTP sends only reads: GET auth-files and one
    /// api-call per account, each carrying the key and an oauth/usage
    /// request with `$TOKEN$` left for the proxy to fill in.
    #[test]
    fn client_dry_run_sends_only_reads_with_expected_shapes() {
        let (base, seen) = stub_server(|req| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v0/management/auth-files") => (200, AUTH_FILES.to_owned()),
            ("POST", "/v0/management/api-call") if req.body.contains("i-soon") => {
                (200, usage_call("2026-09-26T00:00:00Z"))
            }
            ("POST", "/v0/management/api-call") => (200, usage_call("2026-09-30T00:00:00Z")),
            _ => (500, "{}".to_owned()),
        });
        let client = Client::new(&base, "secret-key".to_owned()).unwrap();
        let outcome = execute(&client, ts(NOW), false);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.plan.unwrap().changes.len(), 2);

        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|r| r.method != "PATCH"), "{seen:?}");
        assert!(
            seen.iter()
                .all(|r| r.authorization.as_deref() == Some("Bearer secret-key"))
        );
        let call: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!(call["method"], "GET");
        assert_eq!(call["url"], super::super::api::USAGE_URL);
        assert_eq!(call["header"]["Authorization"], "Bearer $TOKEN$");
    }

    /// Apply over real HTTP sends the documented PATCH body. The stub never
    /// changes its listing, so verification must then fail, which also
    /// proves the re-read happens.
    #[test]
    fn client_apply_sends_priority_patches() {
        let (base, seen) = stub_server(|req| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v0/management/auth-files") => (200, AUTH_FILES.to_owned()),
            ("POST", "/v0/management/api-call") if req.body.contains("i-soon") => {
                (200, usage_call("2026-09-26T00:00:00Z"))
            }
            ("POST", "/v0/management/api-call") => (200, usage_call("2026-09-30T00:00:00Z")),
            ("PATCH", "/v0/management/auth-files/fields") => (200, r#"{"status":"ok"}"#.to_owned()),
            _ => (500, "{}".to_owned()),
        });
        let client = Client::new(&base, "k".to_owned()).unwrap();
        let outcome = execute(&client, ts(NOW), true);
        assert!(outcome.error.unwrap().contains("after writing"));
        let patches: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| serde_json::from_str(&r.body).unwrap())
            .collect();
        assert_eq!(
            patches,
            vec![
                json!({"name": "soon.json", "priority": 20}),
                json!({"name": "late.json", "priority": 10})
            ]
        );
    }

    /// A rejected key surfaces as an actionable error, and the key itself
    /// never appears in the message.
    #[test]
    fn client_reports_rejected_key_without_leaking_it() {
        let (base, _) = stub_server(|_| (401, "{}".to_owned()));
        let client = Client::new(&base, "super-secret".to_owned()).unwrap();
        let outcome = execute(&client, ts(NOW), false);
        let error = outcome.error.unwrap();
        assert!(error.contains("management key was rejected"), "{error}");
        assert!(!error.contains("super-secret"));
    }
}
