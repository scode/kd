//! GitHub operations — repo configuration, branch protection, PR triage.
//! All commands shell out to the `gh` CLI and require it to be authenticated.

pub mod pr;
pub mod repo;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Pull request operations
    Pr {
        #[command(subcommand)]
        cmd: pr::Commands,
    },
    /// Repository operations
    Repo {
        #[command(subcommand)]
        cmd: repo::Commands,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Pr { cmd } => cmd.run(),
            Commands::Repo { cmd } => cmd.run(),
        }
    }
}
