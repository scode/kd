//! `kd tlworker list`: show existing tlworker sandboxes.
//!
//! The listing and ownership filtering live in the parent module
//! ([`super::owned_workers`]) because the other subcommands resolve names
//! through the same listing; this module is only the printing layer.

use super::{Worker, api, owned_workers};
use tracing::info;

/// Entry point for `kd tlworker list`.
pub fn run() -> anyhow::Result<()> {
    let api = api::Api::new(api::load_credentials()?);
    let workers = owned_workers(&api)?;

    if workers.is_empty() {
        // Not an error, and deliberately not on stdout: a caller piping
        // this into another command should see a clean empty stream, not a
        // warning-shaped line to filter out. `-q` silences even this.
        info!("no tlworkers found");
    }

    for worker in &workers {
        println!("{}", render_row(worker));
    }
    Ok(())
}

/// One tab-separated output line: name, sandbox ID, shape, status.
///
/// The shape is printed because `create` lets it vary per worker, and the
/// status because a suspended worker is off the tailnet until resumed,
/// which is the first thing to check when `ssh` cannot reach one.
fn render_row(worker: &Worker) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        worker.name,
        worker.id,
        render_shape(worker.cpus, worker.memory_mb),
        worker.status
    )
}

/// CPUs and memory as `4 vCPU/16 GiB`. Memory is shown in GiB when it is a
/// whole number of them (every shape `create` makes) and in MiB otherwise,
/// so a hand-made sandbox with an odd size is shown exactly rather than
/// rounded. CPUs are fractional in Tensorlake's API (`0.5` is valid), so a
/// whole number drops the `.0` and anything else prints as given.
pub(super) fn render_shape(cpus: f64, memory_mb: u64) -> String {
    let memory = if memory_mb % 1024 == 0 {
        format!("{} GiB", memory_mb / 1024)
    } else {
        format!("{memory_mb} MiB")
    };
    format!("{cpus} vCPU/{memory}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column order is what scripts reading `list` depend on, so pin it.
    #[test]
    fn render_row_is_name_id_shape_status() {
        let worker = Worker {
            name: "tlworker-a".to_string(),
            id: "sbx123".to_string(),
            status: "running".to_string(),
            cpus: 4.0,
            memory_mb: 16384,
        };
        assert_eq!(
            render_row(&worker),
            "tlworker-a\tsbx123\t4 vCPU/16 GiB\trunning"
        );
    }

    /// Odd sizes are shown exactly rather than rounded into something the
    /// sandbox does not have.
    #[test]
    fn render_shape_keeps_odd_sizes_exact() {
        assert_eq!(render_shape(0.5, 1536), "0.5 vCPU/1536 MiB");
    }
}
