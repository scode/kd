//! `kd cli-proxy-api`: operate a CLIProxyAPI instance through its management
//! API. Specified in SPEC.md (`## kd cli-proxy-api`) and SPEC_impl.md.
//!
//! CLIProxyAPI is the Claude subscription router `kd devbox bootstrap`
//! installs. Its built-in routing strategies spread load but know nothing
//! about when each account's quota expires; the monitor supplies that
//! knowledge from outside, by rewriting account priorities, and is the only
//! part of kd that writes them.

pub mod api;
pub mod burn;
pub mod history;
pub mod monitor;
pub mod overview;
pub mod plan;
pub mod service;

use anyhow::Context;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Keep Claude accounts ordered by weekly reset and record usage history
    #[command(subcommand)]
    Monitor(MonitorCommands),
    /// Drain one Claude account first, ahead of reset order, until its weekly reset
    Burn(BurnArgs),
    /// Show the pool's state and each account's weekly-quota use over time
    Overview(OverviewArgs),
}

/// Flags for `overview`.
#[derive(Args, Debug)]
pub struct OverviewArgs {
    /// Chart as many 15-minute buckets as fit instead of each account's week in 4-hour ones
    #[arg(long)]
    pub recent: bool,
}

/// Flags for `burn`: an email to burn, or `--clear`.
#[derive(Args, Debug)]
pub struct BurnArgs {
    /// Email of the Claude account to put on top
    #[arg(required_unless_present = "clear", conflicts_with = "clear")]
    pub email: Option<String>,

    /// Remove the override and return to reset order
    #[arg(long)]
    pub clear: bool,
}

#[derive(Subcommand, Debug)]
pub enum MonitorCommands {
    /// Run the monitor loop in the foreground (what the systemd unit runs)
    Run(RunArgs),
    /// Install and start a systemd user unit that keeps the monitor running
    Enable,
    /// Stop the monitor and remove its systemd user unit
    Disable,
}

/// Flags for `monitor run`. The systemd unit passes none, so the defaults
/// are what the daemon uses; the flags exist for running it by hand, for
/// example against a proxy reached through an SSH tunnel.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// CLIProxyAPI root URL; plain http only to loopback (use an SSH tunnel)
    #[arg(long, default_value = "http://127.0.0.1:8317")]
    pub url: String,

    /// File holding the management key: a secrets.env with
    /// CLIPROXY_MANAGEMENT_KEY=..., or just the key [default:
    /// ~/.config/cliproxy/secrets.env]
    #[arg(long)]
    pub key_file: Option<PathBuf>,
}

/// Resolve `monitor run` flags against the home directory. The log path is
/// not a flag: the daemon and `overview` must always agree on it.
fn run_settings(args: RunArgs, home: PathBuf) -> monitor::Settings {
    monitor::Settings {
        url: args.url,
        key_file: args
            .key_file
            .unwrap_or_else(|| monitor::default_key_file(&home)),
        log_file: monitor::log_file(&home),
        config_file: burn::config_file(&home),
    }
}

/// The user's home directory. Every default path hangs off it; a systemd
/// user unit always has `HOME`, so its absence is a broken environment.
fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::Monitor(MonitorCommands::Run(args)) => {
                monitor::run(run_settings(args, home()?))
            }
            Commands::Monitor(MonitorCommands::Enable) => {
                let exe = std::env::current_exe()
                    .and_then(|p| p.canonicalize())
                    .context("locating the running kd binary")?;
                print!("{}", service::enable(&service::System, &home()?, &exe)?);
                Ok(())
            }
            Commands::Monitor(MonitorCommands::Disable) => {
                print!("{}", service::disable(&service::System, &home()?)?);
                Ok(())
            }
            Commands::Burn(args) => {
                let home = home()?;
                let config = burn::config_file(&home);
                let report = match args.email {
                    Some(email) => burn::set(
                        &config,
                        &monitor::log_file(&home),
                        &email,
                        jiff::Timestamp::now(),
                        &jiff::tz::TimeZone::system(),
                    )?,
                    None => burn::clear(&config)?,
                };
                print!("{report}");
                Ok(())
            }
            Commands::Overview(args) => {
                use std::io::IsTerminal;
                let home = home()?;
                // Eight days covers the week view plus a margin for the
                // readings just before its first edge; the recent view needs
                // less at any terminal width.
                let now = jiff::Timestamp::now();
                let since = now - jiff::SignedDuration::from_hours(8 * 24);
                let records = history::read_since(&monitor::log_file(&home), since)?;
                let burn = burn::read(&burn::config_file(&home));
                let span = if args.recent {
                    overview::Span::Recent
                } else {
                    overview::Span::Week
                };
                // Pipes get plain text at a fixed width; terminals get color
                // and their real width, unless NO_COLOR asks otherwise.
                let terminal = std::io::stdout().is_terminal();
                let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
                let style = if terminal && !no_color {
                    overview::Style::Color
                } else {
                    overview::Style::Plain
                };
                let width = terminal_size::terminal_size()
                    .filter(|_| terminal)
                    .map_or(100, |(w, _)| usize::from(w.0));
                print!(
                    "{}",
                    overview::render(
                        &records,
                        &burn,
                        span,
                        now,
                        &jiff::tz::TimeZone::system(),
                        width,
                        style
                    )
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key file defaults to where bootstrap writes it and can be
    /// overridden; the log always lands at the fixed path under HOME.
    #[test]
    fn run_settings_resolve_against_home() {
        let args = |key_file: Option<&str>| RunArgs {
            url: "http://127.0.0.1:8317".to_owned(),
            key_file: key_file.map(PathBuf::from),
        };
        let home = PathBuf::from("/home/me");
        let s = run_settings(args(None), home.clone());
        assert_eq!(
            s.key_file,
            PathBuf::from("/home/me/.config/cliproxy/secrets.env")
        );
        assert_eq!(
            s.log_file,
            PathBuf::from("/home/me/.local/state/kd/cli-proxy-api-monitor.jsonl")
        );
        assert_eq!(
            run_settings(args(Some("/k")), home).key_file,
            PathBuf::from("/k")
        );
    }
}
