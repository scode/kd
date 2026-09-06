//! Backup must preserve service state on every exit path. Run the real CLI
//! against a fake SSH executable, with an isolated child environment and no
//! agent credentials, so regressions cannot stop a real instance.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// A successful archive and failures before/after export all leave service
/// control to the operator. The fake transport rejects unexpected commands.
#[test]
fn backup_never_controls_services_or_requires_agent_auth() {
    for outcome in ["ok", "export-failure", "incomplete", "hash-mismatch"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("kd")).unwrap();
        std::fs::write(root.join("kd/devboxes.toml"), "[bootstrap]\nuser='testuser'\npublic_key='~/absent.pub'\nrepos=[]\n[devbox.instance]\nhost='fake-host'\nhostname='instance'\nbackup_dir='~/archives'\n").unwrap();
        let ssh = root.join("ssh");
        std::fs::write(&ssh, FAKE_SSH).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kd"))
            .args(["devbox", "backup", "--profile", "instance", "--yes"])
            .env_clear()
            .env("PATH", root)
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root)
            .env("BACKUP_OUTCOME", outcome)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            outcome == "ok",
            "{outcome}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(root.join("calls")).unwrap();
        assert!(!calls.contains("gateway stop"), "{calls}");
        assert!(!calls.contains("gateway start"), "{calls}");
        assert!(!calls.contains("dashboard --stop"), "{calls}");
        assert!(!calls.contains("systemctl"), "{calls}");
        if outcome == "ok" {
            let archives: Vec<_> = std::fs::read_dir(root.join("archives")).unwrap().collect();
            assert_eq!(archives.len(), 1);
            let archive = archives[0].as_ref().unwrap().path();
            assert_eq!(std::fs::read(&archive).unwrap(), b"abc");
            assert_eq!(
                std::fs::metadata(archive).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

/// Shell builtins only: PATH contains just this fake SSH, so an accidental
/// local credential lookup or external command cannot succeed unnoticed.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/calls"
case "$*" in
  *'== agent processes'*) exit 0 ;;
  *'hermes backup -o'*)
    case "$BACKUP_OUTCOME" in
      export-failure) exit 1 ;;
      incomplete) printf 'incomplete archive\n'; exit 0 ;;
    esac ;;
  *sha256sum*)
    if [ "$BACKUP_OUTCOME" = hash-mismatch ]; then printf 'wrong\n'; else
      printf 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n'
    fi ;;
  *'cat "'*hermes-backup-kd.zip*) printf abc ;;
  *'rm -f '*hermes-backup-kd.zip*) exit 0 ;;
  *'hermes --version'*) printf 'test-version\n' ;;
  *) printf 'unexpected SSH call\n' >&2; exit 99 ;;
esac
"#;
