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

use super::transport::Transport;
use anyhow::Context;
use tracing::info;

/// The model every phase runs on.
pub const MODEL: &str = "gpt-6-astra";
/// Reasoning effort, passed as a config override (Codex has no flag for it).
pub const EFFORT: &str = "medium";
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
    let script = format!(
        "\"$HOME\"/{CODEX_BIN} exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
         -m {MODEL} -c model_reasoning_effort={EFFORT} -o \"$HOME\"/{LAST_MESSAGE}"
    );
    let result = t.run_with_stdin(&script, prompt.as_bytes());
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

fn last_message(t: &Transport) -> String {
    t.capture(&format!("cat \"$HOME\"/{LAST_MESSAGE} 2>/dev/null"))
        .map(|c| c.stdout)
        .unwrap_or_default()
}
