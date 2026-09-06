//! Controller-side credential sources for the four agent CLIs.
//!
//! `backup` and `bootstrap` both start by resolving these, before anything is
//! stopped or written, so a missing login on the laptop fails fast instead
//! of after an hour of package installs. The contents are read into memory
//! here and only ever handed to [`super::transport::Transport::push_secret`];
//! nothing in this module logs or prints a value.
//!
//! Locations follow SPEC_impl.md "Secrets". The one non-obvious rule is
//! Claude on macOS: Claude Code stores its login in the Keychain whenever the
//! Keychain is writable and writes `~/.claude/.credentials.json` only as a
//! fallback, so a stale file can sit next to a live Keychain entry. The
//! Keychain is therefore tried first and the file is used only when that
//! lookup fails. The Keychain payload is the same JSON the Linux file holds.

use anyhow::bail;
use std::path::Path;
use std::process::Command;

/// One credential file to place on the target, at its CLI's native
/// location relative to the target user's home.
pub struct AuthSource {
    /// CLI name, for messages only.
    pub cli: &'static str,
    /// Home-relative destination on the target, e.g. `.codex/auth.json`.
    #[expect(dead_code, reason = "read by bootstrap, which lands next")]
    pub remote_relative: &'static str,
    /// The credential bytes. Never logged.
    #[expect(dead_code, reason = "read by bootstrap, which lands next")]
    pub contents: Vec<u8>,
}

/// Keychain service name Claude Code uses on macOS. Observed on the
/// controller; see SPEC_impl.md.
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Resolve all four sources from `home`, failing with one message naming
/// every missing one so a fresh laptop is fixed in one round trip.
pub fn resolve_all(home: &Path) -> anyhow::Result<Vec<AuthSource>> {
    let mut sources = Vec::new();
    let mut missing = Vec::new();

    match read_file(&home.join(".codex/auth.json")) {
        Some(contents) => sources.push(AuthSource {
            cli: "codex",
            remote_relative: ".codex/auth.json",
            contents,
        }),
        None => missing.push(
            "codex: ~/.codex/auth.json (set cli_auth_credentials_store = \"file\" in ~/.codex/config.toml and run `codex login`)",
        ),
    }

    match claude_credentials(home) {
        Some(contents) => sources.push(AuthSource {
            cli: "claude",
            remote_relative: ".claude/.credentials.json",
            contents,
        }),
        None => missing.push(
            "claude: neither the Keychain item nor ~/.claude/.credentials.json (run `claude` and log in)",
        ),
    }

    match read_file(&home.join(".local/share/opencode/auth.json")) {
        Some(contents) => sources.push(AuthSource {
            cli: "opencode",
            remote_relative: ".local/share/opencode/auth.json",
            contents,
        }),
        None => {
            missing.push("opencode: ~/.local/share/opencode/auth.json (run `opencode auth login`)")
        }
    }

    match read_file(&home.join(".config/muse/auth.json")) {
        Some(contents) => sources.push(AuthSource {
            cli: "muse",
            remote_relative: ".config/muse/auth.json",
            contents,
        }),
        None => {
            missing.push("muse: ~/.config/muse/auth.json (run `muse auth set --api-key-stdin`)")
        }
    }

    if !missing.is_empty() {
        bail!(
            "missing agent credentials on the controller:\n  {}",
            missing.join("\n  ")
        );
    }
    Ok(sources)
}

/// Read a file, treating "missing" and "empty" alike as absent. Other errors
/// (permissions, say) are also treated as absent because the remedy is the
/// same: log in again on the controller.
fn read_file(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

/// Keychain first, file second; see the module docs for why the order is
/// not negotiable.
fn claude_credentials(home: &Path) -> Option<Vec<u8>> {
    keychain_password(CLAUDE_KEYCHAIN_SERVICE)
        .or_else(|| read_file(&home.join(".claude/.credentials.json")))
}

/// `security find-generic-password -s <service> -w`, or `None` when the tool
/// is absent (Linux controller), the item is absent, or the Keychain is
/// locked. All three mean "not available here", which is all the caller
/// distinguishes.
fn keychain_password(service: &str) -> Option<Vec<u8>> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `-w` prints the value followed by a newline; the JSON must not carry
    // that newline into the file Claude Code reads.
    let mut bytes = output.stdout;
    while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        bytes.pop();
    }
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Report which sources are present without exposing contents: the line
/// `backup` and `bootstrap` print after the preflight.
pub fn describe(sources: &[AuthSource]) -> String {
    let names: Vec<&str> = sources.iter().map(|s| s.cli).collect();
    format!("controller credentials found for: {}", names.join(", "))
}
