//! The first-boot script every new worker runs as root: join the tailnet with
//! Tailscale SSH, then bootstrap apt packages in the background.
//!
//! Each worker command delivers the script its own way. `kd ubiworker
//! create` hands it to `ubi vm create` as the VM's init script, and `kd
//! boatworker create` puts it behind a sudo prelude in a setup file for
//! `boat new`. The script itself is the same for both, so the ordering and
//! quoting arguments in [`render_init_script`] hold for every worker.

use super::tailscale::is_valid_auth_key;
use anyhow::{Context, bail};
use std::collections::HashSet;

/// Apt packages installed on every worker, in addition to whatever the
/// operator requests via `--pkg` (see [`bootstrap_packages`]). Shared by
/// both fleets, like the rest of the script. Deliberately empty for now —
/// there's no package kd itself needs on every worker unconditionally.
pub(crate) const BASE_PACKAGES: &[&str] = &[];

/// Validate `name` against Debian's package-name syntax (Debian Policy
/// §5.6.7), plus one extra restriction apt itself imposes: non-empty, at
/// least two characters, the first character `[a-z0-9]`, every remaining
/// character `[a-z0-9+.-]`, and the name must not *end* in `-`.
///
/// The charset (no shell metacharacters, whitespace, or quotes) is what
/// makes a validated name safe to interpolate unquoted into the single-quoted
/// `sh -c` body of the bootstrap script — that's a *shell*-level safety
/// property only. It says nothing about apt-level safety, which is why the
/// trailing-`-` rejection exists on top of it: `apt-get install foo-` means
/// "remove foo", a real apt convention this charset alone doesn't rule out.
/// (A trailing `+` is fine to allow — `g++` is a real package name, and
/// `foo+` just means "install foo", same as no suffix at all.) The other
/// half of apt-level safety — that a name containing `.` can't be treated as
/// a regex and match unintended packages — comes not from this function but
/// from [`render_init_script`] using `apt-get satisfy` instead of `install`
/// (see its docs).
///
/// Called twice: once in each worker command's `create::run`, on the
/// combined `BASE_PACKAGES` + `--pkg` list, before any billable or
/// secret-minting step, so a bad name fails the whole run immediately;
/// and again, defensively, inside [`render_init_script`] — mirroring how
/// that function re-validates `auth_key` and `name` rather than trusting a
/// caller to have checked already.
pub(crate) fn validate_package_name(name: &str) -> anyhow::Result<()> {
    // Operates on bytes, not `chars`: the contract above is ASCII-only, and
    // byte indexing/length is what actually enforces that, without the
    // false generality `chars()` would suggest for a value that can never
    // legally contain a multi-byte character.
    let bytes = name.as_bytes();
    let first_ok = matches!(bytes.first(), Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit());
    // `get(1..)` rather than `[1..]`: safe even when `bytes` is shorter than
    // 1 byte (the empty-name case), where a direct slice index would panic.
    let rest_ok =
        bytes.get(1..).unwrap_or(&[]).iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'.' | b'-')
        });
    let trailing_hyphen = bytes.last() == Some(&b'-');
    if bytes.len() < 2 || !first_ok || !rest_ok || trailing_hyphen {
        bail!(
            "invalid package name '{name}': expected Debian package-name syntax (at least 2 \
             characters, starting with a lowercase letter or digit, the rest lowercase \
             letters, digits, '+', '-', or '.', and not ending in '-' — apt reads a trailing \
             '-' as \"remove this package\")"
        );
    }
    Ok(())
}

/// Assemble the final package list handed to the bootstrap unit's `apt-get
/// satisfy` line: `base` followed by the operator's `--pkg` values (`extra`),
/// deduplicated while preserving first-occurrence order.
///
/// Takes `base` as a parameter — `create::run` passes [`BASE_PACKAGES`] —
/// instead of reaching for that constant directly, so the base-first +
/// overlap-dedup behavior is unit-testable against an arbitrary base set
/// without waiting for `BASE_PACKAGES` to stop being empty. Dedup happens over borrowed
/// `&str`s (a `HashSet<&str>`), with the one-time conversion to owned
/// `String` only at the end, on the already-deduplicated survivors — cheaper
/// than cloning every candidate up front just to maybe throw the clone away.
///
/// Kept as a small pure function, separate from [`render_init_script`], so
/// this dedup/ordering behavior is unit-testable on its own — it's the part
/// most likely to regress silently, since a *duplicate* entry in the
/// rendered `apt-get satisfy` line is harmless (apt tolerates repeats) but a
/// *dropped* one is a package the operator asked for and silently didn't
/// get.
pub(crate) fn bootstrap_packages(base: &[&str], extra: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    base.iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .filter(|pkg| seen.insert(*pkg))
        .map(str::to_string)
        .collect()
}

/// Render the first-boot shell script that installs tailscale and joins
/// the tailnet as `name`, authenticating with `auth_key`, with Tailscale
/// SSH enabled — and, once that succeeds, bootstraps `packages` via apt.
///
/// The script assumes it runs as root. Ubicloud runs its init script that
/// way; boat runs setup files as an unprivileged user, so `kd boatworker
/// create` re-runs this script under sudo rather than changing it. The
/// remarks below about the Ubicloud-provisioned OpenSSH server and billing
/// from boot are ubiworker's; the ordering and quoting arguments hold for
/// both fleets.
///
/// # Package bootstrap
///
/// `apt-get dist-upgrade` plus `packages` (if any) runs detached in a
/// transient systemd unit (`systemd-run --unit=kd-bootstrap`), launched
/// only *after* enrollment succeeds and never awaited by the script or by
/// `create`. The ordering is load-bearing: tailscale's installer
/// (`tailscale.com/install.sh`) runs its own `apt-get install` with no
/// dpkg-lock timeout, so a bootstrap started earlier — whose apt calls wait
/// up to 600s for the lock and can legitimately hold it for minutes during
/// a dist-upgrade — would make the installer fail instantly and could burn
/// through enrollment's whole 5×30s retry budget. Launched after, apt can
/// neither delay nor fail the one thing that makes the VM reachable.
///
/// Inside the unit, `-o DPkg::Lock::Timeout=600` waits out the lock that
/// `unattended-upgrades` commonly holds shortly after a fresh Ubuntu boot;
/// the 10-attempt, 30-second retry loop covers mirror flakes. `apt-get
/// satisfy` rather than `install` is deliberate — see
/// [`validate_package_name`] for the apt-level hazards (`name-` meaning
/// remove, `.` triggering a regex match) that `install` would expose with
/// operator-supplied names. Progress: `systemctl status kd-bootstrap` and
/// `/var/log/kd-bootstrap.log`; success marker: `/var/lib/kd/bootstrap-done`.
/// A dist-upgrade that installs a new kernel does not reboot the VM.
///
/// The `bootstrap || true` call site keeps a launch failure (`set -eu` is
/// in effect) from turning an already-enrolled worker into a script
/// failure. The same guard swallows the "Unit kd-bootstrap.service already
/// exists" a *manual* rerun of the script would get; the init script itself
/// runs once per instance, and `systemctl reset-failed kd-bootstrap` clears
/// the stale unit before a rerun.
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
/// time this runs on the VM; neither worker command waits for the tailnet
/// join). Ubicloud bills a VM from the moment it boots, so a single
/// transient hiccup during first boot — a flaky `curl`, a slow DNS answer,
/// tailscaled's package momentarily unavailable — must not permanently
/// strand an already-billed VM with no working path back to it. Retrying
/// with the *same* auth key is safe specifically because the key was minted
/// non-reusable (see [`mint_auth_key`]): Tailscale only consumes a one-use
/// key on a *successful* `tailscale up`, so every failed attempt
/// leaves it untouched for the next one.
///
/// Single-quoting `auth_key` and `name` inside the script is safe *only*
/// because both are validated to quote-free charsets before this function
/// interpolates them: `auth_key` is checked in full by [`is_valid_auth_key`]
/// (prefix `tskey-auth-` plus an `[A-Za-z0-9-]` remainder — already enforced
/// upstream by `tailscale::parse_create_key_response`) and `name` is
/// checked below against `[A-Za-z0-9-]+`, a permissive superset of the
/// narrower rule each worker command already enforces on new names
/// (lowercase-only, no leading/trailing hyphen, max 63 chars; see each
/// command's `has_valid_name_chars`). Neither charset can contain a single
/// quote, so there's no escaping to get wrong — but this function
/// re-validates defensively rather than trusting callers, since a future
/// caller forgetting an upstream check would otherwise turn into a
/// shell-injection bug silently. `packages` gets the same defensive
/// treatment via [`validate_package_name`]: Debian package-name syntax
/// likewise contains no `'`, so every entry is safe to space-join and
/// interpolate unquoted into the `apt-get satisfy` line — though, as that
/// function's docs explain, the shell-safe charset alone is not what makes
/// `satisfy` safe against apt's own regex/removal quirks; `satisfy`'s exact
/// dependency-string syntax is.
///
/// [`mint_auth_key`]: super::tailscale::mint_auth_key
pub(crate) fn render_init_script(
    auth_key: &str,
    name: &str,
    packages: &[String],
) -> anyhow::Result<String> {
    if !is_valid_auth_key(auth_key) {
        bail!(
            "refusing to render init script: auth key failed validation (expected a \
             'tskey-auth-' prefix followed by a non-empty [A-Za-z0-9-] remainder)"
        );
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("refusing to render init script: worker name '{name}' has unexpected characters");
    }
    for pkg in packages {
        validate_package_name(pkg)
            .context("refusing to render init script: invalid package name")?;
    }

    // Omit the whole `apt-get satisfy` line when there's nothing to install:
    // unlike `install`, `satisfy` with zero arguments is an error (observed
    // on apt 2.8.3), so an unconditional line would break the no-`--pkg`
    // case outright. `update`/`dist-upgrade` still run either way.
    let install_line = if packages.is_empty() {
        String::new()
    } else {
        format!(
            "      apt-get -o DPkg::Lock::Timeout=600 -y satisfy {} &&\n",
            packages.join(" ")
        )
    };

    // A raw multiline literal so the script reads as the shell it is. The
    // indentation is carried into the rendered script, which `sh` doesn't
    // care about; only the format-machinery braces (`{{`/`}}`) differ from
    // what actually runs on the VM. Both `enroll` and `bootstrap` are shell
    // functions, so both need that escaping around their bodies.
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
# Package bootstrap: detached transient systemd unit, launched only once
# enrollment has succeeded. Launched earlier it would hold the dpkg lock
# while tailscale's installer needs it (that installer has no lock timeout)
# and could burn through enroll's retry budget; launched after, apt can
# neither delay nor fail the one thing that makes the VM reachable.
# Status: `systemctl status kd-bootstrap`; log: /var/log/kd-bootstrap.log;
# success marker: /var/lib/kd/bootstrap-done.
bootstrap() {{
  systemd-run --quiet --unit=kd-bootstrap --description='kd package bootstrap' /bin/sh -c '
    export DEBIAN_FRONTEND=noninteractive
    mkdir -p /var/lib/kd
    exec >/var/log/kd-bootstrap.log 2>&1
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      apt-get -o DPkg::Lock::Timeout=600 update &&
      apt-get -o DPkg::Lock::Timeout=600 -y dist-upgrade &&
{install_line}      touch /var/lib/kd/bootstrap-done && exit 0
      sleep 30
    done
    exit 1
  '
}}
for attempt in 1 2 3 4 5; do
  if enroll; then
    # Best-effort: a bootstrap that fails to launch must not turn a
    # successfully enrolled worker into a script failure.
    bootstrap || true
    exit 0
  fi
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
pub(crate) fn redact_auth_key(script: &str, auth_key: &str) -> String {
    script.replace(auth_key, "<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_init_script_embeds_key_and_hostname_single_quoted() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &[]).unwrap();
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
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &[]).unwrap();
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
    /// permanently strand an already-billed VM. The success branch is an
    /// `if enroll; then` block rather than a bare `enroll && exit 0`,
    /// because the package bootstrap has to be launched on that branch
    /// only.
    #[test]
    fn render_init_script_retries_the_enroll_sequence() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &[]).unwrap();
        assert!(script.contains("for attempt in 1 2 3 4 5"));
        assert!(script.contains("enroll() {"));
        assert!(script.contains("if enroll; then"));
    }

    #[test]
    fn render_init_script_rejects_auth_key_without_prefix() {
        assert!(render_init_script("not-a-key", "ubiworker-foo", &[]).is_err());
    }

    /// Regression coverage for FX6: `render_init_script` used to only check
    /// the `tskey-auth-` prefix, which would have let a key containing a
    /// `'` or a newline through to unescaped single-quoted interpolation.
    #[test]
    fn render_init_script_rejects_auth_key_with_embedded_quote() {
        assert!(render_init_script("tskey-auth-xx'xx", "ubiworker-foo", &[]).is_err());
    }

    #[test]
    fn render_init_script_rejects_auth_key_with_embedded_newline() {
        assert!(render_init_script("tskey-auth-xx\nxx", "ubiworker-foo", &[]).is_err());
    }

    #[test]
    fn render_init_script_rejects_auth_key_that_is_only_the_prefix() {
        assert!(render_init_script("tskey-auth-", "ubiworker-foo", &[]).is_err());
    }

    #[test]
    fn render_init_script_rejects_name_with_unexpected_chars() {
        assert!(render_init_script("tskey-auth-xxxxx", "ubiworker-foo bar", &[]).is_err());
    }

    // ── package bootstrap ────────────────────────────────────────────────
    // Covers `validate_package_name`, `bootstrap_packages`, and the
    // systemd-run block `render_init_script` emits — the async apt-satisfy
    // path launched only after tailscale enrollment succeeds.

    /// The full end-to-end shape of the bootstrap block when packages are
    /// requested: it must be a detached `systemd-run` unit (not inline apt
    /// calls that could block the script) whose `apt-get satisfy` line for
    /// the requested packages lives inside the unit's `sh -c` body — after
    /// `dist-upgrade`, before the body's closing quote — and, most
    /// importantly, that whole unit must be defined and launched *after*
    /// `enroll() {`, never before it, so tailscale enrollment is
    /// never gated on the bootstrap starting, let alone finishing.
    #[test]
    fn render_init_script_with_packages_bootstraps_after_enroll() {
        let packages = vec!["build-essential".to_string(), "git".to_string()];
        let script =
            render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &packages).unwrap();
        assert!(script.contains("systemd-run --quiet --unit=kd-bootstrap"));
        assert!(
            script.contains("apt-get -o DPkg::Lock::Timeout=600 -y satisfy build-essential git")
        );

        let systemd_run_pos = script
            .find("systemd-run --quiet --unit=kd-bootstrap")
            .expect("bootstrap unit launch present");
        let dist_upgrade_pos = script.find("dist-upgrade").expect("dist-upgrade present");
        let satisfy_pos = script
            .find("apt-get -o DPkg::Lock::Timeout=600 -y satisfy")
            .expect("satisfy line present");
        // The lone `  '` line is the `sh -c '...'` body's closing quote —
        // everything the unit actually runs must appear before it.
        let sh_c_close_pos = script
            .rfind("\n  '\n")
            .expect("sh -c body closing quote present");
        assert!(
            systemd_run_pos < dist_upgrade_pos
                && dist_upgrade_pos < satisfy_pos
                && satisfy_pos < sh_c_close_pos,
            "expected systemd-run < dist-upgrade < satisfy < sh-c-close, got {systemd_run_pos} \
             < {dist_upgrade_pos} < {satisfy_pos} < {sh_c_close_pos}\nscript:\n{script}"
        );

        let enroll_pos = script.find("enroll() {").expect("enroll() present");
        assert!(
            enroll_pos < systemd_run_pos,
            "enroll() must precede the bootstrap unit: enroll at {enroll_pos}, bootstrap at \
             {systemd_run_pos}\nscript:\n{script}"
        );
    }

    /// An empty package list must still run `update`/`dist-upgrade` (so the
    /// VM stays patched even with no `--pkg` given) but must not emit an
    /// `apt-get satisfy` (or `install`) line at all: unlike `install`,
    /// `satisfy` errors out on zero arguments rather than treating it as a
    /// no-op (see `render_init_script`'s docs), so this line has to be
    /// omitted outright, not just left looking odd.
    #[test]
    fn render_init_script_with_no_packages_skips_install_but_keeps_dist_upgrade() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &[]).unwrap();
        assert!(script.contains("dist-upgrade"));
        assert!(
            !script.contains("satisfy"),
            "unexpected satisfy line in script:\n{script}"
        );
        assert!(
            !script
                .lines()
                .any(|line| line.contains("apt-get") && line.contains("install")),
            "unexpected install line in script:\n{script}"
        );
    }

    /// Pins the ordering invariant: enrollment must never wait on
    /// or fail because of apt. Concretely, `bootstrap || true` must appear
    /// *inside* the `if enroll; then ... fi` block (after `if enroll; then`
    /// and before that block's `exit 0`), and no `systemd-run`/`apt-get`
    /// text may appear anywhere before `enroll() {` in the script — the
    /// bootstrap unit must be entirely defined and launched after
    /// enrollment, not merely "non-fatal if it happens to run early". The
    /// concrete hazard this guards against is dpkg-lock contention with
    /// tailscale's own installer, which shells out to `apt-get install`
    /// with no lock timeout of its own (see `render_init_script`'s docs).
    #[test]
    fn render_init_script_bootstrap_runs_only_after_successful_enroll() {
        let script = render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &[]).unwrap();

        let enroll_pos = script.find("enroll() {").expect("enroll() present");
        let before_enroll = &script[..enroll_pos];
        assert!(
            !before_enroll.contains("systemd-run") && !before_enroll.contains("apt-get"),
            "found systemd-run/apt-get text before enroll() {{:\n{before_enroll}"
        );

        let if_pos = script
            .find("if enroll; then")
            .expect("`if enroll; then` present");
        let block = &script[if_pos..];
        let bootstrap_call_pos = block
            .find("bootstrap || true")
            .expect("`bootstrap || true` present");
        let exit_pos = block
            .find("exit 0")
            .expect("`exit 0` present in the enroll block");
        assert!(
            bootstrap_call_pos < exit_pos,
            "`bootstrap || true` must appear before `exit 0` inside the `if enroll; then` \
             block: bootstrap at {bootstrap_call_pos}, exit 0 at {exit_pos}\nblock:\n{block}"
        );
    }

    /// `render_init_script` re-validates every package name itself rather
    /// than trusting the caller (mirroring `auth_key`/`name` above); this
    /// only needs to prove that re-validation wiring exists, not exercise
    /// the full corpus of bad shapes — that lives in
    /// `validate_package_name_rejects_bad_shapes`.
    #[test]
    fn render_init_script_rejects_invalid_package_names() {
        for bad in ["a'b", "-leading"] {
            let packages = vec![bad.to_string()];
            assert!(
                render_init_script("tskey-auth-xxxxx-yyyyy", "ubiworker-foo", &packages).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    /// Pins the accepted Debian package-name shapes, including the
    /// boundary cases the charset description calls out: a `+` (`g++`), a
    /// digit run (`libssl3`), a dotted version-suffixed name
    /// (`python3.12`), and a name containing `.` with no other special
    /// shape (`lib.`). That last one is deliberately here, not in the
    /// rejects list: Debian package-name syntax doesn't forbid an unmatched
    /// `.`, so `validate_package_name` must accept it — apt-level safety
    /// against `install`'s unanchored-regex fallback on such a name comes
    /// from `render_init_script` using `satisfy` instead, not from
    /// rejecting the name here.
    #[test]
    fn validate_package_name_accepts_debian_shapes() {
        for good in ["build-essential", "g++", "libssl3", "python3.12", "lib."] {
            assert!(
                validate_package_name(good).is_ok(),
                "expected '{good}' to be accepted"
            );
        }
    }

    /// Mirrors `render_init_script_rejects_invalid_package_names` plus two
    /// boundary cases: a lone character is rejected (Debian package names
    /// must be at least two characters) even though it's otherwise
    /// charset-valid, and `vim-` is rejected even though every individual
    /// character is charset-legal, because apt reads a trailing `-` as
    /// "remove this package" — the opposite of what `--pkg vim-` looks like
    /// it's asking for.
    #[test]
    fn validate_package_name_rejects_bad_shapes() {
        for bad in ["a'b", "has space", "Upper", "-leading", "", "a", "vim-"] {
            assert!(
                validate_package_name(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    /// The assembly contract: `base` comes first, `extra` (the operator's
    /// `--pkg` values) follows in the order given, and a duplicate within
    /// `extra` is kept only at its first occurrence.
    #[test]
    fn bootstrap_packages_dedupes_preserving_order() {
        let extra = vec![
            "git".to_string(),
            "build-essential".to_string(),
            "git".to_string(),
        ];
        assert_eq!(
            bootstrap_packages(&[], &extra),
            vec!["git".to_string(), "build-essential".to_string()]
        );
    }

    /// The reason `bootstrap_packages` takes `base` as a parameter rather
    /// than reaching for `BASE_PACKAGES` directly:
    /// a name duplicated between `base` and `extra` must survive only once,
    /// at its `base` position, since `base` is chained first — this is the
    /// scenario `BASE_PACKAGES` being empty today can't otherwise exercise.
    #[test]
    fn bootstrap_packages_dedupes_overlap_between_base_and_extra() {
        let base = ["git"];
        let extra = vec!["git".to_string(), "curl".to_string()];
        assert_eq!(
            bootstrap_packages(&base, &extra),
            vec!["git".to_string(), "curl".to_string()]
        );
    }

    /// Each worker command's `create::run` validates the *combined* package
    /// list (`BASE_PACKAGES`
    /// plus every `--pkg` value), not just the operator-supplied half, so a
    /// bad `BASE_PACKAGES` entry is caught before any billable or
    /// secret-minting step rather than deep inside `render_init_script`
    /// after the Tailscale key is already minted. `BASE_PACKAGES` is empty
    /// today, so this test is a tripwire: it fails at `cargo test` time, not
    /// at the first `kd ubiworker create` or `kd boatworker create` after the
    /// change, the moment a
    /// future entry violates Debian package-name syntax.
    #[test]
    fn base_packages_all_pass_validate_package_name() {
        for pkg in BASE_PACKAGES {
            assert!(
                validate_package_name(pkg).is_ok(),
                "BASE_PACKAGES entry '{pkg}' fails validate_package_name"
            );
        }
    }

    /// Guards the one thing standing between `debug!`-level logging and
    /// leaking the minted auth key: the secret must not survive redaction
    /// anywhere in the script, and the surrounding shape must be unchanged.
    #[test]
    fn redact_auth_key_removes_secret_but_keeps_script_shape() {
        let auth_key = "tskey-auth-xxxxx-yyyyy";
        let script = render_init_script(auth_key, "ubiworker-foo", &[]).unwrap();
        let redacted = redact_auth_key(&script, auth_key);

        assert!(!redacted.contains(auth_key));
        assert!(redacted.contains("--auth-key='<redacted>'"));
        assert!(redacted.contains("--hostname='ubiworker-foo'"));
    }
}
