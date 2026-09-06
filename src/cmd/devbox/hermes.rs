//! Explicit service control for the stateful source instance.
//!
//! `suspend` stops Hermes and `resume` starts it again;
//! both go through Hermes's own CLI (`hermes gateway stop|start`,
//! `hermes dashboard --stop`), because
//! the devbox being backed up predates kd and may carry a hand-written
//! dashboard unit under any name. The known dashboard unit is also stopped
//! when present, to prevent Restart=always from undoing suspension. Starting
//! uses that unit or `setsid -f`: the dashboard otherwise runs in the
//! foreground and dies with the SSH session. SPEC_impl.md records this.

use super::transport::Transport;
use tracing::info;

/// Stop both services and verify no matching processes remain. Stop
/// commands may fail when already stopped; process-check errors and a
/// surviving service must fail suspension rather than imply a safe move.
pub fn stop(t: &Transport) -> anyhow::Result<()> {
    info!("stopping Hermes gateway and dashboard on {}", t.destination);
    t.run(STOP_SCRIPT)?;
    // The stop script names Hermes literally and travels in bash's argv.
    // Probe only after that shell exits, or pgrep matches our own command.
    let out = t.capture(STOPPED_CHECK)?;
    require_stopped(out.status)
}

/// pgrep's status 1 means absent; other errors must not masquerade as a
/// stopped instance. This verdict is independent of the stop CLI's status.
fn require_stopped(status: i32) -> anyhow::Result<()> {
    if status != 1 {
        anyhow::bail!("could not verify that Hermes is suspended (process check exit {status})");
    }
    Ok(())
}

/// Keep this separate from STOP_SCRIPT: the shell's command line must not
/// contain literal names that match its own process search.
const STOPPED_CHECK: &str = "pgrep -f '[h]ermes.*(gateway|dashboard)' >/dev/null";

/// Start the gateway and dashboard. Backup never calls this, even on failure.
pub fn start(t: &Transport) -> anyhow::Result<()> {
    info!("starting Hermes gateway and dashboard on {}", t.destination);
    t.run(START_SCRIPT)
}

const START_SCRIPT: &str = r#"
hermes gateway start || exit 1
if systemctl --user cat hermes-dashboard.service >/dev/null 2>&1; then
  systemctl --user start hermes-dashboard.service
else
  setsid -f hermes dashboard --host 127.0.0.1 --port 9119 --no-open
fi
"#;

/// Stop the known dashboard unit as well as its CLI process, since a
/// Restart=always unit would otherwise immediately undo the CLI stop.
const STOP_SCRIPT: &str = r#"
hermes gateway stop || true
if systemctl --user cat hermes-dashboard.service >/dev/null 2>&1; then
  systemctl --user stop hermes-dashboard.service || exit 1
fi
hermes dashboard --stop || true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Execute the actual script with shell functions standing in for
    /// services. No real process is signalled and no test environment is
    /// mutated; exit codes model already-stopped and failed-stop cases.
    fn script_status(
        script: &str,
        gateway: i32,
        unit: i32,
        processes: i32,
    ) -> std::process::Output {
        let fixture = format!(
            "hermes() {{ return {gateway}; }}\nsystemctl() {{ if [ \"$2\" = cat ]; then return 0; fi; return {unit}; }}\npgrep() {{ return {processes}; }}\nsetsid() {{ return 0; }}\n{script}"
        );
        std::process::Command::new("bash")
            .args(["-c", &fixture])
            .output()
            .unwrap()
    }

    /// Idempotent suspension accepts an absent process even if the CLI
    /// reports already stopped, but never masks a failed unit stop or probe.
    #[test]
    fn suspend_requires_verified_stopped_services() {
        assert!(script_status(STOP_SCRIPT, 1, 0, 1).status.success());
        assert!(!script_status(STOP_SCRIPT, 0, 1, 1).status.success());
        assert!(require_stopped(1).is_ok());
        assert!(require_stopped(0).is_err());
        assert!(require_stopped(2).is_err());
        assert!(!STOP_SCRIPT.contains("pgrep"));
        assert!(!STOPPED_CHECK.contains("hermes"));
    }

    /// Exercise real pgrep against the shell's actual argv. A per-test
    /// marker substitutes for Hermes so a developer's running instance
    /// cannot affect the test, while bracket matching behaves identically.
    #[test]
    fn stopped_check_does_not_match_its_own_shell() {
        let check =
            STOPPED_CHECK.replace("ermes", &format!("kd-suspend-test-{}", std::process::id()));
        let out = std::process::Command::new("bash")
            .args(["-c", &check])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A successful dashboard start must not hide a failed gateway start.
    #[test]
    fn resume_reports_either_service_failure() {
        assert!(script_status(START_SCRIPT, 0, 0, 1).status.success());
        assert!(!script_status(START_SCRIPT, 1, 0, 1).status.success());
        assert!(!script_status(START_SCRIPT, 0, 1, 1).status.success());
    }
}
