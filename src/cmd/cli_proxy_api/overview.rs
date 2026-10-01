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

/// Chart height in rows; each row resolves eight levels.
const ROWS: usize = 4;

/// Width of the left margin holding the scale labels and the axis.
const MARGIN: usize = 9;

/// Which time span to chart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Span {
    /// The last 7 days in 4-hour buckets, trimmed to the terminal width.
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
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
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
        Some(&(t1, _)) if t0.duration_until(t1) > MAX_GAP => None,
        Some(&(t1, v1)) => {
            let span = t0.duration_until(t1).as_secs_f64();
            let into = t0.duration_until(t).as_secs_f64();
            Some(v0 + (v1 - v0) * into / span)
        }
        None if t == t0 || t0.duration_until(now) <= STALE_AFTER => Some(v0),
        None => None,
    }
}

/// Bucket edges for `count` buckets ending with the one containing `now`.
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
    let has_accounts = |r: &&Value| r["accounts"].as_array().is_some_and(|a| !a.is_empty());
    let Some(seen) = records.iter().rev().find(has_accounts) else {
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

    let columns = width.saturating_sub(MARGIN + 1).max(1);
    let (count, cell) = match span {
        // A week is 42 buckets; widen them when the terminal has room,
        // trim the oldest when it does not.
        Span::Week => (42.min(columns), (columns / 42).clamp(1, 3)),
        Span::Recent => (columns, 1),
    };
    let edges = edges(span, count, now, tz);

    let accounts = seen["accounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let burned = burn
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .filter(|b| b.is_active(now));
    for account in accounts.iter().filter(|a| a["managed"] == true) {
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
        let values: Vec<Option<f64>> = edges.iter().map(|e| value_at(&series, *e, now)).collect();
        let used: Vec<Option<f64>> = values
            .windows(2)
            .map(|w| Some((w[1]? - w[0]?).max(0.0)))
            .collect();
        let color = if is_burned { MAGENTA } else { CYAN };
        chart(&mut out, &used, &edges, span, cell, tz, style, color);
    }
    let disabled: Vec<&str> = accounts
        .iter()
        .filter(|a| {
            a["managed"] != true
                && a["provider"]
                    .as_str()
                    .is_some_and(|p| p.eq_ignore_ascii_case("claude"))
        })
        .filter_map(account_key)
        .collect();
    if !disabled.is_empty() {
        out.push_str(&format!("\ndisabled: {}\n", disabled.join(", ")));
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

/// A bar chart of per-bucket consumption with a scale on the left and day
/// (and, for the recent view, hour) marks below. Unknown buckets, before the
/// history starts or while the monitor was down, show as a dot.
#[allow(clippy::too_many_arguments)]
fn chart(
    out: &mut String,
    used: &[Option<f64>],
    edges: &[Timestamp],
    span: Span,
    cell: usize,
    tz: &TimeZone,
    style: Style,
    color: &str,
) {
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let scale = used.iter().flatten().fold(MIN_SCALE, |m, v| m.max(*v));
    let unit = match span {
        Span::Week => "4h",
        Span::Recent => "15m",
    };
    let levels: Vec<Option<usize>> = used
        .iter()
        .map(|v| v.map(|v| ((v / scale) * (ROWS * 8) as f64).round() as usize))
        .collect();
    for row in 0..ROWS {
        let label = match row {
            0 => format!("{scale:.1}%"),
            r if r == ROWS - 1 => "0%".to_owned(),
            _ => String::new(),
        };
        let mut bars = String::new();
        for level in &levels {
            let glyph = match level {
                Some(level) => LEVELS[level.saturating_sub((ROWS - 1 - row) * 8).min(8)],
                None if row == ROWS - 1 => '·',
                None => ' ',
            };
            bars.extend(std::iter::repeat_n(glyph, cell));
        }
        out.push_str(&format!(
            "{label:>7} ┤{}\n",
            style.paint(color, bars.trim_end())
        ));
    }

    // Axis with a tick at each labelled bucket, and the labels under it.
    let width = used.len() * cell;
    let mut axis: Vec<char> = vec!['─'; width];
    let mut labels: Vec<char> = vec![' '; width];
    let mut free_from = 0;
    for (i, start) in edges[..used.len()].iter().enumerate() {
        let zoned = start.to_zoned(tz.clone());
        // Buckets are aligned to the local clock, so a day starts exactly at
        // a bucket that starts at midnight. The first bucket gets no label
        // unless it does too: a mid-day start labelled with its date would
        // read as a day boundary.
        let label = if zoned.hour() == 0 && zoned.minute() == 0 {
            Some(zoned.strftime("%a %d").to_string())
        } else if span == Span::Recent && zoned.minute() == 0 && zoned.hour() % 3 == 0 {
            Some(zoned.strftime("%H:%M").to_string())
        } else {
            None
        };
        let column = i * cell;
        if let Some(label) = label
            && column >= free_from
            && column + label.chars().count() <= width
        {
            axis[column] = '┬';
            for (j, c) in label.chars().enumerate() {
                labels[column + j] = c;
            }
            free_from = column + label.chars().count() + 1;
        }
    }
    let axis: String = axis.into_iter().collect();
    let labels: String = labels.into_iter().collect();
    out.push_str(&format!("{:>7} └{axis}\n", format!("/{unit}")));
    out.push_str(&format!("{:>8}{}\n", "", labels.trim_end()));
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
            60,
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
            .take(6)
            .collect();
        let gap = " ".repeat(39);
        let dots = "·".repeat(39);
        assert_eq!(
            chart,
            vec![
                format!("   8.0% ┤{gap}██"),
                format!("        ┤{gap}██"),
                format!("        ┤{gap}██"),
                format!("     0% ┤{dots}██"),
                "    /4h └───┬───────────┬───────────┬──────────────".to_owned(),
                "           Fri 25      Sun 27      Tue 29".to_owned(),
            ],
            "{text}"
        );
        assert!(text.contains("dead@example.com  priority 10\n"), "{text}");
        assert!(text.contains("  usage lookup failed (no priorities are written until fixed): usage endpoint returned HTTP 401: token revoked\n  no usage history yet\n"), "{text}");
        assert!(text.contains("disabled: off@example.com"));
        assert!(!text.contains('\x1b'), "plain output has no escapes");
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
            60,
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
