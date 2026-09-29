//! `kd gh pr list`: the authenticated user's PRs that still need attention,
//! each with how long ago someone other than the user last did something
//! significant on it, most recent first.
//!
//! Here's the TLDR of the mechanics. One GitHub search lists the PRs (open,
//! or closed within the recent window). A second pass fetches, per PR, the
//! tail of its timeline and of its reviews, keeps the significant entries,
//! and takes the newest one not made by the user. An age rather than a "new
//! since you last looked" flag is deliberate: it needs no stored state and
//! no notion of marking something read, and the user can judge "25m ago"
//! against their own memory of when they last replied.
//!
//! GitHub fails silently in several ways this code has to defend against;
//! `SPEC_impl.md` records what was observed and why each defense is shaped
//! the way it is. In short: the search goes through REST because only REST
//! flags a timed-out, partial search result; timelines are fetched
//! unfiltered, in small batches, and checked against `totalCount` because
//! GitHub drops event entries from a query that loads too many timelines;
//! and reviews come from their own connection because the timeline leaves
//! out many of them.

use super::ListArgs;
use anyhow::{Context, bail};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use tracing::{debug, info, warn};
use unicode_width::UnicodeWidthStr;
use xshell::{Shell, cmd};

// ── Tunables ──────────────────────────────────────────────────────────

/// How far back a closed (or merged) PR is still worth listing.
const CLOSED_WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);

/// Search results per request: the REST search API's maximum.
const SEARCH_PAGE_SIZE: usize = 100;

/// GitHub's hard limit on results from any one search. Past it, pages stop
/// coming no matter how many results match.
const SEARCH_RESULT_CAP: usize = 1000;

/// How many times a search that came back incomplete is run again before
/// giving up with an error. Incomplete results come from GitHub-side
/// timeouts under load, which tend to clear on a rerun.
const SEARCH_ATTEMPTS: usize = 3;

/// PRs per activity request. Small enough that ordinary timelines come back
/// complete in one go; a batch that exceeds GitHub's hidden budget anyway
/// is detected and its short tails refetched individually.
const ACTIVITY_BATCH: usize = 10;

/// Entries fetched from the end of each connection (timeline, reviews). The
/// timeline window is unfiltered, so it includes noise such as mentions and
/// branch deletions that are dropped client-side; see `last_by_others` for
/// what happens when a window does not reach back far enough.
const TAIL_WINDOW: usize = 100;

// ── GitHub transport ──────────────────────────────────────────────────

/// The one seam between this command and GitHub: the two kinds of request
/// it makes, each returning the raw response body. Everything above it
/// (pagination, completeness checks, batching, retries) is tested against
/// a fake implementation.
trait GitHub {
    /// Run a GraphQL query. The body is returned even when it carries
    /// `errors`, so `query_data` can tell tolerable errors from fatal ones.
    fn graphql(&mut self, query: &str, variables: Value) -> anyhow::Result<String>;

    /// Fetch one page (1-based) of `GET /search/issues` for `q`, with
    /// advanced search syntax enabled and results sorted by creation time.
    /// Creation time never changes, so a PR updated while the pages are
    /// being fetched cannot move to a page already read.
    fn search_issues(&mut self, q: &str, page: usize) -> anyhow::Result<String>;
}

/// Production transport: `gh api`, which brings its own auth. GraphQL
/// requests go in as a whole JSON body on stdin rather than as `-f`/`-F`
/// flags, so that the activity query's list-of-IDs variable is passed as a
/// plain JSON array instead of relying on `gh`'s flag syntax for arrays.
struct GhCli {
    sh: Shell,
}

impl GitHub for GhCli {
    fn graphql(&mut self, query: &str, variables: Value) -> anyhow::Result<String> {
        let body = json!({ "query": query, "variables": variables }).to_string();
        let sh = &self.sh;
        // `gh` exits non-zero whenever the response has `errors`, including
        // the per-node NOT_FOUND that `query_data` tolerates, so the exit
        // status alone cannot decide. A JSON body is handed on either way;
        // anything else is a transport failure.
        let out = cmd!(sh, "gh api graphql --input -")
            .stdin(body)
            .ignore_status()
            .output()?;
        let stdout = String::from_utf8(out.stdout).context("gh printed non-UTF-8 output")?;
        if !out.status.success() && !stdout.trim_start().starts_with('{') {
            bail!(
                "gh api graphql failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(stdout)
    }

    fn search_issues(&mut self, q: &str, page: usize) -> anyhow::Result<String> {
        let sh = &self.sh;
        let (q, per_page, page) = (
            format!("q={q}"),
            format!("per_page={SEARCH_PAGE_SIZE}"),
            format!("page={page}"),
        );
        Ok(cmd!(
            sh,
            "gh api -X GET search/issues -f {q} -f advanced_search=true -f {per_page} -f {page} -f sort=created -f order=desc"
        )
        .read()?)
    }
}

/// Run a query and unwrap the `data` envelope into `T`.
///
/// `NOT_FOUND` errors are tolerated: GitHub reports one for each ID in a
/// `nodes(ids:)` lookup that no longer resolves (a PR whose repo was
/// deleted or went private after the search), alongside a `null` in its
/// slot of `data`, and the caller treats the `null` as "no activity data
/// for that PR". Any other error fails the query.
fn query_data<T: DeserializeOwned>(
    api: &mut impl GitHub,
    query: &str,
    variables: Value,
) -> anyhow::Result<T> {
    #[derive(Deserialize)]
    struct Envelope<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GraphQlError>,
    }
    #[derive(Deserialize)]
    struct GraphQlError {
        #[serde(rename = "type")]
        kind: Option<String>,
        message: String,
    }
    let raw = api.graphql(query, variables)?;
    let envelope: Envelope<T> =
        serde_json::from_str(&raw).context("parsing GitHub GraphQL response")?;
    let fatal: Vec<&str> = envelope
        .errors
        .iter()
        .filter(|e| e.kind.as_deref() != Some("NOT_FOUND"))
        .map(|e| e.message.as_str())
        .collect();
    if !fatal.is_empty() {
        bail!("GitHub GraphQL errors: {}", fatal.join("; "));
    }
    envelope.data.context("GitHub GraphQL response has no data")
}

// ── Queries and wire types ────────────────────────────────────────────

const VIEWER_QUERY: &str = "query { viewer { login } }";

/// Timeline and review tails for a batch of PRs by node ID.
///
/// Only the timeline types in `ACTIVITY_TYPES` get field selections; every
/// other entry comes back as a bare `__typename` and is ignored, but still
/// occupies a window slot and still counts toward `totalCount`, which is
/// what the completeness check needs.
///
/// Reviews are deliberately not taken from the timeline. The timeline
/// leaves out many of them (review-thread replies especially), and its
/// `PullRequestReview` entries carry the time the review was started rather
/// than submitted. The `reviews` connection has every submitted review with
/// its `submittedAt`; the viewer's own pending review, if any, shows up
/// with a `null` there and is skipped.
const ACTIVITY_QUERY: &str = r#"
query($ids: [ID!]!, $last: Int!) {
  nodes(ids: $ids) {
    ... on PullRequest {
      id
      timelineItems(last: $last) {
        totalCount
        pageInfo { hasPreviousPage }
        nodes {
          __typename
          ... on IssueComment { author { login } createdAt }
          ... on PullRequestCommit { commit { committedDate author { user { login } } } }
          ... on HeadRefForcePushedEvent { actor { login } createdAt }
          ... on ClosedEvent { actor { login } createdAt }
          ... on MergedEvent { actor { login } createdAt }
          ... on ReopenedEvent { actor { login } createdAt }
          ... on ReadyForReviewEvent { actor { login } createdAt }
          ... on ConvertToDraftEvent { actor { login } createdAt }
          ... on LabeledEvent { actor { login } createdAt }
          ... on UnlabeledEvent { actor { login } createdAt }
          ... on ReviewRequestedEvent { actor { login } createdAt }
          ... on ReviewDismissedEvent { actor { login } createdAt }
          ... on RenamedTitleEvent { actor { login } createdAt }
          ... on BaseRefChangedEvent { actor { login } createdAt }
        }
      }
      reviews(last: $last) {
        totalCount
        pageInfo { hasPreviousPage }
        nodes { author { login } submittedAt }
      }
    }
  }
}
"#;

/// Timeline entry types that count as someone doing something to the PR,
/// each with a fragment in `ACTIVITY_QUERY` (a test holds the two lists in
/// step). Mentions, subscriptions, cross-references, branch deletions and
/// the like are left out: they are either a side effect of an entry already
/// counted (a comment that mentions the user) or not activity on the PR at
/// all. Reviews count too, but come from their own connection; see
/// `ACTIVITY_QUERY`.
const ACTIVITY_TYPES: &[&str] = &[
    "IssueComment",
    "PullRequestCommit",
    "HeadRefForcePushedEvent",
    "ClosedEvent",
    "MergedEvent",
    "ReopenedEvent",
    "ReadyForReviewEvent",
    "ConvertToDraftEvent",
    "LabeledEvent",
    "UnlabeledEvent",
    "ReviewRequestedEvent",
    "ReviewDismissedEvent",
    "RenamedTitleEvent",
    "BaseRefChangedEvent",
];

#[derive(Deserialize)]
struct ViewerData {
    viewer: Login,
}

/// One page of REST search results. `total_count` is the number of
/// matches, which may exceed what the API will page through (see
/// `SEARCH_RESULT_CAP`).
#[derive(Deserialize)]
struct SearchPage {
    total_count: usize,
    incomplete_results: bool,
    items: Vec<SearchHit>,
}

/// One PR from the search, in the REST issue shape (a PR is an issue with a
/// `pull_request` member). Timestamps stay strings on the wire and are
/// parsed where they are used (`closed_at` in `collect`, the rest in
/// `Pr::build`), each with the PR's URL in the error.
#[derive(Deserialize)]
struct SearchHit {
    /// The GraphQL node ID, which is what the activity query takes.
    node_id: String,
    number: u64,
    html_url: String,
    title: String,
    /// `open` or `closed`; merged PRs are `closed` with `merged_at` set.
    state: String,
    #[serde(default)]
    draft: bool,
    updated_at: String,
    closed_at: Option<String>,
    pull_request: PullRequestRef,
}

#[derive(Deserialize)]
struct PullRequestRef {
    merged_at: Option<String>,
}

/// `nodes(ids:)` holds a `null` for an ID that no longer resolves; see
/// `query_data`.
#[derive(Deserialize)]
struct ActivityData {
    nodes: Vec<Option<ActivityPr>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityPr {
    id: String,
    #[serde(flatten)]
    activity: PrActivity,
}

/// Everything fetched about one PR's activity: the tails of its timeline
/// and of its reviews.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrActivity {
    timeline_items: Tail<TimelineNode>,
    reviews: Tail<ReviewNode>,
}

impl PrActivity {
    /// Whether GitHub returned every entry both windows should hold.
    /// Anything less is the silent degradation described in `SPEC_impl.md`.
    fn is_complete(&self) -> bool {
        self.timeline_items.is_complete() && self.reviews.is_complete()
    }
}

/// The newest-last tail of a GraphQL connection.
///
/// The two questions asked of a tail use different signals on purpose.
/// Whether it is partial comes from `hasPreviousPage`, which is exact.
/// Whether GitHub silently dropped entries from it can only be judged
/// against `totalCount`, the one count that holds still when GitHub
/// degrades a response (`filteredCount` and `pageCount` shrink along with
/// the dropped entries). But `totalCount` leaves out some entry types
/// (subscription events, among others), so it can read lower than what a
/// healthy response returns; the check is `>=` for that reason, and a
/// degraded response that still reaches the undercount would go unnoticed.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tail<T> {
    total_count: usize,
    page_info: TailPageInfo,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TailPageInfo {
    has_previous_page: bool,
}

impl<T> Tail<T> {
    fn is_complete(&self) -> bool {
        self.nodes.len() >= self.total_count.min(TAIL_WINDOW)
    }

    /// The tail does not reach back to the start of the connection.
    fn is_partial(&self) -> bool {
        self.page_info.has_previous_page
    }
}

/// A timeline entry, flattened across the item types in `ACTIVITY_TYPES`.
/// Comments carry `author`, events carry `actor`, and commits carry neither
/// (their attribution lives under `commit`).
///
/// A `null` author or actor is a deleted ("ghost") account. That is
/// necessarily someone other than the current user, and is treated so.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineNode {
    #[serde(rename = "__typename")]
    typename: String,
    author: Option<Login>,
    actor: Option<Login>,
    created_at: Option<String>,
    commit: Option<Commit>,
}

/// A review. `submitted_at` is `null` only for the viewer's own pending
/// review, which nobody else can see yet and so is not activity.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    author: Option<Login>,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    committed_date: String,
    author: Option<CommitAuthor>,
}

#[derive(Deserialize)]
struct CommitAuthor {
    user: Option<Login>,
}

// ── Domain model ──────────────────────────────────────────────────────

/// Coarse PR state for display. Draft is only ever reported for open PRs;
/// a closed draft is just closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    fn label(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }
}

/// When someone other than the user last did something significant on the
/// PR: the answer this command exists to compute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OthersActivity {
    /// The newest significant entry by someone else.
    At(Timestamp),
    /// Nobody else has a significant entry anywhere on the PR.
    Never,
    /// Cannot tell, and guessing either way would be worse than saying so:
    /// GitHub would not return complete activity even for the PR on its
    /// own, or a connection is longer than its fetched window and what the
    /// window shows cannot settle the answer (see `last_by_others`).
    Unknown,
}

/// A PR as the listing needs it.
#[derive(Debug)]
struct Pr {
    url: String,
    repo: String,
    number: u64,
    /// With control characters replaced (see `Pr::build`).
    title: String,
    state: PrState,
    updated_at: Timestamp,
    others: OthersActivity,
}

impl Pr {
    /// Combine a search hit with its activity. `activity` is `None` when no
    /// complete activity could be had, which makes the answer `Unknown`.
    ///
    /// Control characters in the title become spaces. Titles are written
    /// straight to the terminal, and anyone who can edit the PR can put an
    /// escape sequence or a newline in its title.
    fn build(hit: SearchHit, activity: Option<PrActivity>, me: &str) -> anyhow::Result<Pr> {
        let url = hit.html_url;
        let ts = |s: &str| -> anyhow::Result<Timestamp> {
            s.parse()
                .with_context(|| format!("{url}: timestamp {s:?} does not parse"))
        };
        let merged = hit.pull_request.merged_at.is_some();
        let state = match (hit.state.as_str(), hit.draft, merged) {
            ("open", false, _) => PrState::Open,
            ("open", true, _) => PrState::Draft,
            ("closed", _, true) => PrState::Merged,
            ("closed", _, false) => PrState::Closed,
            (other, _, _) => bail!("{url}: unexpected PR state {other:?}"),
        };
        let repo = repo_of(&url).with_context(|| format!("{url}: not a GitHub PR URL"))?;
        let others = match activity {
            None => OthersActivity::Unknown,
            Some(activity) => {
                let mut tails = Vec::with_capacity(2);
                let timeline = &activity.timeline_items;
                let mut entries = Vec::with_capacity(timeline.nodes.len());
                for node in &timeline.nodes {
                    if let Some((mine, at)) = attribute_timeline(node, me) {
                        entries.push((mine, ts(at)?));
                    }
                }
                tails.push(summarize(&entries, timeline.is_partial()));
                let reviews = &activity.reviews;
                let mut entries = Vec::with_capacity(reviews.nodes.len());
                for node in &reviews.nodes {
                    if let Some((mine, at)) = attribute_review(node, me) {
                        entries.push((mine, ts(at)?));
                    }
                }
                tails.push(summarize(&entries, reviews.is_partial()));
                last_by_others(&tails)
            }
        };
        let title = hit
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        Ok(Pr {
            updated_at: ts(&hit.updated_at)?,
            url,
            repo,
            number: hit.number,
            title,
            state,
            others,
        })
    }
}

/// `owner/repo` from a PR URL of the form
/// `https://github.com/OWNER/REPO/pull/N`, or `None` for anything else.
fn repo_of(url: &str) -> Option<String> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let (owner, repo, kind) = (parts.next()?, parts.next()?, parts.next()?);
    (kind == "pull").then(|| format!("{owner}/{repo}"))
}

/// Decide whose timeline entry this is and when it happened, as
/// `(mine, time)`, or `None` for entry types outside `ACTIVITY_TYPES`.
///
/// Commits are attributed through the GitHub account linked to the commit's
/// author email. When no account is linked, the commit counts as the
/// user's own: it was pushed to the user's PR branch, and treating an
/// unlinked email as a stranger would report every one of the user's own
/// pushes as someone else's activity. The cost is that a push by someone
/// else with an unlinked email goes unnoticed.
fn attribute_timeline<'a>(node: &'a TimelineNode, me: &str) -> Option<(bool, &'a str)> {
    if !ACTIVITY_TYPES.contains(&node.typename.as_str()) {
        return None;
    }
    if let Some(commit) = &node.commit {
        let mine = match commit.author.as_ref().and_then(|a| a.user.as_ref()) {
            Some(user) => user.login == me,
            None => true,
        };
        return Some((mine, &commit.committed_date));
    }
    let at = node.created_at.as_deref()?;
    let who = node.author.as_ref().or(node.actor.as_ref());
    Some((who.is_some_and(|who| who.login == me), at))
}

/// `(mine, submitted)` for a submitted review; `None` for a pending one.
fn attribute_review<'a>(node: &'a ReviewNode, me: &str) -> Option<(bool, &'a str)> {
    let at = node.submitted_at.as_deref()?;
    Some((node.author.as_ref().is_some_and(|a| a.login == me), at))
}

/// What one fetched connection tail says about others' activity.
#[derive(Clone, Copy, Debug)]
struct TailSummary {
    /// The newest entry in the tail not by the user.
    newest_other: Option<Timestamp>,
    /// The oldest dated entry in the tail, by anyone. Everything the tail
    /// did not reach is at least this old.
    oldest: Option<Timestamp>,
    /// The tail does not reach back to the start of the connection.
    partial: bool,
}

/// Summarize a tail's `(mine, time)` entries.
fn summarize(entries: &[(bool, Timestamp)], partial: bool) -> TailSummary {
    TailSummary {
        newest_other: entries.iter().filter(|(m, _)| !m).map(|(_, t)| *t).max(),
        oldest: entries.iter().map(|(_, t)| *t).min(),
        partial,
    }
}

/// The newest entry not by the user across all of a PR's connection tails.
///
/// A tail that shows someone else is harmless even when partial: whatever
/// it did not reach is older than what it shows. A partial tail showing
/// nobody else is the problem, since others' entries may sit in the part
/// that was not fetched. That is still settled if the newest other entry
/// found elsewhere is at least as new as everything that tail reached back
/// to (the unfetched part is older still). If it is not, or nothing was
/// found at all, the answer is `Unknown` rather than an age or a `Never`
/// that might be false.
fn last_by_others(tails: &[TailSummary]) -> OthersActivity {
    let found = tails.iter().filter_map(|t| t.newest_other).max();
    for tail in tails {
        if tail.partial && tail.newest_other.is_none() {
            let settled = matches!((found, tail.oldest), (Some(f), Some(o)) if f >= o);
            if !settled {
                return OthersActivity::Unknown;
            }
        }
    }
    found.map_or(OthersActivity::Never, OthersActivity::At)
}

// ── Fetching ──────────────────────────────────────────────────────────

/// The search this command runs: PRs by `me` that are open or were closed
/// on or after `closed_since_date`, as one query. Two separate searches
/// (open, then closed) would race against a PR changing state between
/// them and could miss it in both; the `OR` needs GitHub's advanced search
/// syntax, which `GitHub::search_issues` enables.
///
/// `-user:` excludes repositories owned by that account (not repositories
/// in organizations the account belongs to), which is the "my own repos"
/// the default exclusion is about. Doing it in the search rather than
/// client-side keeps a user with many PRs on their own repos from paging
/// through all of them just to throw them away.
fn search_string(me: &str, include_mine: bool, closed_since_date: &str) -> String {
    let exclude_mine = if include_mine {
        String::new()
    } else {
        format!(" -user:{me}")
    };
    format!("is:pr author:{me}{exclude_mine} (is:open OR closed:>={closed_since_date})")
}

/// The date for the `closed:>=` qualifier. Search only takes a date there,
/// and the docs do not say which timezone it is read in, so this backs off
/// one extra day to be sure nothing inside the window is missed; `collect`
/// then trims to the exact cutoff.
fn closed_since_date(cutoff: Timestamp) -> String {
    (cutoff - SignedDuration::from_hours(24))
        .strftime("%Y-%m-%d")
        .to_string()
}

/// Run the search to exhaustion, insisting on a complete result.
///
/// An incomplete attempt (see `search_once`) is rerun from scratch up to
/// `SEARCH_ATTEMPTS` times, and if none comes back complete this fails
/// rather than list a subset: a listing that silently leaves out PRs is
/// worse than no listing. More matches than `SEARCH_RESULT_CAP` fail the
/// same way, since the API cannot page past the cap. A complete result has
/// no duplicates (`search_once` checks).
fn search_all(api: &mut impl GitHub, search: &str) -> anyhow::Result<Vec<SearchHit>> {
    let mut problem = String::new();
    for attempt in 1..=SEARCH_ATTEMPTS {
        match search_once(api, search)? {
            SearchOutcome::Complete(hits) => return Ok(hits),
            SearchOutcome::OverCap(total) => bail!(
                "{total} PRs match {search:?}, more than the {SEARCH_RESULT_CAP} \
                 GitHub search will return; refusing to list a partial result"
            ),
            SearchOutcome::Incomplete(why) => {
                warn!("Search {search:?} attempt {attempt} came back incomplete: {why}");
                problem = why;
            }
        }
    }
    bail!("search {search:?} came back incomplete {SEARCH_ATTEMPTS} times (last: {problem})")
}

/// How one pass over a search's pages went.
enum SearchOutcome {
    Complete(Vec<SearchHit>),
    /// More matches than `SEARCH_RESULT_CAP`.
    OverCap(usize),
    /// Worth retrying; the string says what was wrong, for the log.
    Incomplete(String),
}

/// One pass over all pages of a search. The result counts as complete only
/// if GitHub never flagged `incomplete_results`, the match count held still
/// across pages, and exactly that many distinct PRs were collected. The
/// last two catch a result set that changed mid-pass (a PR closing between
/// page fetches shifts the later pages and can skip one), which GitHub
/// does not flag.
///
/// What this cannot catch is a PR the search index itself has not caught
/// up with, since then the match count agrees with the missing result.
fn search_once(api: &mut impl GitHub, search: &str) -> anyhow::Result<SearchOutcome> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut total = None;
    for page in 1..=SEARCH_RESULT_CAP.div_ceil(SEARCH_PAGE_SIZE) {
        let raw = api
            .search_issues(search, page)
            .with_context(|| format!("searching GitHub for {search:?}"))?;
        let data: SearchPage = serde_json::from_str(&raw)
            .with_context(|| format!("parsing search results for {search:?}"))?;
        if data.incomplete_results {
            return Ok(SearchOutcome::Incomplete(format!(
                "GitHub flagged page {page} as incomplete"
            )));
        }
        let expected = *total.get_or_insert(data.total_count);
        if data.total_count != expected {
            return Ok(SearchOutcome::Incomplete(format!(
                "match count changed from {expected} to {} while paging",
                data.total_count
            )));
        }
        if expected > SEARCH_RESULT_CAP {
            return Ok(SearchOutcome::OverCap(expected));
        }
        let empty = data.items.is_empty();
        hits.extend(data.items);
        if hits.len() >= expected || empty {
            break;
        }
    }
    let expected = total.unwrap_or(0);
    let distinct = hits.iter().map(|h| &h.node_id).collect::<HashSet<_>>();
    if hits.len() != expected || distinct.len() != expected {
        return Ok(SearchOutcome::Incomplete(format!(
            "got {} results ({} distinct), GitHub counted {expected}",
            hits.len(),
            distinct.len()
        )));
    }
    Ok(SearchOutcome::Complete(hits))
}

/// One activity request for `ids`, keyed by PR node ID. IDs that no longer
/// resolve are absent from the result.
fn fetch_activity(
    api: &mut impl GitHub,
    ids: &[String],
) -> anyhow::Result<HashMap<String, PrActivity>> {
    let vars = json!({ "ids": ids, "last": TAIL_WINDOW });
    let data: ActivityData =
        query_data(api, ACTIVITY_QUERY, vars).context("fetching PR activity")?;
    Ok(data
        .nodes
        .into_iter()
        .flatten()
        .map(|pr| (pr.id, pr.activity))
        .collect())
}

/// Complete activity for every ID that has one. Batches of
/// `ACTIVITY_BATCH` go first; any PR whose tails come back short is
/// refetched in a request of its own, and one still short after that is
/// left out (its PR reports `OthersActivity::Unknown`) rather than failing
/// the whole listing over one busy PR.
fn complete_activity(
    api: &mut impl GitHub,
    ids: &[String],
) -> anyhow::Result<HashMap<String, PrActivity>> {
    let mut complete = HashMap::new();
    let mut retry = Vec::new();
    for batch in ids.chunks(ACTIVITY_BATCH) {
        let mut fetched = fetch_activity(api, batch)?;
        // Walk the batch rather than the map so retries go out in listing
        // order, not in hash order.
        for id in batch {
            match fetched.remove(id) {
                Some(activity) if activity.is_complete() => {
                    complete.insert(id.clone(), activity);
                }
                Some(_) => retry.push(id.clone()),
                None => {}
            }
        }
    }
    for id in retry {
        if let Some(activity) = fetch_activity(api, std::slice::from_ref(&id))?.remove(&id)
            && activity.is_complete()
        {
            complete.insert(id, activity);
        }
    }
    Ok(complete)
}

// ── Selection and output ──────────────────────────────────────────────

/// Everything between "who am I" and printing: search, drop PRs closed
/// before `cutoff` (the search's day granularity lets a few through),
/// attach activity, and sort by `listing_order`. Dropping happens before
/// activity is fetched so the extra PRs cost nothing more than their
/// search hit.
fn collect(
    api: &mut impl GitHub,
    me: &str,
    include_mine: bool,
    cutoff: Timestamp,
) -> anyhow::Result<Vec<Pr>> {
    let search = search_string(me, include_mine, &closed_since_date(cutoff));
    debug!("Searching: {search}");
    let mut hits = Vec::new();
    for hit in search_all(api, &search)? {
        let closed_at = hit.closed_at.as_deref().map(str::parse::<Timestamp>);
        let closed_at = closed_at
            .transpose()
            .with_context(|| format!("{}: closed_at does not parse", hit.html_url))?;
        if closed_at.is_none_or(|at| at >= cutoff) {
            hits.push(hit);
        }
    }

    let ids: Vec<String> = hits.iter().map(|h| h.node_id.clone()).collect();
    let mut activity = complete_activity(api, &ids)?;
    let mut prs = hits
        .into_iter()
        .map(|hit| {
            let found = activity.remove(&hit.node_id);
            if found.is_none() {
                warn!(
                    "{}: GitHub returned no complete activity; showing it as unknown",
                    hit.html_url
                );
            }
            Pr::build(hit, found, me)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    prs.sort_by_key(listing_order);
    Ok(prs)
}

/// Sort key for the listing: the most recent activity by others first.
/// PRs where that is unknown come next, since they may well be recent, and
/// PRs nobody else has touched come last. Within each of those two groups,
/// and between PRs with the same activity time, the most recently updated
/// PR comes first.
fn listing_order(pr: &Pr) -> impl Ord + use<> {
    let group = match pr.others {
        OthersActivity::At(at) => (0, Reverse(Some(at))),
        OthersActivity::Unknown => (1, Reverse(None)),
        OthersActivity::Never => (2, Reverse(None)),
    };
    (group, Reverse(pr.updated_at))
}

/// How the listing is rendered, decided once from where stdout goes.
///
/// A terminal gets the form meant for reading: aligned columns, the
/// `owner/repo#N` slug as an OSC 8 hyperlink to the PR (so the URL column
/// goes away), and titles cut to fit the terminal width so every PR stays
/// on one line. Anything else (a pipe, a file) gets unpadded plain text
/// with the full title and the URL spelled out, and no escape sequences
/// for `grep` or a script to trip over.
#[derive(Clone, Copy, Debug)]
enum Render {
    Plain,
    /// `width` is `None` when the terminal size cannot be read, in which
    /// case titles are left whole.
    Terminal {
        width: Option<usize>,
    },
}

impl Render {
    fn detect() -> Render {
        if io::stdout().is_terminal() {
            let width = terminal_size::terminal_size().map(|(w, _)| usize::from(w.0));
            Render::Terminal { width }
        } else {
            Render::Plain
        }
    }
}

/// Bright blue rather than plain blue (SGR 34), which is hard to read on
/// many dark terminal themes.
const LINK_COLOR: &str = "\x1b[94m";
const RESET: &str = "\x1b[0m";

/// Between every two columns, in both renderings.
const GAP: &str = "  ";

/// The PR's `owner/repo#N` slug.
fn slug(pr: &Pr) -> String {
    format!("{}#{}", pr.repo, pr.number)
}

/// How long before `now` the time `at` was, as whole units down to the
/// minute: `0m`, `25m`, `1d7h21m`. Zero units are left out (`2d`, `1h5m`)
/// except that something under a minute old reads `0m`. A time in the
/// future (clock skew between here and GitHub) also reads `0m` rather than
/// a negative age.
fn age(now: Timestamp, at: Timestamp) -> String {
    let minutes = now.duration_since(at).as_secs().max(0) / 60;
    let (days, hours, mins) = (minutes / (24 * 60), minutes / 60 % 24, minutes % 60);
    let mut out = String::new();
    if days > 0 {
        out.push_str(&format!("{days}d"));
    }
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if mins > 0 || out.is_empty() {
        out.push_str(&format!("{mins}m"));
    }
    out
}

/// The first column: age of the newest activity by others, `-` when nobody
/// else has any, `?` when it is unknown.
fn age_label(others: OthersActivity, now: Timestamp) -> String {
    match others {
        OthersActivity::At(at) => age(now, at),
        OthersActivity::Never => "-".to_string(),
        OthersActivity::Unknown => "?".to_string(),
    }
}

/// One line per PR: age of others' last activity, state, slug, title, and
/// (in plain form) the URL.
///
/// In terminal form, ages are right-aligned so their units line up and the
/// state and slug columns are padded so titles line up. The title gets
/// whatever width the columns before it leave; when those alone are wider
/// than the terminal, the title is dropped and the line still wraps.
/// Padding goes outside the escape sequences, which take up no columns on
/// screen, and the title's budget is measured from the actual prefix text
/// so it cannot drift from the format.
fn format_lines(prs: &[Pr], render: Render, now: Timestamp) -> Vec<String> {
    let ages: Vec<String> = prs.iter().map(|pr| age_label(pr.others, now)).collect();
    let age_width = ages.iter().map(|a| a.width()).max().unwrap_or(0);
    let state_width = prs
        .iter()
        .map(|pr| pr.state.label().len())
        .max()
        .unwrap_or(0);
    let slug_width = prs.iter().map(|pr| slug(pr).width()).max().unwrap_or(0);
    prs.iter()
        .zip(ages)
        .map(|(pr, age)| {
            let state = pr.state.label();
            let slug = slug(pr);
            match render {
                Render::Plain => [age.as_str(), state, &slug, &pr.title, &pr.url].join(GAP),
                Render::Terminal { width } => {
                    let columns = format!("{age:>age_width$}{GAP}{state:<state_width$}{GAP}");
                    let pad = " ".repeat(slug_width - slug.width());
                    let prefix_width = columns.width() + slug_width + GAP.len();
                    let title = match width {
                        Some(w) => truncate(&pr.title, w.saturating_sub(prefix_width)),
                        None => pr.title.clone(),
                    };
                    format!(
                        "{columns}\x1b]8;;{url}\x1b\\{LINK_COLOR}{slug}{RESET}\x1b]8;;\x1b\\{pad}{GAP}{title}",
                        url = pr.url
                    )
                }
            }
        })
        .collect()
}

/// Cut `s` to at most `max` terminal columns, marking the cut with `…`. A
/// string that already fits is returned unchanged, ellipsis-free.
///
/// Width is always measured on the string built so far rather than summed
/// per character: combining sequences such as `❤` followed by an emoji
/// variation selector are wider together than their characters add up to,
/// and a per-character sum would let the line overflow after all.
fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis itself.
    let budget = max - 1;
    let mut out = String::new();
    for c in s.chars() {
        out.push(c);
        if out.width() > budget {
            out.pop();
            break;
        }
    }
    out.push('…');
    out
}

pub fn run(args: ListArgs) -> anyhow::Result<()> {
    let mut api = GhCli { sh: Shell::new()? };
    let viewer: ViewerData = query_data(&mut api, VIEWER_QUERY, json!({}))
        .context("looking up the authenticated GitHub user")?;
    let me = viewer.viewer.login;

    let now = Timestamp::now();
    let cutoff = now - CLOSED_WINDOW;
    let prs = collect(&mut api, &me, args.include_mine, cutoff)?;
    if prs.is_empty() {
        info!("No matching PRs");
    }

    // A listing is routinely piped into `head` or `grep -m`; a closed pipe
    // is the reader being done, not a failure.
    let mut out = io::stdout().lock();
    for line in format_lines(&prs, Render::detect(), now) {
        match writeln!(out, "{line}") {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            other => other?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn at(s: &str) -> OthersActivity {
        OthersActivity::At(ts(s))
    }

    // ── Fixtures ──

    /// A REST search hit with fixed timestamps. `state` is the REST state,
    /// or `merged` for a closed PR with `merged_at` set; `closed_at` decides
    /// the window.
    fn hit(id: &str, state: &str, closed_at: Option<&str>) -> Value {
        let (state, merged_at) = match state {
            "merged" => ("closed", closed_at),
            other => (other, None),
        };
        json!({
            "node_id": id, "number": 7, "html_url": format!("https://github.com/o/r/pull/{id}"),
            "title": "t", "state": state, "draft": false,
            "updated_at": "2026-09-10T00:00:00Z", "closed_at": closed_at,
            "pull_request": {"merged_at": merged_at}
        })
    }

    /// Complete activity from timeline and review node lists.
    fn activity(timeline: Value, reviews: Value) -> Value {
        let tail = |nodes: Value| {
            let total = nodes.as_array().unwrap().len();
            json!({"totalCount": total, "pageInfo": {"hasPreviousPage": false}, "nodes": nodes})
        };
        json!({"timelineItems": tail(timeline), "reviews": tail(reviews)})
    }

    /// `OthersActivity` for an open PR with the given timeline and reviews.
    fn others_of(timeline: Value, reviews: Value) -> OthersActivity {
        let hit = serde_json::from_value(hit("1", "open", None)).unwrap();
        let activity = serde_json::from_value(activity(timeline, reviews)).unwrap();
        Pr::build(hit, Some(activity), "me").unwrap().others
    }

    fn comment(who: &str, at: &str) -> Value {
        json!({"__typename": "IssueComment", "author": {"login": who}, "createdAt": at})
    }

    fn review(who: &str, submitted: Option<&str>) -> Value {
        json!({"author": {"login": who}, "submittedAt": submitted})
    }

    // ── Attribution and the activity answer ──

    /// The core question the command answers: the newest entry by someone
    /// else, and the user's own later entries do not move it. If they did,
    /// the user's own reply would read as fresh activity from the other
    /// side.
    #[test]
    fn newest_other_entry_ignoring_mine() {
        let timeline = json!([
            comment("bob", "2026-09-02T00:00:00Z"),
            comment("alice", "2026-09-03T00:00:00Z"),
            {"__typename": "PullRequestCommit", "commit": {
                "committedDate": "2026-09-04T00:00:00Z", "author": {"user": {"login": "me"}}}},
            comment("me", "2026-09-05T00:00:00Z"),
        ]);
        assert_eq!(others_of(timeline, json!([])), at("2026-09-03T00:00:00Z"));
    }

    /// Reviews come from their own connection, dated by submission. This is
    /// the fix for two real misses: review-thread replies that the timeline
    /// leaves out, and a review started days ago but submitted just now,
    /// which the timeline would date by when it was started.
    #[test]
    fn reviews_count_by_submission_time() {
        let reviews = json!([
            review("bob", Some("2026-09-06T00:00:00Z")),
            review("me", Some("2026-09-07T00:00:00Z")),
        ]);
        let timeline = json!([comment("alice", "2026-09-03T00:00:00Z")]);
        assert_eq!(others_of(timeline, reviews), at("2026-09-06T00:00:00Z"));
    }

    /// The user's own pending review has no submission time; it is nobody
    /// else's activity and must not break the listing.
    #[test]
    fn pending_review_is_ignored() {
        let reviews = json!([review("me", None)]);
        assert_eq!(others_of(json!([]), reviews), OthersActivity::Never);
    }

    /// A PR nobody else has touched is `Never`, whether its activity is
    /// empty or holds only the user's own entries.
    #[test]
    fn only_my_entries_is_never() {
        assert_eq!(others_of(json!([]), json!([])), OthersActivity::Never);
        let timeline = json!([comment("me", "2026-09-02T00:00:00Z")]);
        assert_eq!(others_of(timeline, json!([])), OthersActivity::Never);
    }

    /// Events are attributed by `actor`: a maintainer merging the PR is
    /// activity by others, the user closing their own PR is not.
    #[test]
    fn events_attribute_by_actor() {
        let event = |kind: &str, who: &str| {
            json!([{"__typename": kind, "actor": {"login": who},
                    "createdAt": "2026-09-05T00:00:00Z"}])
        };
        assert_eq!(
            others_of(event("MergedEvent", "bob"), json!([])),
            at("2026-09-05T00:00:00Z")
        );
        assert_eq!(
            others_of(event("ClosedEvent", "me"), json!([])),
            OthersActivity::Never
        );
    }

    /// A commit with no linked GitHub account counts as the user's own; see
    /// `attribute_timeline` for why the opposite choice would misreport
    /// every such push. A deleted account's comment still counts as someone
    /// else.
    #[test]
    fn unlinked_commit_is_mine_and_ghost_comment_is_not() {
        let timeline = json!([
            {"__typename": "PullRequestCommit", "commit": {
                "committedDate": "2026-09-04T00:00:00Z", "author": {"user": null}}},
        ]);
        assert_eq!(others_of(timeline, json!([])), OthersActivity::Never);
        let ghost = json!([{"__typename": "IssueComment", "author": null,
                            "createdAt": "2026-09-03T00:00:00Z"}]);
        assert_eq!(others_of(ghost, json!([])), at("2026-09-03T00:00:00Z"));
    }

    /// Entry types outside `ACTIVITY_TYPES` are noise even when attributed
    /// to someone else: being mentioned or subscribed is not activity.
    #[test]
    fn ignored_types_are_not_activity() {
        let timeline = json!([
            {"__typename": "MentionedEvent"},
            {"__typename": "SubscribedEvent", "actor": {"login": "bob"},
             "createdAt": "2026-09-03T00:00:00Z"},
        ]);
        assert_eq!(others_of(timeline, json!([])), OthersActivity::Never);
    }

    /// `ACTIVITY_TYPES` and the query's fragments must name the same types.
    /// A type listed without a fragment comes back with no timestamp and is
    /// silently never counted; a fragment without a listed type is fetched
    /// for nothing. Neither mistake would fail any other test.
    #[test]
    fn activity_types_match_query_fragments() {
        let after_timeline = ACTIVITY_QUERY.split_once("timelineItems(").unwrap().1;
        let timeline_part = after_timeline.split_once("reviews(").unwrap().0;
        let fragments: HashSet<&str> = timeline_part
            .split("... on ")
            .skip(1)
            .map(|rest| rest.split_whitespace().next().unwrap())
            .collect();
        let listed: HashSet<&str> = ACTIVITY_TYPES.iter().copied().collect();
        assert_eq!(fragments, listed);
    }

    /// The window rules in `last_by_others`, which decide between an age,
    /// `-`, and `?` when a connection is longer than what was fetched.
    #[test]
    fn partial_tails_are_only_trusted_when_settled() {
        let t = |s: &str| Some(ts(s));
        let summary = |newest_other, oldest, partial| TailSummary {
            newest_other,
            oldest,
            partial,
        };
        // A partial tail with nobody else in it and nothing found elsewhere
        // cannot tell `Never` from an older entry out of reach.
        let mine_only = summary(None, t("2026-09-05T00:00:00Z"), true);
        assert_eq!(last_by_others(&[mine_only]), OthersActivity::Unknown);
        // The same tail when complete really does mean nobody else.
        let complete = summary(None, t("2026-09-05T00:00:00Z"), false);
        assert_eq!(last_by_others(&[complete]), OthersActivity::Never);
        // Someone else shown in a partial tail is exact on its own.
        let theirs = summary(t("2026-09-06T00:00:00Z"), t("2026-09-05T00:00:00Z"), true);
        assert_eq!(last_by_others(&[theirs]), at("2026-09-06T00:00:00Z"));
        // Another tail's find settles a mine-only partial tail when it is
        // at least as new as everything that tail reached...
        let reviews = summary(t("2026-09-07T00:00:00Z"), None, false);
        assert_eq!(
            last_by_others(&[mine_only, reviews]),
            at("2026-09-07T00:00:00Z")
        );
        // ...but not when it is older: something newer may be out of reach.
        let old_review = summary(t("2026-09-01T00:00:00Z"), None, false);
        assert_eq!(
            last_by_others(&[mine_only, old_review]),
            OthersActivity::Unknown
        );
    }

    /// Unknown PR states are an error rather than being shown as something
    /// they are not.
    #[test]
    fn unknown_state_is_an_error() {
        let hit = serde_json::from_value(hit("1", "WEIRD", None)).unwrap();
        assert!(Pr::build(hit, None, "me").is_err());
    }

    /// Merged PRs arrive from REST as `closed` with `merged_at`, drafts as
    /// `open` with `draft`; both must map to their own state, and the repo
    /// comes from the PR URL.
    #[test]
    fn rest_state_and_repo_mapping() {
        let build = |v: Value| Pr::build(serde_json::from_value(v).unwrap(), None, "me").unwrap();
        let merged = build(hit("1", "merged", Some("2026-09-22T00:00:00Z")));
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.repo, "o/r");
        let closed = build(hit("1", "closed", Some("2026-09-22T00:00:00Z")));
        assert_eq!(closed.state, PrState::Closed);
        let mut draft = hit("1", "open", None);
        draft["draft"] = json!(true);
        assert_eq!(build(draft).state, PrState::Draft);
        assert_eq!(repo_of("https://github.com/o/r/issues/1"), None);
    }

    /// Anyone who can edit the PR controls its title, so control characters
    /// (an escape sequence, a newline) must not reach the terminal.
    #[test]
    fn control_characters_in_titles_are_neutralized() {
        let mut v = hit("1", "open", None);
        v["title"] = json!("a\x1b]8;;evil\x07b\nc");
        let pr = Pr::build(serde_json::from_value(v).unwrap(), None, "me").unwrap();
        assert_eq!(pr.title, "a ]8;;evil b c");
    }

    /// The completeness check is what catches GitHub's silent degradation:
    /// short of `totalCount` is incomplete, a connection longer than the
    /// window is complete once the window is full, and more entries than
    /// `totalCount` (which leaves some types out) is fine.
    #[test]
    fn tail_completeness() {
        let tail = |total: usize, len: usize| Tail {
            total_count: total,
            page_info: TailPageInfo {
                has_previous_page: false,
            },
            nodes: vec![(); len],
        };
        assert!(tail(5, 5).is_complete());
        assert!(!tail(5, 1).is_complete());
        assert!(tail(500, TAIL_WINDOW).is_complete());
        assert!(!tail(500, TAIL_WINDOW - 1).is_complete());
        assert!(tail(46, 84).is_complete());
    }

    // ── Search string ──

    /// The default must exclude the user's own repos via the search itself,
    /// --include-mine must drop exactly that exclusion, and open and
    /// recently closed PRs come from one query so a PR changing state
    /// cannot slip between two.
    #[test]
    fn search_string_scope_and_window() {
        assert_eq!(
            search_string("me", false, "2026-09-20"),
            "is:pr author:me -user:me (is:open OR closed:>=2026-09-20)"
        );
        assert_eq!(
            search_string("me", true, "2026-09-20"),
            "is:pr author:me (is:open OR closed:>=2026-09-20)"
        );
    }

    /// The search date backs off a day from the precise cutoff so a
    /// timezone mismatch on GitHub's side can only include extra PRs, which
    /// `collect` then drops, and never miss one.
    #[test]
    fn closed_since_date_backs_off_one_day() {
        assert_eq!(closed_since_date(ts("2026-09-21T03:00:00Z")), "2026-09-20");
    }

    // ── Fake GitHub and end-to-end collection ──

    /// Fake GitHub for `collect`. Search pages are handed out in order from
    /// a queue, one per `search_issues` call, so a test can script a retry
    /// by queueing the bad response before the good one; every search
    /// request's page number is logged. Activity requests are answered from
    /// `activity`, where a missing ID answers the way GitHub does for a PR
    /// that no longer resolves (a `null` plus a NOT_FOUND error), and each
    /// request's ID list is logged so tests can assert on batching and
    /// retries. `degraded_batches` makes every multi-PR request come back
    /// with empty timelines, the way GitHub's hidden budget does.
    struct FakeGitHub {
        search: String,
        pages: VecDeque<Value>,
        page_requests: Vec<usize>,
        activity: HashMap<String, Value>,
        degraded_batches: bool,
        activity_requests: Vec<Vec<String>>,
    }

    impl GitHub for FakeGitHub {
        fn search_issues(&mut self, q: &str, page: usize) -> anyhow::Result<String> {
            assert_eq!(q, self.search);
            self.page_requests.push(page);
            Ok(self
                .pages
                .pop_front()
                .expect("search queue ran dry")
                .to_string())
        }

        fn graphql(&mut self, query: &str, vars: Value) -> anyhow::Result<String> {
            assert_eq!(query, ACTIVITY_QUERY);
            let ids: Vec<String> = serde_json::from_value(vars["ids"].clone()).unwrap();
            let degrade = self.degraded_batches && ids.len() > 1;
            let mut errors = Vec::new();
            let nodes: Vec<Value> = ids
                .iter()
                .map(|id| match self.activity.get(id) {
                    None => {
                        errors.push(json!({"type": "NOT_FOUND", "message": "gone"}));
                        Value::Null
                    }
                    Some(activity) => {
                        let mut node = activity.clone();
                        node["id"] = json!(id);
                        if degrade {
                            node["timelineItems"]["nodes"] = json!([]);
                        }
                        node
                    }
                })
                .collect();
            self.activity_requests.push(ids);
            Ok(json!({"data": {"nodes": nodes}, "errors": errors}).to_string())
        }
    }

    /// One REST search page claiming `total` matches overall.
    fn search_page(items: Vec<Value>, total: usize) -> Value {
        json!({"total_count": total, "incomplete_results": false, "items": items})
    }

    /// The same page, but flagged the way GitHub flags a timed-out search.
    fn incomplete(mut page: Value) -> Value {
        page["incomplete_results"] = json!(true);
        page
    }

    const CUTOFF: &str = "2026-09-21T00:00:00Z";

    /// The standard scenario, spread over two search pages: a quiet open
    /// PR, an open PR someone commented on, a PR closed before the cutoff
    /// (which the search's day granularity lets through), and one merged by
    /// someone else inside the window.
    fn fake(degraded_batches: bool) -> FakeGitHub {
        let pages = VecDeque::from([
            search_page(
                vec![hit("quiet", "open", None), hit("busy", "open", None)],
                4,
            ),
            search_page(
                vec![
                    hit("old", "closed", Some("2026-09-19T00:00:00Z")),
                    hit("merged", "merged", Some("2026-09-22T00:00:00Z")),
                ],
                4,
            ),
        ]);
        let merge = json!([{"__typename": "MergedEvent", "actor": {"login": "alice"},
                            "createdAt": "2026-09-22T00:00:00Z"}]);
        let activity = HashMap::from([
            ("quiet".to_string(), activity(json!([]), json!([]))),
            (
                "busy".to_string(),
                activity(json!([comment("bob", "2026-09-03T00:00:00Z")]), json!([])),
            ),
            ("merged".to_string(), activity(merge, json!([]))),
        ]);
        FakeGitHub {
            search: search_string("me", false, &closed_since_date(ts(CUTOFF))),
            pages,
            page_requests: Vec::new(),
            activity,
            degraded_batches,
            activity_requests: Vec::new(),
        }
    }

    fn collect_fake(api: &mut impl GitHub) -> anyhow::Result<Vec<Pr>> {
        collect(api, "me", false, ts(CUTOFF))
    }

    /// End to end over the fake: the search is paged to exhaustion, a PR
    /// closed before the cutoff is dropped before its activity is ever
    /// requested, and PRs sort by most recent activity by others, with
    /// untouched PRs last.
    #[test]
    fn collect_pages_trims_and_orders() {
        let mut api = fake(false);
        let prs = collect_fake(&mut api).unwrap();
        let listed: Vec<_> = prs.iter().map(|p| (p.url.as_str(), p.others)).collect();
        assert_eq!(
            listed,
            [
                (
                    "https://github.com/o/r/pull/merged",
                    at("2026-09-22T00:00:00Z")
                ),
                (
                    "https://github.com/o/r/pull/busy",
                    at("2026-09-03T00:00:00Z")
                ),
                ("https://github.com/o/r/pull/quiet", OthersActivity::Never),
            ]
        );
        assert_eq!(api.page_requests, [1, 2]);
        assert_eq!(api.activity_requests, [["quiet", "busy", "merged"]]);
    }

    /// The bug this guards against: GitHub flags a timed-out search as
    /// incomplete, and listing it anyway silently drops PRs. The search
    /// must be rerun from page 1 and the complete answer used.
    #[test]
    fn incomplete_search_is_retried_from_scratch() {
        let mut api = fake(false);
        api.pages.push_front(incomplete(search_page(vec![], 4)));
        assert_eq!(collect_fake(&mut api).unwrap().len(), 3);
        assert_eq!(api.page_requests, [1, 1, 2]);
    }

    /// A search that stays incomplete fails the command instead of printing
    /// a listing that looks complete but is not.
    #[test]
    fn persistently_incomplete_search_fails() {
        let mut api = fake(false);
        api.pages = VecDeque::from(vec![incomplete(search_page(vec![], 4)); SEARCH_ATTEMPTS]);
        let err = collect_fake(&mut api).unwrap_err();
        assert!(format!("{err:#}").contains("incomplete"), "{err:#}");
    }

    /// GitHub does not flag a result set that shifts between page fetches;
    /// the count check catches it. Here page 2 repeats a PR from page 1
    /// (as happens when an earlier PR drops out mid-pass), so fewer
    /// distinct PRs arrive than were counted, and the pass is retried.
    #[test]
    fn shifted_pages_are_retried() {
        let mut api = fake(false);
        let repeat = search_page(vec![hit("quiet", "open", None)], 2);
        api.pages.push_front(repeat.clone());
        api.pages.push_front(repeat);
        assert_eq!(collect_fake(&mut api).unwrap().len(), 3);
        assert_eq!(api.page_requests, [1, 2, 1, 2]);
    }

    /// A match count that changes between pages means the result set moved
    /// under the pagination; that pass cannot be trusted either.
    #[test]
    fn changed_match_count_is_retried() {
        let mut api = fake(false);
        api.pages.push_front(search_page(vec![], 5));
        api.pages
            .push_front(search_page(vec![hit("quiet", "open", None)], 4));
        assert_eq!(collect_fake(&mut api).unwrap().len(), 3);
        assert_eq!(api.page_requests, [1, 2, 1, 2]);
    }

    /// Past GitHub's 1,000-result cap the API cannot return everything, so
    /// the command refuses rather than list the first 1,000 as if that
    /// were all.
    #[test]
    fn over_cap_search_fails() {
        let mut api = fake(false);
        api.pages = VecDeque::from([search_page(vec![], SEARCH_RESULT_CAP + 1)]);
        let err = collect_fake(&mut api).unwrap_err();
        assert!(format!("{err:#}").contains("more than"), "{err:#}");
    }

    /// When a batched request comes back short, each short PR is refetched
    /// on its own and the answer is the same as if nothing had gone wrong.
    /// This is the guard against GitHub's silent degradation; without it a
    /// merged-by-maintainer PR would read as untouched.
    #[test]
    fn degraded_batch_is_retried_per_pr() {
        let mut api = fake(true);
        let prs = collect_fake(&mut api).unwrap();
        assert_eq!(prs[0].others, at("2026-09-22T00:00:00Z"));
        assert_eq!(
            api.activity_requests,
            [
                vec!["quiet", "busy", "merged"],
                vec!["busy"],
                vec!["merged"]
            ],
            "only short tails are refetched; empty ones are complete"
        );
    }

    /// Activity that is still short when requested alone yields `?` rather
    /// than a guess, and does not fail the listing. Unknown sorts ahead of
    /// untouched PRs: it may well be recent.
    #[test]
    fn persistently_short_activity_is_unknown() {
        let mut api = AlwaysShort(fake(false));
        let prs = collect_fake(&mut api).unwrap();
        let others: Vec<_> = prs.iter().map(|p| p.others).collect();
        use OthersActivity::{Never, Unknown};
        assert_eq!(others, [Unknown, Unknown, Never]);
        assert!(prs[2].url.ends_with("/quiet"));
    }

    /// A PR that vanished between the search and the activity request (repo
    /// deleted or made private) comes back from GitHub as a `null` with a
    /// NOT_FOUND error. That PR shows `?`; the rest of the listing, and the
    /// rest of its batch, is unaffected.
    #[test]
    fn vanished_pr_is_unknown_not_fatal() {
        let mut api = fake(false);
        api.activity.remove("busy");
        let prs = collect_fake(&mut api).unwrap();
        let busy = prs.iter().find(|p| p.url.ends_with("/busy")).unwrap();
        assert_eq!(busy.others, OthersActivity::Unknown);
        assert_eq!(prs[0].others, at("2026-09-22T00:00:00Z"));
    }

    /// Any GraphQL error other than NOT_FOUND still fails the query; only
    /// the vanished-node case is tolerated.
    #[test]
    fn other_graphql_errors_are_fatal() {
        struct Failing;
        impl GitHub for Failing {
            fn search_issues(&mut self, _: &str, _: usize) -> anyhow::Result<String> {
                unreachable!()
            }
            fn graphql(&mut self, _: &str, _: Value) -> anyhow::Result<String> {
                Ok(json!({"data": null, "errors": [{"type": "RATE_LIMITED", "message": "slow down"}]})
                    .to_string())
            }
        }
        let err = query_data::<Value>(&mut Failing, VIEWER_QUERY, json!({})).unwrap_err();
        assert!(format!("{err:#}").contains("slow down"), "{err:#}");
    }

    /// Wraps `FakeGitHub` so every timeline, batched or not, comes back
    /// empty against a non-zero `totalCount`.
    struct AlwaysShort(FakeGitHub);

    impl GitHub for AlwaysShort {
        fn search_issues(&mut self, q: &str, page: usize) -> anyhow::Result<String> {
            self.0.search_issues(q, page)
        }

        fn graphql(&mut self, query: &str, vars: Value) -> anyhow::Result<String> {
            let raw = self.0.graphql(query, vars)?;
            let mut v: Value = serde_json::from_str(&raw).unwrap();
            for node in v["data"]["nodes"].as_array_mut().unwrap() {
                node["timelineItems"]["nodes"] = json!([]);
            }
            Ok(v.to_string())
        }
    }

    // ── Ordering ──

    /// A PR with the given activity answer and update time.
    fn pr_with(others: OthersActivity, updated: &str) -> Pr {
        Pr {
            url: String::new(),
            repo: "o/r".to_string(),
            number: 1,
            title: updated.to_string(),
            state: PrState::Open,
            updated_at: ts(updated),
            others,
        }
    }

    /// Pin the documented order: newest activity by others first, then
    /// unknown, then untouched, with the most recently updated PR first
    /// among equals. Every other test uses one update time for all PRs, so
    /// this is the only guard on the tie-break.
    #[test]
    fn listing_order_groups_then_update_time() {
        let same = OthersActivity::At(ts("2026-09-05T00:00:00Z"));
        let mut prs = [
            pr_with(OthersActivity::Never, "2026-09-09T00:00:00Z"),
            pr_with(OthersActivity::Unknown, "2026-09-01T00:00:00Z"),
            pr_with(same, "2026-09-02T00:00:00Z"),
            pr_with(OthersActivity::Unknown, "2026-09-03T00:00:00Z"),
            pr_with(at("2026-09-06T00:00:00Z"), "2026-09-01T00:00:00Z"),
            pr_with(same, "2026-09-04T00:00:00Z"),
        ];
        prs.sort_by_key(listing_order);
        let order: Vec<_> = prs.iter().map(|p| (p.others, p.title.as_str())).collect();
        assert_eq!(
            order,
            [
                (at("2026-09-06T00:00:00Z"), "2026-09-01T00:00:00Z"),
                (same, "2026-09-04T00:00:00Z"),
                (same, "2026-09-02T00:00:00Z"),
                (OthersActivity::Unknown, "2026-09-03T00:00:00Z"),
                (OthersActivity::Unknown, "2026-09-01T00:00:00Z"),
                (OthersActivity::Never, "2026-09-09T00:00:00Z"),
            ]
        );
    }

    // ── Rendering ──

    /// Pin the age format the user asked for: largest unit first, zero
    /// units left out, `0m` for anything under a minute, and never
    /// negative when GitHub's clock is ahead of ours.
    #[test]
    fn age_format() {
        let now = ts("2026-09-10T12:00:00Z");
        assert_eq!(age(now, ts("2026-09-10T11:59:30Z")), "0m");
        assert_eq!(age(now, ts("2026-09-10T11:35:00Z")), "25m");
        assert_eq!(age(now, ts("2026-09-09T04:39:00Z")), "1d7h21m");
        assert_eq!(age(now, ts("2026-09-08T12:00:00Z")), "2d");
        assert_eq!(age(now, ts("2026-09-10T10:55:00Z")), "1h5m");
        assert_eq!(age(now, ts("2026-09-10T12:05:00Z")), "0m");
    }

    /// The fixed "now" for rendering tests.
    const NOW: &str = "2026-09-10T12:00:00Z";

    /// A draft PR whose others' activity was at `others_at` (see `NOW`).
    fn pr(repo: &str, number: u64, title: &str, others_at: &str) -> Pr {
        Pr {
            url: format!("https://github.com/{repo}/pull/{number}"),
            repo: repo.to_string(),
            number,
            title: title.to_string(),
            state: PrState::Draft,
            updated_at: ts("2026-09-01T00:00:00Z"),
            others: at(others_at),
        }
    }

    /// Drop OSC 8 and SGR sequences, leaving what the terminal shows.
    fn visible(line: &str) -> String {
        let mut out = String::new();
        let mut rest = line;
        while let Some(start) = rest.find('\x1b') {
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let end = if rest.starts_with("\x1b]") {
                rest.find("\x1b\\").unwrap() + 2
            } else {
                rest.find('m').unwrap() + 1
            };
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// Plain output is what pipes and scripts see, so pin it exactly: no
    /// padding (so the age is always the first field, even for `cut`), the
    /// full title, the URL, and no escape sequences.
    #[test]
    fn plain_lines_are_unpadded() {
        let prs = [
            pr("o/r", 7, "Fix it", "2026-09-10T11:35:00Z"),
            pr("owner/repo", 1234, "Other", "2026-09-09T04:39:00Z"),
        ];
        let lines = format_lines(&prs, Render::Plain, ts(NOW));
        assert_eq!(
            lines,
            [
                "25m  draft  o/r#7  Fix it  https://github.com/o/r/pull/7",
                "1d7h21m  draft  owner/repo#1234  Other  https://github.com/owner/repo/pull/1234",
            ]
        );
    }

    /// In a terminal the slug is a hyperlink to the PR, the URL column is
    /// gone, ages are right-aligned so their units line up, and slugs are
    /// padded so titles line up across rows of different widths.
    #[test]
    fn terminal_lines_link_and_align() {
        let prs = [
            pr("o/r", 7, "Short", "2026-09-10T11:35:00Z"),
            pr("owner/repo", 1234, "Longer", "2026-09-09T04:39:00Z"),
        ];
        let lines = format_lines(&prs, Render::Terminal { width: None }, ts(NOW));
        assert!(lines[0].contains("\x1b]8;;https://github.com/o/r/pull/7\x1b\\"));
        assert!(lines[0].contains(&format!("{LINK_COLOR}o/r#7{RESET}")));
        let shown: Vec<_> = lines.iter().map(|l| visible(l)).collect();
        assert_eq!(
            shown,
            [
                "    25m  draft  o/r#7            Short",
                "1d7h21m  draft  owner/repo#1234  Longer",
            ]
        );
    }

    /// A known width truncates the title so the whole line fits exactly,
    /// which is the point: one PR per screen line, never wrapped.
    #[test]
    fn terminal_lines_truncate_to_width() {
        let prs = [pr(
            "o/r",
            7,
            "A title that is much too long",
            "2026-09-10T11:35:00Z",
        )];
        // The prefix "25m  draft  o/r#7  " is 19 columns, leaving 11.
        let lines = format_lines(&prs, Render::Terminal { width: Some(30) }, ts(NOW));
        let shown = visible(&lines[0]);
        assert_eq!(shown, "25m  draft  o/r#7  A title th…");
        assert_eq!(shown.width(), 30);
    }

    /// Truncation measures terminal columns on the string as built, so wide
    /// characters and combining sequences cannot push a line past the
    /// edge; a title that fits is untouched, and a zero budget yields
    /// nothing rather than a lone ellipsis that would itself overflow.
    #[test]
    fn truncate_measures_columns() {
        assert_eq!(truncate("fits", 4), "fits");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // Each ideograph is two columns: two of them plus the ellipsis
        // would be 5, so only one fits alongside it.
        assert_eq!(truncate("漢字漢字", 4), "漢…");
        assert_eq!(truncate("abc", 0), "");
        // `❤` plus VS16 renders two columns wide, though the characters
        // sum to one; the cut must still leave room for the ellipsis.
        let cut = truncate("\u{2764}\u{fe0f}abc", 3);
        assert!(cut.width() <= 3, "{cut:?} is {} wide", cut.width());
        assert!(cut.ends_with('…'));
    }
}
