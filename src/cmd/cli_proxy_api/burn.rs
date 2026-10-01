//! `kd cli-proxy-api burn EMAIL | --clear`: the manual override that pins
//! one Claude account to the top priority regardless of reset order.
//!
//! The monitor's ordering never wastes quota on its own, but it cannot know
//! about a banked limit reset sitting unused on an account: pressing one
//! restores that account's weekly limit without moving its schedule, so it
//! is worth most when the account is drained with much of its week left.
//! Deciding to drain an account early for that is a human judgment about
//! the week ahead; `burn` is how the human says so.
//!
//! The override lives in a small TOML file the monitor reads on every wake
//! and watches for changes, so setting or clearing it takes effect within
//! seconds without signalling the daemon. A burn ends on its own at the
//! account's next weekly reset as known when it was set: after that the
//! week it was meant for is over, and a forgotten override would otherwise
//! misroute traffic indefinitely.
//!
//! `burn` never talks to CLIProxyAPI. It looks the account up in the
//! monitor's latest log record, which already holds every account's email
//! and weekly reset. That keeps it free of `--url`/`--key-file` settings
//! that would have to match the daemon's, means it cannot add strikes
//! toward CLIProxyAPI's bad-key ban, and doubles as a check that the
//! monitor is actually running.

use super::history;
use anyhow::{Context, bail};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// How old the latest log record may be before `burn` concludes the
/// monitor is not running: two regular intervals, so one slow or skipped
/// wake does not trip it.
const MAX_RECORD_AGE: SignedDuration = SignedDuration::from_mins(30);

/// Fallback burn length for an account whose weekly window has not started.
/// Burning it starts the window, which then runs for a week.
const WEEK: SignedDuration = SignedDuration::from_hours(7 * 24);

/// The override file, `~/.config/kd/cli-proxy-api-monitor.toml`.
pub fn config_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join("kd")
        .join("cli-proxy-api-monitor.toml")
}

/// One active override: burn the account with this email until this time.
#[derive(Clone, Debug, PartialEq)]
pub struct Burn {
    pub email: String,
    pub until: Timestamp,
}

impl Burn {
    /// Whether the override still applies at `now`.
    pub fn is_active(&self, now: Timestamp) -> bool {
        now < self.until
    }
}

/// On-disk shape. Times are quoted RFC 3339 strings so the file stays
/// readable and hand-editable. Unknown keys are errors, so a misspelled key
/// in a hand edit is reported instead of silently ignored.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    burn: Option<RawBurn>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawBurn {
    email: String,
    until: String,
}

/// The override in the config file, if any. A missing file means none. A
/// file that does not parse is an error: the monitor reports it and plans
/// without an override rather than guessing what was meant.
pub fn read(path: &Path) -> anyhow::Result<Option<Burn>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let raw: RawConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    raw.burn
        .map(|b| {
            Ok(Burn {
                until: b.until.parse().with_context(|| {
                    format!(
                        "{}: burn.until {:?} is not a timestamp",
                        path.display(),
                        b.until
                    )
                })?,
                email: b.email,
            })
        })
        .transpose()
}

/// Replace the file with one holding `burn`, atomically: the monitor may
/// read it at any moment, and a rename is also what its directory watch
/// sees as the change.
fn write(path: &Path, burn: &Burn) -> anyhow::Result<()> {
    let raw = RawConfig {
        burn: Some(RawBurn {
            email: burn.email.clone(),
            until: burn.until.to_string(),
        }),
    };
    let text = format!(
        "# Written by `kd cli-proxy-api burn`; `kd cli-proxy-api burn --clear` removes it.\n{}",
        toml::to_string(&raw)?
    );
    let dir = path.parent().context("config path has no parent")?;
    // Private, like the directory the monitor creates when it starts first.
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, text.as_bytes())?;
    tmp.persist(path)?;
    Ok(())
}

/// Plan a burn of `email` from the monitor's latest log record.
///
/// Refuses when there is no record, when it is too old to trust (the
/// monitor is probably not running, so the burn would do nothing and its
/// expiry would come from stale data), or when the email does not name an
/// enabled Claude account in it. The expiry is that account's weekly reset,
/// or a week from now when its window has not started.
pub fn plan_burn(record: Option<&Value>, email: &str, now: Timestamp) -> anyhow::Result<Burn> {
    let Some(record) = record else {
        bail!(
            "the monitor log has no records; start the monitor first (`kd cli-proxy-api monitor enable`)"
        );
    };
    let time: Timestamp = record["time"]
        .as_str()
        .and_then(|t| t.parse().ok())
        .context("latest log record has no valid time")?;
    let age = time.duration_until(now);
    if age > MAX_RECORD_AGE {
        bail!(
            "the latest monitor log record is {} minutes old; the monitor does not appear to be running \
             (`kd cli-proxy-api monitor enable`, or check `journalctl --user -u kd-cli-proxy-api-monitor`)",
            age.as_mins()
        );
    }
    let accounts = record["accounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    // A paused or failed pass lists no accounts; say why rather than
    // reporting every email as unknown.
    if accounts.is_empty()
        && let Some(error) = record["error"].as_str()
    {
        bail!("the monitor's latest pass could not see the accounts ({error}); fix that first");
    }
    let Some(account) = accounts.iter().find(|a| {
        a["email"]
            .as_str()
            .is_some_and(|e| e.eq_ignore_ascii_case(email))
    }) else {
        let known: Vec<&str> = accounts
            .iter()
            .filter(|a| a["managed"] == true)
            .filter_map(|a| a["email"].as_str())
            .collect();
        bail!(
            "no account with email {email} in the latest monitor record; managed accounts: {}",
            known.join(", ")
        );
    };
    if account["managed"] != true {
        bail!(
            "{email} is not an enabled Claude account, so the monitor does not manage its priority"
        );
    }
    let Some(seven_day) = account["seven_day"].as_object() else {
        bail!(
            "the latest monitor record has no usage for {email} (its lookup failed); try again after the next wake"
        );
    };
    let until = match seven_day.get("resets_at").and_then(Value::as_str) {
        Some(reset) => reset
            .parse::<Timestamp>()
            .with_context(|| format!("weekly reset {reset:?} for {email} is not a timestamp"))?,
        None => now + WEEK,
    };
    if until <= now {
        bail!("{email}'s weekly reset ({until}) has already passed; try again after the next wake");
    }
    Ok(Burn {
        email: account["email"].as_str().unwrap_or(email).to_owned(),
        until,
    })
}

/// `burn EMAIL`: write the override and say until when it holds.
/// The end time is shown in `tz`, like the monitor's other output.
pub fn set(
    config: &Path,
    log: &Path,
    email: &str,
    now: Timestamp,
    tz: &TimeZone,
) -> anyhow::Result<String> {
    let burn = plan_burn(history::latest_record(log)?.as_ref(), email, now)?;
    let replaced = read(config)
        .ok()
        .flatten()
        .filter(|b| b.is_active(now) && b.email != burn.email);
    write(config, &burn).with_context(|| format!("writing {}", config.display()))?;
    let mut out = format!(
        "burning {} until {} (its weekly reset); the monitor applies it within seconds\n",
        burn.email,
        burn.until
            .to_zoned(tz.clone())
            .strftime("%a %Y-%m-%d %H:%M %Z")
    );
    if let Some(old) = replaced {
        out.push_str(&format!("replaced the burn of {}\n", old.email));
    }
    Ok(out)
}

/// `burn --clear`: remove the override file. Clearing when nothing is set
/// is not an error.
pub fn clear(config: &Path) -> anyhow::Result<String> {
    match std::fs::remove_file(config) {
        Ok(()) => {
            Ok("burn cleared; the monitor returns to reset order within seconds\n".to_owned())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok("no burn was set\n".to_owned())
        }
        Err(err) => Err(err).with_context(|| format!("removing {}", config.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-10-01T12:00:00Z";

    fn record(time: &str) -> Value {
        json!({
            "v": 1,
            "time": time,
            "accounts": [
                {"email": "a@example.com", "managed": true,
                 "seven_day": {"utilization": 50.0, "resets_at": "2026-10-04T16:00:00Z"}},
                {"email": "fresh@example.com", "managed": true,
                 "seven_day": {"utilization": 0.0, "resets_at": null}},
                {"email": "off@example.com", "managed": false, "seven_day": null},
                {"email": "dead@example.com", "managed": true, "seven_day": null},
            ],
        })
    }

    /// The burn lasts until the account's weekly reset, matched by email
    /// regardless of case; an unstarted week burns for a week from now.
    #[test]
    fn burn_ends_at_the_weekly_reset() {
        let now = ts(NOW);
        let latest = record("2026-10-01T11:50:00Z");
        assert_eq!(
            plan_burn(Some(&latest), "A@Example.com", now).unwrap(),
            Burn {
                email: "a@example.com".to_owned(),
                until: ts("2026-10-04T16:00:00Z")
            }
        );
        assert_eq!(
            plan_burn(Some(&latest), "fresh@example.com", now)
                .unwrap()
                .until,
            ts("2026-10-08T12:00:00Z")
        );
    }

    /// Every refusal names its reason: no monitor, a stale monitor, an
    /// unknown or unmanaged account, or one whose lookup failed.
    #[test]
    fn burn_refuses_what_it_cannot_honour() {
        let now = ts(NOW);
        let err = |record: Option<&Value>, email: &str| {
            plan_burn(record, email, now).unwrap_err().to_string()
        };
        assert!(err(None, "a@example.com").contains("no records"));
        assert!(
            err(Some(&record("2026-10-01T11:00:00Z")), "a@example.com").contains("60 minutes old")
        );
        let paused = json!({"v": 1, "time": "2026-10-01T11:59:00Z", "accounts": [],
                            "error": "the management key was rejected earlier"});
        assert!(err(Some(&paused), "a@example.com").contains(
            "latest pass could not see the accounts (the management key was rejected earlier)"
        ));
        let fresh = record("2026-10-01T11:59:00Z");
        let fresh = Some(&fresh);
        let unknown = err(fresh, "x@example.com");
        assert!(
            unknown
                .contains("managed accounts: a@example.com, fresh@example.com, dead@example.com"),
            "{unknown}"
        );
        assert!(err(fresh, "off@example.com").contains("not an enabled Claude account"));
        assert!(err(fresh, "dead@example.com").contains("lookup failed"));
    }

    /// The file round-trips, `--clear` removes it, and clearing twice is
    /// fine. A burn whose time has passed reads back but is inactive.
    #[test]
    fn config_round_trip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_file(dir.path());
        let log = dir.path().join("log.jsonl");
        std::fs::write(&log, format!("{}\n", record("2026-10-01T11:55:00Z"))).unwrap();
        assert_eq!(read(&config).unwrap(), None);

        let out = set(&config, &log, "a@example.com", ts(NOW), &TimeZone::UTC).unwrap();
        assert!(out.contains("until Sun 2026-10-04 16:00 UTC"), "{out}");
        let burn = read(&config).unwrap().unwrap();
        assert_eq!(burn.email, "a@example.com");
        assert!(burn.is_active(ts(NOW)));
        assert!(!burn.is_active(ts("2026-10-04T16:00:00Z")));

        let out = set(&config, &log, "fresh@example.com", ts(NOW), &TimeZone::UTC).unwrap();
        assert!(out.contains("replaced the burn of a@example.com"), "{out}");

        assert!(clear(&config).unwrap().contains("burn cleared"));
        assert!(clear(&config).unwrap().contains("no burn was set"));
        assert_eq!(read(&config).unwrap(), None);
    }

    /// A hand-edited file with a bad timestamp is an error, not a silent
    /// "no burn", so the monitor can report it.
    #[test]
    fn malformed_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        std::fs::write(
            &config,
            "[burn]\nemail = \"a@example.com\"\nuntil = \"soon\"\n",
        )
        .unwrap();
        assert!(
            read(&config)
                .unwrap_err()
                .to_string()
                .contains("not a timestamp")
        );
        std::fs::write(
            &config,
            "[burn]\nemial = \"a@example.com\"\nuntil = \"2026-10-04T16:00:00Z\"\n",
        )
        .unwrap();
        assert!(read(&config).is_err(), "a misspelled key is reported");
        std::fs::write(&config, "not toml [").unwrap();
        assert!(read(&config).is_err());
    }
}
