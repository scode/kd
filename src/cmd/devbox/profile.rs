//! Shared bootstrap settings and named stateful instances.
//!
//! A disposable environment needs only the shared user, key and manifest.
//! Named profiles identify state that survives a machine: its current host,
//! hostname and archive directory. Package preferences remain in prompts.
//!
//! The file holds no secrets, so its permissions are deliberately not
//! checked. It lives at `$XDG_CONFIG_HOME/kd/devboxes.toml`, falling back to
//! `~/.config/kd/devboxes.toml`. Stateful operations require a profile;
//! bootstrap names its destination explicitly and needs no source profile.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One devbox, as written in `devboxes.toml`.
///
/// `backup_dir` is stored as written; `~` is expanded by
/// [`Profile::resolve`] against a home directory the caller supplies, so the
/// parsing layer never touches the process environment and stays
/// unit-testable.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Current source address for backup, suspend and resume. Bootstrap
    /// never connects here; its destination is always explicit.
    pub host: String,
    /// Source login override; omitted means the shared bootstrap user.
    #[serde(default)]
    pub user: Option<String>,
    /// OS hostname to set; Tailscale uses the same name.
    pub hostname: String,
    /// Controller-side directory where Hermes archives land. `~` allowed.
    pub backup_dir: String,
}

/// Reusable environment identity, independent of any machine or backup.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BootstrapSettings {
    pub user: String,
    pub public_key: String,
    pub repos: Vec<String>,
}

/// The whole `devboxes.toml`: one `[devbox.NAME]` table per profile.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    pub bootstrap: BootstrapSettings,
    #[serde(default)]
    pub devbox: BTreeMap<String, Profile>,
}

/// A profile with its `~` paths expanded, ready for use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub name: String,
    pub host: String,
    pub user: String,
    pub hostname: String,
    pub backup_dir: PathBuf,
}

impl Profile {
    /// Expand `~` in the path fields against `home`, and validate the parts
    /// that get interpolated into shell commands or ssh destinations.
    ///
    /// Validation is deliberately narrow: `user` and `hostname` must be the
    /// plain lowercase tokens Ubuntu accepts, because both end up in
    /// `useradd`/`hostnamectl` arguments and an ssh destination. `host` is not
    /// validated beyond being non-empty because it is only ever passed as a
    /// single argv element to ssh.
    pub fn resolve(
        &self,
        name: &str,
        home: &Path,
        default_user: &str,
    ) -> anyhow::Result<ResolvedProfile> {
        if self.host.trim().is_empty() {
            bail!("profile '{name}': host is empty");
        }
        let user = self.user.as_deref().unwrap_or(default_user);
        validate_token(name, "user", user)?;
        validate_token(name, "hostname", &self.hostname)?;
        Ok(ResolvedProfile {
            name: name.to_owned(),
            host: self.host.clone(),
            user: user.to_owned(),
            hostname: self.hostname.clone(),
            backup_dir: expand_tilde(&self.backup_dir, home),
        })
    }
}

/// `[a-z_][a-z0-9_-]*`, at most 63 characters: the intersection of what
/// `useradd` and hostnames accept, and a charset safe to interpolate.
pub fn validate_token(profile: &str, field: &str, value: &str) -> anyhow::Result<()> {
    let mut chars = value.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_');
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !(first_ok && rest_ok && value.len() <= 63) {
        bail!(
            "profile '{profile}': {field} '{value}' must match [a-z_][a-z0-9_-]* and be at most 63 characters"
        );
    }
    Ok(())
}

/// Split a manifest entry into `(owner, name)`, rejecting anything that is
/// not exactly `owner/name` with a name safe to use as a directory.
pub fn repo_name(entry: &str) -> anyhow::Result<(&str, &str)> {
    let Some((owner, name)) = entry.split_once('/') else {
        bail!("'{entry}' is not owner/name");
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            && s != "."
            && s != ".."
    };
    if !(ok(owner) && ok(name)) {
        bail!("'{entry}' is not owner/name (letters, digits, '-', '_', '.' only)");
    }
    Ok((owner, name))
}

/// Expand a leading `~` or `~/` against `home`; any other path is returned
/// unchanged. `~user` forms are not supported and pass through literally.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Where `devboxes.toml` lives: `$XDG_CONFIG_HOME/kd/devboxes.toml` when the
/// variable is set and non-empty, else `~/.config/kd/devboxes.toml`.
///
/// Takes the environment values as parameters rather than reading them, so
/// the fallback rule is unit-testable without touching process state.
pub fn config_path(xdg_config_home: Option<&OsStr>, home: &Path) -> PathBuf {
    match xdg_config_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("kd").join("devboxes.toml"),
        _ => home.join(".config").join("kd").join("devboxes.toml"),
    }
}

/// Parse a `devboxes.toml` document.
pub fn parse(text: &str) -> anyhow::Result<Profiles> {
    let config: Profiles = toml::from_str(text).context(
        "failed to parse devboxes.toml; shared user, public_key and repos belong in [bootstrap]",
    )?;
    validate_token("bootstrap", "user", &config.bootstrap.user)?;
    if config.bootstrap.public_key.trim().is_empty() {
        bail!("bootstrap public_key is empty");
    }
    for repo in &config.bootstrap.repos {
        repo_name(repo).context("bad bootstrap repos entry")?;
    }
    Ok(config)
}

/// Load and resolve one named profile from the config file at `path`.
///
/// A missing file and a missing profile get distinct, actionable messages:
/// the former tells the user where the file is expected, the latter lists
/// the profiles that do exist.
pub fn load(path: &Path, name: &str, home: &Path) -> anyhow::Result<ResolvedProfile> {
    load_config(path)?.resolve(name, home)
}

/// Read configuration without selecting a stateful instance. Scratch
/// bootstrap uses this path and never inspects backup directories.
pub fn load_config(path: &Path) -> anyhow::Result<Profiles> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "no devbox config at {} (see SPEC.md for the format)",
            path.display()
        )
    })?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

impl Profiles {
    /// Resolve only the requested instance, inheriting the shared user.
    pub fn resolve(&self, name: &str, home: &Path) -> anyhow::Result<ResolvedProfile> {
        let Some(profile) = self.devbox.get(name) else {
            let known: Vec<&str> = self.devbox.keys().map(String::as_str).collect();
            bail!(
                "no profile '{name}'; known profiles: {}",
                if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                }
            );
        };
        profile.resolve(name, home, &self.bootstrap.user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPEC.md example must parse as written, since it is what the user
    /// copies from. Any drift between the doc and the schema shows up here.
    const SPEC_EXAMPLE: &str = r#"
[bootstrap]
user = "scode"
public_key = "~/.ssh/id_ed25519.pub"
repos = ["scode/kd", "scode/voice"]

[devbox.NAME]
host = "203.0.113.5"              # address the controller can ssh to before Tailscale exists
hostname = "devbox"               # OS hostname; Tailscale uses the same name
backup_dir = "~/devbox-backups"   # where Hermes archives land on the controller; ~ is expanded
"#;

    #[test]
    fn spec_example_parses_and_resolves() {
        let profiles = parse(SPEC_EXAMPLE).unwrap();
        let resolved = profiles.resolve("NAME", Path::new("/home/me")).unwrap();
        assert_eq!(resolved.host, "203.0.113.5");
        assert_eq!(
            expand_tilde(&profiles.bootstrap.public_key, Path::new("/home/me")),
            PathBuf::from("/home/me/.ssh/id_ed25519.pub")
        );
        assert_eq!(
            resolved.backup_dir,
            PathBuf::from("/home/me/devbox-backups")
        );
        assert_eq!(profiles.bootstrap.repos, vec!["scode/kd", "scode/voice"]);
        assert_eq!(resolved.user, "scode");
    }

    /// Unknown keys are rejected so a typo like `pubkey =` fails loudly
    /// instead of silently using a default.
    #[test]
    fn unknown_field_is_an_error() {
        let text = SPEC_EXAMPLE.replace("public_key", "pubkey");
        assert!(parse(&text).is_err());
    }

    /// Values that are interpolated into shell arguments are constrained to
    /// a safe charset; this is the guard, not shell quoting downstream.
    #[test]
    fn user_and_hostname_charset_is_enforced() {
        let profiles = parse(SPEC_EXAMPLE).unwrap();
        let mut p = profiles.devbox["NAME"].clone();
        p.user = Some("Bad User".into());
        assert!(p.resolve("NAME", Path::new("/h"), "scode").is_err());
        p.user = Some("ok_user".into());
        p.hostname = "-nope".into();
        assert!(p.resolve("NAME", Path::new("/h"), "scode").is_err());
    }

    #[test]
    fn repo_entries_must_be_owner_slash_name() {
        assert_eq!(repo_name("scode/kd").unwrap(), ("scode", "kd"));
        assert!(repo_name("kd").is_err());
        assert!(repo_name("scode/").is_err());
        assert!(repo_name("scode/../x").is_err());
        assert!(repo_name("scode/a b").is_err());
    }

    #[test]
    fn tilde_expansion_only_touches_leading_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/me"));
        assert_eq!(expand_tilde("~/x", home), PathBuf::from("/home/me/x"));
        assert_eq!(expand_tilde("/abs/~/x", home), PathBuf::from("/abs/~/x"));
        assert_eq!(expand_tilde("~other/x", home), PathBuf::from("~other/x"));
    }

    /// XDG takes precedence only when set to something; an empty value is
    /// treated as unset, matching the XDG spec's own rule.
    #[test]
    fn config_path_prefers_nonempty_xdg() {
        let home = Path::new("/home/me");
        assert_eq!(
            config_path(Some(OsStr::new("/xdg")), home),
            PathBuf::from("/xdg/kd/devboxes.toml")
        );
        assert_eq!(
            config_path(Some(OsStr::new("")), home),
            PathBuf::from("/home/me/.config/kd/devboxes.toml")
        );
        assert_eq!(
            config_path(None, home),
            PathBuf::from("/home/me/.config/kd/devboxes.toml")
        );
    }

    /// A missing profile names the ones that exist, since that is the usual
    /// mistake (a typo in `--profile`).
    #[test]
    fn missing_profile_lists_known_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devboxes.toml");
        std::fs::write(&path, SPEC_EXAMPLE).unwrap();
        let err = load(&path, "nope", Path::new("/h"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("known profiles: NAME"), "{err}");
    }
}
