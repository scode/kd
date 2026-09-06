//! Devbox profiles: the one place identity lives.
//!
//! A profile names a disposable remote development box: where it is, who the
//! one real user on it is, and where its Hermes archives land on the
//! controller. Everything that could identify the user (addresses,
//! hostnames, usernames, key paths) is in this file and nowhere in the
//! binary; package and tool preferences are the reverse (compiled in, never
//! in the profile). SPEC.md's `kd devbox` section documents the file.
//!
//! The file holds no secrets, so its permissions are deliberately not
//! checked. It lives at `$XDG_CONFIG_HOME/kd/devboxes.toml`, falling back to
//! `~/.config/kd/devboxes.toml`; every subcommand requires `--profile`
//! even when only one profile exists, because the commands are not something
//! to run against the wrong box by accident.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One devbox, as written in `devboxes.toml`.
///
/// `public_key` and `backup_dir` are stored as written; `~` is expanded by
/// [`Profile::resolve`] against a home directory the caller supplies, so the
/// parsing layer never touches the process environment and stays
/// unit-testable.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Address the controller can ssh to before Tailscale exists on the box
    /// (the provider's public IP, typically). Never a MagicDNS name: on a real
    /// run Tailscale is the last thing bootstrap sets up.
    pub host: String,
    /// The one real user on the box. On a rehearsal (`--target`) this is
    /// ignored in favour of the target's user.
    pub user: String,
    /// OS hostname to set; Tailscale uses the same name.
    pub hostname: String,
    /// Controller-side public key to authorize on the box. `~` allowed.
    pub public_key: String,
    /// Controller-side directory where Hermes archives land. `~` allowed.
    pub backup_dir: String,
    /// GitHub `owner/name` entries, each cloned to `~/git/<name>` on the box.
    /// `scode/dotfiles` and `scode/voice` are cloned whether or not they are
    /// listed (see SPEC.md); listing them is harmless.
    pub repos: Vec<String>,
}

/// The whole `devboxes.toml`: one `[devbox.NAME]` table per profile.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
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
    pub public_key: PathBuf,
    pub backup_dir: PathBuf,
    pub repos: Vec<String>,
}

impl Profile {
    /// Expand `~` in the path fields against `home`, and validate the parts
    /// that get interpolated into shell commands or ssh destinations.
    ///
    /// Validation is deliberately narrow: `user` and `hostname` must be the
    /// plain lowercase tokens Ubuntu accepts, because both end up in
    /// `useradd`/`hostnamectl` arguments and an ssh destination; `repos`
    /// must be `owner/name` so `~/git/<name>` is derivable. `host` is not
    /// validated beyond being non-empty because it is only ever passed as a
    /// single argv element to ssh.
    pub fn resolve(&self, name: &str, home: &Path) -> anyhow::Result<ResolvedProfile> {
        if self.host.trim().is_empty() {
            bail!("profile '{name}': host is empty");
        }
        validate_token(name, "user", &self.user)?;
        validate_token(name, "hostname", &self.hostname)?;
        for repo in &self.repos {
            repo_name(repo).with_context(|| format!("profile '{name}': bad repos entry"))?;
        }
        Ok(ResolvedProfile {
            name: name.to_owned(),
            host: self.host.clone(),
            user: self.user.clone(),
            hostname: self.hostname.clone(),
            public_key: expand_tilde(&self.public_key, home),
            backup_dir: expand_tilde(&self.backup_dir, home),
            repos: self.repos.clone(),
        })
    }
}

/// `[a-z_][a-z0-9_-]*`, at most 63 characters: the intersection of what
/// `useradd` and hostnames accept, and a charset safe to interpolate.
fn validate_token(profile: &str, field: &str, value: &str) -> anyhow::Result<()> {
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
    toml::from_str(text).context("failed to parse devboxes.toml")
}

/// Load and resolve one named profile from the config file at `path`.
///
/// A missing file and a missing profile get distinct, actionable messages:
/// the former tells the user where the file is expected, the latter lists
/// the profiles that do exist.
pub fn load(path: &Path, name: &str, home: &Path) -> anyhow::Result<ResolvedProfile> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "no devbox config at {} (see SPEC.md for the format)",
            path.display()
        )
    })?;
    let profiles = parse(&text).with_context(|| format!("in {}", path.display()))?;
    let Some(profile) = profiles.devbox.get(name) else {
        let known: Vec<&str> = profiles.devbox.keys().map(String::as_str).collect();
        bail!(
            "no profile '{name}' in {}; known profiles: {}",
            path.display(),
            if known.is_empty() {
                "(none)".to_owned()
            } else {
                known.join(", ")
            }
        );
    };
    profile.resolve(name, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPEC.md example must parse as written, since it is what the user
    /// copies from. Any drift between the doc and the schema shows up here.
    const SPEC_EXAMPLE: &str = r#"
[devbox.NAME]
host = "203.0.113.5"              # address the controller can ssh to before Tailscale exists
user = "scode"                    # the one real user on the box
hostname = "devbox"               # OS hostname; Tailscale uses the same name
public_key = "~/.ssh/id_ed25519.pub"  # controller key to authorize on the box; ~ is expanded
backup_dir = "~/devbox-backups"   # where Hermes archives land on the controller; ~ is expanded
repos = ["scode/kd", "scode/voice"]   # GitHub owner/name, each cloned to ~/git/<name>
"#;

    #[test]
    fn spec_example_parses_and_resolves() {
        let profiles = parse(SPEC_EXAMPLE).unwrap();
        let resolved = profiles.devbox["NAME"]
            .resolve("NAME", Path::new("/home/me"))
            .unwrap();
        assert_eq!(resolved.host, "203.0.113.5");
        assert_eq!(
            resolved.public_key,
            PathBuf::from("/home/me/.ssh/id_ed25519.pub")
        );
        assert_eq!(
            resolved.backup_dir,
            PathBuf::from("/home/me/devbox-backups")
        );
        assert_eq!(resolved.repos, vec!["scode/kd", "scode/voice"]);
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
        p.user = "Bad User".into();
        assert!(p.resolve("NAME", Path::new("/h")).is_err());
        p.user = "ok_user".into();
        p.hostname = "-nope".into();
        assert!(p.resolve("NAME", Path::new("/h")).is_err());
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
