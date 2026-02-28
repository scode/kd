pub mod apply_preferred_settings;
pub mod main_protect;

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

    /// Show settings changes without applying them
    #[arg(long)]
    pub dry_run: bool,

    /// Skip confirmation prompts (useful with --all)
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct MainProtectArgs {
    /// Repository name (e.g., owner/repo). Detected from current directory if omitted.
    pub repo: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Apply preferred merge settings to repositories
    ApplyPreferredSettings(ApplyPreferredSettingsArgs),
    /// Create/update a "main-protect" ruleset on the default branch
    ///
    /// Ensures a ruleset named "main-protect" exists with required linear history
    /// and force-push blocking, then lets you interactively pick which CI status
    /// checks should be required to pass before merging.
    MainProtect(MainProtectArgs),
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::ApplyPreferredSettings(args) => apply_preferred_settings::run(args),
            Commands::MainProtect(args) => main_protect::run(args),
        }
    }
}
