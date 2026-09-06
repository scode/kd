//! `kd devbox resume`: start Hermes on the devbox again.
//!
//! The "never mind" after a `backup` that left Hermes stopped. Nothing else:
//! no archive handling, no preflight beyond loading the profile.

use super::{ResumeArgs, hermes, transport::Transport};

pub fn run(args: ResumeArgs) -> anyhow::Result<()> {
    let profile = args.profile.load()?;
    let t = Transport::ssh(format!("{}@{}", profile.user, profile.host));
    hermes::start(&t)?;
    println!("Hermes started on {}", profile.host);
    Ok(())
}
