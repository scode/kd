pub mod apply_preferred_settings;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ApplyPreferredSettingsArgs {
    /// Repository name (e.g., owner/repo)
    #[arg(required_unless_present = "all")]
    pub repo: Option<String>,

    /// Apply to all non-fork, non-archived repositories (with confirmation)
    #[arg(long)]
    pub all: bool,

    /// Force update even if settings already match
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Apply preferred merge settings to repositories
    ApplyPreferredSettings(ApplyPreferredSettingsArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::ApplyPreferredSettings(args) => apply_preferred_settings::run(args),
        }
    }
}
