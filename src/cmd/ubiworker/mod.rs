//! Ubicloud worker VM lifecycle: disposable VMs that self-enroll into a
//! Tailscale tailnet on boot, so they're reachable over `ssh` without any
//! public-IP or firewall bookkeeping.
//!
//! # Ownership convention
//!
//! `ubi` (Ubicloud's CLI) is a shared, multi-tenant surface with no notion
//! of "VMs kd created for this purpose" — `ubi vm list` just enumerates every
//! VM in the account. We define ownership structurally instead of tracking
//! it anywhere: a VM is a "ubiworker" iff its name starts with
//! [`NAME_PREFIX`] *and* it lives in [`LOCATION`]. Both conditions matter —
//! name prefix alone would let an unrelated VM in a different location that
//! happens to share the naming convention get swept up by `list`/`destroy`.
//! This convention is why `list` and `destroy` (with no name given) can
//! safely operate without a side database of "VMs I created."
//!
//! # Credential handling
//!
//! `ubi` reads `UBI_TOKEN` itself from the environment; kd only checks it's
//! set so a missing token fails fast with a clear message instead of a
//! cryptic error surfacing from deep inside the `ubi` binary. The Tailscale
//! OAuth credentials (`TS_API_CLIENT_ID`/`TS_API_CLIENT_SECRET`) are read
//! once, in [`create::run`], and threaded through as plain parameters from
//! there down — nothing below that dispatch point touches the environment.
//! That's deliberate: it's what makes the tailscale HTTP calls unit-testable
//! without mutating process env vars, and it keeps the secret's flow through
//! the program easy to audit by reading signatures.

pub mod create;
pub mod destroy;
pub mod list;
pub mod ssh;
pub mod tailscale;

use anyhow::bail;
use clap::{Args, Subcommand};
use tracing::debug;
use xshell::{Shell, cmd};

// ── Hardcoded infra parameters ──────────────────────────────────────────
// Not exposed as flags: a ubiworker is meant to be one fixed, disposable
// shape. If you need something else, build it by hand with `ubi` directly.

/// Ubicloud location every ubiworker is created in, and the location half
/// of the ownership test (see module docs).
pub(crate) const LOCATION: &str = "us-east-a2";
/// Ubicloud VM size (vCPU/RAM class).
pub(crate) const SIZE: &str = "standard-4";
/// Boot disk size, in GiB.
pub(crate) const STORAGE_SIZE_GIB: u32 = 80;
/// Boot image slug.
pub(crate) const BOOT_IMAGE: &str = "ubuntu-resolute";
/// Unix login user provisioned on the VM.
pub(crate) const UNIX_USER: &str = "scode";
/// Names of the ubicloud-registered SSH keys installed on every worker.
pub(crate) const SSH_KEY_NAMES: [&str; 2] = ["laptop", "devbox"];
/// Tailscale ACL tag applied to every minted auth key, and — because the
/// key is what a device authenticates with — to every worker that joins
/// using it.
pub(crate) const TAILSCALE_TAG: &str = "tag:ubicloud";
/// Prefix marking a VM as a kd-managed ubiworker (see module docs on the
/// ownership convention).
pub(crate) const NAME_PREFIX: &str = "ubiworker-";

/// `kd ubiworker create [NAME]` arguments.
#[derive(Args)]
pub struct CreateArgs {
    /// Worker name. Defaults to `ubiworker-<local timestamp>`. A custom
    /// name gets the `ubiworker-` prefix prepended automatically if it's
    /// missing.
    pub name: Option<String>,
}

/// `kd ubiworker destroy [NAME...] [--all]` arguments.
#[derive(Args)]
pub struct DestroyArgs {
    /// Worker name(s) to destroy. If omitted (and `--all` isn't given),
    /// exactly one owned ubiworker must exist and it is targeted
    /// automatically.
    pub names: Vec<String>,

    /// Destroy every owned ubiworker. A no-op (not an error) if none
    /// exist, so scripted cleanup doesn't have to special-case an empty
    /// owned-worker listing.
    #[arg(short, long, conflicts_with = "names")]
    pub all: bool,
}

/// `kd ubiworker ssh [ARGS...]` arguments.
///
/// Deliberately a single trailing capture, not a `name: Option<String>`
/// positional followed by a separate `ssh_args: Vec<String>` (an earlier,
/// buggy shape). Splitting into two positionals let clap's *global* `-v`/`-q`
/// flags steal a hyphen-leading token that was meant for `ssh` as soon as the
/// `name` positional had already been satisfied — e.g. `kd ubiworker ssh
/// myname -v` parsed `-v` as kd's own verbosity flag instead of forwarding it,
/// because the trailing-vararg positional hadn't "started" yet from clap's
/// point of view. A single positional has no such handoff point: once
/// anything after `ssh` starts matching, every remaining token — flag-shaped
/// or not — belongs to this vec, `-v`/`-q` included. See `ssh::split_target`
/// for how the first element is then reinterpreted as an optional worker
/// name.
///
/// `OsString`, not `String`: the module docs promise ssh args are forwarded
/// verbatim, and `String` would reject a non-UTF-8 argument (a legal argv
/// entry on Unix) that `ssh` itself would happily accept.
#[derive(Args)]
pub struct SshArgs {
    /// Optionally a worker name, then arguments forwarded verbatim to `ssh`
    /// after the `user@host` destination.
    ///
    /// The first element is treated as the worker name unless it starts with
    /// `-` (see `ssh::split_target`); everything else is forwarded to `ssh`
    /// as-is — a remote command, or flags like `-L`/`-D`. An ssh flag clap
    /// doesn't itself recognize (e.g. `-L 8080:localhost:80`) is captured
    /// here just fine with no name given and no `--` needed. But a flag that
    /// *does* collide with one of kd's own registered flags — `-v`, `-q`,
    /// `-h` (and their long forms) — is claimed by kd first, before this
    /// vec ever sees it, exactly as it would be anywhere else in the
    /// command line; use `--` to force it through instead (e.g. `kd
    /// ubiworker ssh -- -v` to run `ssh -v` for its own verbose output). See
    /// the CLI wiring tests in `crate::cmd::tests` for both cases pinned
    /// directly against clap's parse output.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "NAME_OR_SSH_ARG"
    )]
    pub args: Vec<std::ffi::OsString>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a ubiworker VM and enroll it into the tailnet
    ///
    /// Fetches the registered "laptop" and "devbox" SSH keys, mints a
    /// one-use tailscale auth key, and creates the VM with a first-boot
    /// script that installs tailscale and joins the tailnet under it.
    Create(CreateArgs),
    /// Destroy one or more ubiworker VMs
    ///
    /// Takes any number of names, or --all to destroy every owned
    /// ubiworker. With neither, targets the sole existing ubiworker (an
    /// error if there are zero or more than one). There is no confirmation
    /// prompt (see SPEC.md); resolution is fail-closed, so a name that
    /// doesn't match destroys nothing rather than partially executing.
    Destroy(DestroyArgs),
    /// List existing ubiworker VMs
    List,
    /// Connect to an owned ubiworker over ssh
    ///
    /// With no name, targets the sole existing ubiworker, exactly like
    /// `destroy`. The first argument is the worker name unless it starts
    /// with `-`, in which case there is no name and every argument is
    /// forwarded to `ssh` (a remote command, or flags like `-L`); a leading
    /// flag that also happens to be one of kd's own (`-v`/`-q`/`-h`) is
    /// claimed by kd instead, so route it through `ssh` with `--` (e.g. `kd
    /// ubiworker ssh -- -v`). kd execs directly into `ssh`, replacing
    /// itself, so ssh's exit code/signals/tty behave exactly as they would
    /// running `ssh` by hand. Host-key checking is deliberately bypassed
    /// (see `ssh` module docs and SPEC.md): worker names are reused and each
    /// fresh boot gets a new host key, so ordinary known-hosts pinning would
    /// hard-fail every reconnection. Bypassing it delegates endpoint
    /// identity entirely to Tailscale — reachability is already gated by
    /// tailnet ACLs, not by the auth key a worker enrolled with, so this
    /// isn't inventing a new trust boundary, just declining to add a second,
    /// friction-only one on top.
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

// ── Name normalization/validation ───────────────────────────────────────
// Shared by `create` and `destroy`: both accept an optional user-supplied
// name that needs the same prefixing and charset validation.

/// Render the default worker name, `ubiworker-YYYYMMDD-HHMMSS`, from a
/// caller-supplied local timestamp.
///
/// Takes the timestamp as a parameter rather than calling `Zoned::now()`
/// itself so tests can pin the clock without touching process state.
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

/// Ubicloud's own VM-name rule, mirrored exactly from `ubicloud`'s
/// `ALLOWED_NAME_PATTERN` (`lib/validation.rb`): `^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$`.
/// Lowercase alphanumerics and hyphens only, at most 63 characters, and must
/// not start or end with a hyphen. kd enforces this locally so a name
/// Ubicloud would reject server-side fails fast here, before a one-use
/// tailscale key gets minted for a VM that will never be created (see
/// [`tailscale::mint_auth_key`] and its expiry, which bounds — but doesn't
/// eliminate — the cost of that kind of waste). Because [`NAME_PREFIX`]
/// eats into the 63-character budget, a custom name's *suffix* only has
/// `63 - NAME_PREFIX.len()` characters to work with.
///
/// This charset is also what makes `create::render_init_script`'s
/// unescaped single-quote interpolation of the name safe: no character in
/// it is a `'`. That charset is strictly narrower than the pattern it
/// replaced, so the safety property still holds; changing it again means
/// re-checking that justification.
///
/// Implemented over `chars()` rather than a regex dependency — the pattern
/// is simple enough that spelling it out as "first/last char alnum, all
/// chars alnum-or-hyphen, length in 1..=63" is clearer than pulling in a
/// crate for it.
fn has_valid_name_chars(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    let is_alnum = |c: &char| c.is_ascii_digit() || c.is_ascii_lowercase();

    !chars.is_empty()
        && chars.len() <= 63
        && is_alnum(&chars[0])
        && is_alnum(chars.last().unwrap())
        && chars.iter().all(|c| is_alnum(c) || *c == '-')
}

/// Add the `ubiworker-` ownership prefix if it's not already present, so a
/// bare name like `foo` and an explicit `ubiworker-foo` both resolve to the
/// same VM.
fn with_prefix(name: &str) -> String {
    if name.starts_with(NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{NAME_PREFIX}{name}")
    }
}

/// Normalize a user-supplied worker name for `create`/`destroy`: add the
/// ownership prefix if missing, then validate the resulting charset. This
/// is the one gate every custom name passes through.
pub(crate) fn normalize_worker_name(name: &str) -> anyhow::Result<String> {
    let prefixed = with_prefix(name);
    if !has_valid_name_chars(&prefixed) {
        bail!(
            "invalid worker name '{prefixed}': must be lowercase alphanumerics and hyphens, \
             at most 63 characters, and must not start or end with a hyphen (Ubicloud's \
             VM-name rule; checked after adding the 'ubiworker-' prefix)"
        );
    }
    Ok(prefixed)
}

// ── Auth-key validation ──────────────────────────────────────────────────
// Shared by `tailscale::parse_create_key_response` (the first place a
// minted key value exists at all) and `create::render_init_script` (which
// interpolates it unescaped into a single-quoted shell argument). One
// predicate, checked in both places, so the two call sites can't drift.

/// Full charset/shape validation for a tailscale auth key: the
/// `tskey-auth-` prefix, a non-empty remainder, and — over the *whole*
/// string, prefix included — nothing outside `[A-Za-z0-9-]`.
///
/// This must stay a full-string check, not just a prefix check: the reason
/// this predicate exists at all is to justify `render_init_script`
/// interpolating the key unescaped inside a single-quoted shell argument,
/// and a prefix-only check would let a `'` anywhere after the prefix slip
/// through into that interpolation.
pub(crate) fn is_valid_auth_key(key: &str) -> bool {
    const PREFIX: &str = "tskey-auth-";
    key.strip_prefix(PREFIX)
        .is_some_and(|rest| !rest.is_empty())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

// ── Worker listing ───────────────────────────────────────────────────────
// Shared by `list` and by `destroy` when no name is given.

/// One row of `ubi vm list` output, using the exact columns we requested
/// (`-f location,name,id,ip4`) so parsing doesn't depend on `ubi`'s default
/// column set, which is not a contract kd controls.
///
/// `ip4` is the only nullable column: `ubi`'s `format_rows` (in `ubi_cli.rb`)
/// renders a null ip4 as an empty, space-padded cell rather than omitting
/// the column, so a whitespace split of an IPv4-less VM's row yields 3
/// tokens instead of 4. `location`/`name`/`id` are never null in `ubi`'s own
/// data model, so they stay plain `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VmRow {
    pub(crate) location: String,
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) ip4: Option<String>,
}

/// Parse `ubi vm list -N -f location,name,id,ip4` output into rows.
///
/// `-N` suppresses the header, so every line is (supposed to be) data,
/// whitespace-separated. Blank lines are skipped. A row with 4 tokens has
/// an ip4; one with 3 tokens is missing its (nullable) ip4 — see [`VmRow`].
/// Any other non-blank token count is an error naming the offending line:
/// this used to be tolerated (ragged rows silently dropped), but `destroy`
/// with no name argument resolves its target by counting rows from this
/// parse, so silently dropping a row here could make it misidentify "the
/// sole worker" when a row actually failed to parse. Fail closed instead —
/// a parse error surfacing as a command failure is much safer than a wrong
/// VM getting destroyed.
pub(crate) fn parse_vm_list(output: &str) -> anyhow::Result<Vec<VmRow>> {
    let mut rows = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            [location, name, id, ip4] => VmRow {
                location: location.to_string(),
                name: name.to_string(),
                id: id.to_string(),
                ip4: Some(ip4.to_string()),
            },
            [location, name, id] => VmRow {
                location: location.to_string(),
                name: name.to_string(),
                id: id.to_string(),
                ip4: None,
            },
            _ => bail!(
                "unexpected 'ubi vm list' row (expected 3 columns, or 4 with an ip4): '{line}'"
            ),
        };
        rows.push(row);
    }
    Ok(rows)
}

/// The ownership test itself (see module docs): location match plus name
/// prefix match. Kept separate from [`parse_vm_list`] so it stays testable
/// on its own and reusable if a future command needs the same filter.
pub(crate) fn is_owned(row: &VmRow) -> bool {
    row.location == LOCATION && row.name.starts_with(NAME_PREFIX)
}

/// List every VM `ubi` knows about and filter down to the ones kd owns.
///
/// Filtering by location happens client-side here rather than via `ubi
/// vm list`'s own `-l` flag: `-l`'s exact matching semantics (prefix vs.
/// exact match, and its behavior on a typo'd location) aren't documented or
/// confirmed, so this keeps the filter in code kd controls and can test.
pub(crate) fn owned_workers(sh: &Shell) -> anyhow::Result<Vec<VmRow>> {
    let output = cmd!(sh, "ubi vm list -N -f location,name,id,ip4").read()?;
    let rows = parse_vm_list(&output)?;
    let total = rows.len();
    let owned: Vec<VmRow> = rows.into_iter().filter(is_owned).collect();
    // The only -v visibility into this path: how much the ownership filter
    // discarded. A worker "missing" from `list` is almost always sitting in
    // the total-minus-owned gap (wrong location or prefix), and this line is
    // what shows that without hand-running `ubi vm list`.
    debug!(
        "ubi vm list: {} rows total, {} owned ubiworkers",
        total,
        owned.len()
    );
    Ok(owned)
}

// ── Env preflight ────────────────────────────────────────────────────────

/// Read a required env var, failing fast with the specific variable name.
///
/// Called directly from each command's `run()`, not from any helper below
/// it — helpers take credentials as parameters instead, which is both what
/// keeps them testable without env mutation and what keeps the secret's
/// flow through the program legible from function signatures alone.
pub(crate) fn require_env(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("{name} is not set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    /// `default_worker_name` must format from the supplied instant, not the
    /// system clock — this is what makes the function safe to call from
    /// tests at all. Uses UTC so the assertion doesn't depend on the host's
    /// local timezone.
    #[test]
    fn default_worker_name_formats_fixed_instant() {
        let now = date(2026, 8, 3)
            .at(15, 4, 5, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(default_worker_name(&now), "ubiworker-20260803-150405");
    }

    /// Guards against a regression where the implementation converts to UTC
    /// before reading off date/time components. America/New_York in January
    /// is fixed at EST (UTC-5, no DST), so 21:30 local on the 15th is 02:30
    /// UTC on the *16th* — if `default_worker_name` ever read UTC components
    /// instead of the `Zoned`'s own local ones, this would fail on the date
    /// alone, not just the time.
    #[test]
    fn default_worker_name_uses_local_components_in_non_utc_zone() {
        let tz = TimeZone::get("America/New_York").expect("tzdb must include America/New_York");
        let now = date(2026, 1, 15).at(21, 30, 0, 0).to_zoned(tz).unwrap();
        assert_eq!(default_worker_name(&now), "ubiworker-20260115-213000");
    }

    #[test]
    fn has_valid_name_chars_rejects_empty() {
        assert!(!has_valid_name_chars(""));
    }

    #[test]
    fn has_valid_name_chars_rejects_invalid_chars() {
        assert!(!has_valid_name_chars("ubiworker-foo bar"));
        assert!(!has_valid_name_chars("ubiworker-foo_bar"));
    }

    /// Uppercase used to be accepted under the old `[A-Za-z0-9-]+` charset;
    /// Ubicloud's actual rule is lowercase-only.
    #[test]
    fn has_valid_name_chars_rejects_uppercase() {
        assert!(!has_valid_name_chars("ubiworker-Foo-123"));
    }

    #[test]
    fn has_valid_name_chars_accepts_alnum_and_hyphen() {
        assert!(has_valid_name_chars("ubiworker-foo-123"));
    }

    /// Ubicloud's `ALLOWED_NAME_PATTERN` forbids leading/trailing hyphens;
    /// the old charset didn't. A bare custom name of `""` prefixed to
    /// `ubiworker-` is the sneakiest way to hit this: it *looks* like a
    /// normal prefixed name but ends in a hyphen with nothing after it.
    #[test]
    fn has_valid_name_chars_rejects_trailing_hyphen() {
        assert!(!has_valid_name_chars("ubiworker-foo-"));
        assert!(!has_valid_name_chars("ubiworker-"));
    }

    #[test]
    fn has_valid_name_chars_rejects_leading_hyphen() {
        assert!(!has_valid_name_chars("-ubiworker-foo"));
    }

    #[test]
    fn has_valid_name_chars_rejects_64_chars() {
        let name = "a".repeat(64);
        assert!(!has_valid_name_chars(&name));
    }

    #[test]
    fn has_valid_name_chars_accepts_63_chars() {
        let name = "a".repeat(63);
        assert!(has_valid_name_chars(&name));
    }

    /// The error message is user-facing guidance for picking a valid name,
    /// not just an internal assertion — it must actually describe the rule.
    #[test]
    fn normalize_worker_name_error_names_the_rule() {
        let err = normalize_worker_name("").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("lowercase"), "unexpected error: {message}");
        assert!(message.contains("hyphen"), "unexpected error: {message}");
        assert!(message.contains("63"), "unexpected error: {message}");
    }

    #[test]
    fn with_prefix_adds_prefix_when_missing() {
        assert_eq!(with_prefix("foo"), "ubiworker-foo");
    }

    #[test]
    fn with_prefix_preserves_existing_prefix() {
        assert_eq!(with_prefix("ubiworker-foo"), "ubiworker-foo");
    }

    /// A name given without the prefix should end up identical to the same
    /// name given with it — that equivalence is the point of prefixing.
    #[test]
    fn normalize_worker_name_prefix_and_no_prefix_agree() {
        assert_eq!(
            normalize_worker_name("foo").unwrap(),
            normalize_worker_name("ubiworker-foo").unwrap()
        );
    }

    #[test]
    fn normalize_worker_name_rejects_invalid_chars() {
        let err = normalize_worker_name("foo bar").unwrap_err();
        assert!(err.to_string().contains("invalid worker name"));
    }

    #[test]
    fn parse_vm_list_parses_well_formed_rows() {
        let output = "us-east-a2 ubiworker-a id-1 1.2.3.4\nus-east-a2 ubiworker-b id-2 5.6.7.8\n";
        let rows = parse_vm_list(output).unwrap();
        assert_eq!(
            rows,
            vec![
                VmRow {
                    location: "us-east-a2".to_string(),
                    name: "ubiworker-a".to_string(),
                    id: "id-1".to_string(),
                    ip4: Some("1.2.3.4".to_string()),
                },
                VmRow {
                    location: "us-east-a2".to_string(),
                    name: "ubiworker-b".to_string(),
                    id: "id-2".to_string(),
                    ip4: Some("5.6.7.8".to_string()),
                },
            ]
        );
    }

    #[test]
    fn parse_vm_list_skips_blank_lines() {
        let output = "\nus-east-a2 ubiworker-a id-1 1.2.3.4\n\n";
        let rows = parse_vm_list(output).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ubiworker-a");
    }

    /// A 3-token row is `ubi`'s rendering of an IPv4-less VM (see [`VmRow`]
    /// docs), not a malformed row — it must parse successfully with
    /// `ip4: None`, not get skipped or error out.
    #[test]
    fn parse_vm_list_three_tokens_means_missing_ip4() {
        let output = "us-east-a2 ubiworker-a id-1\n";
        let rows = parse_vm_list(output).unwrap();
        assert_eq!(
            rows,
            vec![VmRow {
                location: "us-east-a2".to_string(),
                name: "ubiworker-a".to_string(),
                id: "id-1".to_string(),
                ip4: None,
            }]
        );
    }

    /// Replaces the old "ragged rows are silently skipped" behavior: since
    /// `destroy` with no name resolves its target by counting rows from
    /// this parse, silently dropping a malformed row could make it
    /// misidentify "the sole worker" and destroy the wrong VM. Any token
    /// count other than 3 or 4 must now be a hard error instead.
    #[test]
    fn parse_vm_list_rejects_two_token_row() {
        let err = parse_vm_list("only two\n").unwrap_err();
        assert!(
            err.to_string().contains("only two"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_vm_list_rejects_five_token_row() {
        let err = parse_vm_list("us-east-a2 too many cols here\n").unwrap_err();
        assert!(
            err.to_string().contains("us-east-a2 too many cols here"),
            "unexpected error: {err}"
        );
    }

    fn vm_row(location: &str, name: &str) -> VmRow {
        VmRow {
            location: location.to_string(),
            name: name.to_string(),
            id: "id".to_string(),
            ip4: Some("1.2.3.4".to_string()),
        }
    }

    #[test]
    fn is_owned_true_for_matching_location_and_prefix() {
        assert!(is_owned(&vm_row(LOCATION, "ubiworker-a")));
    }

    /// Ownership requires *both* conditions — a matching prefix in the
    /// wrong location must not count as kd-owned.
    #[test]
    fn is_owned_false_for_other_location() {
        assert!(!is_owned(&vm_row("eu-west-h1", "ubiworker-a")));
    }

    #[test]
    fn is_owned_false_for_non_prefixed_name() {
        assert!(!is_owned(&vm_row(LOCATION, "some-other-vm")));
    }

    #[test]
    fn is_valid_auth_key_accepts_well_formed_key() {
        assert!(is_valid_auth_key("tskey-auth-xxxxx-yyyyy"));
    }

    #[test]
    fn is_valid_auth_key_rejects_prefix_only() {
        assert!(!is_valid_auth_key("tskey-auth-"));
    }

    #[test]
    fn is_valid_auth_key_rejects_missing_prefix() {
        assert!(!is_valid_auth_key("xxxxx-yyyyy"));
    }

    /// The whole point of this predicate is to keep a `'` out of a value
    /// that gets interpolated inside a single-quoted shell argument, so a
    /// key that smuggles one in after an otherwise-valid prefix must fail.
    #[test]
    fn is_valid_auth_key_rejects_embedded_quote() {
        assert!(!is_valid_auth_key("tskey-auth-xx'xx"));
    }
}
