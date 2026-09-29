//! Pull-request-level GitHub commands. Unlike `gh repo`, these are about
//! the authenticated user's PRs across all of GitHub, not about one repo.

pub mod list;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ListArgs {
    /// Also list PRs to repositories you own (excluded by default)
    #[arg(long)]
    pub include_mine: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List your open and recently closed PRs by others' latest activity
    ///
    /// Shows every PR you authored that is open or was closed (merged or
    /// not) in the last 7 days, with how long ago someone other than you
    /// last commented, reviewed, pushed, or otherwise acted on it, most
    /// recent first. PRs to repositories owned by your own account are left
    /// out unless --include-mine is given.
    List(ListArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::List(args) => list::run(args),
        }
    }
}
