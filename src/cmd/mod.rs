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
    fn ubiworker_destroy_with_two_names_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy", "a", "b"]));
    }

    #[test]
    fn ubiworker_destroy_long_all_flag_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy", "--all"]));
    }

    #[test]
    fn ubiworker_destroy_short_all_flag_parses() {
        assert!(parses(&["kd", "ubiworker", "destroy", "-a"]));
    }

    /// `--all` and an explicit name are mutually exclusive (`conflicts_with`
    /// in `DestroyArgs`): resolving both at once is nonsensical, and this
    /// guards against that wiring regressing silently.
    #[test]
    fn ubiworker_destroy_all_with_name_is_rejected() {
        assert!(!parses(&["kd", "ubiworker", "destroy", "--all", "myname"]));
    }

    /// Regression test for the removed `--yes`/`-y` flag: the interactive
    /// confirmation prompt it used to skip is gone entirely (see
    /// `SPEC.md`), so the flag itself must no longer parse.
    #[test]
    fn ubiworker_destroy_yes_flag_is_rejected() {
        assert!(!parses(&["kd", "ubiworker", "destroy", "myname", "--yes"]));
    }

    /// The short form of the removed flag is a separate parse path — `-a`
    /// still exists (as --all), so reintroducing only `-y` must be caught
    /// on its own.
    #[test]
    fn ubiworker_destroy_short_yes_flag_is_rejected() {
        assert!(!parses(&["kd", "ubiworker", "destroy", "-y", "myname"]));
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
