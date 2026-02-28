//! YouTube thumbnail operations. Thumbnails have a 2 MB upload limit,
//! so the main operation here is resizing images to fit.

pub mod resize;

use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Commands {
    /// Resize image to be under 2MB for YouTube thumbnail upload
    Resize {
        /// Path to the image file
        file: PathBuf,
    },
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Resize { file } => resize::run(&file),
        }
    }
}
