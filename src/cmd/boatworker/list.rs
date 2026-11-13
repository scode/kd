//! `kd boatworker list`: show existing boatworker sandboxes.
//!
//! The parsing and ownership filtering live in the parent module
//! ([`super::owned_workers`]) because `destroy` and `ssh` resolve names
//! through the same listing; this module is only the printing layer.
//!
//! Names are printed as boat reports them, unescaped. Boat would accept a
//! name containing a tab or newline, which would break the columns; that is
//! accepted rather than handled, since nobody names a worker that way on
//! purpose.

use super::{Worker, owned_workers};
use tracing::info;
use xshell::Shell;

/// Entry point for `kd boatworker list`. An incomplete listing has already
/// been warned about by [`owned_workers`]; `list` prints what it can see.
pub fn run() -> anyhow::Result<()> {
    let sh = Shell::new()?;
    let workers = owned_workers(&sh)?.workers;

    if workers.is_empty() {
        // Not an error, and deliberately not on stdout: a caller piping
        // this into another command should see a clean empty stream, not a
        // warning-shaped line to filter out. `-q` silences even this.
        info!("no boatworkers found");
    }

    for worker in &workers {
        println!("{}", render_row(worker));
    }
    Ok(())
}

/// One tab-separated output line: name, id, machine type, state.
///
/// ubiworker prints name, id and IPv4. A boatworker's address is not
/// something to connect to (`kd boatworker ssh` goes through boat by id, and
/// the tailnet name is the worker name), so this prints what is useful
/// instead: the machine type, since `create --type` makes it vary per
/// worker, and boat's state, which shows a worker that is still
/// provisioning or went to `error`.
fn render_row(worker: &Worker) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        worker.name, worker.id, worker.machine_type, worker.state
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column order is what scripts reading `list` depend on, so pin it.
    #[test]
    fn render_row_is_name_id_type_state() {
        let worker = Worker {
            name: "boatworker-a".to_string(),
            id: "bx_aaaaaaaa".to_string(),
            state: "idle".to_string(),
            machine_type: "large".to_string(),
        };
        assert_eq!(
            render_row(&worker),
            "boatworker-a\tbx_aaaaaaaa\tlarge\tidle"
        );
    }
}
