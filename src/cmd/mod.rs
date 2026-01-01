pub mod yt;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// YouTube related commands
    Yt {
        #[command(subcommand)]
        cmd: yt::Commands,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Yt { cmd } => cmd.run(),
        }
    }
}
