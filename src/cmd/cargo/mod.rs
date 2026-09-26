//! `kd cargo`: helpers around tools installed with `cargo install`.
//! Specified in SPEC.md (`## kd cargo scode`).
//!
//! `scode update` exists because devbox bootstrap installs kd, and possibly
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
/// the tools `scode update` updates. cargo records the URL exactly as it was
/// given to `cargo install --git`, so the same repository can appear as
/// HTTPS, plain HTTP, or SSH. Compiled in on purpose: the command's name
/// promises exactly this owner.
const SCODE_PREFIXES: [&str; 3] = [
    "https://github.com/scode/",
    "http://github.com/scode/",
    "ssh://git@github.com/scode/",
];

/// `kd cargo ...` subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Tools installed from github.com/scode repositories
    Scode {
        #[command(subcommand)]
        cmd: ScodeCommands,
    },
}

/// `kd cargo scode ...`: operations on tools from github.com/scode
/// repositories, grouped so related verbs sit under one noun.
#[derive(Subcommand, Debug)]
pub enum ScodeCommands {
    /// Update every tool installed from a github.com/scode repository, kd included
    Update(ScodeUpdateArgs),
    /// Install a tool from github.com/scode/NAME, tracking its default branch
    Install(ScodeToolArgs),
    /// Uninstall a tool that was installed from github.com/scode/NAME
    Uninstall(ScodeToolArgs),
}

/// Arguments for `install` and `uninstall`: one repository under
/// github.com/scode holding a binary package with the same name.
#[derive(Args, Debug)]
pub struct ScodeToolArgs {
    /// Repository name under github.com/scode, which is also the package name
    pub name: String,

    /// Show what would be run, without running it
    #[arg(long)]
    pub dry_run: bool,
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
            Commands::Scode { cmd } => match cmd {
                ScodeCommands::Update(args) => scode_update(&Shell::new()?, args.dry_run),
                ScodeCommands::Install(args) => {
                    scode_install(&Shell::new()?, &args.name, args.dry_run)
                }
                ScodeCommands::Uninstall(args) => {
                    scode_uninstall(&Shell::new()?, &args.name, args.dry_run)
                }
            },
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

/// What `scode update` does with the installed tools from scode repositories.
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

/// Set so that installing and updating tools from private github.com/scode
/// repositories, which require authentication, works. It is cargo's switch
/// for fetching git sources with the `git` command, which uses the
/// credentials the user's git is already configured with, instead of
/// cargo's bundled libgit2, which supports some credential setups but fails
/// with others (it failed with "failed to acquire username/password" for a
/// private repository `git ls-remote` read fine). cargo-update reads the
/// same variable (and the `net.git-fetch-with-cli` config key) for its own
/// remote lookups.
const GIT_FETCH_WITH_CLI: &str = "CARGO_NET_GIT_FETCH_WITH_CLI";

/// Whether an executable `git` is on the shell's PATH.
fn git_available(sh: &Shell) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let path = sh.var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join("git"))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Make a cargo or cargo-update command that fetches from GitHub use the
/// `git` command, so private github.com/scode repositories, which require
/// authentication, can be installed and updated with the user's git
/// credentials (see [`GIT_FETCH_WITH_CLI`]).
///
/// - A non-empty value the user set in the environment, even `false`, wins
///   and is passed through untouched. (A `net.git-fetch-with-cli` setting in
///   cargo's config is overridden when kd sets the variable.)
/// - Otherwise the `git` command is used only if `git` is on PATH. Fetching
///   with the CLI makes git a hard requirement, and a machine set up by
///   install.sh need not have it; there, cargo's built-in fetching still
///   serves public repositories, so it is left in place.
/// - An empty value counts as unset, and is removed rather than passed on
///   when git is absent, since cargo rejects an empty boolean.
fn with_git_cli<'a>(sh: &Shell, cmd: xshell::Cmd<'a>) -> xshell::Cmd<'a> {
    let explicit = sh.var_os(GIT_FETCH_WITH_CLI).filter(|v| !v.is_empty());
    if explicit.is_some() {
        cmd
    } else if git_available(sh) {
        cmd.env(GIT_FETCH_WITH_CLI, "true")
    } else {
        cmd.env_remove(GIT_FETCH_WITH_CLI)
    }
}

fn scode_update(sh: &Shell, dry_run: bool) -> anyhow::Result<()> {
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
    if !cargo_update_available(sh) {
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
    with_git_cli(sh, cmd!(sh, "cargo {args...}"))
        .run()
        .context("cargo install-update failed")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// install / uninstall
// ---------------------------------------------------------------------------

/// Accept only names that are both a valid GitHub repository name and a
/// valid cargo package name, since `install` uses NAME as both: an ASCII
/// letter first, then letters, digits, `-` or `_`, at most 64 characters.
/// (GitHub also allows `.` in repository names, but cargo never allows it in
/// a package name, so such a repository could not be installed this way.)
/// The name becomes part of a URL and a cargo argument, so anything else is
/// refused rather than escaped; a leading `-` would otherwise read as a
/// cargo flag.
fn validate_name(name: &str) -> anyhow::Result<()> {
    let valid = name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !valid {
        bail!(
            "{name:?} is not a usable name: it must be both a github.com/scode repository name and a cargo package name (ASCII letter first, then letters, digits, `-` or `_`, at most 64 characters)"
        );
    }
    Ok(())
}

/// `cargo install` arguments for one tool. The URL has no `.git` suffix and
/// no `--branch`, `--tag` or `--rev`, so the install tracks the default
/// branch and `kd cargo scode update` (and devbox bootstrap's probe) see the
/// same recorded source as for a bootstrap-installed kd. `--locked` builds
/// the dependency versions in the repository's committed `Cargo.lock`. The
/// package name is passed explicitly so a repository holding more than one
/// package still installs the one named after it.
fn install_args(name: &str) -> Vec<String> {
    vec![
        "install".to_owned(),
        "--locked".to_owned(),
        "--git".to_owned(),
        format!("https://github.com/scode/{name}"),
        name.to_owned(),
    ]
}

/// The repository path (`owner/name`) of a recorded git source in any of the
/// URL forms `scode update` recognises, lowercased, without a `.git` suffix,
/// query or fragment. `None` for anything that is not a scode git source.
fn scode_repo(source: &str) -> Option<String> {
    let prefix = SCODE_PREFIXES.iter().find(|p| {
        source.len() > p.len()
            && source.is_char_boundary(p.len())
            && source[..p.len()].eq_ignore_ascii_case(p)
    })?;
    let rest = &source[prefix.len()..];
    let repo = rest.split(['?', '#']).next().unwrap_or_default();
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some(format!("scode/{}", repo.to_ascii_lowercase()))
}

/// What `install` should do for `name`, given `cargo install --list`.
///
/// cargo does not protect a same-named install from another source: when
/// the installed package has the same name, it treats a different source as
/// an upgrade and replaces it silently (`check_upgrade` in cargo's install
/// code deliberately ignores the source). So every existing install of the
/// name is decided here instead of being left to cargo.
#[derive(Debug, PartialEq)]
enum InstallPlan {
    /// Not installed: install it.
    Install,
    /// Already installed, unpinned, from exactly github.com/scode/NAME;
    /// `update` is what keeps it current.
    AlreadyInstalled,
    /// Installed from github.com/scode/NAME but pinned to a tag or commit.
    /// `update` skips pinned tools, so pointing there would be a dead end.
    Pinned(String),
    /// Installed from anywhere else (crates.io, another owner, a different
    /// scode repository, a path). Replacing it is the user's decision.
    Foreign(Option<String>),
}

fn install_plan(installed: &[Installed], name: &str) -> InstallPlan {
    let Some(existing) = installed.iter().find(|i| i.name == name) else {
        return InstallPlan::Install;
    };
    let wanted = format!("scode/{}", name.to_ascii_lowercase());
    match existing.source.as_deref() {
        Some(source) if scode_repo(source).as_deref() == Some(wanted.as_str()) => {
            if is_pinned(source) {
                InstallPlan::Pinned(source.to_owned())
            } else {
                InstallPlan::AlreadyInstalled
            }
        }
        other => InstallPlan::Foreign(other.map(str::to_owned)),
    }
}

/// What `uninstall` should do for `name`, given `cargo install --list`.
#[derive(Debug, PartialEq)]
enum UninstallPlan {
    /// Installed from a github.com/scode repository: uninstall it.
    Uninstall,
    /// Not installed at all.
    Missing,
    /// Installed from somewhere else (crates.io, another owner, a path);
    /// the source is carried for the error message.
    Foreign(Option<String>),
}

fn uninstall_plan(installed: &[Installed], name: &str) -> UninstallPlan {
    match installed.iter().find(|i| i.name == name) {
        None => UninstallPlan::Missing,
        Some(i) if i.source.as_deref().is_some_and(is_scode_source) => UninstallPlan::Uninstall,
        Some(i) => UninstallPlan::Foreign(i.source.clone()),
    }
}

fn installed_tools(sh: &Shell) -> anyhow::Result<Vec<Installed>> {
    let listing = cmd!(sh, "cargo install --list")
        .quiet()
        .read()
        .context("running `cargo install --list`")?;
    Ok(parse_install_list(&listing))
}

fn cargo_update_available(sh: &Shell) -> bool {
    cmd!(sh, "cargo install-update --help")
        .quiet()
        .ignore_stdout()
        .ignore_stderr()
        .run()
        .is_ok()
}

/// Install one tool from github.com/scode/NAME and mark its future
/// cargo-update reinstalls as locked. Every existing install of NAME is
/// decided by [`install_plan`] first; the only case that proceeds is "not
/// installed", so this command never replaces anything.
///
/// A failure of the lock step after a successful install is a warning, not
/// an error: the tool is installed and usable, and `update` applies the
/// same setting to every tool before it updates.
fn scode_install(sh: &Shell, name: &str, dry_run: bool) -> anyhow::Result<()> {
    validate_name(name)?;
    match install_plan(&installed_tools(sh)?, name) {
        InstallPlan::Install => {}
        InstallPlan::AlreadyInstalled => {
            println!(
                "{name} is already installed from github.com/scode/{name}; use `kd cargo scode update` to update it"
            );
            return Ok(());
        }
        InstallPlan::Pinned(source) => bail!(
            "{name} is installed from {source}, pinned to a tag or commit, which `kd cargo scode update` skips; run `kd cargo scode uninstall {name}` first to switch it to the default branch"
        ),
        InstallPlan::Foreign(source) => bail!(
            "{name} is already installed from {}; run `cargo uninstall {name}` first if you want it replaced by github.com/scode/{name}",
            source.as_deref().unwrap_or("crates.io")
        ),
    }
    let install = install_args(name);
    let lock = lock_args(name);
    let can_lock = cargo_update_available(sh);
    if dry_run {
        println!("dry run: would run `cargo {}`", install.join(" "));
        if can_lock {
            println!("dry run: then `cargo {}`", lock.join(" "));
        } else {
            println!("dry run: cargo-update is not installed, so no lock setting would be written");
        }
        return Ok(());
    }
    with_git_cli(sh, cmd!(sh, "cargo {install...}"))
        .run()
        .with_context(|| format!("installing {name}"))?;
    if !can_lock {
        println!(
            "note: cargo-update is not installed; `kd cargo scode update` needs it, and marks {name}'s updates as locked when it runs"
        );
    } else if let Err(err) = cmd!(sh, "cargo {lock...}").quiet().ignore_stdout().run() {
        eprintln!(
            "warning: {name} is installed, but marking its updates as locked failed ({err}); `kd cargo scode update` applies the setting before it updates"
        );
    }
    Ok(())
}

/// Uninstall one tool, but only one that came from a github.com/scode
/// repository: the command's name promises that scope, and a same-named
/// crates.io or other-owner install is not this command's to remove.
///
/// The tool's cargo-update settings are deliberately left in place, even
/// though cargo-update could delete them (`install-update-config --reset`
/// drops an entry that ends up at its defaults): they are harmless, and
/// they keep a later reinstall locked.
fn scode_uninstall(sh: &Shell, name: &str, dry_run: bool) -> anyhow::Result<()> {
    validate_name(name)?;
    match uninstall_plan(&installed_tools(sh)?, name) {
        UninstallPlan::Missing => bail!("{name} is not installed"),
        UninstallPlan::Foreign(source) => bail!(
            "{name} is installed from {}, not a github.com/scode repository; use `cargo uninstall {name}` if you mean it",
            source.as_deref().unwrap_or("crates.io")
        ),
        UninstallPlan::Uninstall => {}
    }
    if dry_run {
        println!("dry run: would run `cargo uninstall {name}`");
        return Ok(());
    }
    cmd!(sh, "cargo uninstall {name}")
        .run()
        .with_context(|| format!("uninstalling {name}"))?;
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

    /// Names end up in a URL and a cargo argument; only GitHub-legal
    /// repository names pass, and nothing that could read as a flag or a
    /// path.
    #[test]
    fn validates_repository_names() {
        for ok in ["kd", "voice", "my-tool", "tool_2", "A1"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        let long = "x".repeat(65);
        for bad in [
            "",
            "-x",
            ".x",
            "a.b",
            "1kd",
            "_x",
            "a/b",
            "../kd",
            "a b",
            "kd;rm",
            "ü",
            long.as_str(),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    /// The install must record the same source a bootstrap install does
    /// (no `.git`, no branch), build locked, and name the package.
    #[test]
    fn install_args_track_default_branch_locked() {
        assert_eq!(
            install_args("kd"),
            vec![
                "install",
                "--locked",
                "--git",
                "https://github.com/scode/kd",
                "kd"
            ]
        );
    }

    /// Install proceeds only when nothing of that name is installed. cargo
    /// itself would silently replace a same-named install from another
    /// source, and a pinned scode install cannot be helped by `update`, so
    /// both are refused with a way out; an unpinned install from exactly
    /// scode/NAME (in any URL form) is "already installed".
    #[test]
    fn install_never_replaces_an_existing_install() {
        let installed = parse_install_list(LISTING);
        assert_eq!(install_plan(&installed, "missing"), InstallPlan::Install);
        assert_eq!(
            install_plan(&installed, "kd"),
            InstallPlan::AlreadyInstalled
        );
        assert_eq!(
            install_plan(&installed, "voice"),
            InstallPlan::AlreadyInstalled
        );
        assert_eq!(
            install_plan(&installed, "viassh"),
            InstallPlan::AlreadyInstalled
        );
        assert!(matches!(
            install_plan(&installed, "pinned"),
            InstallPlan::Pinned(_)
        ));
        assert!(matches!(
            install_plan(&installed, "revved"),
            InstallPlan::Pinned(_)
        ));
        assert_eq!(
            install_plan(&installed, "cargo-update"),
            InstallPlan::Foreign(None)
        );
        assert!(matches!(
            install_plan(&installed, "other"),
            InstallPlan::Foreign(Some(_))
        ));
        let other_repo =
            parse_install_list("foo v1.0.0 (https://github.com/scode/bar#1):\n    foo\n");
        assert!(matches!(
            install_plan(&other_repo, "foo"),
            InstallPlan::Foreign(Some(_))
        ));
    }

    /// Uninstall touches only scode installs: other sources, crates.io, and
    /// absent tools are refused with the reason.
    #[test]
    fn uninstall_only_removes_scode_installs() {
        let installed = parse_install_list(LISTING);
        assert_eq!(uninstall_plan(&installed, "kd"), UninstallPlan::Uninstall);
        assert_eq!(
            uninstall_plan(&installed, "viassh"),
            UninstallPlan::Uninstall
        );
        assert_eq!(
            uninstall_plan(&installed, "missing"),
            UninstallPlan::Missing
        );
        assert_eq!(
            uninstall_plan(&installed, "cargo-update"),
            UninstallPlan::Foreign(None)
        );
        assert_eq!(
            uninstall_plan(&installed, "other"),
            UninstallPlan::Foreign(Some("https://github.com/someone/other#abc123".to_owned()))
        );
    }

    /// Runs the real command functions against a stub `cargo` whose
    /// directory is the xshell `Shell`'s entire PATH (the test process
    /// environment is never touched), plus a stub `git` when `git` is true,
    /// so both the git-present and git-absent cases are testable on any
    /// machine. The stubs use only shell builtins. `cargo` answers
    /// `install --list` from `listing`, succeeds at everything else, and
    /// logs `<CARGO_NET_GIT_FETCH_WITH_CLI or unset>|<args>` per call.
    /// `fetch_with_cli` of `""` stands for "not set": kd treats an empty value
    /// as unset, and a Shell cannot remove a variable the test process has.
    fn run_with_stub_cargo(
        listing: &str,
        fetch_with_cli: &str,
        git: bool,
        run: impl FnOnce(&Shell) -> anyhow::Result<()>,
    ) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let list = dir.path().join("listing");
        std::fs::write(&list, listing).unwrap();
        let exe = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        exe(
            "cargo",
            "#!/bin/sh\nprintf '%s|%s\\n' \"${CARGO_NET_GIT_FETCH_WITH_CLI-unset}\" \"$*\" >> \"$STUB_LOG\"\nif [ \"$1 $2\" = 'install --list' ]; then while IFS= read -r l; do printf '%s\\n' \"$l\"; done < \"$STUB_LISTING\"; fi\nexit 0\n",
        );
        if git {
            exe("git", "#!/bin/sh\nexit 0\n");
        }
        let sh = Shell::new().unwrap();
        sh.set_var("PATH", dir.path());
        sh.set_var("STUB_LOG", &log);
        sh.set_var("STUB_LISTING", &list);
        sh.set_var(GIT_FETCH_WITH_CLI, fetch_with_cli);
        run(&sh).unwrap();
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    const KD_LISTING: &str = "kd v0.1.0 (https://github.com/scode/kd#abc):\n    kd\n";
    const TOOL_INSTALL: &str = "install --locked --git https://github.com/scode/tool tool";

    /// Private scode repositories only install and update when cargo and
    /// cargo-update fetch with the `git` command (whose credentials work),
    /// so with git on PATH and no explicit setting, both fetching commands
    /// must get the variable.
    #[test]
    fn fetching_commands_use_the_git_cli_when_git_exists() {
        let log = run_with_stub_cargo(KD_LISTING, "", true, |sh| scode_update(sh, false));
        assert!(
            log.contains(&"true|install-update -g kd".to_owned()),
            "{log:?}"
        );
        let log = run_with_stub_cargo("", "", true, |sh| scode_install(sh, "tool", false));
        assert!(log.contains(&format!("true|{TOOL_INSTALL}")), "{log:?}");
    }

    /// Without git the CLI cannot be used at all; public repositories must
    /// keep working through cargo's built-in fetching, so the variable is
    /// not set (and an empty one is removed, not passed on).
    #[test]
    fn without_git_cargo_keeps_its_builtin_fetching() {
        let log = run_with_stub_cargo(KD_LISTING, "", false, |sh| scode_update(sh, false));
        assert!(
            log.contains(&"unset|install-update -g kd".to_owned()),
            "{log:?}"
        );
        let log = run_with_stub_cargo("", "", false, |sh| scode_install(sh, "tool", false));
        assert!(log.contains(&format!("unset|{TOOL_INSTALL}")), "{log:?}");
    }

    /// An explicit user setting, even `false`, is passed through untouched
    /// for both commands, git or not.
    #[test]
    fn explicit_setting_wins() {
        for git in [true, false] {
            let log = run_with_stub_cargo(KD_LISTING, "false", git, |sh| scode_update(sh, false));
            assert!(
                log.contains(&"false|install-update -g kd".to_owned()),
                "{log:?}"
            );
            let log = run_with_stub_cargo("", "false", git, |sh| scode_install(sh, "tool", false));
            assert!(log.contains(&format!("false|{TOOL_INSTALL}")), "{log:?}");
        }
    }

    /// Nothing that looks unlike a header may become a tool name.
    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_install_list("warning: something\n\nnot a header\n").is_empty());
    }
}
