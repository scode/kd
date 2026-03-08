//! `kd` — a small personal CLI toolbox for GitHub repo management
//! and YouTube thumbnail operations.

mod cmd;

use clap::{ArgAction, Parser};
use tracing_subscriber::filter::LevelFilter;

#[derive(Parser)]
#[command(name = "kd")]
#[command(about = "Small personal toolbox")]
struct Cli {
    /// Increase verbosity (-v = DEBUG, -vv = TRACE)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Decrease verbosity (-q = WARN, -qq = ERROR, -qqq = OFF)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    quiet: u8,

    #[command(subcommand)]
    command: cmd::Commands,
}

/// Resolve log verbosity from the mutually-exclusive `-v/--verbose`
/// and `-q/--quiet` flags so `main` can stay focused on orchestration.
fn resolve_log_level(verbose: u8, quiet: u8) -> anyhow::Result<LevelFilter> {
    let level = match (verbose, quiet) {
        (0, 0) => LevelFilter::INFO,
        (1, 0) => LevelFilter::DEBUG,
        (_, 0) => LevelFilter::TRACE,
        (0, 1) => LevelFilter::WARN,
        (0, 2) => LevelFilter::ERROR,
        (0, _) => LevelFilter::OFF,
        _ => anyhow::bail!("Cannot use both --verbose and --quiet"),
    };

    Ok(level)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let level = resolve_log_level(cli.verbose, cli.quiet)?;

    // Log to stderr so stdout remains clean for machine-readable output.
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();

    cli.command.run()
}
