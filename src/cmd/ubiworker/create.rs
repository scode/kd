//! `kd ubiworker create`: provision a new Ubicloud VM and enroll it into
//! the tailnet in one shot.
//!
//! The flow: fetch the two registered SSH public keys, mint a single-use
//! tailscale auth key, embed it in a first-boot shell script that installs
//! and joins tailscale, and hand everything to `ubi vm create`. No polling
//! for the VM to actually join the tailnet — this returns as soon as
//! `ubi vm create` does.

use super::tailscale;
use super::{
    BOOT_IMAGE, CreateArgs, LOCATION, SIZE, SSH_KEY_NAMES, STORAGE_SIZE_GIB, TAILSCALE_TAG,
    UNIX_USER, default_worker_name, is_valid_auth_key, normalize_worker_name, require_env,
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

pub fn run(args: CreateArgs) -> anyhow::Result<()> {
    // Fail fast, naming the specific missing variable, before doing any
    // work. UBI_TOKEN itself is read by the `ubi` binary, not by kd — we
    // only check it's set so a missing token surfaces here instead of as an
    // opaque `ubi` error later.
    require_env("UBI_TOKEN")?;
    let client_id = require_env("TS_API_CLIENT_ID")?;
    let client_secret = require_env("TS_API_CLIENT_SECRET")?;

    let sh = Shell::new()?;
    let name = match args.name {
        Some(n) => normalize_worker_name(&n)?,
        None => default_worker_name(&Zoned::now()),
    };
    info!("Creating ubiworker '{}'", name);

    info!("Fetching SSH keys ({})...", SSH_KEY_NAMES.join(", "));
    let ssh_keys = fetch_ssh_keys(&sh)?;

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
    create_vm(&sh, &name, &ssh_keys, &init_script)?;

    print_summary(&name);
    Ok(())
}

/// Run `ubi sk <name> show` for every registered key in [`SSH_KEY_NAMES`]
/// and join the results with `\n` into the single positional argument `ubi
/// vm create` expects. `ubi` accepts raw key content in that positional
/// argument, not a file path. Each element being
/// joined may itself already be a multi-line authorized_keys payload (see
/// [`parse_public_keys`]); this join is what stitches multiple registered
/// keys' payloads together, not what separates lines within one.
fn fetch_ssh_keys(sh: &Shell) -> anyhow::Result<String> {
    let mut keys = Vec::with_capacity(SSH_KEY_NAMES.len());
    for key_name in SSH_KEY_NAMES {
        let output = cmd!(sh, "ubi sk {key_name} show")
            .read()
            .with_context(|| format!("failed to fetch ssh key '{key_name}' from ubicloud"))?;
        let key = parse_public_keys(&output)
            .with_context(|| format!("could not parse public key '{key_name}' from ubi output"))?;
        keys.push(key);
    }
    Ok(keys.join("\n"))
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
/// the tailnet as `name`, authenticating with `auth_key`.
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
  tailscale up --auth-key='{auth_key}' --hostname='{name}'
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
fn create_vm(sh: &Shell, name: &str, ssh_keys: &str, init_script: &str) -> anyhow::Result<()> {
    let vm_ref = format!("{LOCATION}/{name}");
    let storage_size = STORAGE_SIZE_GIB.to_string();
    debug!("ubi vm {} create ...", vm_ref);
    cmd!(
        sh,
        "ubi vm {vm_ref} create
            --boot-image={BOOT_IMAGE}
            --size={SIZE}
            --storage-size={storage_size}
            --unix-user={UNIX_USER}
            --init-script={init_script}
            {ssh_keys}"
    )
    .quiet()
    .secret()
    .env_remove("UBI_DEBUG")
    .run()
    .context("ubi vm create failed")
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
fn render_summary(name: &str) -> String {
    [
        format!("Created ubiworker '{name}'"),
        format!("  ssh keys installed: {}", SSH_KEY_NAMES.join(", ")),
        format!("  tailscale tag: {TAILSCALE_TAG}"),
        format!("  connect: kd ubiworker ssh {name}"),
        format!("  destroy: kd ubiworker destroy {name}"),
    ]
    .join("\n")
}

/// Print the final human-readable summary to stdout (not tracing), so it
/// survives `-q` and stays easy to read at the end of a create run.
fn print_summary(name: &str) {
    println!("{}", render_summary(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_public_keys_extracts_single_key_after_marker() {
        let output = "name: laptop\npublic key:\nssh-ed25519 AAAAC3abc laptop\n";
        assert_eq!(
            parse_public_keys(output).unwrap(),
            "ssh-ed25519 AAAAC3abc laptop"
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
        let err = parse_public_keys("name: laptop\nssh-ed25519 AAAAC3abc laptop\n").unwrap_err();
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
        assert!(validate_authorized_keys_line("restrict,pty ssh-ed25519 AAAA laptop").is_ok());
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
    /// pre-`ssh`-subcommand form (a bare `ssh scode@<name>`). Either
    /// regression would compile and run fine — `create` doesn't fail — it
    /// would just quietly strand users on stale advice at the exact moment
    /// they're looking for the next command to run.
    #[test]
    fn render_summary_recommends_kd_ssh_not_raw_ssh() {
        let summary = render_summary("ubiworker-foo");
        assert!(
            summary.contains("kd ubiworker ssh ubiworker-foo"),
            "unexpected summary: {summary}"
        );
        assert!(
            !summary.contains("ssh scode@ubiworker-foo"),
            "summary should not recommend the obsolete raw ssh form: {summary}"
        );
    }
}
