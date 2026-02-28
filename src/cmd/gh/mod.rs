//! GitHub operations — repo configuration, branch protection, etc.
//! All commands shell out to the `gh` CLI and require it to be authenticated.

pub mod repo;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Repository operations
    Repo {
        #[command(subcommand)]
        cmd: repo::Commands,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Repo { cmd } => cmd.run(),
        }
    }
}
