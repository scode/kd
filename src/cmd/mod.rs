//! Top-level command dispatch. Each submodule owns a domain of functionality.

pub mod gh;
pub mod ubiworker;
pub mod yt;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// GitHub related commands
    Gh {
        #[command(subcommand)]
        cmd: gh::Commands,
    },
    /// Ubicloud worker VM commands
    Ubiworker {
        #[command(subcommand)]
        cmd: ubiworker::Commands,
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
            Commands::Ubiworker { cmd } => cmd.run(),
            Commands::Yt { cmd } => cmd.run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Wraps [`Commands`] in the one piece of clap machinery it needs to be
    /// parsed on its own (`#[derive(Parser)]`'s top-level entry point),
    /// without depending on the real `Cli` struct in `main.rs` — that lives
    /// in the binary crate root, not this library-shaped module tree, so
    /// it isn't reachable from here.
    ///
    /// These tests are deliberately narrow: they exercise argv *wiring*
    /// (does this subcommand shape parse, does clap reject an unknown one)
    /// rather than command behavior, which each command's own module tests.
    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: Commands,
    }

    fn parses(args: &[&str]) -> bool {
        TestCli::try_parse_from(args).is_ok()
    }

    #[test]
    fn ubiworker_create_parses() {
        assert!(parses(&["kd", "ubiworker", "create"]));
    }

    #[test]
    fn ubiworker_create_with_name_parses() {
        assert!(parses(&["kd", "ubiworker", "create", "myname"]));
    }

    #[test]
    fn ubiworker_destroy_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy"]));
    }

    #[test]
    fn ubiworker_destroy_with_name_and_long_yes_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy", "myname", "--yes"]));
    }

    #[test]
    fn ubiworker_destroy_short_yes_flag_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy", "-y", "myname"]));
    }

    #[test]
    fn ubiworker_list_parses() {
        assert!(parses(&["kd", "ubiworker", "list"]));
    }

    /// clap must reject an unknown subcommand rather than silently
    /// swallowing it or matching the wrong one.
    #[test]
    fn ubiworker_unknown_subcommand_is_rejected() {
        assert!(!parses(&["kd", "ubiworker", "frobnicate"]));
    }
}
