//! `kd cli-proxy-api`: operate a CLIProxyAPI instance through its management
//! API. Specified in SPEC.md (`## kd cli-proxy-api`) and SPEC_impl.md.
//!
//! CLIProxyAPI is the Claude subscription router `kd devbox bootstrap`
//! installs. Its built-in routing strategies spread load but know nothing
//! about when each account's quota expires; the monitor supplies that
//! knowledge from outside, by rewriting account priorities, and is the only
//! part of kd that writes them.

pub mod api;
pub mod monitor;
pub mod plan;

use anyhow::Context;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Keep Claude accounts ordered by weekly reset and record usage history
    #[command(subcommand)]
    Monitor(MonitorCommands),
}

#[derive(Subcommand, Debug)]
pub enum MonitorCommands {
    /// Run the monitor loop in the foreground (what the systemd unit runs)
    Run(RunArgs),
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
