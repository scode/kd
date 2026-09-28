//! Controller-side credential sources for the agent CLIs whose logins
//! bootstrap copies to the target for good: OpenCode and Muse.
//!
//! Codex and Claude are deliberately absent. On the target they go through
//! codex-lb and CLIProxyAPI and need no login of their own; Claude's is never
//! copied, and Codex's is only lent to the setup agent for the length of a
//! run (see [`super::codex_login`]). A copied login whose refresh token
//! rotates breaks whichever copy refreshes second, which is why those two
//! stopped being copied.
//!
//! `bootstrap` resolves these before anything is written, so a missing login
//! on the laptop fails fast instead of after an hour of package installs.
//! The contents are read into memory here and only ever handed to
//! [`super::transport::Transport::push_secret`]; nothing in this module logs
//! or prints a value. Locations follow SPEC_impl.md "Secrets".

use anyhow::{Context, bail};
use std::path::Path;
use std::process::Command;

/// One credential file to place on the target, at its CLI's native
/// location relative to the target user's home.
pub struct AuthSource {
    /// CLI name, for messages only.
    pub cli: &'static str,
    /// Home-relative destination on the target, e.g. `.config/muse/auth.json`.
    pub remote_relative: &'static str,
    /// The credential bytes. Never logged.
    pub contents: Vec<u8>,
}

/// Resolve both sources from `home`, failing with one message naming
/// every missing one so a fresh laptop is fixed in one round trip.
pub fn resolve_all(home: &Path) -> anyhow::Result<Vec<AuthSource>> {
    let mut sources = Vec::new();
    let mut missing = Vec::new();

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

    match muse_credentials(home) {
        Ok(contents) => sources.push(AuthSource {
            cli: "muse",
            remote_relative: ".config/muse/auth.json",
            contents,
        }),
        Err(e) => {
            // The reason is worth carrying: a Keychain miss and a missing
            // file need different fixes.
            missing.push(Box::leak(
                format!("muse: {e:#} (run `muse login` on the controller)").into_boxed_str(),
            ));
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

/// Keychain service Muse Code uses on macOS for the OAuth secrets that its
/// schema-2 `auth.json` only points at. Observed on the controller.
const MUSE_KEYCHAIN_SERVICE: &str = "ai.meta.dev.credentials";
const MUSE_KEYCHAIN_ACCOUNT: &str = "meta";

/// Muse's auth file in the form a Linux Muse reads.
///
/// Muse 1.0 has two on-disk shapes with the same version string. On Linux
/// it writes schema 1: one JSON with the provider's `api_key` and
/// `access_token` inline. On macOS it writes schema 2: the same metadata
/// with `"storage": "keychain"` instead of the secrets, which live in the
/// Keychain as a small JSON of their own. A Linux Muse rejects schema 2
/// outright ("unsupported auth schema version 2"), so copying the macOS
/// file verbatim can never work. Merging the two back into schema 1 is
/// deterministic and is what this does; a schema-1 file passes through.
fn muse_credentials(home: &Path) -> anyhow::Result<Vec<u8>> {
    let path = home.join(".config/muse/auth.json");
    let file = read_file(&path).ok_or_else(|| anyhow::anyhow!("{} is missing", path.display()))?;
    let file_json: serde_json::Value =
        serde_json::from_slice(&file).context("auth.json is not JSON")?;
    if file_json.get("schema_version") != Some(&serde_json::Value::from(2)) {
        return Ok(file);
    }
    let secrets = keychain_password_for(MUSE_KEYCHAIN_SERVICE, Some(MUSE_KEYCHAIN_ACCOUNT))
        .context("schema-2 auth.json points at the Keychain but the item is not readable")?;
    let secrets_json: serde_json::Value =
        serde_json::from_slice(&secrets).context("Muse Keychain payload is not JSON")?;
    muse_schema1(&file_json, &secrets_json).map(|v| v.to_string().into_bytes())
}

/// Pure merge of a schema-2 file and its Keychain payload into schema 1.
fn muse_schema1(
    file: &serde_json::Value,
    secrets: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Map, Value};
    let providers = file
        .get("providers")
        .and_then(Value::as_object)
        .context("auth.json has no providers")?;
    let mut out = Map::new();
    for (name, provider) in providers {
        let mut p = provider.as_object().cloned().unwrap_or_default();
        p.remove("storage");
        for key in ["api_key", "access_token"] {
            if let Some(v) = secrets.get(key) {
                p.insert(key.to_owned(), v.clone());
            }
        }
        if !p.contains_key("api_key") && !p.contains_key("access_token") {
            bail!("no secret found for Muse provider '{name}'");
        }
        out.insert(name.clone(), Value::Object(p));
    }
    let mut root = Map::new();
    root.insert("schema_version".into(), Value::from(1));
    root.insert("providers".into(), Value::Object(out));
    Ok(Value::Object(root))
}

/// `security find-generic-password -s <service> [-a <account>] -w`, or
/// `None` when the tool is absent (Linux controller), the item is absent, or
/// the Keychain is locked. All three mean "not available here", which is
/// all the caller distinguishes.
fn keychain_password_for(service: &str, account: Option<&str>) -> Option<Vec<u8>> {
    let mut cmd = Command::new("security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    let output = cmd.arg("-w").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `-w` prints the value followed by a newline; the JSON must not carry
    // that newline into the file the CLI reads.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The macOS-to-Linux merge: metadata from the file, secrets from the
    /// Keychain payload, `storage` dropped, schema bumped back to 1. The
    /// shapes here mirror what was observed on the controller and the
    /// devbox on 2026-09-05.
    #[test]
    fn muse_schema2_plus_keychain_becomes_schema1() {
        let file = json!({
            "schema_version": 2,
            "providers": {"meta": {
                "mechanism": "oauth", "storage": "keychain", "obtained_via": "device_code",
                "api_base_url": "https://x", "user_email": "e", "user_full_name": "n"
            }}
        });
        let secrets = json!({"secret_schema_version": 1, "api_key": "K", "access_token": "T"});
        let out = muse_schema1(&file, &secrets).unwrap();
        assert_eq!(out["schema_version"], 1);
        let meta = &out["providers"]["meta"];
        assert_eq!(meta["api_key"], "K");
        assert_eq!(meta["access_token"], "T");
        assert_eq!(meta["mechanism"], "oauth");
        assert!(meta.get("storage").is_none());
    }

    #[test]
    fn muse_merge_refuses_a_payload_without_secrets() {
        let file = json!({"schema_version": 2, "providers": {"meta": {"storage": "keychain"}}});
        assert!(muse_schema1(&file, &json!({})).is_err());
    }
}
