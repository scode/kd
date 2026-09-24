//! Running a Codex phase on the target.
//!
//! Codex runs on the box being configured, as the user, so a yolo agent's
//! filesystem reach is exactly the machine it is supposed to configure. kd
//! streams the prompt in on stdin, lets the agent's output flow to the
//! terminal, and afterwards reads back the agent's final message, which by
//! contract ends with a "Workarounds" section. Nothing is parsed out of that
//! message; it is printed whole after the probe.
//!
//! Model and effort are two constants so a rename is a one-line change. A
//! different model is a config change, never a silent retry.
//!
//! Every phase selects Codex's built-in provider explicitly. Bootstrap makes
//! codex-lb the default provider, and codex-lb has no accounts until the user
//! logs in after bootstrap, so on a rerun the configured default would fail.
//! The agent always runs on the native login kd copied from the controller.

use super::routers::CODEX_NATIVE_OVERRIDE;
use super::transport::Transport;
use anyhow::Context;
use tracing::info;

/// The model every phase runs on.
pub const MODEL: &str = "gpt-6-sol";
/// Reasoning effort, passed as a config override (Codex has no flag for it).
pub const EFFORT: &str = "high";
/// Where Codex is installed by seeding; invoked by path so the phase does not
/// depend on the login shell having picked up `~/.local/bin` yet.
pub const CODEX_BIN: &str = ".local/bin/codex";
/// Home-relative path of the agent's final message, overwritten per phase.
pub const LAST_MESSAGE: &str = ".kd-agent-last-message.md";

/// Run one phase. Returns the agent's final message on success. On failure
/// the final message, if any, is printed before the error is returned, so
/// the user sees what the agent thought went wrong.
pub fn run_phase(t: &Transport, name: &str, prompt: &str) -> anyhow::Result<String> {
    info!(
        "starting {name} phase with {MODEL} ({EFFORT}) on {}",
        t.destination
    );
    let result = t.run_with_stdin(&phase_command(), prompt.as_bytes());
    let last = last_message(t);
    match result {
        Ok(()) => Ok(last),
        Err(e) => {
            if !last.trim().is_empty() {
                eprintln!("\n--- agent's final message ({name} phase) ---\n{last}");
            }
            Err(e).with_context(|| format!("{name} phase failed"))
        }
    }
}

/// The remote command for one phase; the prompt arrives on stdin.
fn phase_command() -> String {
    format!(
        "\"$HOME\"/{CODEX_BIN} exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
         {CODEX_NATIVE_OVERRIDE} -m {MODEL} -c model_reasoning_effort={EFFORT} -o \"$HOME\"/{LAST_MESSAGE}"
    )
}

fn last_message(t: &Transport) -> String {
    t.capture(&format!("cat \"$HOME\"/{LAST_MESSAGE} 2>/dev/null"))
        .map(|c| c.stdout)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rerun finds config.toml already pointing at codex-lb, which has no
    /// accounts until the user logs in. Without the explicit native provider
    /// every phase after the first bootstrap would fail on such a box.
    #[test]
    fn phases_bypass_the_codex_router() {
        let command = phase_command();
        assert!(
            command.contains(r#"-c 'model_provider="openai"'"#),
            "{command}"
        );
        assert!(command.contains("-m gpt-6-sol -c model_reasoning_effort=high"));
    }
}
