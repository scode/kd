//! boat.dev worker VM lifecycle: the boat counterpart of `kd ubiworker`.
//! Boat calls its VMs sandboxes; a boatworker is a disposable sandbox that
//! kd creates, lists, connects to and destroys with the same rules as a
//! ubiworker. Like a ubiworker, it enrolls itself into the tailnet with
//! Tailscale SSH on first boot (under its own tag, [`TAILSCALE_TAG`]), so
//! "my workers come up on the tailnet" holds for both fleets.
//!
//! kd drives boat entirely through the `boat` CLI, which keeps its own login
//! (`boat login` / `boat onboard`), so unlike ubiworker there is no API
//! token for kd to preflight: a signed-out `boat` fails at its first call
//! with boat's own error, and kd surfaces that unchanged.
//!
//! # Ownership convention
//!
//! A sandbox is a "boatworker" iff its boat `name` starts with
//! [`NAME_PREFIX`]. That is the whole test. ubiworker's second condition, a
//! fixed Ubicloud location, has no boat equivalent because boat has no
//! locations. Sandboxes boat creates without a name (the CLI's default) are
//! never owned.
//!
//! The test applies within whatever account scope the `boat` CLI is in: the
//! personal account, unless `BOAT_ORG` or `boat org switch` selects an
//! organization. In an organization, boat's listing can include sandboxes
//! other members shared, and a shared one named `boatworker-...` counts as
//! owned too. Each record does say how the caller may use it (an `access`
//! field, `owner` for one's own), but its other values are undocumented, so
//! kd does not filter on it rather than risk losing track of its own
//! workers. kd is a single-operator tool, and the personal scope is the one
//! it is built for.
//!
//! Every action resolves by name but acts on the immutable `bx_...` id, for
//! the same reason `kd ubiworker destroy` acts on Ubicloud's VM id: a name
//! can be reused between kd's listing and its action, an id cannot.

pub mod create;
pub mod destroy;
pub mod list;
pub mod ssh;

use crate::cmd::worker_bootstrap::EnvVar;
use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use tracing::{debug, warn};
use xshell::{Shell, cmd};

/// Prefix marking a boat sandbox as a kd-managed boatworker (see module docs
/// on the ownership convention).
pub(crate) const NAME_PREFIX: &str = "boatworker-";

/// Tailscale ACL tag on every auth key `create` mints, and so on every
/// boatworker's tailnet node. Its own tag, not ubiworker's `tag:ubicloud`,
/// so tailnet policy can treat the two fleets separately.
pub(crate) const TAILSCALE_TAG: &str = "tag:boat";

/// The Unix account on every boat sandbox. Boat provisions it (with
/// passwordless sudo) and offers no way to choose another, so unlike
/// ubiworker's account it does not follow the operator's local username.
pub(crate) const UNIX_USER: &str = "user";

/// Tailscale OAuth client id for minting boatworker auth keys. The same
/// variable ubiworker reads; the hint differs only in naming `tag:boat`,
/// which the client must own. A test pins the hint to [`TAILSCALE_TAG`].
pub(crate) const TS_API_CLIENT_ID: EnvVar = EnvVar {
    name: "TS_API_CLIENT_ID",
    how_to_get: "open https://console.tailscale.com/admin/settings/trust-credentials, create an \
                 OAuth credential with the auth_keys write scope and tag tag:boat; the client \
                 id is shown on the credential afterwards",
};

/// Boat machine sizes accepted by `create --type`, named as boat names them.
///
/// Spelled out rather than passed through as a free string so a typo fails
/// in kd's own argument parsing, before an auth key is minted. Whether the
/// account's plan allows a size (`xlarge` needs a higher plan) is boat's
/// to decide; kd does not look it up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MachineType {
    /// 2 vCPU, 4 GB
    Small,
    /// 4 vCPU, 8 GB
    Default,
    /// 8 vCPU, 16 GB
    Large,
    /// 16 vCPU, 32 GB
    Xlarge,
}

impl MachineType {
    /// The value `boat new --type` expects.
    pub(crate) fn as_boat_type(self) -> &'static str {
        match self {
            MachineType::Small => "small",
            MachineType::Default => "default",
            MachineType::Large => "large",
            MachineType::Xlarge => "xlarge",
        }
    }
}

/// `kd boatworker create [NAME] [--pkg PACKAGE]... [--type TYPE] [--ttl DURATION]` arguments.
#[derive(Args)]
pub struct CreateArgs {
    /// Worker name. Defaults to `boatworker-<local timestamp>`. A custom
    /// name gets the `boatworker-` prefix prepended if it's missing, and
    /// must then be a valid DNS label, since it is also the tailnet
    /// hostname.
    pub name: Option<String>,

    /// Apt package to install on the worker after it joins the tailnet
    /// (repeatable). Installed asynchronously, with an `apt-get
    /// dist-upgrade`, by the same `kd-bootstrap` unit as on ubiworkers; see
    /// `kd ubiworker create --help`.
    #[arg(long = "pkg", value_name = "PACKAGE")]
    pub pkgs: Vec<String>,

    /// Boat machine size. `large` has the 16 GB of memory a ubiworker has.
    #[arg(long = "type", value_enum, default_value_t = MachineType::Large)]
    pub machine_type: MachineType,

    /// Let boat delete the worker after this long, e.g. `500s`, `90m`, `3h`
    /// or `7d`. Without it the worker runs until `kd boatworker destroy`.
    /// Nothing of the worker is kept: boatworkers have no snapshots.
    #[arg(long, value_name = "DURATION", value_parser = parse_ttl)]
    pub ttl: Option<Ttl>,
}

/// A `--ttl` value: the lifetime in seconds, as `boat new --ttl` takes it,
/// and the text the user typed, which the create summary repeats.
#[derive(Clone, Debug, PartialEq)]
pub struct Ttl {
    pub seconds: u64,
    pub text: String,
}

/// Parse a `--ttl` value: a positive whole number directly followed by one
/// unit, `s`, `m`, `h` or `d` (`500s`, `90m`, `3h`, `7d`).
///
/// The unit is required. boat itself counts seconds (`boat new --ttl`) but
/// also hours (`boat extend --hours`), so a bare `3` has no obvious
/// meaning, and reading it the wrong way is off by a factor of 3600 on a
/// limit that ends in the worker's disk being erased. Compound values such
/// as `1h30m` are refused rather than half-parsed; `90m` says the same.
///
/// Anything under a minute is refused: setup alone takes about that long,
/// so such a value is almost certainly a slip (`30s` for `30m`), and acting
/// on it would spend a sandbox start and a minted key on a worker deleted
/// before it is usable. Values beyond what a signed 64-bit count of seconds
/// holds are refused as too large, so kd never hands boat a number the
/// summary cannot turn into a time either.
pub(crate) fn parse_ttl(value: &str) -> Result<Ttl, String> {
    let format = "expected a whole number followed by s, m, h or d, such as 500s, 90m, 3h or 7d";
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("'{value}' has no unit; {format}"))?;
    let (number, unit) = value.split_at(split);
    let scale: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(format!("invalid duration '{value}'; {format}")),
    };
    if number.is_empty() {
        return Err(format!("invalid duration '{value}'; {format}"));
    }
    // `number` is all ASCII digits here, so the only way to fail parsing
    // is overflow.
    let too_large = || format!("duration '{value}' is too large");
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(scale))
        .filter(|&s| i64::try_from(s).is_ok())
        .ok_or_else(too_large)?;
    if seconds < 60 {
        return Err(format!(
            "duration '{value}' is under a minute; the worker would be deleted before its setup \
             finishes (did you mean '{number}m'?)"
        ));
    }
    Ok(Ttl {
        seconds,
        text: value.to_owned(),
    })
}

/// `kd boatworker destroy [NAME...] [--all]` arguments.
#[derive(Args)]
pub struct DestroyArgs {
    /// Worker name(s) to destroy. If omitted (and `--all` isn't given),
    /// exactly one owned boatworker must exist and it is targeted
    /// automatically.
    pub names: Vec<String>,

    /// Destroy every owned boatworker. A no-op (not an error) if none
    /// exist, so scripted cleanup doesn't have to special-case an empty
    /// owned-worker listing.
    #[arg(short, long, conflicts_with = "names")]
    pub all: bool,
}

/// `kd boatworker ssh [ARGS...]` arguments.
///
/// A single trailing capture rather than a name positional plus a separate
/// argument vector, for the reason `kd ubiworker ssh` documents on its own
/// `SshArgs`: with two positionals, clap's global `-v`/`-q` would steal a
/// flag-shaped token meant for the remote side once the name was filled.
/// `ssh::split_target` then reads the first element as an optional worker
/// name. `OsString` so arguments reach `boat ssh` byte for byte.
#[derive(Args)]
pub struct SshArgs {
    /// Optionally a worker name, then arguments forwarded verbatim to
    /// `boat ssh` after the sandbox id: a remote command, optionally preceded
    /// by boat's own `-t` to give it a terminal.
    ///
    /// The first element is the worker name unless it starts with `-`. A
    /// leading `-v`, `-q` or `-h` (or `--verbose`, `--quiet`, `--help`) is
    /// claimed by kd itself, as anywhere else on the command line. ssh's own
    /// flags such as `-L` cannot be passed through `boat ssh`; use `boat
    /// forward` for port forwarding.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "NAME_OR_BOAT_SSH_ARG"
    )]
    pub args: Vec<std::ffi::OsString>,
}

/// `kd boatworker` subcommands. Each one's doc comment is its `--help` text.
#[derive(Subcommand)]
pub enum Commands {
    /// Create a boatworker sandbox that enrolls itself into the tailnet
    ///
    /// Mints a one-use tailscale auth key (tag:boat) and starts a boat
    /// sandbox whose first-boot setup installs tailscale, joins the tailnet
    /// with Tailscale SSH, and then bootstraps apt packages (`--pkg`) in a
    /// detached unit, exactly as on ubiworkers. The sandbox runs until
    /// destroyed, or with --ttl until boat deletes it, and gets none of the
    /// boat account's stored secrets.
    /// Returns once boat reports the sandbox ready, without waiting for
    /// enrollment.
    Create(CreateArgs),
    /// Delete one or more boatworker sandboxes
    ///
    /// Takes any number of names, or --all to delete every owned
    /// boatworker. With neither, targets the sole existing boatworker (an
    /// error if there are zero or more than one). There is no confirmation
    /// prompt (see SPEC.md); resolution is fail-closed, so a name that
    /// doesn't match deletes nothing rather than partially executing.
    Destroy(DestroyArgs),
    /// List existing boatworker sandboxes
    List,
    /// Open a shell on an owned boatworker, or run a command there, via boat ssh
    ///
    /// With no name, targets the sole existing boatworker, like `destroy`.
    /// The first argument is the worker name unless it starts with `-`;
    /// everything after it goes to `boat ssh <id>` unchanged: a remote
    /// command, optionally preceded by `-t` for a terminal. kd execs into
    /// `boat ssh`, replacing itself. ssh flags such as `-L` do not pass
    /// through boat (use `boat forward`), and a failing remote command exits
    /// with status 1 rather than its own status (see SPEC.md).
    Ssh(SshArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Create(args) => create::run(args),
            Commands::Destroy(args) => destroy::run(args),
            Commands::List => list::run(),
            Commands::Ssh(args) => ssh::run(args),
        }
    }
}

/// Add the `boatworker-` ownership prefix to a user-supplied name if it's
/// missing, so `foo` and `boatworker-foo` name the same worker.
///
/// This is all a name used for lookup goes through. It is deliberately not
/// validated: boat accepts nearly any sandbox name, so an owned sandbox made
/// by hand can carry a name kd would never have chosen, and `destroy` and
/// `ssh` must still be able to find it by the name `list` shows.
pub(crate) fn with_prefix(name: &str) -> String {
    if name.starts_with(NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{NAME_PREFIX}{name}")
    }
}

// ── Names for new workers ───────────────────────────────────────────────
// Only `create` validates names; lookups just prefix (see `with_prefix`).

/// Render the default worker name, `boatworker-YYYYMMDD-HHMMSS`, from a
/// caller-supplied local timestamp (a parameter so tests can pin it).
pub(crate) fn default_worker_name(now: &jiff::Zoned) -> String {
    format!(
        "{}{:04}{:02}{:02}-{:02}{:02}{:02}",
        NAME_PREFIX,
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// The name rule for a new boatworker: lowercase alphanumerics and hyphens,
/// at most 63 characters, starting and ending with an alphanumeric.
///
/// Boat itself accepts nearly any name up to 120 characters, so this is not
/// mirroring a server-side rule the way ubiworker's check mirrors
/// Ubicloud's. It is the DNS label rule, because the name doubles as the
/// worker's tailnet hostname (`tailscale up --hostname`), and a name only
/// boat would accept would come up on the tailnet under some mangled form.
/// It is also what makes the name safe to interpolate into the first-boot
/// script and to pass to `boat new`: no quote, whitespace or shell
/// metacharacter, and no leading `-`.
fn has_valid_name_chars(name: &str) -> bool {
    let bytes = name.as_bytes();
    let is_alnum = |b: &u8| b.is_ascii_digit() || b.is_ascii_lowercase();
    match (bytes.first(), bytes.last()) {
        (Some(first), Some(last)) => {
            bytes.len() <= 63
                && is_alnum(first)
                && is_alnum(last)
                && bytes.iter().all(|b| is_alnum(b) || *b == b'-')
        }
        _ => false,
    }
}

/// Prefix (see [`with_prefix`]) and validate a name for a new worker against
/// [`has_valid_name_chars`], before anything is minted or started.
pub(crate) fn normalize_worker_name(name: &str) -> anyhow::Result<String> {
    let prefixed = with_prefix(name);
    if !has_valid_name_chars(&prefixed) {
        bail!(
            "invalid worker name '{prefixed}': must be lowercase alphanumerics and hyphens, at \
             most 63 characters, and must not start or end with a hyphen (the name is also the \
             worker's tailnet hostname; checked after adding the 'boatworker-' prefix)"
        );
    }
    Ok(prefixed)
}

// ── Worker listing ───────────────────────────────────────────────────────
// Shared by `list` and by every subcommand that resolves a name.

/// One sandbox from `boat list --all --json`, reduced to the fields kd uses.
/// Boat's records carry several dozen more; serde ignores them, so a field
/// boat adds or drops elsewhere in the record cannot break parsing.
///
/// `name` is optional because boat only names a sandbox when asked to
/// (`boat new --name`). An unnamed sandbox is never a boatworker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Sandbox {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) state: String,
    #[serde(rename = "type")]
    pub(crate) machine_type: String,
}

/// The top level of `boat list --json`: one page of sandboxes plus the
/// paging information.
#[derive(Debug, Deserialize)]
struct SandboxPage {
    sandboxes: Vec<Sandbox>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

/// Boat's paging information. Only `hasMore` matters to kd: it is how kd
/// knows the listing it is working from is incomplete (see
/// [`owned_workers`]).
#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasMore")]
    has_more: bool,
}

/// An owned boatworker: a [`Sandbox`] whose name passed the ownership test,
/// with the name unwrapped so callers never re-check the `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worker {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) machine_type: String,
}

/// The owned workers kd could see, and whether that is all of them.
///
/// `complete` is false when boat said there were more sandboxes than its
/// one page held. Callers that would otherwise infer something from what is
/// *absent* (the no-name "sole worker" forms of `destroy` and `ssh`) must
/// refuse in that case, since the worker they would pick might not be the
/// only one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedWorkers {
    pub(crate) workers: Vec<Worker>,
    pub(crate) complete: bool,
}

/// Parse `boat list --json` output, failing on anything that is not the
/// expected shape.
///
/// Fail-closed for the same reason as ubiworker's `parse_vm_list`: a
/// listing kd only half understood must not turn into an empty or partial
/// list of workers, which `destroy` would then act on.
fn parse_sandbox_page(json: &str) -> anyhow::Result<SandboxPage> {
    serde_json::from_str(json).context("failed to parse 'boat list --json' output")
}

/// The ownership test itself (see module docs), plus unwrapping the name.
fn owned(sandbox: Sandbox) -> Option<Worker> {
    let name = sandbox.name.filter(|name| name.starts_with(NAME_PREFIX))?;
    Some(Worker {
        name,
        id: sandbox.id,
        state: sandbox.state,
        machine_type: sandbox.machine_type,
    })
}

/// Reduce a parsed page to the owned workers on it.
fn owned_workers_from(page: SandboxPage) -> OwnedWorkers {
    OwnedWorkers {
        workers: page.sandboxes.into_iter().filter_map(owned).collect(),
        complete: !page.page_info.has_more,
    }
}

/// List every sandbox in the account, in every state, and filter down to
/// the ones kd owns.
///
/// `--all` because boat's default listing shows only running sandboxes. A
/// boatworker that is still provisioning, or that went to `error`, is still
/// kd's, still listed by `kd boatworker list`, and still something `destroy`
/// must be able to find.
///
/// This reads one page. Boat's page holds 200 sandboxes, and the CLI has no
/// cursor flag to read further. ubiworker accepts the same single-page
/// limit for Ubicloud's listing; the difference here is that boat says when
/// there is more, so kd warns instead of truncating silently, and reports
/// the listing as incomplete (see [`OwnedWorkers`]).
pub(crate) fn owned_workers(sh: &Shell) -> anyhow::Result<OwnedWorkers> {
    // boat needs none of the Tailscale OAuth pair `create` reads, which is
    // often exported in the same shell; keep it out of boat's environment
    // as the `boat new` and `boat ssh` calls do.
    let output = cmd!(sh, "boat list --all --json")
        .env_remove("TS_API_CLIENT_ID")
        .env_remove("TS_API_CLIENT_SECRET")
        .read()
        .context("failed to list boat sandboxes")?;
    let page = parse_sandbox_page(&output)?;
    let total = page.sandboxes.len();
    let owned = owned_workers_from(page);
    if !owned.complete {
        warn!(
            "boat listed only the first {total} sandboxes in this account and has more; \
             boatworkers beyond that page are not visible to kd"
        );
    }
    // Same -v aid as ubiworker: a worker "missing" from `list` is almost
    // always a sandbox whose name lacks the prefix, and the gap between the
    // two counts is what shows that without hand-running `boat list`.
    debug!(
        "boat list: {} sandboxes total, {} owned boatworkers",
        total,
        owned.workers.len()
    );
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare name and its prefixed form must name the same worker, which is
    /// what lets `destroy foo` find `boatworker-foo`.
    #[test]
    fn with_prefix_adds_prefix_only_when_missing() {
        assert_eq!(with_prefix("foo"), "boatworker-foo");
        assert_eq!(with_prefix("boatworker-foo"), "boatworker-foo");
    }

    /// A lookup name is not validated: an owned sandbox made by hand with a
    /// name kd would never choose must still be reachable by that name.
    #[test]
    fn with_prefix_keeps_unusual_names_intact() {
        assert_eq!(with_prefix("My_Box"), "boatworker-My_Box");
    }

    // ── names for new workers ────────────────────────────────────────────

    /// The default name comes from the supplied local time, so tests (and
    /// `create`) never depend on the system clock or timezone.
    #[test]
    fn default_worker_name_formats_fixed_instant() {
        let now = jiff::civil::date(2026, 8, 3)
            .at(15, 4, 5, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        assert_eq!(default_worker_name(&now), "boatworker-20260803-150405");
    }

    /// The default name must itself pass the name rule, or every `create`
    /// without a name would fail.
    #[test]
    fn default_worker_name_is_a_valid_name() {
        let now = jiff::civil::date(2026, 12, 31)
            .at(23, 59, 59, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        assert!(has_valid_name_chars(&default_worker_name(&now)));
    }

    /// The rule is the DNS-label rule the tailnet hostname needs, so every
    /// way out of it must be refused: uppercase, underscore, space, quote, a
    /// trailing hyphen (including the empty custom name, which leaves just
    /// the prefix), and a name over 63 characters after prefixing.
    #[test]
    fn normalize_worker_name_rejects_non_dns_label_names() {
        let too_long = "a".repeat(64 - NAME_PREFIX.len());
        for name in ["Foo", "foo_bar", "foo bar", "it's", "foo-", "", &too_long] {
            assert!(
                normalize_worker_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    /// The 63-character boundary itself passes, so the limit is exactly the
    /// DNS label length; and a bare name gets the prefix.
    #[test]
    fn normalize_worker_name_accepts_valid_names() {
        let suffix = "a".repeat(63 - NAME_PREFIX.len());
        assert_eq!(normalize_worker_name(&suffix).unwrap().len(), 63);
        assert_eq!(normalize_worker_name("foo-1").unwrap(), "boatworker-foo-1");
    }

    /// The error is user-facing guidance, so it must describe the rule and
    /// say why it exists (the tailnet hostname), not just refuse.
    #[test]
    fn normalize_worker_name_error_names_the_rule() {
        let msg = normalize_worker_name("Foo").unwrap_err().to_string();
        assert!(msg.contains("lowercase") && msg.contains("63"), "{msg}");
        assert!(msg.contains("tailnet hostname"), "{msg}");
    }

    // ── create settings ──────────────────────────────────────────────────

    /// `--type` values go to `boat new --type` verbatim, so each variant must
    /// map to boat's own spelling.
    #[test]
    fn machine_type_maps_to_boat_names() {
        let names: Vec<&str> = MachineType::value_variants()
            .iter()
            .map(|t| t.as_boat_type())
            .collect();
        assert_eq!(names, vec!["small", "default", "large", "xlarge"]);
    }

    /// The credential hint names the tag the OAuth client must own; this
    /// keeps it from drifting if the tag ever changes.
    #[test]
    fn tailscale_client_id_hint_names_the_boat_tag() {
        assert!(TS_API_CLIENT_ID.how_to_get.contains(TAILSCALE_TAG));
    }

    /// A trimmed copy of real `boat list --all --json` output (boat CLI
    /// 1.0.97), with a few of the fields kd ignores left in to show they
    /// don't matter, and identifying values replaced.
    const LIST_FIXTURE: &str = r#"{
      "pageInfo": {"hasMore": false, "limit": 200, "nextCursor": null},
      "sandboxes": [
        {"access": "owner", "id": "bx_aaaaaaaa", "name": "boatworker-dev1", "state": "idle",
         "type": "large", "vcpu": 8, "memoryGB": 16, "setupStatus": "done", "labels": {}},
        {"access": "owner", "id": "bx_bbbbbbbb", "name": "something-else", "state": "ready",
         "type": "default"},
        {"access": "owner", "id": "bx_cccccccc", "name": null, "state": "error", "type": "small"}
      ]
    }"#;

    /// The ownership filter keeps only prefixed names and drops both an
    /// unrelated name and an unnamed sandbox; the kept worker carries the
    /// fields `list` prints, and a page without `hasMore` is complete.
    #[test]
    fn owned_workers_from_filters_real_listing_by_prefix() {
        let owned = owned_workers_from(parse_sandbox_page(LIST_FIXTURE).unwrap());
        assert_eq!(
            owned,
            OwnedWorkers {
                workers: vec![Worker {
                    name: "boatworker-dev1".to_string(),
                    id: "bx_aaaaaaaa".to_string(),
                    state: "idle".to_string(),
                    machine_type: "large".to_string(),
                }],
                complete: true,
            }
        );
    }

    /// `hasMore` must surface as an incomplete listing, which is what makes
    /// the no-name forms refuse instead of guessing.
    #[test]
    fn owned_workers_from_marks_truncated_page_incomplete() {
        let json = r#"{"pageInfo": {"hasMore": true}, "sandboxes": []}"#;
        assert!(!owned_workers_from(parse_sandbox_page(json).unwrap()).complete);
    }

    /// The prefix test is a prefix test, not a substring test: a name that
    /// merely contains `boatworker-` somewhere else is not owned.
    #[test]
    fn owned_requires_the_prefix_at_the_start() {
        let sandbox = Sandbox {
            id: "bx_x".to_string(),
            name: Some("my-boatworker-1".to_string()),
            state: "idle".to_string(),
            machine_type: "large".to_string(),
        };
        assert_eq!(owned(sandbox), None);
    }

    /// Output kd doesn't understand (not JSON, or JSON missing the fields it
    /// needs) must be an error, never an empty listing: an empty listing
    /// would read as "no workers" and let `destroy --all` succeed as a
    /// no-op, or make a no-argument `destroy` report that none exist.
    #[test]
    fn parse_sandbox_page_rejects_unexpected_shapes() {
        for bad in [
            "not json",
            r#"{"sandboxes": []}"#,
            r#"{"pageInfo": {"hasMore": false}}"#,
            r#"{"pageInfo": {"hasMore": false}, "sandboxes": [{"name": "boatworker-x"}]}"#,
        ] {
            assert!(parse_sandbox_page(bad).is_err(), "{bad:?} should fail");
        }
    }

    /// The documented forms parse to boat's seconds, keeping the text the
    /// summary repeats.
    #[test]
    fn parse_ttl_accepts_each_unit() {
        for (text, seconds) in [
            ("500s", 500),
            ("90m", 5400),
            ("3h", 10_800),
            ("7d", 604_800),
        ] {
            assert_eq!(
                parse_ttl(text),
                Ok(Ttl {
                    seconds,
                    text: text.to_owned()
                })
            );
        }
    }

    /// Anything that is not one whole number and one known unit is refused
    /// rather than guessed at: a bare number (seconds or hours?), a compound
    /// value, an unknown or upper-case unit, signs, spaces, decimals,
    /// non-ASCII digits, anything under a minute (a likely slip for
    /// minutes), and values too large for boat's seconds, which must say
    /// "too large" rather than claim the input is malformed.
    #[test]
    fn parse_ttl_refuses_everything_else() {
        for bad in [
            "",
            "3",
            "h",
            "1h30m",
            "3H",
            "3 h",
            " 3h",
            "+3h",
            "-3h",
            "1.5h",
            "3w",
            "0s",
            "0d",
            "59s",
            "\u{0663}h",
            "18446744073709551615d",
            "99999999999999999999999s",
            "9223372036854775808s",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(parse_ttl("3").unwrap_err().contains("no unit"));
        assert!(parse_ttl("30s").unwrap_err().contains("'30m'"));
        for huge in [
            "99999999999999999999999s",
            "9223372036854775808s",
            "18446744073709551615d",
        ] {
            assert!(parse_ttl(huge).unwrap_err().contains("too large"), "{huge}");
        }
        assert!(parse_ttl("60s").is_ok() && parse_ttl("1m").is_ok());
    }
}
