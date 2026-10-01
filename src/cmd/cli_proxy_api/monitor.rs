//! `kd cli-proxy-api monitor run`: a foreground loop that keeps
//! CLIProxyAPI's Claude accounts ordered by weekly reset time, so quota about
//! to expire is spent first, and records what it saw on every wake.
//!
//! Every wake runs the same pass: list accounts, look up each managed
//! account's quota windows, plan priorities (see [`super::plan`]), write and
//! verify the changes, print a report, and append one versioned JSON line to
//! the log. The pass is stateless: it starts from what the proxy and
//! Anthropic report now, so a crashed or skipped wake is harmless and the
//! next one converges. Appending to the log and writing priorities are the
//! loop's only side effects; the log doubles as the usage history
//! `overview` charts.
//!
//! Fail-safe rule: if any managed account's usage lookup fails, nothing is
//! written. A partial picture could promote the wrong account, and leaving
//! the previous priorities in place costs at most one interval.
//!
//! The one piece of state carried between wakes is a rejected management
//! key (see [`Monitor`]): CLIProxyAPI bans an address for 30 minutes after
//! five bad keys, so a loop that kept retrying a stale key would keep the
//! user locked out of the web panel indefinitely.

use super::api::{Client, is_key_rejection};
use super::plan::{self, Account, Change, Flag, Observed, Plan, Usage, Window};
use anyhow::{Context, bail};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Schema version written as `"v"` on every log line. Bump it whenever a
/// field changes meaning or shape, so readers can skip lines they do not
/// understand instead of misreading them.
pub const LOG_SCHEMA_VERSION: u64 = 1;

/// Regular wake interval. Short enough that a usage history built from the
/// log has useful resolution, long enough that the usage lookups stay
/// negligible for Anthropic and the proxy.
pub const INTERVAL: SignedDuration = SignedDuration::from_mins(15);

/// Extra wake delay after a known window reset. Anthropic's reset times
/// jitter by a fraction of a second and its view takes a moment to settle;
/// the margin only has to catch the common case, because the next regular
/// wake catches the rest.
pub const RESET_GRACE: SignedDuration = SignedDuration::from_secs(60);

/// The operations a pass needs from CLIProxyAPI. [`Client`] implements it
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
    /// blocks all writes for the pass.
    pub usage_error: Option<String>,
    pub flags: Vec<Flag>,
}

/// Everything one pass learned and did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub rows: Vec<Row>,
    /// `None` when planning was not possible (listing or a lookup failed).
    pub plan: Option<Plan>,
    /// Changes actually written, in order.
    pub applied: Vec<Change>,
    /// The reason the pass failed, if it did.
    pub error: Option<String>,
    /// The management API rejected the key during this pass. The loop then
    /// stops calling it until the key file holds a different key.
    pub key_rejected: bool,
    /// The pass made no API calls because the key file still holds a key
    /// the management API rejected earlier.
    pub paused: bool,
}

impl Outcome {
    /// A pass that failed before reaching the proxy.
    fn failed(error: String) -> Self {
        Outcome {
            error: Some(error),
            ..Outcome::default()
        }
    }

    /// Record a failure, noting whether it was the key being rejected.
    fn fail(&mut self, err: &anyhow::Error) {
        self.key_rejected |= is_key_rejection(err);
        self.error = Some(format!("{err:#}"));
    }
}

/// Run one observe, plan, apply, and verify pass against `api`.
///
/// Changes are written one at a time; the first write error stops the rest
/// (earlier writes stay, and the next pass finishes the job). After writing,
/// the listing is read again and every planned priority is checked, so
/// "applied" in the report means CLIProxyAPI reports the new value, not
/// merely that the PATCH returned 200.
pub fn execute(api: &dyn Management, now: Timestamp) -> Outcome {
    let mut outcome = Outcome::default();
    let accounts = match api.list_accounts() {
        Ok(accounts) => accounts,
        Err(err) => {
            outcome.fail(&err);
            return outcome;
        }
    };

    for account in accounts {
        // Once the key has been rejected, every further call with it is
        // another strike toward CLIProxyAPI's ban, so the remaining accounts
        // are recorded as not looked up instead. That is still a failed
        // lookup, so the fail-safe rule below blocks all writes.
        let (usage, usage_error) = if plan::is_managed(&account) && outcome.key_rejected {
            (
                None,
                Some("not looked up: the management key was rejected".to_owned()),
            )
        } else if plan::is_managed(&account) {
            match api.claude_usage(&account.auth_index) {
                Ok(usage) => (Some(usage), None),
                Err(err) => {
                    outcome.key_rejected |= is_key_rejection(&err);
                    (None, Some(format!("{err:#}")))
                }
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
    // listing whose shape kd no longer understands, must show up as a
    // failed pass rather than "already in order".
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
    if !plan.changes.is_empty() {
        for change in &plan.changes {
            if let Err(err) = api.set_priority(&change.name, change.to) {
                outcome.fail(&err);
                break;
            }
            outcome.applied.push(change.clone());
        }
        if outcome.error.is_none()
            && let Err(err) = verify(api, &plan)
        {
            outcome.fail(&err);
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

/// When the loop should wake next: the regular interval, or shortly after
/// the earliest known window reset if that comes first, so a weekly
/// rollover reorders the pool within a minute instead of up to an interval
/// later. Both windows of every looked-up account count. Only candidates
/// still in the future qualify, so a reset Anthropic has not yet moved past
/// (or a stale one) cannot make the loop spin.
pub fn next_wake(outcome: &Outcome, now: Timestamp) -> Timestamp {
    let regular = now + INTERVAL;
    outcome
        .rows
        .iter()
        .filter_map(|r| r.observed.usage)
        .flat_map(|u| [u.five_hour.resets_at, u.seven_day.resets_at])
        .flatten()
        .filter_map(|reset| reset.checked_add(RESET_GRACE).ok())
        .filter(|wake| *wake > now)
        .fold(regular, Timestamp::min)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Opens a management client for a key. Injected so the loop's key
/// handling can be tested without a server.
pub type Connect<'a> = dyn Fn(&str) -> anyhow::Result<Box<dyn Management>> + 'a;

/// State carried from one wake to the next: only the key the management
/// API last rejected, if any. The key file is re-read on every wake, and
/// while it still yields that key no request is sent; any other key (or a
/// fixed file) resumes normal passes on the next wake. The key lives only
/// in memory and is never printed or logged.
#[derive(Default)]
pub struct Monitor {
    rejected_key: Option<String>,
}

impl Monitor {
    /// One wake: read the key, then run a pass unless the key is the one
    /// already rejected. `key` is the result of reading the key file now.
    pub fn wake(
        &mut self,
        key: anyhow::Result<String>,
        connect: &Connect<'_>,
        now: Timestamp,
    ) -> Outcome {
        let key = match key {
            Ok(key) => key,
            Err(err) => return Outcome::failed(format!("{err:#}")),
        };
        if self.rejected_key.as_deref() == Some(key.as_str()) {
            return Outcome {
                error: Some(
                    "the management key was rejected earlier; not calling CLIProxyAPI until the key file changes"
                        .to_owned(),
                ),
                paused: true,
                ..Outcome::default()
            };
        }
        let outcome = match connect(&key) {
            Ok(api) => execute(api.as_ref(), now),
            Err(err) => Outcome::failed(format!("{err:#}")),
        };
        self.rejected_key = outcome.key_rejected.then_some(key);
        outcome
    }
}

/// Resolved inputs for `monitor run`; see [`super::RunArgs`] for the flags.
#[derive(Debug)]
pub struct Settings {
    pub url: String,
    pub key_file: PathBuf,
    pub log_file: PathBuf,
}

/// Run the loop forever. Only a setup error the loop cannot recover from
/// (an invalid `--url`) ends it; every per-pass failure is reported, logged,
/// and retried at the next wake, because under systemd an exit would just
/// restart the process and send the same failing requests again.
pub fn run(settings: Settings) -> anyhow::Result<()> {
    // Validates the URL once up front; per-wake clients are built the same
    // way, so they cannot fail on it later.
    Client::new(&settings.url, String::new())?;
    info!(
        "monitoring {}; logging to {}",
        settings.url,
        settings.log_file.display()
    );
    let url = settings.url.clone();
    let connect = move |key: &str| -> anyhow::Result<Box<dyn Management>> {
        Ok(Box::new(Client::new(&url, key.to_owned())?))
    };
    let mut monitor = Monitor::default();
    loop {
        let now = Timestamp::now();
        let outcome = monitor.wake(read_key_file(&settings.key_file), &connect, now);
        let wake_at = next_wake(&outcome, now);
        report(&settings, &outcome, now, wake_at);
        let wait = Timestamp::now().duration_until(wake_at);
        if let Ok(wait) = std::time::Duration::try_from(wait) {
            std::thread::sleep(wait);
        }
    }
}

/// Print the pass report and append its log line. A failed log write is a
/// warning, not an exit: the priorities still need tending.
fn report(settings: &Settings, outcome: &Outcome, now: Timestamp, wake_at: Timestamp) {
    let tz = TimeZone::system();
    let mut text = render(outcome, now, &tz);
    text.push_str(&format!(
        "next wake {}\n",
        wake_at.to_zoned(tz).strftime("%a %Y-%m-%d %H:%M:%S %Z")
    ));
    // A closed stdout must not kill the loop; print! would panic on EPIPE.
    let _ = std::io::stdout().write_all(text.as_bytes());
    for row in &outcome.rows {
        for flag in row.flags.iter().filter(|f| f.is_warning()) {
            warn!("{}: {}", row.observed.account.name, flag.as_str());
        }
    }
    let line = log_line(outcome, now, &settings.url);
    if let Err(err) = append_log(&settings.log_file, &line) {
        warn!("writing log {}: {err:#}", settings.log_file.display());
    }
}

/// Where kd devbox bootstrap keeps CLIProxyAPI's generated keys.
pub fn default_key_file(home: &Path) -> PathBuf {
    home.join(".config").join("cliproxy").join("secrets.env")
}

/// The monitor's log, which is also the usage history `overview` reads.
/// The path is fixed rather than following `XDG_STATE_HOME`, because the
/// systemd user manager usually lacks that variable while an interactive
/// shell may set it, and the daemon and `overview` must agree on one file.
pub fn log_file(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("kd")
        .join("cli-proxy-api-monitor.jsonl")
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
/// accounts last), then the outcome. Under systemd this is what the journal
/// shows. Times are shown in `tz` with the hours remaining, since "resets
/// in 31h" is what a reader actually wants.
pub fn render(outcome: &Outcome, now: Timestamp, tz: &TimeZone) -> String {
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
    let summary = match &outcome.error {
        Some(error) if outcome.paused => format!("paused: {error}"),
        Some(error) => format!("error: {error}"),
        None if outcome.applied.is_empty() => "priorities already in order".to_owned(),
        None => format!("applied and verified {} change(s)", outcome.applied.len()),
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

/// One JSON object per wake, schema version [`LOG_SCHEMA_VERSION`]. It
/// records what CLIProxyAPI and Anthropic reported and what kd decided:
/// the usage history `overview` charts, and the evidence a reader needs to
/// judge the policy or spot a stuck cooldown. It holds account names and
/// emails, never keys or tokens.
pub fn log_line(outcome: &Outcome, now: Timestamp, url: &str) -> Value {
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
                "quota": a.quota,
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
        "v": LOG_SCHEMA_VERSION,
        "time": now.to_string(),
        "url": loggable_url(url),
        "accounts": accounts,
        "planned_changes": outcome.plan.as_ref().map(|p| p.changes.iter().map(change).collect::<Vec<_>>()),
        "applied_changes": outcome.applied.iter().map(change).collect::<Vec<_>>(),
        "error": outcome.error,
        "paused": outcome.paused,
    })
}

/// The URL as it may appear in the log. A URL with userinfo is refused at
/// startup, but keep the log safe even if that check ever moves.
fn loggable_url(url: &str) -> String {
    if url.contains('@') {
        "<rejected: contains credentials>".to_owned()
    } else {
        url.to_owned()
    }
}

/// Append one line, creating the file 0600 and its directory 0700 when
/// missing: the log names accounts and their usage. An existing file keeps
/// its mode. Each wake writes one complete line in a single `write_all` on
/// an append-mode file, so lines from finished passes never interleave.
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
    use super::super::api::status_error;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

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
            quota: None,
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

    /// A pass writes exactly the planned changes, leaves the disabled
    /// account alone (it was never even looked up), and verifies by
    /// re-reading.
    #[test]
    fn apply_writes_planned_changes_and_verifies() {
        let fake = fake_pool();
        let outcome = execute(&fake, ts(NOW));
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            *fake.writes.borrow(),
            vec![("soon".to_owned(), 20), ("late".to_owned(), 10)]
        );
        assert_eq!(outcome.applied.len(), 2);
        let again = execute(&fake, ts(NOW));
        assert!(again.plan.unwrap().changes.is_empty());
        assert_eq!(
            fake.writes.borrow().len(),
            2,
            "a settled pool writes nothing"
        );
    }

    /// The fail-safe rule: one failed usage lookup means zero writes, an
    /// error for the pass, and the failure visible in the row.
    #[test]
    fn any_failed_lookup_blocks_all_writes() {
        let mut fake = fake_pool();
        fake.usage.insert(
            "idx-late".to_owned(),
            Err("usage endpoint returned HTTP 429".to_owned()),
        );
        let outcome = execute(&fake, ts(NOW));
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

    /// A management 401 during the usage lookups (the key rotated after the
    /// listing) flags the pass for the pause and stops sending the rejected
    /// key: with a pool of five or more, one pass could otherwise trigger the
    /// very ban the pause exists to avoid.
    #[test]
    fn key_rejected_mid_pass_stops_further_lookups() {
        let mut fake = fake_pool();
        fake.accounts.borrow_mut().push(account("third", 1));
        fake.usage
            .insert("idx-late".to_owned(), Err("REJECT".to_owned()));
        struct Wrap(Fake, Cell<u32>);
        impl Management for Wrap {
            fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
                self.0.list_accounts()
            }
            fn claude_usage(&self, auth_index: &str) -> anyhow::Result<Usage> {
                self.1.set(self.1.get() + 1);
                match self.0.claude_usage(auth_index) {
                    Err(e) if e.to_string() == "REJECT" => {
                        Err(status_error("usage lookup", 401, "{}"))
                    }
                    other => other,
                }
            }
            fn set_priority(&self, name: &str, priority: i64) -> anyhow::Result<()> {
                self.0.set_priority(name, priority)
            }
        }
        let api = Wrap(fake, Cell::new(0));
        let outcome = execute(&api, ts(NOW));
        assert!(outcome.key_rejected);
        assert_eq!(api.1.get(), 1, "no lookup after the rejection");
        assert!(api.0.writes.borrow().is_empty());
        let third = outcome
            .rows
            .iter()
            .find(|r| r.observed.account.name == "third")
            .unwrap();
        assert_eq!(
            third.usage_error.as_deref(),
            Some("not looked up: the management key was rejected")
        );
    }

    /// A pool with nothing to manage (all disabled) fails the pass instead of
    /// reporting "already in order".
    #[test]
    fn no_managed_accounts_is_an_error() {
        let mut off = account("off", 1);
        off.disabled = true;
        let fake = Fake {
            accounts: RefCell::new(vec![off]),
            ..Fake::default()
        };
        let outcome = execute(&fake, ts(NOW));
        assert!(
            outcome
                .error
                .unwrap()
                .contains("no enabled Claude accounts")
        );
        assert!(fake.writes.borrow().is_empty());
    }

    /// Pass summaries: what the journal says after a real write, a no-op, a
    /// failure, and a pause on a rejected key.
    #[test]
    fn pass_summaries() {
        let now = ts(NOW);
        let fake = fake_pool();
        let done = execute(&fake, now);
        assert!(render(&done, now, &TimeZone::UTC).ends_with("applied and verified 2 change(s)\n"));
        let noop = execute(&fake, now);
        assert!(render(&noop, now, &TimeZone::UTC).ends_with("priorities already in order\n"));
        let failed = Outcome::failed("boom".to_owned());
        assert_eq!(render(&failed, now, &TimeZone::UTC), "error: boom\n");
        let paused = Outcome {
            paused: true,
            ..Outcome::failed("key".to_owned())
        };
        assert_eq!(render(&paused, now, &TimeZone::UTC), "paused: key\n");
    }

    /// A listing failure (proxy down, garbled response) is an error with no
    /// rows. An error that only mentions 401 in its text is not a typed key
    /// rejection and must not pause the loop.
    #[test]
    fn listing_failure_is_reported() {
        let fake = Fake {
            fail_list: true,
            ..Fake::default()
        };
        let outcome = execute(&fake, ts(NOW));
        assert!(outcome.rows.is_empty());
        assert_eq!(outcome.error.as_deref(), Some("HTTP 401"));
        assert!(!outcome.key_rejected);
    }

    /// A write that returns success but does not take effect must not be
    /// reported as applied-and-verified.
    #[test]
    fn verification_catches_writes_that_did_not_stick() {
        let fake = Fake {
            ignore_writes: true,
            ..fake_pool()
        };
        let outcome = execute(&fake, ts(NOW));
        let error = outcome.error.unwrap();
        assert!(error.contains("after writing"), "{error}");
    }

    /// The report shows transitions and usage; the log line carries the
    /// same decision in machine-readable form, under the schema version
    /// readers key on, with CLIProxyAPI's raw quota signals alongside.
    #[test]
    fn report_and_log_describe_the_decision() {
        let fake = fake_pool();
        fake.accounts.borrow_mut()[1].quota = Some(json!({"signals": {"x": "0.5"}}));
        let now = ts(NOW);
        let outcome = execute(&fake, now);
        let text = render(&outcome, now, &TimeZone::UTC);
        assert!(text.contains("10 -> 20"), "{text}");
        assert!(text.contains("3 (unmanaged)"), "{text}");
        assert!(
            text.contains("30% used, resets Sat 2026-09-26 00:00 UTC (in 16.0h)"),
            "{text}"
        );
        let soon = text.find("soon@").unwrap();
        let late = text.find("late@").unwrap();
        assert!(soon < late, "planned order first: {text}");

        let line = log_line(&outcome, now, "http://127.0.0.1:8317");
        assert_eq!(line["v"], LOG_SCHEMA_VERSION);
        assert_eq!(line["paused"], false);
        assert_eq!(line["planned_changes"][0]["name"], "soon");
        assert_eq!(line["applied_changes"][0]["name"], "soon");
        let soon_row = line["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "soon")
            .unwrap();
        assert_eq!(soon_row["priority_planned"], 20);
        assert_eq!(soon_row["seven_day"]["resets_at"], "2026-09-26T00:00:00Z");
        assert_eq!(soon_row["quota"]["signals"]["x"], "0.5");
        assert_eq!(
            log_line(&outcome, now, "http://u:pw@h")["url"],
            "<rejected: contains credentials>"
        );
    }

    /// The wake rule: the regular interval, unless a known reset (either
    /// window, any looked-up account) plus the grace period comes sooner.
    /// Candidates already in the past are ignored so a reset Anthropic has
    /// not rolled over yet cannot make the loop spin.
    #[test]
    fn next_wake_prefers_an_imminent_reset() {
        let now = ts(NOW);
        let window = |reset: Option<&str>| Window {
            utilization: Some(1.0),
            resets_at: reset.map(ts),
        };
        let outcome_with = |five: Option<&str>, seven: Option<&str>| Outcome {
            rows: vec![Row {
                observed: Observed {
                    account: account("a", 1),
                    usage: Some(Usage {
                        five_hour: window(five),
                        seven_day: window(seven),
                    }),
                },
                usage_error: None,
                flags: Vec::new(),
            }],
            ..Outcome::default()
        };
        let regular = ts("2026-09-25T08:15:00Z");
        assert_eq!(next_wake(&Outcome::default(), now), regular);
        assert_eq!(next_wake(&outcome_with(None, None), now), regular);
        assert_eq!(
            next_wake(&outcome_with(Some("2026-09-25T08:05:00Z"), None), now),
            ts("2026-09-25T08:06:00Z")
        );
        assert_eq!(
            next_wake(
                &outcome_with(Some("2026-09-25T09:00:00Z"), Some("2026-09-25T08:10:00Z")),
                now
            ),
            ts("2026-09-25T08:11:00Z")
        );
        assert_eq!(
            next_wake(&outcome_with(Some("2026-09-25T07:00:00Z"), None), now),
            regular,
            "a past reset is ignored"
        );
        assert_eq!(
            next_wake(&outcome_with(Some("2026-09-25T07:59:30Z"), None), now),
            ts("2026-09-25T08:00:30Z"),
            "a reset that just passed still gets its grace-period wake"
        );
    }

    /// Fake whose every call fails with a typed management 401, counting the
    /// calls so the test can prove a paused monitor sends nothing.
    struct Rejecting(Rc<Cell<u32>>);

    impl Management for Rejecting {
        fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
            self.0.set(self.0.get() + 1);
            Err(status_error("listing auth files", 401, "{}"))
        }
        fn claude_usage(&self, _: &str) -> anyhow::Result<Usage> {
            unreachable!()
        }
        fn set_priority(&self, _: &str, _: i64) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    /// The 401 pause: after the management API rejects a key, later wakes
    /// with the same key send no request at all (each would count toward
    /// CLIProxyAPI's 30-minute ban), and a different key resumes passes.
    /// An unreadable key file is a failed pass, not a pause.
    #[test]
    fn rejected_key_pauses_until_the_key_changes() {
        let calls = Rc::new(Cell::new(0));
        let rejecting = calls.clone();
        let connect = move |key: &str| -> anyhow::Result<Box<dyn Management>> {
            if key == "good" {
                Ok(Box::new(fake_pool()))
            } else {
                Ok(Box::new(Rejecting(rejecting.clone())))
            }
        };
        let now = ts(NOW);
        let mut monitor = Monitor::default();

        let first = monitor.wake(Ok("stale".to_owned()), &connect, now);
        assert!(first.key_rejected && !first.paused);
        assert!(first.error.unwrap().contains("management key was rejected"));
        assert_eq!(calls.get(), 1);

        let second = monitor.wake(Ok("stale".to_owned()), &connect, now);
        assert!(second.paused);
        assert_eq!(calls.get(), 1, "a paused wake sends nothing");

        let unreadable = monitor.wake(Err(anyhow::anyhow!("reading key file")), &connect, now);
        assert!(!unreadable.paused);
        assert_eq!(unreadable.error.as_deref(), Some("reading key file"));
        assert!(
            monitor.wake(Ok("stale".to_owned()), &connect, now).paused,
            "an unreadable key file in between does not lift the pause"
        );

        let fixed = monitor.wake(Ok("good".to_owned()), &connect, now);
        assert!(fixed.error.is_none(), "{:?}", fixed.error);
        let again = monitor.wake(Ok("stale".to_owned()), &connect, now);
        assert!(
            again.key_rejected,
            "a key is retried once it has been replaced"
        );
        assert_eq!(calls.get(), 2);
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

    /// The log path is fixed under HOME, ignoring `XDG_STATE_HOME`, so the
    /// systemd unit and an interactive `overview` always agree on one file.
    #[test]
    fn log_file_is_fixed_under_home() {
        assert_eq!(
            log_file(Path::new("/home/me")),
            PathBuf::from("/home/me/.local/state/kd/cli-proxy-api-monitor.jsonl")
        );
    }

    /// The log holds account names and usage, so a new file and directory
    /// must be private; each pass adds exactly one parseable line.
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

    /// The same pool already in order: a pass over it must only read.
    const SETTLED_AUTH_FILES: &str = r#"{"files":[
      {"name":"late.json","auth_index":"i-late","provider":"claude","priority":10},
      {"name":"soon.json","auth_index":"i-soon","provider":"claude","priority":20}]}"#;

    fn usage_call(reset: &str) -> String {
        let body = json!({"seven_day": {"utilization": 30.0, "resets_at": reset}}).to_string();
        json!({"status_code": 200, "header": {}, "body": body}).to_string()
    }

    /// A pass over a settled pool sends only reads: GET auth-files and one
    /// api-call per account, each carrying the key and an oauth/usage
    /// request with `$TOKEN$` left for the proxy to fill in.
    #[test]
    fn client_settled_pass_sends_only_reads_with_expected_shapes() {
        let (base, seen) = stub_server(|req| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v0/management/auth-files") => (200, SETTLED_AUTH_FILES.to_owned()),
            ("POST", "/v0/management/api-call") if req.body.contains("i-soon") => {
                (200, usage_call("2026-09-26T00:00:00Z"))
            }
            ("POST", "/v0/management/api-call") => (200, usage_call("2026-09-30T00:00:00Z")),
            _ => (500, "{}".to_owned()),
        });
        let client = Client::new(&base, "secret-key".to_owned()).unwrap();
        let outcome = execute(&client, ts(NOW));
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(outcome.plan.unwrap().changes.is_empty());

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

    /// A pass that reorders sends the documented PATCH body over real HTTP. The stub never
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
        let outcome = execute(&client, ts(NOW));
        let error = outcome.error.unwrap();
        assert!(error.contains("after writing"), "{error}");
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

    /// A rejected key surfaces as an actionable error flagged for the pause,
    /// and the key itself never appears in the message.
    #[test]
    fn client_reports_rejected_key_without_leaking_it() {
        let (base, _) = stub_server(|_| (401, "{}".to_owned()));
        let client = Client::new(&base, "super-secret".to_owned()).unwrap();
        let outcome = execute(&client, ts(NOW));
        assert!(outcome.key_rejected);
        let error = outcome.error.unwrap();
        assert!(error.contains("management key was rejected"), "{error}");
        assert!(!error.contains("super-secret"));
    }
}
