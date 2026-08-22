//! `kd ubiworker create`: provision a new Ubicloud VM and enroll it into
//! the tailnet in one shot.
//!
//! The flow: fetch every SSH public key registered in the Ubicloud account,
//! mint a single-use tailscale auth key, embed it in a first-boot shell
//! script that installs and joins tailscale, and hand everything to `ubi vm
//! create`. No polling for the VM to actually join the tailnet — this
//! returns as soon as `ubi vm create` does.

use super::tailscale;
use super::{
    BOOT_IMAGE, CreateArgs, LOCATION, SIZE, STORAGE_SIZE_GIB, TAILSCALE_TAG, TS_API_CLIENT_ID,
    TS_API_CLIENT_SECRET, UBI_TOKEN, default_worker_name, is_valid_auth_key, local_unix_user,
    normalize_worker_name, require_envs,
};
use anyhow::{Context, bail};
use jiff::Zoned;
use tracing::{debug, info};
use xshell::{Shell, cmd};

/// Marker line `ubi sk <name> show` prints immediately before the SSH key
/// payload. Parsing anchors on this exact line rather than, say, always
/// taking the last line of output, because `ubi`'s output includes other
/// metadata around it that isn't a contract kd controls.
const PUBLIC_KEY_MARKER: &str = "public key:";

/// Where the tailnet policy file is edited, for the create summary's
/// "go add the ssh rule" pointer. A console URL is the most precise
/// instruction kd can give, but it is also the kind of detail Tailscale can
/// move without notice — hence the "may be stale" wording next to its use.
const POLICY_EDITOR_URL: &str = "https://login.tailscale.com/admin/acls/file";

pub fn run(args: CreateArgs) -> anyhow::Result<()> {
    // Fail fast, before doing any work, with every missing variable and
    // how to obtain it in one error (see the env preflight section in
    // `mod.rs`). UBI_TOKEN itself is read by the `ubi` binary, not by kd —
    // we only check it's set so a missing token surfaces here instead of
    // as an opaque `ubi` error later.
    let creds = require_envs(&[UBI_TOKEN, TS_API_CLIENT_ID, TS_API_CLIENT_SECRET])?;
    let [_, client_id, client_secret]: [String; 3] = creds
        .try_into()
        .expect("require_envs returns one value per requested variable");

    let sh = Shell::new()?;
    let name = match args.name {
        Some(n) => normalize_worker_name(&n)?,
        None => default_worker_name(&Zoned::now()),
    };
    info!("Creating ubiworker '{}'", name);

    // Resolved up front, before any billable or secret-minting step, so a
    // bad local username fails before a VM exists (see `local_unix_user`).
    let unix_user = local_unix_user(&sh)?;

    info!("Fetching SSH keys from Ubicloud...");
    let (ssh_keys, ssh_key_names) = fetch_ssh_keys(&sh)?;

    info!("Minting tailscale auth key...");
    // One shared, timeout-configured agent for both Tailscale calls (see
    // tailscale::build_agent's docs).
    let agent = tailscale::build_agent();
    let access_token = tailscale::exchange_access_token(&agent, &client_id, &client_secret)?;
    let auth_key = tailscale::mint_auth_key(&agent, &access_token, &name)?;

    let init_script = render_init_script(&auth_key, &name)?;
    // Never log `init_script` verbatim: it embeds the minted auth key
    // (see module docs on the "never log the secret" rule). Redact it
    // before this hits `debug!`, which -v/-vv can turn on.
    debug!("init script:\n{}", redact_auth_key(&init_script, &auth_key));

    info!("Creating VM {}/{}...", LOCATION, name);
    create_vm(&sh, &name, &unix_user, &ssh_keys, &init_script)?;

    // Best-effort and last: the login only feeds the printed policy
    // suggestion, so a machine where it can't be resolved still gets its
    // worker (and a placeholder in the rule).
    let tailscale_login = tailscale::local_login(&sh);
    print_summary(
        &name,
        &unix_user,
        &ssh_key_names,
        tailscale_login.as_deref(),
    );
    Ok(())
}

/// One row of `ubi sk list -N` output: a registered SSH key's Ubicloud id
/// and its display name.
///
/// Mirrors [`super::VmRow`]/[`parse_vm_list`](super::parse_vm_list): same
/// server-side `format_rows(%i[id name], ...)` rendering, just a different
/// pair of columns. Kept local to `create.rs` rather than promoted to
/// `mod.rs` alongside `VmRow` because nothing else in the codebase needs to
/// list Ubicloud SSH keys.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SshKeyRow {
    id: String,
    name: String,
}

/// Parse `ubi sk list -N` output into rows.
///
/// `-N` suppresses the header, so every non-blank line is `<id> <name>`,
/// whitespace-separated. Ubicloud key names can't contain whitespace (the
/// server rejects one that does at registration time), so a plain
/// `split_whitespace` never misparses a legitimate name — there is no name
/// containing a space to confuse with the id column. Blank lines are
/// skipped; any other token count is a hard error naming the offending
/// line, the same fail-closed stance as [`parse_vm_list`](super::parse_vm_list):
/// `fetch_ssh_keys` refuses to install a worker's keys at all rather than
/// silently working from a listing it couldn't fully parse.
fn parse_ssh_key_list(output: &str) -> anyhow::Result<Vec<SshKeyRow>> {
    let mut rows = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            [id, name] => rows.push(SshKeyRow {
                id: id.to_string(),
                name: name.to_string(),
            }),
            _ => bail!("unexpected 'ubi sk list' row (expected 2 columns: id, name): '{line}'"),
        }
    }
    Ok(rows)
}

/// List every SSH key registered in the Ubicloud account (`ubi sk list
/// -N`), fetch each one's authorized_keys payload (`ubi sk <id> show`), and
/// join the payloads with `\n` into the single positional argument `ubi vm
/// create` expects. `ubi` accepts raw key content in that positional
/// argument, not a file path. Each element being joined may itself already
/// be a multi-line authorized_keys payload (see [`parse_public_keys`]);
/// this join is what stitches multiple registered keys' payloads together,
/// not what separates lines within one.
///
/// Fetches by id, not name: `ubi sk (sk-id | sk-name) show` accepts either,
/// but the id is what `ubi sk list` itself hands back as the unambiguous
/// identifier for a row, so there's no reason to round-trip through a name
/// that (unlike the id) isn't guaranteed unique.
///
/// Returns the joined key content alongside the list of key *names* that
/// were installed, purely so [`run`] can report them in the create summary
/// without re-deriving names from raw key content (which doesn't carry
/// Ubicloud's own key name at all — see [`parse_public_keys`]).
///
/// Every worker used to get a fixed set of hard-coded key names (one
/// person's own keys, which made kd unusable for anyone else); this installs
/// *every* key currently registered in the account instead, so the set of
/// keys that can log in follows Ubicloud's key registry rather than a
/// constant baked into kd.
///
/// Zero registered keys is therefore a hard error, checked before a single
/// key is fetched: a worker provisioned with no authorized_keys entry at
/// all would be unreachable over plain ssh, and this preflight catches that
/// before the Tailscale key mint or VM create that follow in [`run`] spend
/// anything.
fn fetch_ssh_keys(sh: &Shell) -> anyhow::Result<(String, Vec<String>)> {
    let list_output = cmd!(sh, "ubi sk list -N")
        .read()
        .context("failed to list ubicloud ssh keys")?;
    let rows = parse_ssh_key_list(&list_output)?;
    if rows.is_empty() {
        bail!(
            "no SSH keys registered in Ubicloud; register at least one first (e.g. `ubi sk \
             create`) — a worker with none would be unreachable over plain ssh"
        );
    }

    let mut keys = Vec::with_capacity(rows.len());
    let mut names = Vec::with_capacity(rows.len());
    for row in &rows {
        let id = &row.id;
        let output = cmd!(sh, "ubi sk {id} show").read().with_context(|| {
            format!(
                "failed to fetch ssh key '{}' (id {id}) from ubicloud",
                row.name
            )
        })?;
        let key = parse_public_keys(&output).with_context(|| {
            format!(
                "could not parse public key '{}' (id {id}) from ubi output",
                row.name
            )
        })?;
        keys.push(key);
        names.push(row.name.clone());
    }
    Ok((keys.join("\n"), names))
}

/// Extract the full authorized_keys payload from `ubi sk ... show` output:
/// every line from immediately after the [`PUBLIC_KEY_MARKER`] line to the
/// end of output.
///
/// An Ubicloud "SSH key" is not one key line — it's a whole stored
/// authorized_keys value. `ubi sk show` prints that value verbatim after
/// the marker, and Ubicloud's own registration validation
/// (`VALID_SSH_AUTHORIZED_KEYS`) allows multiple key lines, blank lines, and
/// `#`-comment lines within it. Taking only the one line right after the
/// marker (the old behavior) silently truncated any payload with more than
/// one key line. Blank and comment lines are dropped; every surviving line
/// is validated (see [`validate_authorized_keys_line`]); the survivors are
/// rejoined with `\n`, preserving their original order.
fn parse_public_keys(output: &str) -> anyhow::Result<String> {
    let Some(marker_pos) = output
        .lines()
        .position(|line| line.trim() == PUBLIC_KEY_MARKER)
    else {
        bail!("could not find a '{PUBLIC_KEY_MARKER}' line in ubi output");
    };

    let lines: Vec<&str> = output
        .lines()
        .skip(marker_pos + 1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    if lines.is_empty() {
        bail!("no authorized_keys lines found after the '{PUBLIC_KEY_MARKER}' marker");
    }
    for line in &lines {
        validate_authorized_keys_line(line)?;
    }

    Ok(lines.join("\n"))
}

/// Sanity-check one authorized_keys line pulled from the `public key:`
/// block: it must contain a whitespace-delimited token starting with
/// `ssh-`, `ecdsa-`, or `sk-` — the OpenSSH, ECDSA, and security-key-backed
/// key type prefixes. A real authorized_keys line may carry leading
/// *options* before the key type (e.g. `restrict,pty ssh-ed25519 AAAA...`),
/// so the check looks for the prefix on any token, not just the first one.
///
/// This is a sanity check that [`parse_public_keys`] sliced out the right
/// region of `ubi sk show`'s output — not a security gate: Ubicloud already
/// validated the full authorized_keys grammar
/// (`VALID_SSH_AUTHORIZED_KEYS`) when the key was registered. Its job here
/// is to fail loudly if `ubi`'s output changed shape or is garbage, rather
/// than silently bake something unexpected into a VM's authorized_keys.
fn validate_authorized_keys_line(line: &str) -> anyhow::Result<()> {
    const VALID_PREFIXES: [&str; 3] = ["ssh-", "ecdsa-", "sk-"];
    let looks_like_a_key = line.split_whitespace().any(|token| {
        VALID_PREFIXES
            .iter()
            .any(|prefix| token.starts_with(prefix))
    });
    if looks_like_a_key {
        Ok(())
    } else {
        bail!("line does not look like an authorized_keys entry: '{line}'");
    }
}

/// Render the first-boot shell script that installs tailscale and joins
/// the tailnet as `name`, authenticating with `auth_key`, with Tailscale
/// SSH enabled.
///
/// `--ssh` on `tailscale up` is what makes `tailscale ssh <name>` (and so
/// `kd ubiworker ssh`) work at all: a node that merely joins the tailnet is
/// not a Tailscale SSH server, and its Tailscale-managed host key is only
/// generated and published to the coordination server once SSH is on.
/// That published key is the whole point — it's how the client verifies
/// the worker's identity without known-hosts pinning, which can't work for
/// a fleet whose names are reused and whose OpenSSH host keys change on
/// every recreation. The Ubicloud-provisioned OpenSSH server keeps running
/// alongside, but note what that does and doesn't buy: Tailscale SSH takes
/// over port 22 on the worker's *tailnet* address, so plain `ssh` to the
/// MagicDNS name is still Tailscale SSH (tailnet policy, Tailscale host
/// key) — the system sshd and the Ubicloud-installed keys are reachable
/// only via a non-tailnet address such as the VM's public IP. Who may log
/// in over Tailscale SSH is governed by the tailnet policy's `ssh`
/// section, not by anything here (see SPEC.md).
///
/// Retries the whole install-and-join sequence up to 5 times, 30 seconds
/// apart, entirely inside the script (kd itself has already returned by the
/// time this runs on the VM — see the module docs on no tailnet-join
/// polling). Ubicloud bills a VM from the moment it boots, so a single
/// transient hiccup during first boot — a flaky `curl`, a slow DNS answer,
/// tailscaled's package momentarily unavailable — must not permanently
/// strand an already-billed VM with no working path back to it. Retrying
/// with the *same* auth key is safe specifically because the key was minted
/// non-reusable (see [`tailscale::mint_auth_key`]): Tailscale only consumes
/// a one-use key on a *successful* `tailscale up`, so every failed attempt
/// leaves it untouched for the next one.
///
/// Single-quoting `auth_key` and `name` inside the script is safe *only*
/// because both are validated to quote-free charsets before this function
/// interpolates them: `auth_key` is checked in full by [`is_valid_auth_key`]
/// (prefix `tskey-auth-` plus an `[A-Za-z0-9-]` remainder — already enforced
/// upstream by [`tailscale::parse_create_key_response`]) and `name` is
/// checked below against `[A-Za-z0-9-]+`, a permissive superset of
/// Ubicloud's actual (narrower: lowercase-only, no leading/trailing hyphen,
/// max 63 chars) VM-name rule that's already enforced upstream by
/// [`normalize_worker_name`]/[`default_worker_name`] (see
/// `super::has_valid_name_chars`). Neither charset can contain a single
/// quote, so there's no escaping to get wrong — but this function
/// re-validates defensively rather than trusting callers, since a future
/// caller forgetting an upstream check would otherwise turn into a
/// shell-injection bug silently.
fn render_init_script(auth_key: &str, name: &str) -> anyhow::Result<String> {
    if !is_valid_auth_key(auth_key) {
        bail!(
            "refusing to render init script: auth key failed validation (expected a \
             'tskey-auth-' prefix followed by a non-empty [A-Za-z0-9-] remainder)"
        );
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("refusing to render init script: worker name '{name}' has unexpected characters");
    }

    // A raw multiline literal so the script reads as the shell it is. The
    // indentation is carried into the rendered script, which `sh` doesn't
    // care about; only the format-machinery braces (`{{`/`}}`) differ from
    // what actually runs on the VM.
    Ok(format!(
        r#"#!/bin/sh
set -eu
# Cosmetic only, hence best-effort: make the OS hostname match the
# tailscale identity so shell prompts on the VM say who they are. A
# failure here must never cost us enrollment.
hostnamectl set-hostname '{name}' || true
enroll() {{
  installer=$(mktemp) &&
  curl -fsSL https://tailscale.com/install.sh -o "$installer" &&
  sh "$installer" &&
  rm -f "$installer" &&
  systemctl enable --now tailscaled &&
  tailscale up --auth-key='{auth_key}' --hostname='{name}' --ssh
}}
for attempt in 1 2 3 4 5; do
  enroll && exit 0
  sleep 30
done
exit 1
"#
    ))
}

/// Replace the literal auth key inside a rendered init script with a
/// placeholder, so the script's shape can go to `debug!` (gated behind
/// `-v`/`-vv`, but still a log a user could paste somewhere) without ever
/// putting the secret itself in a log stream. This is the only place the
/// minted auth key is allowed to touch a `tracing` call.
fn redact_auth_key(script: &str, auth_key: &str) -> String {
    script.replace(auth_key, "<redacted>")
}

/// Invoke `ubi vm <location>/<name> create ...` to provision the worker.
/// `ubi`'s own stdout (VM details) is allowed straight through so the user
/// sees the same progress they'd get running `ubi` by hand.
///
/// The command is marked `.secret()` because its argument list embeds the
/// init script, which embeds the minted auth key: without it, xshell echoes
/// the full command line to stderr on `run()` and also formats it into the
/// error message when the command fails — both would leak the secret.
/// `.quiet()` additionally suppresses the echo entirely — with `.secret()`
/// alone it would print a literal `$ <secret>` line, which is pure noise.
/// The redacted `debug!` of the init script in [`run`] is the sanctioned
/// way to see what was sent.
///
/// `.env_remove("UBI_DEBUG")` covers a second leak path `.secret()` doesn't:
/// the `ubi` binary itself logs its full argv — init script and all — when
/// `UBI_DEBUG=1` is set in *its* environment (verified in `cli/ubi.go`).
/// xshell's redaction only controls what kd prints about the command it
/// ran; it has no say over what `ubi` chooses to log internally. Clearing
/// the variable in the child's env closes that path for kd's own
/// invocation. What this does *not* cover: the auth key remains visible in
/// this process's argv to anything with local procfs access for as long as
/// `ubi` runs — an accepted residual risk on a machine the operator already
/// trusts, and one that would go away entirely if VM creation moved to
/// Ubicloud's REST API instead of shelling out to the `ubi` CLI (see
/// SPEC.md).
fn create_vm(
    sh: &Shell,
    name: &str,
    unix_user: &str,
    ssh_keys: &str,
    init_script: &str,
) -> anyhow::Result<()> {
    debug!("ubi vm {}/{} create ...", LOCATION, name);
    let args = create_vm_args(name, unix_user, ssh_keys, init_script);
    cmd!(sh, "ubi {args...}")
        .quiet()
        .secret()
        .env_remove("UBI_DEBUG")
        .run()
        .context("ubi vm create failed")
}

/// The `ubi` argv (everything after the program name) for provisioning a
/// worker — the fixed shape constants plus the per-worker values.
///
/// Split out of [`create_vm`] purely so the argument assembly is
/// unit-testable: the one value most likely to regress is `--unix-user`,
/// which used to be a hard-coded constant and now follows the operator
/// (see `local_unix_user`). A test pinning the exact `--unix-user=<user>`
/// token is what keeps the provisioned account in lockstep with the one
/// `kd ubiworker ssh` later targets.
fn create_vm_args(name: &str, unix_user: &str, ssh_keys: &str, init_script: &str) -> Vec<String> {
    vec![
        "vm".to_string(),
        format!("{LOCATION}/{name}"),
        "create".to_string(),
        format!("--boot-image={BOOT_IMAGE}"),
        format!("--size={SIZE}"),
        format!("--storage-size={STORAGE_SIZE_GIB}"),
        format!("--unix-user={unix_user}"),
        format!("--init-script={init_script}"),
        ssh_keys.to_string(),
    ]
}

/// Render the final human-readable summary for a newly created worker
/// named `name`.
///
/// Split out from [`print_summary`] specifically so the "how do you connect
/// to what you just created" line is unit-testable as a string: it's the
/// one line most likely to silently regress back to a stale form (a bare
/// `ssh user@name` invocation, from before `kd ubiworker ssh` existed) if
/// someone edits this text without noticing what it's supposed to say. That
/// kind of regression wouldn't fail any behavior test — `create` itself
/// still works — it would just quietly point every future reader at the
/// wrong command.
fn render_summary(
    name: &str,
    unix_user: &str,
    ssh_key_names: &[String],
    tailscale_login: Option<&str>,
) -> String {
    [
        format!("Created ubiworker '{name}'"),
        format!("  unix user: {unix_user}"),
        format!("  ssh keys installed: {}", ssh_key_names.join(", ")),
        format!("  tailscale tag: {TAILSCALE_TAG}"),
        format!("  connect: kd ubiworker ssh {name}"),
        format!("  destroy: kd ubiworker destroy {name}"),
        // Printed every time, not just once: the tailnet policy pieces are
        // prerequisites kd can neither install nor detect (see SPEC.md),
        // and without them `tailscale ssh` fails with a host-key or
        // permission error that says nothing about policy. Repeating the
        // exact rule here puts the fix next to the command that needs it.
        // Rendered as a single rule *object* to append to the policy's
        // existing `ssh` array — never as a whole `"ssh": [...]` property,
        // which would duplicate or clobber the rules already there.
        format!(
            "  requires, in the tailnet policy file:\n    \
             1. an ssh rule letting you log in to {TAILSCALE_TAG} as '{unix_user}' — append this \
             object to the existing \"ssh\" array (create the array if there is none):\n       {}\n    \
             2. an acl/grant that lets you reach {TAILSCALE_TAG} on port 22\n  \
             edit the policy file at {POLICY_EDITOR_URL}\n  \
             (admin console -> Access controls -> Edit file; UI path may be stale)",
            render_ssh_policy_rule(unix_user, tailscale_login)
        ),
    ]
    .join("\n")
}

/// Placeholder `src` for the printed policy rule when the operator's own
/// Tailscale login couldn't be resolved (see [`tailscale::local_login`]).
/// Deliberately unmistakable and unpasteable — the alternative of
/// defaulting to `autogroup:member` would quietly grant every tailnet
/// member a shell as the operator on every worker.
const LOGIN_PLACEHOLDER: &str = "<your-tailscale-login>";

/// The tailnet policy `ssh` rule object a worker needs before `kd ubiworker
/// ssh` can log in, rendered as a HuJSON object for `unix_user`.
///
/// `src` is the operator's own Tailscale login when known, so the rule
/// grants exactly the person who just created the worker and nobody else.
/// kd never suggests `autogroup:member`: on a shared tailnet that selector
/// is every member (including invited users on shared destinations), and a
/// copy-pasted security example tends to become production policy
/// unchanged. Operators who genuinely want tailnet-wide access can widen
/// it themselves. Kept separate from [`render_summary`] so the snippet is
/// unit-testable as the one line most likely to drift from what Tailscale
/// actually accepts.
fn render_ssh_policy_rule(unix_user: &str, tailscale_login: Option<&str>) -> String {
    let src = tailscale_login.unwrap_or(LOGIN_PLACEHOLDER);
    format!(
        r#"{{ "action": "accept", "src": ["{src}"], "dst": ["{TAILSCALE_TAG}"], "users": ["{unix_user}"] }}"#
    )
}

/// Print the final human-readable summary to stdout (not tracing), so it
/// survives `-q` and stays easy to read at the end of a create run.
fn print_summary(
    name: &str,
    unix_user: &str,
    ssh_key_names: &[String],
    tailscale_login: Option<&str>,
) {
    println!(
        "{}",
        render_summary(name, unix_user, ssh_key_names, tailscale_login)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_ssh_key_list ──────────────────────────────────────────────
    // `fetch_ssh_keys` uses this parse to decide *which* keys to fetch and
    // install, so a misparse here would silently change who can log in to
    // every future worker (installing too few keys, or erroring outright) —
    // these tests pin the same fail-closed contract as `parse_vm_list`.

    #[test]
    fn parse_ssh_key_list_parses_well_formed_rows() {
        let output = "sk1 work-key\nsk2 phone\n";
        assert_eq!(
            parse_ssh_key_list(output).unwrap(),
            vec![
                SshKeyRow {
                    id: "sk1".to_string(),
                    name: "work-key".to_string(),
                },
                SshKeyRow {
                    id: "sk2".to_string(),
                    name: "phone".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_ssh_key_list_skips_blank_lines() {
        let output = "\nsk1 work-key\n\n";
        let rows = parse_ssh_key_list(output).unwrap();
        assert_eq!(
            rows,
            vec![SshKeyRow {
                id: "sk1".to_string(),
                name: "work-key".to_string()
            }]
        );
    }

    /// A row that isn't exactly `<id> <name>` must be a hard error, not
    /// silently dropped or truncated: `fetch_ssh_keys` treats the parsed
    /// row count as "how many keys exist," and installing fewer keys than
    /// are actually registered (because a malformed row was ignored) would
    /// be a silent access regression nobody asked for.
    #[test]
    fn parse_ssh_key_list_rejects_malformed_row() {
        let err = parse_ssh_key_list("only-one-token\n").unwrap_err();
        assert!(
            err.to_string().contains("only-one-token"),
            "unexpected error: {err}"
        );
        let err = parse_ssh_key_list("sk1 name extra\n").unwrap_err();
        assert!(
            err.to_string().contains("sk1 name extra"),
            "unexpected error: {err}"
        );
    }

    /// Empty `ubi sk list -N` output (no keys registered at all) must parse
    /// to an empty vec rather than erroring — it's [`fetch_ssh_keys`], not
    /// the parser, that turns "zero keys" into a hard error, so the parser
    /// itself must stay a faithful, judgment-free translation of the rows
    /// it's given.
    #[test]
    fn parse_ssh_key_list_empty_output_yields_empty_vec() {
        assert_eq!(parse_ssh_key_list("").unwrap(), Vec::new());
        assert_eq!(parse_ssh_key_list("\n\n").unwrap(), Vec::new());
    }

    #[test]
    fn parse_public_keys_extracts_single_key_after_marker() {
        let output = "name: mykey\npublic key:\nssh-ed25519 AAAAC3abc mykey\n";
        assert_eq!(
            parse_public_keys(output).unwrap(),
            "ssh-ed25519 AAAAC3abc mykey"
        );
    }

    /// The whole reason this function exists rather than taking just the
    /// line right after the marker: a stored Ubicloud "SSH key" can be a
    /// multi-line authorized_keys payload, and every line must survive in
    /// its original order.
    #[test]
    fn parse_public_keys_preserves_multiple_lines_in_order() {
        let output = "public key:\nssh-ed25519 AAAA1 one\nssh-ed25519 AAAA2 two\n";
        assert_eq!(
            parse_public_keys(output).unwrap(),
            "ssh-ed25519 AAAA1 one\nssh-ed25519 AAAA2 two"
        );
    }

    #[test]
    fn parse_public_keys_drops_blank_and_comment_lines() {
        let output = "public key:\nssh-ed25519 AAAA1 one\n\n# a comment\nssh-ed25519 AAAA2 two\n";
        assert_eq!(
            parse_public_keys(output).unwrap(),
            "ssh-ed25519 AAAA1 one\nssh-ed25519 AAAA2 two"
        );
    }

    #[test]
    fn parse_public_keys_errors_when_marker_missing() {
        let err = parse_public_keys("name: mykey\nssh-ed25519 AAAAC3abc mykey\n").unwrap_err();
        assert!(
            err.to_string().contains("public key:"),
            "unexpected error: {err}"
        );
    }

    /// If every line after the marker is blank/comment, there is no key
    /// content at all — this must be a hard error, not an empty string
    /// silently handed to `ubi vm create`.
    #[test]
    fn parse_public_keys_errors_when_block_is_empty_after_marker() {
        let output = "public key:\n\n# nothing but a comment\n";
        assert!(parse_public_keys(output).is_err());
    }

    /// A line that follows the marker but doesn't look like any known SSH
    /// key type must be rejected here, not silently passed to `ubi vm
    /// create` where it would fail confusingly (or, worse, succeed with
    /// broken auth).
    #[test]
    fn parse_public_keys_rejects_garbage_key() {
        let output = "public key:\nnot a real key\n";
        assert!(parse_public_keys(output).is_err());
    }

    #[test]
    fn validate_authorized_keys_line_accepts_known_prefixes() {
        assert!(validate_authorized_keys_line("ssh-ed25519 AAAA").is_ok());
        assert!(validate_authorized_keys_line("ecdsa-sha2-nistp256 AAAA").is_ok());
        assert!(validate_authorized_keys_line("sk-ssh-ed25519@openssh.com AAAA").is_ok());
    }

    /// authorized_keys lines may carry leading options before the key
    /// type; FX8's fix is exactly this case — the old byte-zero prefix
    /// check would have rejected it.
    #[test]
    fn validate_authorized_keys_line_accepts_option_prefixed_line() {
        assert!(validate_authorized_keys_line("restrict,pty ssh-ed25519 AAAA mykey").is_ok());
    }

    #[test]
    fn validate_authorized_keys_line_rejects_garbage() {
        assert!(validate_authorized_keys_line("not a real key").is_err());
    }

    #[test]
    fn render_init_script_embeds_key_and_hostname_single_quoted() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo").unwrap();
        assert!(script.contains("--auth-key='tskey-auth-xxxxx-yyyyy'"));
        assert!(script.contains("--hostname='ubiworker-foo'"));
        // The OS hostname is cosmetic and must stay best-effort (`|| true`):
        // a hostnamectl failure must never cost the VM its enrollment.
        assert!(script.contains("hostnamectl set-hostname 'ubiworker-foo' || true"));
    }

    /// `--ssh` must stay on the `tailscale up` line: without it the worker
    /// joins the tailnet but never becomes a Tailscale SSH server, so
    /// `tailscale ssh` (and hence `kd ubiworker ssh`) fails with a missing
    /// host-key error that looks nothing like "SSH isn't enabled". Nothing
    /// else in kd would catch its loss — `create` succeeds, the VM boots,
    /// and only the first connection attempt reveals it.
    #[test]
    fn render_init_script_enables_tailscale_ssh_on_enrollment() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo").unwrap();
        let up_line = script
            .lines()
            .find(|line| line.trim_start().starts_with("tailscale up "))
            .expect("script has a `tailscale up` line");
        let tokens: Vec<&str> = up_line.split_whitespace().collect();
        assert!(
            tokens.iter().any(|t| t.starts_with("--auth-key=")),
            "unexpected line: {up_line}"
        );
        assert!(
            tokens.iter().any(|t| t.starts_with("--hostname=")),
            "unexpected line: {up_line}"
        );
        // Exact-token match, not a substring: `--ssh=false` would also
        // contain " --ssh" and is precisely the regression this guards.
        assert!(tokens.contains(&"--ssh"), "unexpected line: {up_line}");
    }

    /// FX17: the enrollment sequence must be wrapped in a bounded retry
    /// loop rather than run once, so a transient first-boot failure doesn't
    /// permanently strand an already-billed VM.
    #[test]
    fn render_init_script_retries_the_enroll_sequence() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo").unwrap();
        assert!(script.contains("for attempt in 1 2 3 4 5"));
        assert!(script.contains("enroll() {"));
        assert!(script.contains("enroll && exit 0"));
    }

    #[test]
    fn render_init_script_rejects_auth_key_without_prefix() {
        assert!(render_init_script("not-a-key", "ubiworker-foo").is_err());
    }

    /// Regression coverage for FX6: `render_init_script` used to only check
    /// the `tskey-auth-` prefix, which would have let a key containing a
    /// `'` or a newline through to unescaped single-quoted interpolation.
    #[test]
    fn render_init_script_rejects_auth_key_with_embedded_quote() {
        assert!(render_init_script("tskey-auth-xx'xx", "ubiworker-foo").is_err());
    }

    #[test]
    fn render_init_script_rejects_auth_key_with_embedded_newline() {
        assert!(render_init_script("tskey-auth-xx\nxx", "ubiworker-foo").is_err());
    }

    #[test]
    fn render_init_script_rejects_auth_key_that_is_only_the_prefix() {
        assert!(render_init_script("tskey-auth-", "ubiworker-foo").is_err());
    }

    #[test]
    fn render_init_script_rejects_name_with_unexpected_chars() {
        assert!(render_init_script("tskey-auth-xxxxx", "ubiworker-foo bar").is_err());
    }

    /// Guards the one thing standing between `debug!`-level logging and
    /// leaking the minted auth key: the secret must not survive redaction
    /// anywhere in the script, and the surrounding shape must be unchanged.
    #[test]
    fn redact_auth_key_removes_secret_but_keeps_script_shape() {
        let auth_key = "tskey-auth-xxxxx-yyyyy";
        let script = render_init_script(auth_key, "ubiworker-foo").unwrap();
        let redacted = redact_auth_key(&script, auth_key);

        assert!(!redacted.contains(auth_key));
        assert!(redacted.contains("--auth-key='<redacted>'"));
        assert!(redacted.contains("--hostname='ubiworker-foo'"));
    }

    /// Pins the summary's connect instruction to the current `kd ubiworker
    /// ssh <name>` form and guards against it silently reverting to the
    /// pre-`ssh`-subcommand form (a bare `ssh alice@<name>`). Either
    /// regression would compile and run fine — `create` doesn't fail — it
    /// would just quietly strand users on stale advice at the exact moment
    /// they're looking for the next command to run.
    #[test]
    fn render_summary_recommends_kd_ssh_not_raw_ssh() {
        let summary = render_summary("ubiworker-foo", "alice", &[], None);
        assert!(
            summary.contains("kd ubiworker ssh ubiworker-foo"),
            "unexpected summary: {summary}"
        );
        assert!(
            !summary.contains("ssh alice@ubiworker-foo"),
            "summary should not recommend the obsolete raw ssh form: {summary}"
        );
    }

    /// The summary's key listing must reflect whichever keys `fetch_ssh_keys`
    /// actually installed, not a hardcoded set: this is the regression test
    /// for the "install every registered key" change, since the old
    /// implementation printed a constant here regardless of what was
    /// installed.
    #[test]
    fn render_summary_lists_the_installed_ssh_key_names() {
        let names = vec!["work-key".to_string(), "phone".to_string()];
        let summary = render_summary("ubiworker-foo", "alice", &names, None);
        assert!(
            summary.contains("ssh keys installed: work-key, phone"),
            "unexpected summary: {summary}"
        );
    }

    /// The summary must carry the tailnet SSH policy rule, rendered for the
    /// actual provisioned user and tag: it is the only place kd tells the
    /// operator about the one prerequisite it can't set up or detect, so a
    /// summary that dropped it (or rendered it for a stale hard-coded user)
    /// would leave the first `kd ubiworker ssh` failing with no pointer to
    /// the fix.
    #[test]
    fn render_summary_includes_policy_rule_for_the_provisioned_user() {
        let summary = render_summary("ubiworker-foo", "alice", &[], Some("alice@github"));
        // Independently written expectation — not derived from
        // `render_ssh_policy_rule` — so a wrong action, widened src, or
        // broken syntax in the renderer can't leak into the oracle.
        let expected = r#"{ "action": "accept", "src": ["alice@github"], "dst": ["tag:ubicloud"], "users": ["alice"] }"#;
        assert!(summary.contains(expected), "unexpected summary: {summary}");
        // It must be a rule object to append, never a whole `"ssh":`
        // property that would duplicate/clobber the policy's existing one.
        assert!(
            !summary.contains(r#""ssh":"#),
            "summary must not render a top-level ssh property: {summary}"
        );
        assert!(
            summary.contains("port 22"),
            "summary must mention the separate port-22 acl/grant prerequisite: {summary}"
        );
    }

    /// The rule's `src` must never default to a tailnet-wide selector: when
    /// the operator's login is unknown, the output must be an obvious
    /// placeholder the reader has to replace, not `autogroup:member`.
    #[test]
    fn render_ssh_policy_rule_never_defaults_to_autogroup_member() {
        let rule = render_ssh_policy_rule("alice", None);
        assert_eq!(
            rule,
            r#"{ "action": "accept", "src": ["<your-tailscale-login>"], "dst": ["tag:ubicloud"], "users": ["alice"] }"#
        );
        assert!(!rule.contains("autogroup"), "unexpected rule: {rule}");
    }

    /// Pins that the operator-derived username is what actually reaches
    /// `ubi` as `--unix-user=`, as an exact token, and that it's the *only*
    /// `--unix-user=` token present. Restoring the old hard-coded account —
    /// whether alongside or instead of the operator-derived one — or
    /// dropping the flag, must fail here even though every summary/ssh test
    /// would still pass.
    #[test]
    fn create_vm_args_pass_the_unix_user_and_fixed_shape() {
        let args = create_vm_args("ubiworker-foo", "alice", "ssh-ed25519 AAAA", "#!/bin/sh\n");
        let unix_user_args: Vec<&str> = args
            .iter()
            .filter(|a| a.starts_with("--unix-user="))
            .map(String::as_str)
            .collect();
        assert_eq!(
            unix_user_args,
            vec!["--unix-user=alice"],
            "expected exactly one --unix-user= token, matching the passed-in user: {args:?}"
        );
        assert_eq!(args[0], "vm");
        assert_eq!(args[1], "us-east-a2/ubiworker-foo");
        assert_eq!(args[2], "create");
        assert!(args.contains(&"--init-script=#!/bin/sh\n".to_string()));
        assert_eq!(args.last().unwrap(), "ssh-ed25519 AAAA");
    }
}
