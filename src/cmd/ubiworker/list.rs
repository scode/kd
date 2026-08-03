//! `kd ubiworker list`: show existing ubiworker VMs.
//!
//! All the parsing/filtering logic lives in the parent module
//! ([`super::owned_workers`]) so `destroy` (when no name is given) can reuse
//! it; this module is just the thin printing layer on top.

use super::{owned_workers, require_env};
use tracing::info;
use xshell::Shell;

pub fn run() -> anyhow::Result<()> {
    // Same fail-fast rationale as `create`: `ubi` reads UBI_TOKEN itself,
    // kd just surfaces a missing token as a clear error up front.
    require_env("UBI_TOKEN")?;
    let sh = Shell::new()?;
    let workers = owned_workers(&sh)?;

    if workers.is_empty() {
        // Not an error, and deliberately not printed to stdout: a caller
        // piping this into another command should see a clean empty
        // stream, not a warning-shaped line to filter out. `-q` silences
        // even this via the usual tracing level. No early return needed —
        // the loop below is simply empty, so both paths fall through to
        // the same `Ok(())`.
        info!("no ubiworkers found");
    }

    for worker in workers {
        // `ip4` is `None` for a VM `ubi` hasn't assigned an IPv4 to yet
        // (see `VmRow` docs); render that as `-` rather than an empty
        // column, which would be easy to misread as a parsing bug.
        println!(
            "{}\t{}\t{}",
            worker.name,
            worker.id,
            worker.ip4.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}
