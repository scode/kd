pub mod gh;
pub mod yt;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// GitHub related commands
    Gh {
        #[command(subcommand)]
        cmd: gh::Commands,
    },
    /// YouTube related commands
    Yt {
        #[command(subcommand)]
        cmd: yt::Commands,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Gh { cmd } => cmd.run(),
            Commands::Yt { cmd } => cmd.run(),
        }
    }
}
