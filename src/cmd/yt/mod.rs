pub mod thumb;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Thumbnail operations
    Thumb {
        #[command(subcommand)]
        cmd: thumb::Commands,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Thumb { cmd } => cmd.run(),
        }
    }
}
