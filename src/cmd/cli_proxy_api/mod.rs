//! `kd cli-proxy-api`: operate a CLIProxyAPI instance through its management
//! API. Specified in SPEC.md (`## kd cli-proxy-api manage-priorities`) and
//! SPEC_impl.md.
//!
//! CLIProxyAPI is the Claude subscription router `kd devbox bootstrap`
//! installs. Its built-in routing strategies spread load but know nothing
//! about when each account's quota expires; this command supplies that
//! knowledge from outside, by rewriting account priorities.

pub mod api;
pub mod manage_priorities;
pub mod plan;

use anyhow::Context;
use clap::{Args, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Order Claude accounts by weekly quota reset so expiring quota is used first
    ManagePriorities(ManagePrioritiesArgs),
}

/// Flags for `manage-priorities`. A bare invocation is a dry run against
/// the local proxy, using the key file bootstrap writes.
#[derive(Args, Debug)]
pub struct ManagePrioritiesArgs {
    /// CLIProxyAPI root URL; plain http only to loopback (use an SSH tunnel)
    #[arg(long, default_value = "http://127.0.0.1:8317")]
    pub url: String,

    /// File holding the management key: a secrets.env with
    /// CLIPROXY_MANAGEMENT_KEY=..., or just the key [default:
    /// ~/.config/cliproxy/secrets.env]
    #[arg(long)]
    pub key_file: Option<PathBuf>,

    /// Append the run record here [default:
    /// $XDG_STATE_HOME/kd/cli-proxy-api-priorities.jsonl]
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Write the planned priorities; without this nothing is changed
    #[arg(long)]
    pub apply: bool,
}

/// Resolve flags to concrete paths. `HOME` is only needed for a default
/// path, so a service environment without it still works when both paths
/// are given. Environment values are passed in so tests do not touch the
/// process environment.
fn settings(
    args: ManagePrioritiesArgs,
    home: Option<PathBuf>,
    xdg_state_home: Option<OsString>,
) -> anyhow::Result<manage_priorities::Settings> {
    let home = || {
        home.clone()
            .context("HOME is not set; pass --key-file and --log-file")
    };
    let key_file = match args.key_file {
        Some(path) => path,
        None => manage_priorities::default_key_file(&home()?),
    };
    let log_file = match args.log_file {
        Some(path) => path,
        None => manage_priorities::default_log_file(xdg_state_home.as_deref(), &home()?),
    };
    Ok(manage_priorities::Settings {
        url: args.url,
        key_file,
        log_file,
        apply: args.apply,
    })
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::ManagePriorities(args) => {
                let settings = settings(
                    args,
                    std::env::var_os("HOME").map(PathBuf::from),
                    std::env::var_os("XDG_STATE_HOME"),
                )?;
                manage_priorities::run(settings)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(key_file: Option<&str>, log_file: Option<&str>) -> ManagePrioritiesArgs {
        ManagePrioritiesArgs {
            url: "http://127.0.0.1:8317".to_owned(),
            key_file: key_file.map(PathBuf::from),
            log_file: log_file.map(PathBuf::from),
            apply: false,
        }
    }

    /// A timer or service unit may run without HOME; with both paths given
    /// that must not matter, and without them the error says what to pass.
    #[test]
    fn home_is_only_needed_for_default_paths() {
        let s = settings(args(Some("/k"), Some("/l")), None, None).unwrap();
        assert_eq!(
            (s.key_file, s.log_file),
            (PathBuf::from("/k"), PathBuf::from("/l"))
        );
        let err = settings(args(Some("/k"), None), None, None).unwrap_err();
        assert!(err.to_string().contains("pass --key-file and --log-file"));
        let s = settings(args(None, None), Some(PathBuf::from("/home/me")), None).unwrap();
        assert_eq!(
            s.key_file,
            PathBuf::from("/home/me/.config/cliproxy/secrets.env")
        );
    }
}
