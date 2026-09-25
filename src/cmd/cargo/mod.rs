//! `kd cargo`: helpers around tools installed with `cargo install`.
//! Specified in SPEC.md (`## kd cargo scode-update`).
//!
//! `scode-update` exists because devbox bootstrap installs kd, and possibly
//! other tools, straight from their GitHub repositories under `scode/`, and
//! updating exactly those is otherwise a two-step chore: remember which
//! installed tools came from those repositories, then remember that
//! cargo-update needs `-g` to touch git installs at all. The command finds
//! them in `cargo install --list` and hands them to cargo-update, which
//! already knows how to compare a git install with its branch head and
//! rebuild only what moved.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use xshell::{Shell, cmd};

/// Source prefixes, one per URL form cargo may have recorded, that select
/// the tools `scode-update` updates. cargo records the URL exactly as it was
/// given to `cargo install --git`, so the same repository can appear as
/// HTTPS, plain HTTP, or SSH. Compiled in on purpose: the command's name
/// promises exactly this owner.
const SCODE_PREFIXES: [&str; 3] = [
    "https://github.com/scode/",
    "http://github.com/scode/",
    "ssh://git@github.com/scode/",
];

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Update every tool installed from a github.com/scode repository, kd included
    ScodeUpdate(ScodeUpdateArgs),
}

#[derive(Args, Debug)]
pub struct ScodeUpdateArgs {
    /// Show which tools would be updated and the command, without running it
    #[arg(long)]
    pub dry_run: bool,
}

impl Commands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            Commands::ScodeUpdate(args) => scode_update(args.dry_run),
        }
    }
}

/// One `cargo install --list` entry: `name vVERSION (SOURCE):` followed by
/// indented binary names. Registry installs have no parenthesised source.
#[derive(Debug, PartialEq)]
struct Installed {
    name: String,
    source: Option<String>,
}

/// Parse `cargo install --list`. Only the header lines matter; binary lines
/// are indented and skipped. A header that does not look like
/// `name vVERSION ...:` is skipped rather than failing the run, since the
/// command only needs to recognise the entries it updates.
fn parse_install_list(text: &str) -> Vec<Installed> {
    text.lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            let header = line.trim_end().strip_suffix(':')?;
            let (name, rest) = header.split_once(' ')?;
            if !rest.starts_with('v') {
                return None;
            }
            let source = rest
                .split_once(" (")
                .and_then(|(_, s)| s.strip_suffix(')'))
                .map(str::to_owned);
            Some(Installed {
                name: name.to_owned(),
                source,
            })
        })
        .collect()
}

/// Whether an install came from a repository under github.com/scode, in any
/// URL form cargo may have recorded. Matches the owner exactly, ignoring
/// case as GitHub does, so an owner that merely starts with "scode"
/// (github.com/scodeX/...) does not count.
fn is_scode_source(source: &str) -> bool {
    SCODE_PREFIXES.iter().any(|prefix| {
        source.len() > prefix.len()
            && source.is_char_boundary(prefix.len())
            && source[..prefix.len()].eq_ignore_ascii_case(prefix)
    })
}

/// Whether a git install is pinned to a tag or a commit. cargo-update only
/// understands `?branch=`: it would compare a pinned install with the
/// default branch and reinstall it from there, silently dropping the pin
/// (confirmed against cargo-update 22.1.1). Such tools are skipped.
fn is_pinned(source: &str) -> bool {
    let query = source
        .split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    query
        .split('&')
        .any(|pair| pair.starts_with("tag=") || pair.starts_with("rev="))
}

/// What `scode-update` does with the installed tools from scode repositories.
#[derive(Debug, Default, PartialEq)]
struct Selection {
    /// Tools to update, in listing order.
    update: Vec<String>,
    /// Tools skipped because they are pinned, with their recorded source.
    pinned: Vec<(String, String)>,
}

fn select(installed: &[Installed]) -> Selection {
    let mut selection = Selection::default();
    for i in installed {
        let Some(source) = i.source.as_deref().filter(|s| is_scode_source(s)) else {
            continue;
        };
        if is_pinned(source) {
            selection.pinned.push((i.name.clone(), source.to_owned()));
        } else {
            selection.update.push(i.name.clone());
        }
    }
    selection
}

/// Arguments that mark one tool's cargo-update reinstalls as locked
/// (`enforce_lock = true` in its per-package config), so it is built with
/// the dependency versions in its repository's committed `Cargo.lock`.
/// Idempotent, and a no-op for tools devbox bootstrap already configured.
///
/// This per-package setting is the only way to lock these updates. Passing
/// cargo-update's global `--locked` as well would make it hand `--locked` to
/// cargo twice for every configured tool, and cargo rejects that ("cannot be
/// used multiple times"), so the update would fail exactly when there is
/// something to install.
fn lock_args(tool: &str) -> Vec<String> {
    vec![
        "install-update-config".to_owned(),
        "--enforce-lock".to_owned(),
        tool.to_owned(),
    ]
}

/// Arguments for the update itself. `-g` is required: cargo-update skips
/// git-installed packages without it. No `--locked`; see [`lock_args`].
fn update_args(tools: &[String]) -> Vec<String> {
    let mut args = vec!["install-update".to_owned(), "-g".to_owned()];
    args.extend(tools.iter().cloned());
    args
}

fn scode_update(dry_run: bool) -> anyhow::Result<()> {
    let sh = Shell::new()?;
    let listing = cmd!(sh, "cargo install --list")
        .quiet()
        .read()
        .context("running `cargo install --list`")?;
    let selection = select(&parse_install_list(&listing));
    for (name, source) in &selection.pinned {
        println!("skipping {name}: pinned to a tag or commit ({source})");
    }
    if selection.update.is_empty() {
        println!("no unpinned tools installed from github.com/scode; nothing to update");
        return Ok(());
    }
    let tools = &selection.update;
    println!("tools from github.com/scode: {}", tools.join(", "));
    if dry_run {
        for tool in tools {
            println!("dry run: would run `cargo {}`", lock_args(tool).join(" "));
        }
        println!(
            "dry run: would run `cargo {}`",
            update_args(tools).join(" ")
        );
        return Ok(());
    }
    // Checked up front so a missing cargo-update gets a useful message
    // instead of cargo's generic "no such command".
    if cmd!(sh, "cargo install-update --help")
        .quiet()
        .ignore_stdout()
        .ignore_stderr()
        .run()
        .is_err()
    {
        bail!(
            "`cargo install-update` is not available; install cargo-update (`brew install cargo-update` or `cargo install cargo-update`)"
        );
    }
    for tool in tools {
        let args = lock_args(tool);
        cmd!(sh, "cargo {args...}")
            .quiet()
            .ignore_stdout()
            .run()
            .with_context(|| format!("marking {tool}'s updates as locked"))?;
    }
    // Output streams through (xshell echoes the command first), so
    // cargo-update's own progress and per-tool results are what the user
    // sees. Updating kd itself while this kd runs is fine: cargo installs by
    // writing a new file and renaming it into place, and the running process
    // keeps its old inode.
    let args = update_args(tools);
    cmd!(sh, "cargo {args...}")
        .run()
        .context("cargo install-update failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of real `cargo install --list` output: registry installs have
    /// no source, git installs carry the URL exactly as it was given at
    /// install time (a `.git` suffix included) plus `#commit`, binaries are
    /// indented.
    const LISTING: &str = "\
cargo-update v22.1.1:
    cargo-install-update
    cargo-install-update-config
kd v0.1.0 (https://github.com/scode/kd#59c1f5e4):
    kd
other v1.0.0 (https://github.com/someone/other#abc123):
    other
voice v0.2.0 (https://github.com/Scode/voice.git?branch=main#def456):
    voice
lookalike v0.1.0 (https://github.com/scodex/lookalike#1):
    lookalike
pinned v0.3.0 (https://github.com/scode/pinned?tag=v0.3.0#aaa111):
    pinned
revved v0.3.0 (https://github.com/scode/revved?rev=abc#bbb222):
    revved
viassh v0.1.0 (ssh://git@github.com/scode/viassh#ccc333):
    viassh
local v0.1.0 (/home/me/src/local):
    local
";

    /// The parser must read every header, keep git and path sources, and
    /// ignore binary lines.
    #[test]
    fn parses_headers_and_sources() {
        let installed = parse_install_list(LISTING);
        assert_eq!(installed.len(), 9);
        assert_eq!(
            installed[0],
            Installed {
                name: "cargo-update".to_owned(),
                source: None
            }
        );
        assert_eq!(
            installed[1].source.as_deref(),
            Some("https://github.com/scode/kd#59c1f5e4")
        );
    }

    /// Exactly the scode-owned git installs are selected, in any URL form
    /// cargo records (HTTPS with or without `.git`, SSH), owner case
    /// ignored; not other owners, not an owner that only starts with
    /// "scode", not registry or path installs. Tag and commit pins are
    /// reported separately, because cargo-update would silently unpin them.
    #[test]
    fn selects_scode_repositories_and_skips_pins() {
        let selection = select(&parse_install_list(LISTING));
        assert_eq!(
            selection.update,
            vec!["kd".to_owned(), "voice".to_owned(), "viassh".to_owned()]
        );
        let pinned: Vec<&str> = selection.pinned.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(pinned, vec!["pinned", "revved"]);
    }

    /// A branch is not a pin: cargo-update follows `?branch=` correctly.
    #[test]
    fn branch_is_not_a_pin() {
        assert!(!is_pinned("https://github.com/scode/x?branch=main#1"));
        assert!(!is_pinned("https://github.com/scode/x#tag"));
        assert!(is_pinned("https://github.com/scode/x?rev=1#1"));
        assert!(is_pinned("https://github.com/scode/x?branch=a&tag=b#1"));
    }

    /// The update must not pass cargo-update's global `--locked`: for a tool
    /// with `enforce_lock` set (every tool bootstrap installs, and every
    /// tool after `lock_args` ran) cargo would get `--locked` twice and
    /// refuse. Locking comes from the per-package setting instead, and `-g`
    /// is what makes cargo-update look at git installs at all.
    #[test]
    fn update_is_locked_per_package_not_globally() {
        let update = update_args(&["kd".to_owned(), "voice".to_owned()]);
        assert_eq!(update, vec!["install-update", "-g", "kd", "voice"]);
        assert!(!update.iter().any(|a| a == "--locked"));
        assert_eq!(
            lock_args("kd"),
            vec!["install-update-config", "--enforce-lock", "kd"]
        );
    }

    /// Nothing that looks unlike a header may become a tool name.
    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_install_list("warning: something\n\nnot a header\n").is_empty());
    }
}
