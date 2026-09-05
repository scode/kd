//! Starting and stopping Hermes on the devbox without naming its units.
//!
//! `backup` stops Hermes before the archive and `resume` starts it again;
//! both go through Hermes's own CLI (`hermes gateway stop|start`,
//! `hermes dashboard --stop`) rather than `systemctl --user <unit>`, because
//! the devbox being backed up predates kd and may carry a hand-written
//! dashboard unit under any name. The one exception is starting the
//! dashboard: `hermes dashboard` runs in the foreground and would die with
//! the ssh session, so start goes through the unit when one exists and
//! `setsid -f` otherwise. SPEC_impl.md "Paths and names" records this.

use super::transport::Transport;
use tracing::{info, warn};

/// Stop the gateway and the dashboard. Errors are ignored on purpose: if
/// nothing was running there is nothing to stop, and the caller is about to
/// take a backup either way.
pub fn stop(t: &Transport) -> anyhow::Result<()> {
    info!("stopping Hermes gateway and dashboard on {}", t.destination);
    let out = t.capture("hermes gateway stop; hermes dashboard --stop; true")?;
    if !out.success() {
        warn!(
            "stopping Hermes reported a problem (continuing):\n{}",
            out.stderr.trim_end()
        );
    }
    Ok(())
}

/// Start the gateway and the dashboard. This is what `resume` does and what
/// `backup` does on its failure path.
pub fn start(t: &Transport) -> anyhow::Result<()> {
    info!("starting Hermes gateway and dashboard on {}", t.destination);
    t.run(START_SCRIPT)
}

const START_SCRIPT: &str = r#"
hermes gateway start
if systemctl --user cat hermes-dashboard.service >/dev/null 2>&1; then
  systemctl --user start hermes-dashboard.service
else
  setsid -f hermes dashboard --host 127.0.0.1 --port 9119 --no-open
fi
"#;
