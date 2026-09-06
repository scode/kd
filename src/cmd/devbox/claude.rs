//! Complete Claude's first-run gate after transferring working credentials.
//!
//! Claude's interactive onboarding is separate from OAuth authentication:
//! `claude -p` can succeed while the first interactive launch still asks for
//! login. Keep this small state repair deterministic instead of depending
//! on an agent to notice a gate that its noninteractive checks never enter.

use super::transport::Transport;

/// Mark onboarding complete only after the installed CLI accepts the login.
/// Existing preferences and project trust decisions stay on the target;
/// copying the controller's entire `.claude.json` would transplant those too.
pub fn complete_onboarding(t: &Transport) -> anyhow::Result<()> {
    t.run(COMPLETE_ONBOARDING)
}

/// Run after the user-space installer, when both Claude and jq are present.
/// The target must not have an interactive Claude process writing its config
/// during bootstrap. A same-directory rename prevents a partial JSON write;
/// it does not serialize against Claude's own concurrent config updates.
pub const COMPLETE_ONBOARDING: &str = r#"
set -o pipefail
status=$(claude auth status) || exit 1
printf '%s' "$status" | jq -e '.loggedIn == true' >/dev/null || exit 1
config="$HOME/.claude.json"
if [ -L "$config" ]; then
  printf 'refusing to replace symlink: ~/.claude.json\n' >&2
  exit 1
fi
if [ -e "$config" ]; then
  jq -e 'type == "object"' "$config" >/dev/null || exit 1
  if jq -e '.hasCompletedOnboarding == true' "$config" >/dev/null; then
    exit 0
  fi
else
  config=/dev/null
fi
umask 077
staged=$(mktemp "$HOME/.claude.json.kd.XXXXXXXX") || exit 1
trap 'rm -f -- "$staged"' EXIT
if [ "$config" = /dev/null ]; then
  printf '{}\n' > "$staged" || exit 1
else
  cat "$config" > "$staged" || exit 1
fi
updated=$(jq '.hasCompletedOnboarding = true' "$staged") || exit 1
printf '%s\n' "$updated" > "$staged" || exit 1
mv -- "$staged" "$HOME/.claude.json" || exit 1
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Output};

    /// Isolate the script's target home and CLI in child-process environment
    /// variables. jq and bash are real so the test exercises the actual file
    /// update, rather than a second implementation of its JSON merge.
    fn run(home: &std::path::Path, authenticated: bool) -> Output {
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let cli = bin.join("claude");
        fs::write(
            &cli,
            format!("#!/bin/sh\n[ \"$*\" = 'auth status' ] || exit 2\nprintf '%s' '{{\"loggedIn\":{authenticated}}}'\n"),
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        Command::new("bash")
            .args(["-c", COMPLETE_ONBOARDING])
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    /// Fresh and partially configured homes must skip login onboarding without
    /// losing existing preferences or acquiring the controller's project trust.
    #[test]
    fn completes_onboarding_preserving_settings_and_is_idempotent() {
        for initial in [
            None,
            Some(r#"{"theme":"dark","projects":{"/work":{"trusted":false}}}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join(".claude.json");
            if let Some(initial) = initial {
                fs::write(&config, initial).unwrap();
            }
            assert!(run(dir.path(), true).status.success());
            let bytes = fs::read(&config).unwrap();
            let mut expected: serde_json::Value =
                serde_json::from_str(initial.unwrap_or("{}")).unwrap();
            expected["hasCompletedOnboarding"] = true.into();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                expected
            );
            assert_eq!(
                fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(run(dir.path(), true).status.success());
            assert_eq!(fs::read(&config).unwrap(), bytes);
        }
    }

    /// Corrupt, unexpected and externally managed configs must not be replaced
    /// with an empty one just to make the first-run gate disappear.
    #[test]
    fn rejects_invalid_or_symlinked_config_without_replacing_it() {
        for initial in ["{", "[]", "null"] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join(".claude.json");
            fs::write(&config, initial).unwrap();
            assert!(!run(dir.path(), true).status.success());
            assert_eq!(fs::read_to_string(config).unwrap(), initial);
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("managed.json");
        fs::write(&target, "{}").unwrap();
        symlink(&target, dir.path().join(".claude.json")).unwrap();
        assert!(!run(dir.path(), true).status.success());
        assert_eq!(fs::read_to_string(target).unwrap(), "{}");
    }

    /// A failed login must leave onboarding available to the user, even when
    /// an auth command exits successfully while reporting loggedIn=false.
    #[test]
    fn does_not_mark_an_unauthenticated_install_complete() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!run(dir.path(), false).status.success());
        assert!(!dir.path().join(".claude.json").exists());
    }
}
