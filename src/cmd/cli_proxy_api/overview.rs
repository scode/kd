//! `kd cli-proxy-api overview [--recent]`: the state of the pool and a chart
//! of each account's weekly-quota consumption over time, built from the
//! monitor's log.
//!
//! The log holds snapshots of each account's weekly utilization, not
//! consumption. Utilization drops back to zero when a weekly window rolls
//! over, and also when the user presses a banked limit reset (the window's
//! reset time stays put then). So the snapshots are first turned into a
//! cumulative-consumption series that only ever grows, and only then is
//! consumption per bucket read off it: the cumulative value at each bucket
//! edge is interpolated linearly between the last sample at or before the
//! edge and the first sample after it, and a bucket's consumption is the
//! difference between its two edges. A sample landing just inside or just
//! outside a bucket therefore moves the chart only by the share of the
//! interval it represents, rather than by a whole sample.
//!
//! Two sources feed the series. Every pass logs Anthropic's usage lookup
//! (whole percents, every 15 minutes) and CLIProxyAPI's quota signals (two
//! decimals, but refreshed only when the account serves a request, and
//! re-logged unchanged while it is idle). The signals are converted to
//! percents and de-duplicated by the time CLIProxyAPI observed them.
//!
//! Everything here is pure: rendering takes the records, the override,
//! "now", the time zone, the width, and whether to color, so tests pin the
//! output exactly.

use super::burn::Burn;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;

/// A record older than this means the monitor is not running: two regular
/// intervals, so one slow wake does not trigger the warning.
const STALE_AFTER: SignedDuration = SignedDuration::from_mins(30);

/// Two readings whose weekly reset times differ by less than this belong to
/// the same window. Anthropic jitters a reset by a fraction of a second, and
/// the two sources may round it differently.
const SAME_RESET: SignedDuration = SignedDuration::from_mins(5);

/// Readings further apart than this bracket a gap in the history (the
/// monitor was down), and buckets whose edges fall in it are unknown rather
/// than smoothed over. Regular readings are 15 minutes apart; the margin
/// covers a slow pass or a restart.
const MAX_GAP: SignedDuration = SignedDuration::from_mins(40);

/// A drop in utilization within one window larger than this is a manual
/// limit reset. Smaller drops are rounding differences between the whole
/// percents of the usage lookup and the two-decimal signals, and are
/// absorbed by keeping the series monotonic.
const RESET_DROP: f64 = 2.0;

/// Smallest vertical scale, in percent per bucket, so a nearly idle account
/// does not draw sampling noise as full-height bars.
const MIN_SCALE: f64 = 1.0;

/// Margin shown before a weekly window's start and after its end, an eighth
/// of a week, so both rollovers sit visibly inside the chart.
const WINDOW_MARGIN: SignedDuration = SignedDuration::from_hours(21);

/// The weekly window's length.
const WEEK: SignedDuration = SignedDuration::from_hours(7 * 24);

/// Rollover marker: the moment a weekly window resets.
const ROLLOVER: char = '│';

/// "Now" marker: thin, so a thick white bar in the same bucket reads as
/// use in progress rather than as the marker.
const NOW: char = '│';

/// Chart height in rows; each row resolves eight levels.
const ROWS: usize = 4;

/// Width of the left margin holding the scale labels and the axis.
const MARGIN: usize = 9;

/// Length of the current-usage gauges drawn right of each chart, in
/// columns, so one column is 5%.
const GAUGE_BAR: usize = 20;

/// Columns the gauges take when they are drawn (see [`FULL_WEEK_BUCKETS`]):
/// a gap, the window label, the bar, and the spelled-out percentage.
const GAUGE_WIDTH: usize = 2 + 2 + 1 + GAUGE_BAR + 1 + 4;

/// Chart columns the default view needs to show a whole week with its
/// margins: 7 days plus twice 21 hours is 210 hours, which the 4-hour grid
/// turns into 53 or 54 buckets depending on where the reset falls (a
/// daylight-saving change in the range does not push it past 54). Gauges
/// are only drawn when this many columns remain beside them, in both views,
/// so they appear and disappear at the same terminal width whichever view
/// is shown: they repeat numbers the account's header line already shows,
/// so on a narrow terminal the chart gets the room instead.
const FULL_WEEK_BUCKETS: usize = 54;

/// Which time span to chart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Span {
    /// Each account's current weekly window in 4-hour buckets, from the
    /// rollover that started it to the one that ends it, with an eighth of
    /// a week of margin on either side (1.25 weeks in all).
    Week,
    /// As many 15-minute buckets as fit the terminal width.
    Recent,
}

impl Span {
    fn bucket(self) -> SignedDuration {
        match self {
            Span::Week => SignedDuration::from_hours(4),
            Span::Recent => SignedDuration::from_mins(15),
        }
    }
}

/// Output styling. Plain is for pipes and tests; Color adds ANSI escapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Plain,
    Color,
}

const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
/// The main accent: bold blue, chosen to match a status-line style the
/// tool's users already know. Terminals that show bold as bright render it
/// as a light blue; on others it is plain blue.
const BLUE: &str = "\x1b[1;34m";
const MAGENTA: &str = "\x1b[35m";
/// Rollover markers: a soft blue-violet that sets the week's boundaries
/// apart from the bars without competing with the warning colors.
const PERIWINKLE: &str = "\x1b[38;5;147m";
/// The present: the now marker and the bars of the bucket in progress.
const WHITE: &str = "\x1b[97m";
const RESET: &str = "\x1b[0m";

impl Style {
    fn paint(self, code: &str, text: &str) -> String {
        match self {
            Style::Plain => text.to_owned(),
            Style::Color => format!("{code}{text}{RESET}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Series
// ---------------------------------------------------------------------------

/// One weekly-utilization reading for an account.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    at: Timestamp,
    /// Percent of the weekly quota used, 0-100.
    used: f64,
    /// When the window resets; `None` for a window that has not started.
    reset: Option<Timestamp>,
}

/// How a log record names an account: by email when it has one, since that
/// is how people think of their accounts, else by auth file name.
fn account_key(account: &Value) -> Option<&str> {
    account["email"]
        .as_str()
        .filter(|e| !e.is_empty())
        .or_else(|| account["name"].as_str())
}

/// All weekly readings for `key`, from both sources, oldest first, with
/// readings at the same instant collapsed (a quota signal is re-logged on
/// every pass until the account serves another request).
fn readings(records: &[Value], key: &str) -> Vec<Reading> {
    let mut out = Vec::new();
    for record in records {
        let Some(time) = record["time"].as_str().and_then(|t| t.parse().ok()) else {
            continue;
        };
        let accounts = record["accounts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for account in accounts.iter().filter(|a| account_key(a) == Some(key)) {
            if let Some(used) = account["seven_day"]["utilization"].as_f64() {
                out.push(Reading {
                    at: time,
                    used,
                    reset: account["seven_day"]["resets_at"]
                        .as_str()
                        .and_then(|t| t.parse().ok()),
                });
            }
            if let Some(reading) = signal_reading(&account["quota"]) {
                out.push(reading);
            }
        }
    }
    out.sort_by_key(|r| r.at);
    out.dedup_by_key(|r| r.at);
    out
}

/// The weekly reading in CLIProxyAPI's raw quota signals, if they carry
/// one. Header names are matched without regard to case, and anything
/// unexpected means "no reading" rather than an error: the signals are an
/// undocumented upstream detail.
fn signal_reading(quota: &Value) -> Option<Reading> {
    let at = quota["observed_at"].as_str()?.parse().ok()?;
    let signals = quota["signals"].as_object()?;
    let get = |name: &str| {
        signals
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.as_str())
    };
    let used = get("Anthropic-Ratelimit-Unified-7d-Utilization")?
        .parse::<f64>()
        .ok()?
        * 100.0;
    // A signal without a usable reset cannot be placed in a window, and
    // treating it as "window not started" would read as a rollover.
    let reset = get("Anthropic-Ratelimit-Unified-7d-Reset")?
        .parse::<i64>()
        .ok()
        .and_then(|s| Timestamp::from_second(s).ok())?;
    Some(Reading {
        at,
        used,
        reset: Some(reset),
    })
}

fn same_window(a: Option<Timestamp>, b: Option<Timestamp>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.duration_until(b).abs() < SAME_RESET,
        (None, None) => true,
        _ => false,
    }
}

/// Turn readings into cumulative consumption, in percent of the weekly
/// quota, at each reading's time. When the window changes, or utilization
/// falls sharply within one window (a manual reset), the total so far
/// becomes the base the new window's utilization is added to. Consumption between the last
/// reading of a window and its reset is never observed and is lost; the
/// monitor's wake just after each reset keeps that gap small. The series
/// never decreases.
///
/// Weekly reset times only move forward, so a reading that names a window
/// older than the newest one seen (a quota signal CLIProxyAPI observed
/// before a rollover, re-logged after it) is stale and skipped; counting it
/// would look like two rollovers and add that week's usage twice.
fn cumulative(readings: &[Reading]) -> Vec<(Timestamp, f64)> {
    let mut out: Vec<(Timestamp, f64)> = Vec::new();
    let mut base = 0.0;
    let mut previous: Option<Reading> = None;
    let mut newest_reset: Option<Timestamp> = None;
    for r in readings {
        if let (Some(reset), Some(newest)) = (r.reset, newest_reset)
            && reset < newest - SAME_RESET
        {
            continue;
        }
        newest_reset = newest_reset.max(r.reset);
        if let Some(p) = previous
            && (!same_window(p.reset, r.reset) || r.used < p.used - RESET_DROP)
        {
            base = out.last().map_or(0.0, |(_, total)| *total);
        }
        let total = base + r.used;
        let total = out.last().map_or(total, |(_, last)| total.max(*last));
        out.push((r.at, total));
        previous = Some(*r);
    }
    out
}

/// The cumulative value at `t`, interpolated between the readings around
/// it. Before the first reading, and inside a gap longer than [`MAX_GAP`],
/// it is unknown. After the last reading it holds that reading's value as
/// long as the reading is fresh relative to `now`: nothing has been
/// observed since, so the bucket in progress (and one that just ended
/// before the next wake) shows what was seen so far rather than nothing.
fn value_at(series: &[(Timestamp, f64)], t: Timestamp, now: Timestamp) -> Option<f64> {
    let after = series.partition_point(|(at, _)| *at <= t);
    let (t0, v0) = *series.get(after.checked_sub(1)?)?;
    match series.get(after) {
        // A reading taken exactly at `t` is known, whatever gap follows.
        _ if t == t0 => Some(v0),
        Some(&(t1, _)) if t0.duration_until(t1) > MAX_GAP => None,
        Some(&(t1, v1)) => {
            let span = t0.duration_until(t1).as_secs_f64();
            let into = t0.duration_until(t).as_secs_f64();
            Some(v0 + (v1 - v0) * into / span)
        }
        None if t0.duration_until(now) <= STALE_AFTER => Some(v0),
        None => None,
    }
}

/// Bucket edges for `count` buckets ending with the one containing `now`
/// (or, for the window view, the window's far end).
/// Buckets are aligned to whole multiples of their length in local clock
/// time (4-hour buckets start at midnight, 04:00, ...), so the same bucket
/// covers the same hours on every run. Edges are stepped on the local
/// clock, not in absolute time, so they stay on those hours across a
/// daylight-saving change; the bucket containing the change is an hour
/// longer or shorter instead.
fn edges(span: Span, count: usize, now: Timestamp, tz: &TimeZone) -> Vec<Timestamp> {
    let local = now.to_zoned(tz.clone()).datetime();
    let minutes = i64::from(local.hour()) * 60 + i64::from(local.minute());
    let bucket_minutes = span.bucket().as_mins();
    let start = local
        .date()
        .to_datetime(jiff::civil::Time::midnight())
        .checked_add(jiff::Span::new().minutes(minutes - minutes % bucket_minutes))
        .expect("a time of day stays within the day");
    (0..=count as i64)
        .map(|i| {
            let offset = (i - count as i64 + 1) * bucket_minutes;
            start
                .checked_add(jiff::Span::new().minutes(offset))
                .and_then(|dt| dt.to_zoned(tz.clone()))
                .map(|z| z.timestamp())
                .expect("bucket edges are within jiff's range")
        })
        .collect()
}

/// What one bucket of the chart shows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Bucket {
    /// Weekly quota used during the bucket, in percent. A bucket whose use
    /// is too small to draw shows the dotted baseline.
    Used(f64),
    /// In the past, but not observed: before the history starts, or while
    /// the monitor was down. Left blank, like the future: there is no data.
    Unknown,
    /// Starts after now; left blank.
    Future,
}

/// A vertical line through the chart at one moment, with a label under
/// the axis.
struct Marker {
    at: Timestamp,
    glyph: char,
    color: &'static str,
    /// The label, longest form first; the first that fits is used.
    labels: Vec<String>,
}

/// The time range one account's chart covers, and the markers in it. For
/// the default view it is the account's current weekly window plus a margin
/// on each side, with both rollovers marked, so a glance shows how far into
/// its week the account is and how the week's use was spread. A window
/// that has not started, or whose reset is already past (the accounts come
/// from an older pass while the monitor is paused), has no current
/// rollovers to show; the last week plus the margin ahead is shown instead.
/// The recent view ends with the bucket in progress and has no markers.
///
/// When the terminal cannot fit the whole range, it is cut to `columns`
/// buckets that still contain now: the end of the window if now is within
/// reach of it, otherwise a stretch ending a quarter of the width after
/// now, so the present and the use leading up to it stay visible.
fn chart_range(
    span: Span,
    account: &Value,
    now: Timestamp,
    columns: usize,
    tz: &TimeZone,
) -> (Vec<Timestamp>, Vec<Marker>) {
    if span == Span::Recent {
        return (edges(span, columns, now, tz), Vec::new());
    }
    let reset: Option<Timestamp> = account["seven_day"]["resets_at"]
        .as_str()
        .and_then(|t| t.parse().ok())
        .filter(|end| *end > now);
    let (from, to, mut markers) = match reset {
        Some(end) => {
            let start = end - WEEK;
            let rollover = |at: Timestamp| Marker {
                at,
                glyph: ROLLOVER,
                color: PERIWINKLE,
                labels: vec![
                    format!("rollover {}", local(at, tz, "%a %d %H:%M")),
                    format!("rollover {}", local(at, tz, "%a %H:%M")),
                    local(at, tz, "%a %d %H:%M"),
                    local(at, tz, "%a %H:%M"),
                ],
            };
            (
                start - WINDOW_MARGIN,
                end + WINDOW_MARGIN,
                vec![rollover(start), rollover(end)],
            )
        }
        None => (now - WEEK, now + WINDOW_MARGIN * 2, Vec::new()),
    };
    markers.push(Marker {
        at: now,
        glyph: NOW,
        color: WHITE,
        labels: vec!["now".to_owned()],
    });
    let bucket = span.bucket();
    let room = bucket * columns as i32;
    let (from, to) = if from.duration_until(to) <= room {
        (from, to)
    } else if to - room <= now {
        (to - room, to)
    } else {
        let end = (now + bucket * (columns as i32 / 4).max(1)).max(from + room);
        (end - room, end)
    };
    // Enough buckets ending with the one containing `to` to reach back to
    // `from`, then the ones that start too early dropped, so the margins
    // come out as close to their nominal size as the bucket grid allows.
    let count =
        (from.duration_until(to).as_secs() as u64).div_ceil(bucket.as_secs() as u64) as usize + 1;
    let mut edges = edges(span, count, to, tz);
    while edges.len() > 2 && (edges[1] <= from || edges.len() - 1 > columns) {
        edges.remove(0);
    }
    (edges, markers)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Everything `overview` prints.
pub fn render(
    records: &[Value],
    burn: &anyhow::Result<Option<Burn>>,
    span: Span,
    now: Timestamp,
    tz: &TimeZone,
    width: usize,
    style: Style,
) -> String {
    let Some(latest) = records.last() else {
        return "no monitor records yet; start the monitor with `kd cli-proxy-api monitor enable`\n".to_owned();
    };
    let mut out = String::new();
    status(&mut out, latest, burn, now, tz, style);

    // A paused or failed pass logs no accounts, which is exactly when the
    // charts are still wanted; take the accounts from the newest pass that
    // saw them and say how old that is.
    let Some(seen) = seen_record(records) else {
        out.push_str("no monitor pass in the last week has listed the accounts\n");
        return out;
    };
    if !std::ptr::eq(seen, latest)
        && let Some(time) = seen["time"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok())
    {
        out.push_str(&format!(
            "accounts as of the last pass that saw them, {}\n",
            local(time, tz, "%a %Y-%m-%d %H:%M %Z")
        ));
    }

    let beside_gauges = width.saturating_sub(MARGIN + 1 + GAUGE_WIDTH);
    let show_gauges = beside_gauges >= FULL_WEEK_BUCKETS;
    let columns = if show_gauges {
        beside_gauges
    } else {
        width.saturating_sub(MARGIN + 1)
    }
    .max(1);

    let (charted, disabled) = shown_accounts(seen);
    let burned = burn
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .filter(|b| b.is_active(now));
    for account in charted {
        let Some(key) = account_key(account) else {
            continue;
        };
        out.push('\n');
        let is_burned = burned.is_some_and(|b| b.email.eq_ignore_ascii_case(key));
        account_header(&mut out, account, seen, key, is_burned, tz, style);
        let series = cumulative(&readings(records, key));
        if series.is_empty() {
            out.push_str("  no usage history yet\n");
            continue;
        }
        let (edges, markers) = chart_range(span, account, now, columns, tz);
        // Widen the buckets when the terminal has room for it.
        // Every account gets the same bucket width, sized for the longest
        // possible week range, so neighbouring charts share one time scale
        // and end near the gauges; widening each account's buckets to fill
        // its own (53 or 54 bucket) range would not.
        let cell = match span {
            Span::Week => columns / FULL_WEEK_BUCKETS.max(edges.len() - 1),
            Span::Recent => columns / (edges.len() - 1),
        }
        .clamp(1, 3);
        let values: Vec<Option<f64>> = edges.iter().map(|e| value_at(&series, *e, now)).collect();
        let used: Vec<Bucket> = edges
            .windows(2)
            .zip(values.windows(2))
            .map(|(e, v)| match (v[0], v[1]) {
                // A bucket starting exactly now is the one in progress.
                _ if e[0] > now => Bucket::Future,
                (Some(a), Some(b)) => Bucket::Used((b - a).max(0.0)),
                _ => Bucket::Unknown,
            })
            .collect();
        let color = if is_burned { MAGENTA } else { BLUE };
        let window_gauge = |label: &str, window: &Value| {
            let reset = window["resets_at"].as_str().and_then(|t| t.parse().ok());
            gauge(label, window["utilization"].as_f64(), reset, now, style)
        };
        let gauges: Vec<String> = if show_gauges {
            vec![
                window_gauge("7d", &account["seven_day"]),
                window_gauge("5h", &account["five_hour"]),
            ]
        } else {
            Vec::new()
        };
        let plot = Plot {
            used: &used,
            edges: &edges,
            markers: &markers,
            span,
            cell,
            now,
            gauge_column: match span {
                Span::Week => (FULL_WEEK_BUCKETS * cell).min(columns),
                Span::Recent => columns,
            },
        };
        chart(&mut out, &plot, &gauges, tz, style, color);
    }
    let disabled: Vec<&str> = disabled.into_iter().filter_map(account_key).collect();
    if !disabled.is_empty() {
        out.push_str(&format!("\ndisabled: {}\n", disabled.join(", ")));
    }
    out
}

/// The newest record whose pass saw the accounts. A paused or failed pass
/// logs no accounts, which is exactly when the charts are still wanted.
fn seen_record(records: &[Value]) -> Option<&Value> {
    records
        .iter()
        .rev()
        .find(|r| r["accounts"].as_array().is_some_and(|a| !a.is_empty()))
}

/// The accounts `overview` shows from a record, in the order it shows
/// them: the managed (enabled Claude) accounts, each with a section, then
/// the disabled Claude accounts, listed by name. Accounts of other
/// providers are not shown. Both the renderer and [`anonymize`] use this,
/// so the privacy numbering always follows what is on screen.
fn shown_accounts(record: &Value) -> (Vec<&Value>, Vec<&Value>) {
    let accounts = record["accounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let is_claude = |a: &&Value| {
        a["provider"]
            .as_str()
            .is_some_and(|p| p.eq_ignore_ascii_case("claude"))
    };
    let charted = accounts.iter().filter(|a| a["managed"] == true).collect();
    let disabled = accounts
        .iter()
        .filter(|a| a["managed"] != true)
        .filter(is_claude)
        .collect();
    (charted, disabled)
}

/// Rewrite rendered `overview` text so it names no account: for
/// `--privacy`, when the output is meant for a screenshot or a shared
/// screen. Shown accounts become "Account 1", "Account 2", ..., numbered in
/// the order the output shows them.
///
/// It works on the finished text rather than inside the renderer so that
/// every path that can print an account, including CLIProxyAPI's own error
/// messages quoted in the status lines, is covered without each having to
/// remember. Every email-shaped token is looked up whole, ignoring case,
/// against the shown accounts' emails and auth file names (which embed the
/// email and so are email-shaped themselves) and becomes that account's
/// label, or `<email>` when it names no shown account. Matching whole
/// tokens means a longer address that merely contains a known one is never
/// half replaced. The few auth file names without an `@` are replaced as
/// plain text. `home`, when given, becomes `~`, since a home directory path
/// (in a config file error, say) usually carries the user name.
pub fn anonymize(text: &str, records: &[Value], home: Option<&std::path::Path>) -> String {
    let (charted, disabled) = seen_record(records).map(shown_accounts).unwrap_or_default();
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut plain: Vec<(String, String)> = Vec::new();
    for (i, account) in charted.iter().chain(disabled.iter()).enumerate() {
        let label = format!("Account {}", i + 1);
        for field in ["name", "email"] {
            let Some(value) = account[field].as_str().filter(|v| !v.is_empty()) else {
                continue;
            };
            if value.contains('@') {
                labels.insert(value.to_lowercase(), label.clone());
            } else {
                plain.push((value.to_owned(), label.clone()));
            }
        }
    }
    let mut out = text.to_owned();
    if let Some(home) = home.and_then(|h| h.to_str()).filter(|h| h.len() > 1) {
        out = out.replace(home, "~");
    }
    plain.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    for (value, label) in &plain {
        out = out.replace(value.as_str(), label);
    }
    replace_emails(&out, &|email| {
        labels
            .get(&email.to_lowercase())
            .cloned()
            .unwrap_or_else(|| "<email>".to_owned())
    })
}

/// Replace each email-shaped token with `label(token)`, leaving ANSI escape
/// sequences alone (their letters must not be read as part of an address
/// next to them).
fn replace_emails(text: &str, label: &dyn Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push_str(&replace_plain(&plain, label));
            plain.clear();
            out.push(c);
            // CSI: ESC [ parameters, ending at the first letter.
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c in chars.by_ref() {
                    out.push(c);
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    out.push_str(&replace_plain(&plain, label));
    out
}

/// [`replace_emails`] for text without escape sequences. A token is a run
/// of address characters (any letter or digit, plus `._%+-`) before an `@`
/// and a run of domain characters (letters, digits, `.` and `-`) with at
/// least one dot after it; a trailing dot, as at the end of a sentence, is
/// not part of it.
fn replace_plain(text: &str, label: &dyn Fn(&str) -> String) -> String {
    let local = |c: char| c.is_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_alphanumeric() || ".-".contains(c);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = (0..i).rev().take_while(|&j| local(chars[j])).last();
            let mut end = (i + 1..chars.len())
                .take_while(|&j| domain(chars[j]))
                .last();
            while let Some(e) = end.filter(|&e| e > i && chars[e] == '.') {
                end = Some(e - 1).filter(|&e| e > i);
            }
            if let (Some(start), Some(end)) = (start, end)
                && chars[i + 1..=end].contains(&'.')
            {
                // `out` already holds the local part; take it back.
                for _ in start..i {
                    out.pop();
                }
                let token: String = chars[start..=end].iter().collect();
                out.push_str(&label(&token));
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The pool-level lines: how fresh the data is, what the last pass did, and
/// the override.
fn status(
    out: &mut String,
    latest: &Value,
    burn: &anyhow::Result<Option<Burn>>,
    now: Timestamp,
    tz: &TimeZone,
    style: Style,
) {
    let time: Option<Timestamp> = latest["time"].as_str().and_then(|t| t.parse().ok());
    if let Some(time) = time {
        let age = time.duration_until(now);
        out.push_str(&format!(
            "last monitor pass {} ({} min ago)\n",
            local(time, tz, "%a %Y-%m-%d %H:%M %Z"),
            age.as_mins()
        ));
        if age > STALE_AFTER {
            out.push_str(&style.paint(
                RED,
                "warning: the monitor appears to have stopped; check `journalctl --user -u kd-cli-proxy-api-monitor`",
            ));
            out.push('\n');
        }
    }
    if let Some(error) = latest["error"].as_str() {
        let label = if latest["paused"] == true {
            "paused"
        } else {
            "last pass failed"
        };
        out.push_str(&style.paint(RED, &format!("{label}: {error}")));
        out.push('\n');
    }
    match burn {
        Ok(Some(b)) if b.is_active(now) => out.push_str(&style.paint(
            MAGENTA,
            &format!(
                "burn: {} until {}",
                b.email,
                local(b.until, tz, "%a %Y-%m-%d %H:%M %Z")
            ),
        )),
        Ok(Some(b)) => out.push_str(&format!(
            "burn: {} (expired; clear it with `kd cli-proxy-api burn --clear`)",
            b.email
        )),
        Ok(None) => out.push_str("burn: none"),
        Err(err) => out.push_str(&style.paint(RED, &format!("burn override unreadable: {err:#}"))),
    }
    out.push('\n');
}

/// One account's title line and, when its usage lookup failed (often a
/// dead login, which freezes the whole pool's priorities), a loud error.
fn account_header(
    out: &mut String,
    account: &Value,
    record: &Value,
    key: &str,
    is_burned: bool,
    tz: &TimeZone,
    style: Style,
) {
    let window = |w: &Value, format: &str| {
        let used = w["utilization"]
            .as_f64()
            .map_or("?".to_owned(), |u| format!("{u:.0}%"));
        match w["resets_at"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok())
        {
            Some(reset) => format!("{used} (resets {})", local(reset, tz, format)),
            None => format!("{used} (not started)"),
        }
    };
    // The planned value is what the proxy holds only if the pass finished
    // without error; a failed write or verification leaves the old one.
    let planned = account["priority_planned"]
        .as_i64()
        .filter(|_| record["error"].is_null());
    let priority = planned
        .or_else(|| account["priority_before"].as_i64())
        .map_or("?".to_owned(), |p| p.to_string());
    let mut line = format!("{}  priority {priority}", style.paint(BOLD, key));
    if is_burned {
        line.push_str(&format!("  {}", style.paint(MAGENTA, "BURNING")));
    }
    if account["seven_day"].is_object() {
        line.push_str(&format!(
            "  7d {}",
            window(&account["seven_day"], "%a %m-%d %H:%M")
        ));
    }
    if account["five_hour"].is_object() {
        line.push_str(&format!("  5h {}", window(&account["five_hour"], "%H:%M")));
    }
    let flags: Vec<&str> = account["flags"]
        .as_array()
        .map(|f| f.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !flags.is_empty() {
        line.push_str(&format!("  {}", style.paint(YELLOW, &flags.join(", "))));
    }
    out.push_str(&line);
    out.push('\n');
    if let Some(error) = account["usage_error"].as_str() {
        out.push_str(&style.paint(
            RED,
            &format!("  usage lookup failed (no priorities are written until fixed): {error}"),
        ));
        out.push('\n');
    }
}

/// How much of a window is used right now, as a horizontal bar with the
/// percentage spelled out: `7d ▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░  72%`. The bar shows
/// the level, which the time chart beside it (a rate) cannot. Filled and
/// empty cells are shaded blocks colored together with the number, so the
/// bar and its value read as one unit; a cell is 5%, rounded down so that
/// only an exhausted window draws a full bar, and the number carries the
/// precision. The color turns yellow from 80% and red from 95%, where a
/// window is close to cutting the account off.
///
/// The value shows as `?` when it is unknown (no usage lookup, or no such
/// window in it) or out of date: when `reset` has already passed, the
/// window has started over since the reading was taken, as happens while
/// the monitor is paused and the accounts come from an older pass.
fn gauge(
    label: &str,
    used: Option<f64>,
    reset: Option<Timestamp>,
    now: Timestamp,
    style: Style,
) -> String {
    let Some(used) = used.filter(|_| reset.is_none_or(|r| r > now)) else {
        return format!("{label} {:>w$}", "?", w = GAUGE_BAR + 5);
    };
    let filled = ((used.clamp(0.0, 100.0) / 100.0) * GAUGE_BAR as f64).floor() as usize;
    let color = if used >= 95.0 {
        RED
    } else if used >= 80.0 {
        YELLOW
    } else {
        BLUE
    };
    let text = format!(
        "{}{} {:>4}",
        "▓".repeat(filled),
        "░".repeat(GAUGE_BAR - filled),
        format!("{used:.0}%")
    );
    format!("{label} {}", style.paint(color, &text))
}

/// One account's chart data: per-bucket use, the bucket edges (one more
/// than the buckets), markers, and the columns per bucket.
struct Plot<'a> {
    used: &'a [Bucket],
    edges: &'a [Timestamp],
    markers: &'a [Marker],
    span: Span,
    cell: usize,
    now: Timestamp,
    /// Column the gauges start at (after a two-column gap). It is the
    /// whole chart budget rather than this chart's own width, which varies
    /// by account, so the gauges line up from one account to the next.
    gauge_column: usize,
}

impl Plot<'_> {
    /// The chart column a moment falls in, proportionally inside its
    /// bucket, or `None` outside the chart.
    fn column(&self, at: Timestamp) -> Option<usize> {
        let i = self.edges.partition_point(|e| *e <= at).checked_sub(1)?;
        let (start, end) = (*self.edges.get(i)?, *self.edges.get(i + 1)?);
        let into = start.duration_until(at).as_secs_f64() / start.duration_until(end).as_secs_f64();
        Some(i * self.cell + ((into * self.cell as f64) as usize).min(self.cell - 1))
    }
}

/// A row of characters, each with an optional color, painted as runs.
fn paint_row(cells: &[(char, Option<&str>)], style: Style) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut run_color: Option<&str> = None;
    let flush = |run: &mut String, color: Option<&str>, out: &mut String| {
        if !run.is_empty() {
            match color {
                Some(c) => out.push_str(&style.paint(c, run)),
                None => out.push_str(run),
            }
            run.clear();
        }
    };
    for &(c, color) in cells {
        if color != run_color {
            flush(&mut run, run_color, &mut out);
            run_color = color;
        }
        run.push(c);
    }
    flush(&mut run, run_color, &mut out);
    out
}

/// A bar chart of per-bucket consumption with a scale on the left, day
/// (and, for the recent view, hour) marks below, markers drawn as vertical
/// lines through the empty parts of the chart and labelled under the axis,
/// and `side` (the current usage gauges) to the right of its top rows.
/// Observed buckets sit on a dotted baseline, so idle time reads as "0%
/// here" rather than as missing; buckets without data (before the history,
/// while the monitor was down, or in the future) stay blank.
fn chart(
    out: &mut String,
    plot: &Plot<'_>,
    side: &[String],
    tz: &TimeZone,
    style: Style,
    color: &'static str,
) {
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let scale = plot
        .used
        .iter()
        .filter_map(|b| match b {
            Bucket::Used(v) => Some(*v),
            _ => None,
        })
        .fold(MIN_SCALE, f64::max);
    let unit = match plot.span {
        Span::Week => "4h",
        Span::Recent => "15m",
    };
    let width = plot.used.len() * plot.cell;
    // Only the default view marks the present; `--recent` always ends at
    // now anyway.
    let now_bucket = (plot.span == Span::Week)
        .then(|| plot.column(plot.now))
        .flatten()
        .map(|column| column / plot.cell);
    let marks: Vec<(usize, &Marker)> = plot
        .markers
        .iter()
        .filter_map(|m| Some((plot.column(m.at)?, m)))
        .collect();
    for row in 0..ROWS {
        let label = match row {
            0 => format!("{scale:.1}%"),
            r if r == ROWS - 1 => "0%".to_owned(),
            _ => String::new(),
        };
        let mut cells: Vec<(char, Option<&str>)> = Vec::with_capacity(width);
        for (i, bucket) in plot.used.iter().enumerate() {
            // The bucket in progress is drawn white, like the now marker in
            // it: a thin white line on the dotted baseline means nothing used
            // yet, a thick white bar means use in progress.
            let color = if Some(i) == now_bucket { WHITE } else { color };
            let glyph = match bucket {
                Bucket::Used(v) => {
                    let level = ((v / scale) * (ROWS * 8) as f64).round() as usize;
                    match level.saturating_sub((ROWS - 1 - row) * 8).min(8) {
                        0 if row == ROWS - 1 => '·',
                        filled => LEVELS[filled],
                    }
                }
                Bucket::Unknown | Bucket::Future => ' ',
            };
            cells.extend(std::iter::repeat_n((glyph, Some(color)), plot.cell));
        }
        // Markers go where the chart is empty; a bar keeps its cell so no
        // usage is hidden, and the axis below still marks the column.
        // When two markers share a column, the first (a rollover, listed
        // before now) keeps it, matching the label line: once drawn, its
        // glyph is no longer an empty cell.
        for (column, marker) in &marks {
            if matches!(cells[*column].0, ' ' | '·') {
                cells[*column] = (marker.glyph, Some(marker.color));
            }
        }
        // Rows carrying a gauge are padded to the gauge column so the
        // gauges line up; the rest are trimmed.
        let gauge = side.get(row);
        if gauge.is_some() {
            if cells.len() < plot.gauge_column {
                cells.resize(plot.gauge_column, (' ', None));
            }
        } else {
            while cells.last().is_some_and(|(c, _)| *c == ' ') {
                cells.pop();
            }
        }
        let bars = paint_row(&cells, style);
        match gauge {
            Some(gauge) => out.push_str(&format!("{label:>7} ┤{bars}  {gauge}\n")),
            None => out.push_str(&format!("{label:>7} ┤{bars}\n")),
        }
    }

    // Axis with a tick at each labelled bucket, and the labels under it.
    let mut axis: Vec<(char, Option<&str>)> = vec![('─', None); width];
    let mut labels: Vec<char> = vec![' '; width];
    let mut free_from = 0;
    for (i, start) in plot.edges[..plot.used.len()].iter().enumerate() {
        let zoned = start.to_zoned(tz.clone());
        // Buckets are aligned to the local clock, so a day starts exactly at
        // a bucket that starts at midnight. The first bucket gets no label
        // unless it does too: a mid-day start labelled with its date would
        // read as a day boundary.
        let label = if zoned.hour() == 0 && zoned.minute() == 0 {
            Some(zoned.strftime("%a %d").to_string())
        } else if plot.span == Span::Recent && zoned.minute() == 0 && zoned.hour() % 3 == 0 {
            Some(zoned.strftime("%H:%M").to_string())
        } else {
            None
        };
        let column = i * plot.cell;
        if let Some(label) = label
            && column >= free_from
            && column + label.chars().count() <= width
        {
            axis[column] = ('┬', None);
            for (j, c) in label.chars().enumerate() {
                labels[column + j] = c;
            }
            free_from = column + label.chars().count() + 1;
        }
    }
    // Marker labels get a line of their own, so they never fight the day
    // labels for space. Each marker's glyph sits exactly in its column,
    // continuing the line drawn through the chart, so the label reads as
    // the marker itself rather than as a legend. Markers that land in the
    // same column (now within the bucket of a rollover) share one glyph,
    // the first marker's, and their labels are joined. Each label uses its
    // longest form that fits right of the glyph before the next marker,
    // else left of the glyph after the previous label, else its shortest
    // form cut to the room on the right. The last label may run past the
    // chart, under the gauges.
    let mut marks = marks;
    marks.sort_by_key(|(column, _)| *column);
    let mut merged: Vec<(usize, &Marker, Vec<String>)> = Vec::new();
    for (column, marker) in marks {
        match merged.last_mut() {
            Some((last, _, labels)) if *last == column => {
                let joined: Vec<String> = labels
                    .iter()
                    .map(|l| format!("{l} · {}", marker.labels[0]))
                    .collect();
                *labels = joined;
            }
            _ => merged.push((column, marker, marker.labels.clone())),
        }
    }
    let mut marker_line: Vec<(char, Option<&str>)> = vec![(' ', None); width];
    let mut free_from = 0;
    for (k, (column, marker, labels)) in merged.iter().enumerate() {
        let (column, color) = (*column, Some(marker.color));
        axis[column] = ('┼', color);
        let next = merged.get(k + 1).map_or(usize::MAX, |(c, _, _)| *c);
        let right_room = next.saturating_sub(column + 3);
        let fits_right = labels.iter().find(|l| l.chars().count() <= right_room);
        let fits_left = labels
            .iter()
            .find(|l| column >= free_from + l.chars().count() + 2);
        let (start, text): (usize, Vec<char>) = if let Some(l) = fits_right {
            (column + 2, l.chars().collect())
        } else if let Some(l) = fits_left {
            (column - 1 - l.chars().count(), l.chars().collect())
        } else {
            let shortest = labels.last().map(String::as_str).unwrap_or_default();
            let mut short: Vec<char> = shortest
                .chars()
                .take(right_room.saturating_sub(1))
                .collect();
            if !short.is_empty() {
                short.push('…');
            }
            (column + 2, short)
        };
        let end = (start + text.len()).max(column + 1);
        if marker_line.len() < end {
            marker_line.resize(end, (' ', None));
        }
        marker_line[column] = (marker.glyph, color);
        for (j, c) in text.iter().enumerate() {
            marker_line[start + j] = (*c, color);
        }
        free_from = end + 1;
    }
    while marker_line.last().is_some_and(|(c, _)| *c == ' ') {
        marker_line.pop();
    }
    let labels: String = labels.into_iter().collect();
    out.push_str(&format!(
        "{:>7} └{}\n",
        format!("/{unit}"),
        paint_row(&axis, style)
    ));
    // Both lines are indented by the width of the chart's left margin plus
    // its axis character, so their columns match the chart's.
    out.push_str(&format!("{:>9}{}\n", "", labels.trim_end()));
    if !marker_line.is_empty() {
        out.push_str(&format!("{:>9}{}\n", "", paint_row(&marker_line, style)));
    }
}

/// `t` formatted in the viewer's time zone, since resets and bucket labels
/// are read against a local clock.
fn local(t: Timestamp, tz: &TimeZone, format: &str) -> String {
    t.to_zoned(tz.clone()).strftime(format).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn reading(at: &str, used: f64, reset: Option<&str>) -> Reading {
        Reading {
            at: ts(at),
            used,
            reset: reset.map(ts),
        }
    }

    const WEEK1: &str = "2026-10-04T16:00:00Z";
    const WEEK2: &str = "2026-10-11T16:00:00Z";

    /// Utilization within one window accumulates directly; a rollover
    /// carries the old window's last reading forward, as does a sharp drop
    /// within a window (a pressed limit reset). Reset jitter and small
    /// rounding drops between sources are neither.
    #[test]
    fn cumulative_survives_rollovers_and_manual_resets() {
        let series = cumulative(&[
            reading("2026-10-01T00:00:00Z", 10.0, Some(WEEK1)),
            reading("2026-10-01T01:00:00Z", 20.0, Some("2026-10-04T16:00:00.4Z")),
            reading("2026-10-01T02:00:00Z", 19.6, Some(WEEK1)),
            reading("2026-10-01T03:00:00Z", 5.0, Some(WEEK1)),
            reading("2026-10-01T04:00:00Z", 8.0, Some(WEEK1)),
            reading("2026-10-04T17:00:00Z", 0.0, None),
            reading("2026-10-04T18:00:00Z", 3.0, Some(WEEK2)),
        ]);
        let totals: Vec<f64> = series.iter().map(|(_, v)| *v).collect();
        assert_eq!(totals, vec![10.0, 20.0, 20.0, 25.0, 28.0, 28.0, 31.0]);
    }

    /// The edge value comes from the readings on either side of it, so a
    /// reading just inside or outside a bucket shifts the chart only by its
    /// share of the interval.
    #[test]
    fn edges_interpolate_between_readings() {
        let now = ts("2026-10-01T01:20:00Z");
        let series = vec![
            (ts("2026-10-01T00:00:00Z"), 10.0),
            (ts("2026-10-01T00:20:00Z"), 14.0),
            (ts("2026-10-01T02:00:00Z"), 99.0),
            (ts("2026-10-01T02:10:00Z"), 100.0),
        ];
        let at = |t: &str, now: Timestamp| value_at(&series, ts(t), now);
        assert_eq!(at("2026-10-01T00:15:00Z", now), Some(13.0));
        assert_eq!(at("2026-10-01T00:00:00Z", now), Some(10.0));
        assert_eq!(at("2026-09-30T23:00:00Z", now), None, "before history");
        assert_eq!(
            at("2026-10-01T01:00:00Z", now),
            None,
            "inside a gap the monitor was down for"
        );
        let now = ts("2026-10-01T02:30:00Z");
        assert_eq!(
            at("2026-10-01T02:15:00Z", now),
            Some(100.0),
            "after the last reading but before now: held while fresh"
        );
        assert_eq!(
            at("2026-10-01T04:00:00Z", now),
            Some(100.0),
            "the bucket in progress holds the fresh latest value"
        );
        assert_eq!(
            at("2026-10-01T12:00:00Z", ts("2026-10-01T09:00:00Z")),
            None,
            "stale"
        );
    }

    /// A quota signal CLIProxyAPI observed before a rollover but logged
    /// after it names the old window; it must not count as a second
    /// rollover. Neither may a signal without a usable reset time.
    #[test]
    fn stale_and_unplaceable_signals_are_ignored() {
        let series = cumulative(&[
            reading("2026-10-04T15:00:00Z", 40.0, Some(WEEK1)),
            reading("2026-10-04T17:00:00Z", 2.0, Some(WEEK2)),
            reading("2026-10-04T17:10:00Z", 40.0, Some(WEEK1)),
            reading("2026-10-04T17:20:00Z", 3.0, Some(WEEK2)),
        ]);
        let totals: Vec<f64> = series.iter().map(|(_, v)| *v).collect();
        assert_eq!(totals, vec![40.0, 42.0, 43.0]);
        let unplaceable = json!({"observed_at": "2026-10-01T00:00:00Z",
            "signals": {"Anthropic-Ratelimit-Unified-7d-Utilization": "0.4"}});
        assert_eq!(signal_reading(&unplaceable), None);
    }

    /// Both sources are read, signals are scaled to percent and de-duplicated
    /// by observation time, and readings for other accounts are ignored.
    #[test]
    fn readings_merge_both_sources() {
        let quota = json!({"observed_at": "2026-10-01T00:30:00-07:00", "signals": {
            "Anthropic-Ratelimit-Unified-7d-Utilization": "0.25",
            "Anthropic-Ratelimit-Unified-7d-Reset": "1790899200"}});
        let record = |time: &str, used: f64| {
            json!({"v": 1, "time": time, "accounts": [
                {"email": "a@example.com", "managed": true,
                 "seven_day": {"utilization": used, "resets_at": WEEK1}, "quota": quota},
                {"email": "b@example.com", "managed": true,
                 "seven_day": {"utilization": 99.0, "resets_at": WEEK1}},
            ]})
        };
        let got = readings(
            &[
                record("2026-10-01T07:00:00Z", 24.0),
                record("2026-10-01T07:15:00Z", 26.0),
            ],
            "a@example.com",
        );
        assert_eq!(
            got,
            vec![
                reading("2026-10-01T07:00:00Z", 24.0, Some(WEEK1)),
                reading("2026-10-01T07:15:00Z", 26.0, Some(WEEK1)),
                reading("2026-10-01T07:30:00Z", 25.0, Some("2026-10-02T00:00:00Z")),
            ]
        );
    }

    /// Buckets are aligned in local time: 4-hour buckets start at local
    /// midnight and every fourth hour, and the last bucket contains now.
    #[test]
    fn buckets_align_to_local_time() {
        let tz = TimeZone::get("America/Los_Angeles").unwrap();
        let now = ts("2026-10-01T12:34:56Z"); // 05:34:56 PDT
        let e = edges(Span::Week, 2, now, &tz);
        assert_eq!(
            e,
            vec![
                ts("2026-10-01T07:00:00Z"),
                ts("2026-10-01T11:00:00Z"),
                ts("2026-10-01T15:00:00Z")
            ]
        );
        let e = edges(Span::Recent, 1, now, &TimeZone::UTC);
        assert_eq!(
            e,
            vec![ts("2026-10-01T12:30:00Z"), ts("2026-10-01T12:45:00Z")]
        );

        // Across the November fall-back, every edge still sits on a local
        // hour divisible by four; the bucket containing the change is just
        // five hours long.
        let after_dst = ts("2026-11-02T20:00:00Z");
        for edge in edges(Span::Week, 12, after_dst, &tz) {
            let local = edge.to_zoned(tz.clone());
            assert_eq!((local.hour() % 4, local.minute()), (0, 0), "{local}");
        }
    }

    /// Eight hours of records every 15 minutes: one healthy account using 2%
    /// of its week per hour, one whose login died, and one disabled.
    fn log() -> Vec<Value> {
        let mut records = Vec::new();
        let start = ts("2026-10-01T00:00:00Z");
        for i in 0..=32 {
            let time = start + SignedDuration::from_mins(15 * i);
            records.push(json!({
                "v": 1, "time": time.to_string(), "error": null, "paused": false,
                "accounts": [
                    {"email": "a@example.com", "managed": true, "provider": "claude",
                     "priority_before": 10, "priority_planned": 20,
                     "seven_day": {"utilization": (i as f64) * 0.5, "resets_at": WEEK1},
                     "five_hour": {"utilization": 5.0, "resets_at": "2026-10-01T10:00:00Z"},
                     "flags": [], "usage_error": null},
                    {"email": "dead@example.com", "managed": true, "provider": "claude", "priority_planned": 10,
                     "seven_day": null, "five_hour": null, "flags": [],
                     "usage_error": "usage endpoint returned HTTP 401: token revoked"},
                    {"email": "off@example.com", "managed": false, "provider": "Claude"},
                ],
            }));
        }
        records
    }

    /// The whole view, plain: status, burn, an account with a chart whose
    /// steady 2%/hour shows as equal 8% bars per 4-hour bucket, a dead login
    /// called out, and the disabled account listed.
    #[test]
    fn renders_status_and_charts() {
        let now = ts("2026-10-01T08:10:00Z");
        let burn = Ok(Some(Burn {
            email: "a@example.com".to_owned(),
            until: ts(WEEK1),
        }));
        let text = render(
            &log(),
            &burn,
            Span::Week,
            now,
            &TimeZone::UTC,
            94,
            Style::Plain,
        );
        assert!(
            text.starts_with("last monitor pass Thu 2026-10-01 08:00 UTC (10 min ago)\n"),
            "{text}"
        );
        assert!(
            text.contains("burn: a@example.com until Sun 2026-10-04 16:00 UTC\n"),
            "{text}"
        );
        assert!(
            text.contains("a@example.com  priority 20  BURNING  7d 16% (resets Sun 10-04 16:00)  5h 5% (resets 10:00)\n"),
            "{text}"
        );
        assert!(text.contains("  8.0% ┤"), "{text}");
        let chart: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("a@example.com"))
            .skip(1)
            .take(7)
            .collect();
        // The chart spans the account's whole week (rollovers on Sun 27 and
        // Sun 04 at 16:00) with its margins, gauges beside it; now
        // (Thu 01 08:10) falls in the bucket after the two used ones, which
        // has seen nothing yet, so only the thin now line shows there.
        let row = |label: &str, fill: &str| {
            format!(
                "{label:>7} ┤{f6}│{f19}██│{blank19}│",
                f6 = fill.repeat(6),
                f19 = fill.repeat(19),
                blank19 = " ".repeat(19)
            )
        };
        assert_eq!(
            chart,
            vec![
                format!("{}       7d ▓▓▓░░░░░░░░░░░░░░░░░  16%", row("8.0%", " ")),
                format!("{}       5h ▓░░░░░░░░░░░░░░░░░░░   5%", row("", " ")),
                row("", " "),
                row("0%", " "),
                "    /4h └──┬───┼───────┬───────────┬─┼─────────┬─────────┼─────".to_owned(),
                "           Sun 27      Tue 29      Thu 01      Sat 03".to_owned(),
                "               │ rollover Sun 16:00  │ now               │ rollover Sun 04 16:00"
                    .to_owned(),
            ],
            "{text}"
        );
        // Every marker's glyph on the label line sits under its axis tick,
        // which sits under the line through the chart.
        let column = |line: &str, c: char| -> Vec<usize> {
            line.chars()
                .enumerate()
                .filter(|(_, x)| *x == c)
                .map(|(i, _)| i)
                .collect()
        };
        assert_eq!(
            column(chart[4], '┼'),
            column(
                &chart[6][..chart[6].find(" rollover Sun 04").unwrap() + 1],
                '│'
            )
        );
        assert!(text.contains("dead@example.com  priority 10\n"), "{text}");
        assert!(text.contains("  usage lookup failed (no priorities are written until fixed): usage endpoint returned HTTP 401: token revoked\n  no usage history yet\n"), "{text}");
        assert!(text.contains("disabled: off@example.com"));
        assert!(!text.contains('\x1b'), "plain output has no escapes");
    }

    /// Observed idle time sits on a dotted 0% baseline, while time without
    /// data (before the history starts) is blank, so a gap between bursts
    /// of use never reads as missing data and missing data never reads as
    /// idle.
    #[test]
    fn idle_time_has_a_baseline_and_missing_data_does_not() {
        let mut records = Vec::new();
        let start = ts("2026-10-01T00:00:00Z");
        for i in 0..=16 {
            // Two hours of use, then idle, with the monitor down from 02:15
            // to 03:30 (no readings in between, a gap over 40 minutes).
            if (10..=13).contains(&i) {
                continue;
            }
            let used = if i <= 8 { i as f64 } else { 8.0 };
            records.push(json!({
                "v": 1, "time": (start + SignedDuration::from_mins(15 * i)).to_string(),
                "accounts": [{"email": "a@example.com", "managed": true, "provider": "claude",
                    "seven_day": {"utilization": used, "resets_at": WEEK1}, "five_hour": null}],
            }));
        }
        let text = render(
            &records,
            &Ok(None),
            Span::Recent,
            ts("2026-10-01T04:00:00Z"),
            &TimeZone::UTC,
            40,
            Style::Plain,
        );
        let baseline = text.lines().find(|l| l.starts_with("     0% ┤")).unwrap();
        let cells: String = baseline.chars().skip(9).collect();
        // 30 buckets ending with the one in progress, which starts exactly
        // now: 13 before the history (blank), 8 with use (bars), one idle
        // bucket observed at both ends (dot), 5 inside the outage (blank),
        // and 3 idle again (dots, including the one in progress).
        assert_eq!(
            cells,
            format!("{}{}·{}···", " ".repeat(13), "█".repeat(8), " ".repeat(5)),
            "{text}"
        );
    }

    /// The bucket in progress is drawn white, so a thick white bar means use
    /// in progress and a thin white line alone means none yet; rollovers
    /// are periwinkle.
    #[test]
    fn now_bucket_and_markers_are_colored() {
        let render_at = |now: &str| {
            render(
                &log(),
                &Ok(None),
                Span::Week,
                ts(now),
                &TimeZone::UTC,
                91,
                Style::Color,
            )
        };
        let busy = render_at("2026-10-01T07:50:00Z");
        assert!(
            busy.contains(&format!("{WHITE}█")),
            "use in progress is a white bar: {busy}"
        );
        let idle = render_at("2026-10-01T08:10:00Z");
        assert!(!idle.contains(&format!("{WHITE}█")), "{idle}");
        assert!(
            idle.contains(&format!("{WHITE}│")),
            "only the thin now line: {idle}"
        );
        assert!(idle.contains(&format!("{PERIWINKLE}│")), "{idle}");
    }

    /// Without a current week there are no rollovers to mark: neither when
    /// the week has not started nor when the recorded reset is already past
    /// (accounts from an older pass). The chart then shows the last week and
    /// the margin ahead, with now marked. The recent view has no markers.
    #[test]
    fn chart_range_without_a_current_week() {
        let now = ts("2026-10-01T08:10:00Z");
        for reset in [json!(null), json!("2026-09-30T16:00:00Z")] {
            let account = json!({"seven_day": {"utilization": 0.0, "resets_at": reset}});
            let (edges, markers) = chart_range(Span::Week, &account, now, 200, &TimeZone::UTC);
            assert_eq!(markers.len(), 1, "{reset}");
            assert_eq!(markers[0].labels, vec!["now"]);
            assert!(edges[0] <= now - WEEK && *edges.last().unwrap() >= now + WINDOW_MARGIN * 2);
        }
        let account = json!({"seven_day": {"utilization": 0.0, "resets_at": null}});
        let (edges, markers) = chart_range(Span::Recent, &account, now, 10, &TimeZone::UTC);
        assert_eq!(edges.len(), 11);
        assert!(*edges.last().unwrap() > now && markers.is_empty());
    }

    /// The window spans the week with margins of about 21 hours, as close
    /// as the 4-hour grid allows. When the terminal is too narrow, the cut
    /// keeps now on the chart: early in a week the end of the window goes,
    /// not the present.
    #[test]
    fn chart_range_fits_the_week_and_keeps_now() {
        let account =
            json!({"seven_day": {"utilization": 1.0, "resets_at": "2026-10-04T16:00:00Z"}});
        let start = ts("2026-09-27T16:00:00Z");
        let end = ts("2026-10-04T16:00:00Z");
        let now = ts("2026-09-28T02:00:00Z");
        let (edges, _) = chart_range(Span::Week, &account, now, 200, &TimeZone::UTC);
        let (first, last) = (edges[0], *edges.last().unwrap());
        assert!(
            first <= start - WINDOW_MARGIN
                && first.duration_until(start - WINDOW_MARGIN) < Span::Week.bucket()
        );
        assert!(
            last >= end + WINDOW_MARGIN
                && (end + WINDOW_MARGIN).duration_until(last) < Span::Week.bucket()
        );

        let (edges, _) = chart_range(Span::Week, &account, now, 40, &TimeZone::UTC);
        assert_eq!(edges.len(), 41);
        assert!(
            edges[0] < start && now < *edges.last().unwrap(),
            "both the week's start and now are shown"
        );
        let late = ts("2026-10-04T10:00:00Z");
        let (edges, _) = chart_range(Span::Week, &account, late, 40, &TimeZone::UTC);
        assert!(
            *edges.last().unwrap() >= end + WINDOW_MARGIN,
            "late in the week the end stays"
        );
    }

    /// When now falls in the same column as the closing rollover, the two
    /// share it: the rollover's glyph stays, on the chart and on the
    /// label line, and the labels are joined rather than one hiding the
    /// other.
    #[test]
    fn markers_in_one_column_are_merged() {
        // A reset inside a bucket (16:30), with now just before it in the
        // same bucket and, at this width, the same column.
        let mut records = log();
        records.last_mut().unwrap()["accounts"][0]["seven_day"]["resets_at"] =
            json!("2026-10-04T16:30:00Z");
        let text = render(
            &records,
            &Ok(None),
            Span::Week,
            ts("2026-10-04T16:10:00Z"),
            &TimeZone::UTC,
            91,
            Style::Plain,
        );
        let line = text.lines().find(|l| l.contains("now")).unwrap();
        assert!(line.contains("│ rollover Sun 04 16:30 · now"), "{line}");
    }

    /// `--privacy` output names no account: shown accounts are numbered in
    /// display order (accounts of other providers, which are not shown, take
    /// no number), auth file names quoted in errors become their account's
    /// label, a burn of a shown account uses its label, an unknown address
    /// (even one containing a known address) becomes `<email>`, case does
    /// not matter, the home directory becomes `~`, and color escapes survive.
    #[test]
    fn anonymize_replaces_every_account_reference() {
        let mut records = log();
        let last = records.last_mut().unwrap();
        last["accounts"][0]["name"] = json!("claude-1a2b-a@example.com.json");
        last["accounts"]
            .as_array_mut()
            .unwrap()
            .insert(1, json!({"name": "codex-x@example.com.json", "email": "x@example.com", "provider": "codex", "managed": false}));
        last["error"] = json!(
            "usage lookup failed for claude-1a2b-A@Example.com.json; also xa@example.com and /home/someone/.config"
        );
        let burn = Ok(Some(Burn {
            email: "a@example.com".to_owned(),
            until: ts(WEEK1),
        }));
        let text = render(
            &records,
            &burn,
            Span::Week,
            ts("2026-10-01T08:10:00Z"),
            &TimeZone::UTC,
            91,
            Style::Color,
        );
        let private = anonymize(&text, &records, Some(std::path::Path::new("/home/someone")));
        assert!(!private.contains('@'), "{private}");
        assert!(!private.contains("someone"), "{private}");
        assert!(
            private.contains(&format!("{BOLD}Account 1{RESET}  priority")),
            "{private}"
        );
        assert!(
            private.contains(&format!("{BOLD}Account 2{RESET}  priority")),
            "{private}"
        );
        assert!(private.contains("disabled: Account 3\n"), "{private}");
        assert!(
            private.contains("usage lookup failed for Account 1; also <email> and ~/.config"),
            "{private}"
        );
        assert!(private.contains("burn: Account 1 until"), "{private}");

        let plain = render(
            &records,
            &Ok(None),
            Span::Week,
            ts("2026-10-01T08:10:00Z"),
            &TimeZone::UTC,
            91,
            Style::Plain,
        );
        assert!(!anonymize(&plain, &records, None).contains('@'));
        assert_eq!(
            anonymize("x@y.io", &[], None),
            "<email>",
            "with no record listing accounts, every address is scrubbed"
        );
    }

    /// The token rules: escapes stay intact, non-ASCII addresses are caught
    /// whole, a sentence's final dot survives, and things that are not
    /// addresses (no dot in the domain) are left alone.
    #[test]
    fn email_tokens_are_found_whole() {
        let scrub = |t: &str| replace_emails(t, &|_| "<email>".to_owned());
        assert_eq!(
            scrub("\x1b[1mx@y.io\x1b[0m and a@b, not-an-email@localhost"),
            "\x1b[1m<email>\x1b[0m and a@b, not-an-email@localhost"
        );
        assert_eq!(scrub("jörg@exämple.com."), "<email>.");
    }

    /// When the latest pass saw no accounts (paused, proxy down), the charts
    /// still come from the newest pass that did, labelled as such, and the
    /// priorities shown are the ones that pass left in place.
    #[test]
    fn a_failed_latest_pass_keeps_the_charts() {
        let mut records = log();
        records.last_mut().unwrap()["error"] = json!("verification failed");
        records.push(
            json!({"v": 1, "time": "2026-10-01T08:05:00Z", "accounts": [],
                            "error": "listing auth files: connection refused", "paused": false}),
        );
        let text = render(
            &records,
            &Ok(None),
            Span::Week,
            ts("2026-10-01T08:10:00Z"),
            &TimeZone::UTC,
            91,
            Style::Plain,
        );
        assert!(
            text.contains("last pass failed: listing auth files: connection refused\n"),
            "{text}"
        );
        assert!(
            text.contains("accounts as of the last pass that saw them, Thu 2026-10-01 08:00 UTC\n"),
            "{text}"
        );
        assert!(
            text.contains("a@example.com  priority 10  7d"),
            "an unverified plan is not shown as applied: {text}"
        );
        assert!(text.contains("   8.0% ┤"), "{text}");
    }

    /// The gauge fills whole 5% cells, rounding down so only an exhausted
    /// window is full, spells the exact percentage out, clamps what it draws
    /// to the bar while still printing the real value, turns yellow and then
    /// red as a window nears its limit, and shows `?` for an unknown value
    /// or one whose window has reset since; a window that has not started
    /// is current.
    #[test]
    fn gauges_show_the_current_level() {
        let now = ts("2026-10-01T08:00:00Z");
        let later = Some(ts("2026-10-02T00:00:00Z"));
        let g = |used: Option<f64>, style| gauge("7d", used, later, now, style);
        assert_eq!(g(Some(16.0), Style::Plain), "7d ▓▓▓░░░░░░░░░░░░░░░░░  16%");
        assert_eq!(
            g(Some(0.0), Style::Plain),
            format!("7d {}   0%", "░".repeat(20))
        );
        assert_eq!(
            g(Some(99.0), Style::Plain),
            format!("7d {}░  99%", "▓".repeat(19)),
            "only an exhausted window draws a full bar"
        );
        assert_eq!(
            g(Some(104.0), Style::Plain),
            format!("7d {} 104%", "▓".repeat(20))
        );
        assert_eq!(g(None, Style::Plain), format!("7d {}?", " ".repeat(24)));
        assert_eq!(
            gauge(
                "7d",
                Some(58.0),
                Some(ts("2026-10-01T07:00:00Z")),
                now,
                Style::Plain
            ),
            format!("7d {}?", " ".repeat(24)),
            "a reading from before the window reset is out of date"
        );
        assert_eq!(
            gauge("5h", Some(0.0), None, now, Style::Plain),
            format!("5h {}   0%", "░".repeat(20)),
            "a window that has not started is current"
        );
        assert_eq!(
            g(Some(50.0), Style::Color),
            format!("7d {BLUE}{}{}  50%{RESET}", "▓".repeat(10), "░".repeat(10)),
            "bar and number share one color span"
        );
        assert!(g(Some(85.0), Style::Color).contains(YELLOW));
        assert!(g(Some(96.0), Style::Color).contains(RED));
    }

    /// Gauges are drawn only when the chart beside them still gets a whole
    /// week (54 buckets): from 94 columns, not at 93, in both views. Without
    /// them the chart gets the whole width. Where drawn, they start in the
    /// same column for every account, and charts share one bucket width.
    #[test]
    fn gauges_give_way_on_narrow_terminals_and_line_up() {
        let mut records = log();
        let last = records.last_mut().unwrap();
        let mut other = last["accounts"][0].clone();
        other["email"] = json!("b@example.com");
        other["seven_day"]["resets_at"] = json!("2026-10-05T02:00:00Z");
        last["accounts"].as_array_mut().unwrap().push(other);
        let now = ts("2026-10-01T08:10:00Z");
        let at = |span, width| {
            render(
                &records,
                &Ok(None),
                span,
                now,
                &TimeZone::UTC,
                width,
                Style::Plain,
            )
        };
        let has_gauges = |text: &str| {
            text.lines()
                .any(|l| l.contains("  7d ") && l.contains('%') && l.contains('┤'))
        };
        for span in [Span::Week, Span::Recent] {
            assert!(has_gauges(&at(span, 94)), "{span:?} at 94");
            assert!(!has_gauges(&at(span, 93)), "{span:?} at 93");
        }
        // At 93 columns the week chart has 83 columns, enough for the whole
        // range: both rollovers are on it.
        let narrow = at(Span::Week, 93);
        for axis in narrow.lines().filter(|l| l.contains('└')) {
            assert_eq!(
                axis.matches('┼').count(),
                3,
                "both rollovers and now: {narrow}"
            );
        }

        let wide = at(Span::Week, 200);
        let gauge_columns: Vec<usize> = wide
            .lines()
            .filter_map(|l| {
                l.find("  7d ")
                    .filter(|_| l.contains('┤'))
                    .map(|b| l[..b].chars().count())
            })
            .collect();
        assert_eq!(gauge_columns.len(), 2, "{wide}");
        assert_eq!(gauge_columns[0], gauge_columns[1], "{wide}");
        let axis_widths: Vec<usize> = wide
            .lines()
            .filter(|l| l.contains('└'))
            .map(|l| l.chars().count())
            .collect();
        assert!(axis_widths.iter().all(|w| *w <= gauge_columns[0]), "{wide}");
    }

    /// Stale data and a failed or paused last pass are impossible to miss.
    #[test]
    fn warns_about_a_stopped_or_failing_monitor() {
        let mut records = log();
        let last = records.last_mut().unwrap();
        last["error"] = json!("the management key was rejected earlier");
        last["paused"] = json!(true);
        let text = render(
            &records,
            &Ok(None),
            Span::Week,
            ts("2026-10-01T09:00:00Z"),
            &TimeZone::UTC,
            80,
            Style::Color,
        );
        assert!(text.contains("monitor appears to have stopped"), "{text}");
        assert!(text.contains(&format!(
            "{RED}paused: the management key was rejected earlier{RESET}"
        )));
        assert!(text.contains("burn: none"));
        assert_eq!(
            render(
                &[],
                &Ok(None),
                Span::Week,
                ts("2026-10-01T09:00:00Z"),
                &TimeZone::UTC,
                80,
                Style::Plain
            ),
            "no monitor records yet; start the monitor with `kd cli-proxy-api monitor enable`\n"
        );
    }
}
