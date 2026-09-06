//! `kd devbox`: bootstrap environments and move a stateful instance.
//!
//! Commands specified in SPEC.md (`## kd devbox`) and SPEC_impl.md:
//!
//! - `backup`: pull a Hermes archive without changing service state.
//! - `suspend` / `resume`: stop / start the stateful instance's services.
//! - `bootstrap`: configure an explicit SSH target from shared settings,
//!   optionally restoring a named instance's backup.
//!
//! `kd` itself never wipes anything; the reinstall happens by hand between
//! `backup` and `bootstrap`. The governing constraint is size: Rust owns
//! only what an agent cannot or must not do (transport, secrets, ordering,
//! the reboot, the guard, prompts, the probe), and everything drift-prone
//! lives in the agent prompts. SPEC_impl.md's NOTE paragraphs list the
//! decisions that are deliberate; do not "fix" them.

pub mod agent;
pub mod backup;
pub mod bootstrap;
pub mod hermes;
pub mod probe;
pub mod profile;
pub mod prompts;
pub mod resume;
pub mod secrets;
pub mod transport;

use anyhow::Context;
use clap::{Args, Subcommand};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use profile::ResolvedProfile;

/// Stateful operations always name their source instance explicitly.
#[derive(Args, Debug, Clone)]
pub struct ProfileArg {
    /// Profile name from devboxes.toml
    #[arg(long)]
    pub profile: String,
}

impl ProfileArg {
    /// Locate and load the named profile. Reads `XDG_CONFIG_HOME` and `HOME`
    /// here, at the edge, and hands plain values down so everything below is
    /// testable without the environment.
    pub fn load(&self) -> anyhow::Result<ResolvedProfile> {
        let home = home_dir()?;
        let path = profile::config_path(std::env::var_os("XDG_CONFIG_HOME").as_deref(), &home);
        profile::load(&path, &self.profile, &home)
    }
}

/// The controller's home directory, from `HOME`. Failing here rather than
/// defaulting is right: every path in a profile may be relative to it.
pub fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// One yes/no question on stderr, answered on stdin. Anything but `y`/`yes`
/// (case-insensitive) is "no", including EOF, so a non-interactive run that
/// forgot `--yes` stops rather than proceeds.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    let mut err = std::io::stderr();
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// An instruction the user carries out by hand, acknowledged with Enter.
/// Not a yes/no: there is nothing to decline, only something to do first.
/// EOF is an error rather than an acknowledgement, for the same reason
/// [`confirm`] treats it as "no".
pub fn wait_for_enter(instruction: &str) -> anyhow::Result<()> {
    let mut err = std::io::stderr();
    writeln!(err, "{instruction}")?;
    err.flush()?;
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("stdin closed while waiting for you to press Enter");
    }
    Ok(())
}

#[derive(Args, Debug, Clone)]
pub struct BackupArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Skip the "ok to proceed?" question (the preflight report is still printed)
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ResumeArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
}

#[derive(Args, Debug, Clone)]
pub struct BootstrapArgs {
    /// SSH destination; a bare host uses the shared bootstrap user
    #[arg(long, value_name = "USER@HOST")]
    pub target: String,
    /// Restore the newest backup of this named instance (currently Hermes)
    #[arg(long, value_name = "PROFILE")]
    pub restore: Option<String>,
    /// OS hostname; required from scratch, otherwise taken from the restore profile
    #[arg(long, required_unless_present = "restore")]
    pub hostname: Option<String>,
    /// Reach the target with plain ssh even if it is a tailnet peer
    #[arg(long)]
    pub plain_ssh: bool,
    /// Restore for inspection without enabling or starting restored services
    #[arg(long, requires = "restore", conflicts_with = "enroll_tailscale")]
    pub rehearsal: bool,
    /// Install and enroll Tailscale if the target is not already enrolled
    #[arg(long)]
    pub enroll_tailscale: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Back up the devbox's Hermes state to the controller
    Backup(BackupArgs),
    /// Stop Hermes on the named instance before a consistent backup or move
    Suspend(ResumeArgs),
    /// Start Hermes on the named instance again after suspend
    Resume(ResumeArgs),
    /// Bootstrap an SSH target, optionally restoring a named instance's backup
    Bootstrap(BootstrapArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Backup(args) => backup::run(args),
            Commands::Suspend(args) => resume::suspend(args),
            Commands::Resume(args) => resume::run(args),
            Commands::Bootstrap(args) => bootstrap::run(args),
        }
    }
}
