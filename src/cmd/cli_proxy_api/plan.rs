//! Pure planning: from observed accounts and their quota windows to the
//! priorities CLIProxyAPI should hold, plus the anomaly flags a human (or a
//! watching agent) should look at. No I/O lives here, so every rule is
//! testable with literal data.
//!
//! The policy is deliberately the simplest one that serves the goal "don't
//! let paid quota expire unused": strict priority by weekly reset time. The
//! account whose weekly window resets soonest is used first, because its
//! remaining quota is the first to be lost. CLIProxyAPI already skips an
//! account that hits a limit (cooldown, then failover to the next priority
//! tier), so nothing here tries to model 5-hour windows or balance load.
//! Plan sizes do not matter under this rule, which is why none are asked for.

use jiff::Timestamp;

/// Distance between adjacent priority tiers when kd renumbers. CLIProxyAPI
/// only compares priorities, so the spacing carries no meaning; it just
/// keeps written values readable.
pub const PRIORITY_STEP: i64 = 10;

/// How far past the latest of Anthropic's reset times a quota cooldown may
/// run before it is flagged. The margin only keeps a cooldown that ends
/// around the reset itself (clock skew between CLIProxyAPI and Anthropic,
/// a retry scheduled a moment after the reset) from being reported; the
/// bug it hunts overruns by days.
const COOLDOWN_GRACE_SECONDS: i64 = 15 * 60;

/// One credential as CLIProxyAPI reports it in `GET /auth-files`, reduced to
/// what planning and the log need.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// Auth file name; the key `PATCH /auth-files/fields` accepts.
    pub name: String,
    /// Opaque index the management `api-call` endpoint uses to pick the
    /// credential whose token it substitutes.
    pub auth_index: String,
    pub provider: String,
    pub email: Option<String>,
    /// Current routing priority; CLIProxyAPI treats a missing value as 0.
    pub priority: i64,
    pub disabled: bool,
    pub unavailable: bool,
    pub status: String,
    pub status_message: String,
    pub next_retry_after: Option<Timestamp>,
    pub cooldowns: Vec<Cooldown>,
    /// Totals over CLIProxyAPI's short `recent_requests` buckets. Useful in
    /// the log to see whether a top-priority account is actually serving.
    pub recent_success: u64,
    pub recent_failed: u64,
}

/// One active cooldown from `GET /auth-files`.
#[derive(Clone, Debug, PartialEq)]
pub struct Cooldown {
    pub scope: String,
    pub model_key: Option<String>,
    pub reason: String,
    pub retry_at: Option<Timestamp>,
    pub http_status: Option<u16>,
}

/// One quota window from Anthropic's `oauth/usage` response.
/// `utilization` is a percentage (0-100). `resets_at` is absent when the
/// window has not started, which Anthropic reports as `null`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Window {
    pub utilization: Option<f64>,
    pub resets_at: Option<Timestamp>,
}

/// The two subscription windows planning and flagging look at.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub five_hour: Window,
    pub seven_day: Window,
}

/// An account together with what the usage lookup returned for it.
/// `usage` is `None` for accounts that were not looked up (not managed);
/// a managed account whose lookup failed never reaches planning, because
/// the caller aborts before writing anything (see [`plan`]).
#[derive(Clone, Debug)]
pub struct Observed {
    pub account: Account,
    pub usage: Option<Usage>,
}

/// Only enabled Claude accounts are managed. Other providers are routed by
/// their own rules, and a disabled account's priority is irrelevant until
/// the user re-enables it, at which point the next run picks it up.
pub fn is_managed(account: &Account) -> bool {
    account.provider.eq_ignore_ascii_case("claude") && !account.disabled
}

/// A priority change the plan wants.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub name: String,
    pub from: i64,
    pub to: i64,
}

/// Target priority for every managed account, keyed by account name, in
/// descending priority order, plus the changes needed to get there.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub targets: Vec<(String, i64)>,
    pub changes: Vec<Change>,
}

impl Plan {
    /// Planned priority for `name`, or `None` for an unmanaged account.
    pub fn target(&self, name: &str) -> Option<i64> {
        self.targets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, priority)| *priority)
    }
}

/// Assign priorities by weekly reset time: soonest reset gets the highest
/// priority. Accounts whose resets round to the same minute share a tier, so
/// CLIProxyAPI round-robins between them (they are equally urgent). An
/// account without a known weekly reset (window not started) sorts last:
/// its quota is not on a clock yet, so it is the last place to spend.
///
/// If the current priorities already rank the managed accounts the same
/// way (same order, same ties), the plan keeps them as they are, whatever
/// their values, so hand-set numbers survive and a settled pool is never
/// rewritten. When the order differs, the accounts are renumbered: the
/// highest tier to `PRIORITY_STEP * tiers`, down to `PRIORITY_STEP`, and
/// only accounts whose value changes are written. That is not a minimal
/// set of moves: when the soonest account resets and drops to the bottom,
/// every account moves up, so a weekly rotation typically rewrites the
/// whole pool. Pools are a handful of accounts, and rotations happen about
/// once per account per week, so the simpler numbering wins. Writes are
/// still worth keeping rare because of a risk noted in SPEC_impl.md: a
/// write makes CLIProxyAPI rebuild the account, and it is unverified
/// whether live cooldown state survives that.
/// Ties inside a tier are ordered by name only to make the output and log
/// deterministic.
///
/// Callers must pass usage for every managed account. A managed account
/// without usage is a programming error upstream and panics, rather than
/// being guessed into some tier.
pub fn plan(observed: &[Observed]) -> Plan {
    let mut managed: Vec<(&Account, Option<i64>)> = observed
        .iter()
        .filter(|o| is_managed(&o.account))
        .map(|o| {
            let usage = o
                .usage
                .expect("managed account reached planning without usage");
            let minute = usage.seven_day.resets_at.map(nearest_minute);
            (&o.account, minute)
        })
        .collect();
    // `None` (no reset yet) must sort after every known reset.
    managed.sort_by(|(a, a_min), (b, b_min)| {
        (a_min.is_none(), a_min, &a.name).cmp(&(b_min.is_none(), b_min, &b.name))
    });

    let mut tiers: Vec<Option<i64>> = managed.iter().map(|(_, minute)| *minute).collect();
    tiers.dedup();
    let tier_count = tiers.len() as i64;

    let ranked: Vec<(&Account, i64)> = managed
        .into_iter()
        .map(|(account, minute)| {
            let rank = tiers.iter().position(|t| *t == minute).unwrap() as i64;
            (account, PRIORITY_STEP * (tier_count - rank))
        })
        .collect();

    let mut plan = Plan::default();
    if same_order(&ranked) {
        plan.targets = ranked
            .iter()
            .map(|(a, _)| (a.name.clone(), a.priority))
            .collect();
        return plan;
    }
    for (account, target) in ranked {
        plan.targets.push((account.name.clone(), target));
        if account.priority != target {
            plan.changes.push(Change {
                name: account.name.clone(),
                from: account.priority,
                to: target,
            });
        }
    }
    plan
}

/// The minute a reset time is bucketed into, rounded to the nearest minute.
/// Anthropic reports the same nominal reset with sub-second jitter on
/// either side of the minute (observed live: `14:00:00.054` in one run and
/// `13:59:59.507` in the next, for the same account), so truncating would
/// let two accounts with the same reset flip between tied and untied from
/// run to run, each flip a pointless rewrite.
fn nearest_minute(ts: Timestamp) -> i64 {
    (ts.as_millisecond() + 30_000).div_euclid(60_000)
}

/// Whether the current priorities rank every pair of accounts the way the
/// targets do: same order, and ties exactly where the targets tie. Pairwise
/// is fine at pool sizes of a handful of accounts.
fn same_order(ranked: &[(&Account, i64)]) -> bool {
    ranked.iter().enumerate().all(|(i, (a, a_target))| {
        ranked[i + 1..]
            .iter()
            .all(|(b, b_target)| a.priority.cmp(&b.priority) == a_target.cmp(b_target))
    })
}

/// Something worth a human look. None of these change the plan: the plan
/// is the same whatever the flags say, and flags only surface in output,
/// the log, and the warnings a watching agent reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    /// A quota cooldown runs more than a small grace period past the moment
    /// Anthropic says the quota is back: the reset of the last exhausted
    /// window, or the later of the two resets when neither is exhausted. A
    /// window without a reset (`null`, not started) counts as "available
    /// now". The account's
    /// quota will be back well before CLIProxyAPI tries it again, which is
    /// the signature of upstream issue #5770 (Claude accounts kept in a
    /// days-long cooldown after their quota recovered; only a restart
    /// cleared it). Strong signal.
    CooldownOutlivesReset,
    /// A quota cooldown is active while Anthropic reports headroom in both
    /// windows. Can be transient (Anthropic's numbers and CLIProxyAPI's
    /// cooldown are observed at slightly different moments), so it is a
    /// weak signal on its own; it matters when it persists across runs.
    BlockedWithQuota,
    /// Weekly quota is used up. Expected near the end of a window;
    /// CLIProxyAPI's cooldown already routes around it.
    WeeklyExhausted,
    /// 5-hour quota is used up; informational for the same reason.
    FiveHourExhausted,
}

impl Flag {
    /// Stable snake_case name for output and the JSON log.
    pub fn as_str(self) -> &'static str {
        match self {
            Flag::CooldownOutlivesReset => "cooldown_outlives_reset",
            Flag::BlockedWithQuota => "blocked_with_quota",
            Flag::WeeklyExhausted => "weekly_exhausted",
            Flag::FiveHourExhausted => "five_hour_exhausted",
        }
    }

    /// Whether this flag warrants a warning rather than just a log entry.
    pub fn is_warning(self) -> bool {
        matches!(self, Flag::CooldownOutlivesReset | Flag::BlockedWithQuota)
    }
}

/// Flags for one account as of `now`. Accounts without usage (unmanaged)
/// get no flags: every rule compares CLIProxyAPI's view with Anthropic's.
pub fn flags(observed: &Observed, now: Timestamp) -> Vec<Flag> {
    let Some(usage) = observed.usage else {
        return Vec::new();
    };
    let account = &observed.account;
    let mut flags = Vec::new();

    // Latest moment CLIProxyAPI intends to keep the account out for quota
    // reasons. Other cooldowns are deliberately ignored: CLIProxyAPI also
    // cools down single models for hours after errors such as an unsupported
    // model or a 401/403, and those say nothing about quota recovery.
    let quota_blocked_until = account
        .cooldowns
        .iter()
        .filter(|c| is_quota_cooldown(c))
        .filter_map(|c| c.retry_at)
        .filter(|ts| *ts > now)
        .max();

    let exhausted = |w: Window| w.utilization.is_some_and(|u| u >= 100.0);
    // When Anthropic says the quota is back. If a window is exhausted, that
    // is when the last exhausted window resets: a cooldown for a spent
    // 5-hour window must end at the 5-hour reset even though the weekly
    // window runs on for days. If nothing is exhausted, the later of the
    // two resets is used, which stays quiet about ordinary short cooldowns
    // and leaves them to the weak flag. A window without a reset has not
    // started, so its quota is available now.
    let windows = [usage.five_hour, usage.seven_day];
    let reset_or_now = |w: &Window| w.resets_at.unwrap_or(now);
    let back_at = if windows.iter().any(|w| exhausted(*w)) {
        windows
            .iter()
            .filter(|w| exhausted(**w))
            .map(reset_or_now)
            .max()
    } else {
        windows.iter().map(reset_or_now).max()
    }
    .unwrap_or(now);
    if let Some(until) = quota_blocked_until
        && until.as_second() > back_at.as_second() + COOLDOWN_GRACE_SECONDS
    {
        flags.push(Flag::CooldownOutlivesReset);
    }

    let has_headroom = |w: Window| w.utilization.is_some_and(|u| u < 100.0);
    if quota_blocked_until.is_some()
        && has_headroom(usage.five_hour)
        && has_headroom(usage.seven_day)
    {
        flags.push(Flag::BlockedWithQuota);
    }
    if exhausted(usage.seven_day) {
        flags.push(Flag::WeeklyExhausted);
    }
    if exhausted(usage.five_hour) {
        flags.push(Flag::FiveHourExhausted);
    }
    flags
}

/// Cooldowns CLIProxyAPI records for quota exhaustion. In v7.3.17 its
/// cooldown view labels them `quota` or `credential_quota` (a 429 always
/// maps to one of those). Both credential-wide and per-model cooldowns
/// count: the upstream bug these flags hunt was reported per model. The
/// cost is that a per-model weekly cap, which kd does not track, can raise
/// a flag while both tracked windows look fine.
fn is_quota_cooldown(cooldown: &Cooldown) -> bool {
    matches!(cooldown.reason.as_str(), "quota" | "credential_quota")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn account(name: &str, priority: i64) -> Account {
        Account {
            name: name.to_owned(),
            auth_index: format!("idx-{name}"),
            provider: "claude".to_owned(),
            email: None,
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

    fn usage(seven_day_reset: Option<&str>) -> Usage {
        Usage {
            five_hour: Window {
                utilization: Some(10.0),
                resets_at: Some(ts("2026-09-25T12:00:00Z")),
            },
            seven_day: Window {
                utilization: Some(40.0),
                resets_at: seven_day_reset.map(ts),
            },
        }
    }

    fn observed(account: Account, seven_day_reset: Option<&str>) -> Observed {
        Observed {
            account,
            usage: Some(usage(seven_day_reset)),
        }
    }

    /// The whole point of the command: the soonest weekly reset gets the
    /// highest priority, and only accounts whose value differs are written.
    #[test]
    fn soonest_weekly_reset_gets_highest_priority() {
        let plan = plan(&[
            observed(account("late", 20), Some("2026-09-30T00:00:00Z")),
            observed(account("soon", 10), Some("2026-09-26T00:00:00Z")),
            observed(account("middle", 20), Some("2026-09-28T00:00:00Z")),
        ]);
        assert_eq!(
            plan.targets,
            vec![
                ("soon".to_owned(), 30),
                ("middle".to_owned(), 20),
                ("late".to_owned(), 10)
            ]
        );
        assert_eq!(
            plan.changes,
            vec![
                Change {
                    name: "soon".to_owned(),
                    from: 10,
                    to: 30
                },
                Change {
                    name: "late".to_owned(),
                    from: 20,
                    to: 10
                },
            ]
        );
    }

    /// Resets jitter by a fraction of a second around the nominal minute, in
    /// both directions; the two readings of one nominal reset must land in
    /// the same bucket.
    #[test]
    fn reset_jitter_across_the_minute_boundary_still_ties() {
        let plan = plan(&[
            observed(account("a", 0), Some("2026-09-25T13:59:59.507Z")),
            observed(account("b", 0), Some("2026-09-25T14:00:00.054Z")),
            observed(account("c", 0), Some("2026-09-27T16:00:00Z")),
        ]);
        assert_eq!(plan.target("a"), Some(20));
        assert_eq!(plan.target("b"), Some(20));
        assert_eq!(plan.target("c"), Some(10));
    }

    /// Accounts resetting in the same minute are equally urgent and must
    /// share a tier (CLIProxyAPI then round-robins them); an account whose
    /// weekly window has not started has no deadline and goes last.
    #[test]
    fn same_minute_resets_share_a_tier_and_unstarted_windows_go_last() {
        let plan = plan(&[
            observed(account("fresh", 0), None),
            observed(account("b", 0), Some("2026-09-26T10:00:25Z")),
            observed(account("a", 0), Some("2026-09-26T10:00:05Z")),
        ]);
        assert_eq!(
            plan.targets,
            vec![
                ("a".to_owned(), 20),
                ("b".to_owned(), 20),
                ("fresh".to_owned(), 10)
            ]
        );
    }

    /// Disabled accounts and other providers keep whatever priority they
    /// have: their routing is not this command's business.
    #[test]
    fn unmanaged_accounts_are_left_alone() {
        let mut disabled = account("off", 5);
        disabled.disabled = true;
        let mut codex = account("codex", 7);
        codex.provider = "codex".to_owned();
        let plan = plan(&[
            Observed {
                account: disabled,
                usage: None,
            },
            Observed {
                account: codex,
                usage: None,
            },
            observed(account("on", 10), Some("2026-09-26T00:00:00Z")),
        ]);
        assert_eq!(plan.targets, vec![("on".to_owned(), 10)]);
        assert!(plan.changes.is_empty());
        assert_eq!(plan.target("off"), None);
    }

    /// Writes happen only when the order is wrong. Hand-set values that
    /// already rank the accounts correctly (including an unstarted account
    /// left at 0 below them) are kept as they are and become the targets.
    #[test]
    fn correct_order_is_kept_whatever_the_values() {
        let plan = plan(&[
            observed(account("soon", 7), Some("2026-09-26T00:00:00Z")),
            observed(account("late", 3), Some("2026-09-29T00:00:00Z")),
            observed(account("fresh", 0), None),
        ]);
        assert!(plan.changes.is_empty());
        assert_eq!(plan.target("soon"), Some(7));
        assert_eq!(plan.target("fresh"), Some(0));
    }

    /// A tie that should not be a tie is a wrong order and gets renumbered,
    /// and so is a split that should be a tie.
    #[test]
    fn wrong_ties_are_renumbered() {
        let split_tie = plan(&[
            observed(account("a", 10), Some("2026-09-26T00:00:00Z")),
            observed(account("b", 10), Some("2026-09-29T00:00:00Z")),
        ]);
        assert_eq!(split_tie.target("a"), Some(20));
        assert_eq!(split_tie.target("b"), Some(10));
        let joined = plan(&[
            observed(account("a", 20), Some("2026-09-26T00:00:00Z")),
            observed(account("b", 10), Some("2026-09-26T00:00:20Z")),
        ]);
        assert_eq!(joined.target("a"), Some(10));
        assert_eq!(joined.target("b"), Some(10));
    }

    /// The weekly rotation: the account that just reset (its week now not
    /// started) drops from the top to the bottom, and the renumbering
    /// moves every account. Pinned so the doc's "whole pool" claim stays
    /// true.
    #[test]
    fn weekly_rotation_renumbers_the_pool() {
        let plan = plan(&[
            observed(account("a", 30), None),
            observed(account("b", 20), Some("2026-09-27T00:00:00Z")),
            observed(account("c", 10), Some("2026-09-29T00:00:00Z")),
        ]);
        assert_eq!(
            plan.targets,
            vec![
                ("b".to_owned(), 30),
                ("c".to_owned(), 20),
                ("a".to_owned(), 10)
            ]
        );
        assert_eq!(plan.changes.len(), 3);
    }

    /// A settled pool must produce no writes, so frequent runs are free.
    #[test]
    fn already_ordered_pool_needs_no_changes() {
        let plan = plan(&[
            observed(account("soon", 20), Some("2026-09-26T00:00:00Z")),
            observed(account("late", 10), Some("2026-09-29T00:00:00Z")),
        ]);
        assert!(plan.changes.is_empty());
    }

    fn cooldown(retry_at: &str) -> Cooldown {
        Cooldown {
            scope: "auth".to_owned(),
            model_key: None,
            reason: "quota".to_owned(),
            retry_at: Some(ts(retry_at)),
            http_status: Some(429),
        }
    }

    /// Issue #5770's signature: CLIProxyAPI keeps the account cooled down
    /// long after Anthropic says its quota is back. That must raise the
    /// strong flag, while a cooldown ending at the reset (the normal case)
    /// must not.
    #[test]
    fn cooldown_running_past_both_resets_is_flagged() {
        let now = ts("2026-09-25T08:00:00Z");
        let mut stuck = account("stuck", 10);
        stuck.cooldowns.push(cooldown("2026-10-13T00:00:00Z"));
        let got = flags(&observed(stuck, Some("2026-09-28T00:00:00Z")), now);
        assert!(got.contains(&Flag::CooldownOutlivesReset), "{got:?}");

        let mut normal = account("normal", 10);
        normal.cooldowns.push(cooldown("2026-09-28T00:05:00Z"));
        let got = flags(&observed(normal, Some("2026-09-28T00:00:00Z")), now);
        assert!(!got.contains(&Flag::CooldownOutlivesReset), "{got:?}");
    }

    /// Only quota cooldowns count. CLIProxyAPI also parks single models
    /// for hours after errors (unsupported model, 401/403); those must not
    /// raise quota flags, and neither may a quota cooldown that has already
    /// expired or the account-level `next_retry_after`.
    #[test]
    fn only_active_quota_cooldowns_count() {
        let now = ts("2026-09-25T08:00:00Z");
        let mut a = account("a", 10);
        a.cooldowns.push(Cooldown {
            scope: "model".to_owned(),
            model_key: Some("claude-x".to_owned()),
            reason: "model_not_supported".to_owned(),
            retry_at: Some(ts("2026-09-25T20:00:00Z")),
            http_status: Some(404),
        });
        a.next_retry_after = Some(ts("2026-10-13T00:00:00Z"));
        a.cooldowns.push(cooldown("2026-09-25T07:00:00Z"));
        assert!(flags(&observed(a, Some("2026-09-25T09:00:00Z")), now).is_empty());

        let mut error_429 = account("b", 10);
        error_429.cooldowns.push(Cooldown {
            reason: "unknown".to_owned(),
            ..cooldown("2026-10-13T00:00:00Z")
        });
        assert!(flags(&observed(error_429, Some("2026-09-28T00:00:00Z")), now).is_empty());
    }

    /// A spent 5-hour window with a week still running: the quota is back at
    /// the 5-hour reset, so a cooldown parked until the weekly reset is the
    /// stuck shape even though it does not outlive the weekly window.
    #[test]
    fn cooldown_past_the_exhausted_windows_reset_is_flagged() {
        let now = ts("2026-09-25T08:00:00Z");
        let mut parked = account("parked", 10);
        parked.cooldowns.push(cooldown("2026-09-28T00:00:00Z"));
        let mut o = observed(parked, Some("2026-09-28T00:00:00Z"));
        o.usage.as_mut().unwrap().five_hour.utilization = Some(100.0);
        let got = flags(&o, now);
        assert!(got.contains(&Flag::CooldownOutlivesReset), "{got:?}");

        let mut fair = account("fair", 10);
        fair.cooldowns.push(cooldown("2026-09-25T12:05:00Z"));
        let mut o = observed(fair, Some("2026-09-28T00:00:00Z"));
        o.usage.as_mut().unwrap().five_hour.utilization = Some(100.0);
        assert!(!flags(&o, now).contains(&Flag::CooldownOutlivesReset));
    }

    /// The fully idle #5770 state: the stuck account serves nothing, so after
    /// its resets pass Anthropic reports both windows as not started
    /// (`resets_at: null`). That means "quota available now", and a quota
    /// cooldown days out must still raise the strong flag.
    #[test]
    fn unstarted_windows_count_as_available_now() {
        let now = ts("2026-09-25T08:00:00Z");
        let mut stuck = account("stuck", 10);
        stuck.cooldowns.push(cooldown("2026-09-28T00:00:00Z"));
        let mut o = observed(stuck, None);
        o.usage.as_mut().unwrap().five_hour = Window {
            utilization: Some(0.0),
            resets_at: None,
        };
        let got = flags(&o, now);
        assert!(got.contains(&Flag::CooldownOutlivesReset), "{got:?}");

        let mut brief = account("brief", 10);
        brief.cooldowns.push(cooldown("2026-09-25T08:10:00Z"));
        let mut o = observed(brief, None);
        o.usage.as_mut().unwrap().five_hour.resets_at = None;
        assert!(!flags(&o, now).contains(&Flag::CooldownOutlivesReset));
    }

    /// Blocked-with-headroom is the weak signal: it fires while a quota
    /// cooldown is active and both windows have room, and not when the block
    /// is explained by an exhausted window.
    #[test]
    fn blocked_with_quota_requires_headroom_in_both_windows() {
        let now = ts("2026-09-25T08:00:00Z");
        let mut a = account("a", 10);
        a.cooldowns.push(cooldown("2026-09-25T10:00:00Z"));
        assert_eq!(
            flags(&observed(a.clone(), Some("2026-09-28T00:00:00Z")), now),
            vec![Flag::BlockedWithQuota]
        );

        let mut exhausted = observed(a, Some("2026-09-28T00:00:00Z"));
        exhausted.usage.as_mut().unwrap().five_hour.utilization = Some(100.0);
        assert_eq!(flags(&exhausted, now), vec![Flag::FiveHourExhausted]);
    }

    /// Without usage there is nothing to compare against, so no flags.
    #[test]
    fn unmanaged_accounts_get_no_flags() {
        let mut a = account("a", 10);
        a.cooldowns.push(cooldown("2026-10-13T00:00:00Z"));
        let o = Observed {
            account: a,
            usage: None,
        };
        assert!(flags(&o, ts("2026-09-25T08:00:00Z")).is_empty());
    }
}
