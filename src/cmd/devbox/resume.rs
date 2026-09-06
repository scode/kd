//! `kd devbox resume`: start Hermes on the devbox again.
//!
//! The inverse of `suspend`, including after abandoning a move. Nothing else:
//! no archive handling, no preflight beyond loading the profile.

use super::{ResumeArgs, hermes, transport::Transport};

/// Restart the named source's services without importing an archive or
/// changing which machine the profile identifies.
pub fn run(args: ResumeArgs) -> anyhow::Result<()> {
    let profile = args.profile.load()?;
    let t = Transport::ssh(format!("{}@{}", profile.user, profile.host));
    hermes::start(&t)?;
    println!("Hermes started on {}", profile.host);
    Ok(())
}

/// Stop the named instance without taking a backup. Keeping service
/// control separate lets a failed backup leave a suspended source alone.
pub fn suspend(args: ResumeArgs) -> anyhow::Result<()> {
    let profile = args.profile.load()?;
    let t = Transport::ssh(format!("{}@{}", profile.user, profile.host));
    hermes::stop(&t)?;
    println!("Hermes suspended on {}", profile.host);
    Ok(())
}
