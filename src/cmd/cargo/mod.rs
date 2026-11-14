//! `kd cargo`: helpers around tools installed with `cargo install`.
//! Specified in SPEC.md (`## kd cargo scode`).
//!
//! `scode update` exists because devbox bootstrap installs kd, and possibly
//! other tools, straight from their GitHub repositories under `scode/`, and
//! updating exactly those is otherwise a chore: remember which installed
//! tools came from those repositories and how each was installed. The
//! command finds them in `cargo install --list` and reruns
//! `cargo install --git` for each with the recorded URL and branch. cargo
//! itself compares the installed commit with the branch head, rebuilds a tool
//! whose branch moved, and skips one that is current.
//!
//! It deliberately does not go through cargo-update (`cargo install-update
//! -g`). cargo-update does its own update check, and as released that check
//! ignores `CARGO_NET_GIT_FETCH_WITH_CLI=true`: it parses the variable as a
//! TOML document, so a bare `true` is silently dropped, and it checks through
//! libgit2, which cannot authenticate to a private repository whose
//! credentials only the `git` command has. Such a tool then shows as "git
//! error" / "Needs update: No" and is never updated. A fix is proposed
//! upstream in <https://github.com/nabijaczleweli/cargo-update/pull/345>. If
//! it is released, cargo-update would work here again, but there is no need
//! to switch back: plain `cargo install` does the same job with fewer moving
//! parts, and without cargo-update having to be installed.
//!
//! `scode status` answers "what would `update` do" without doing it. cargo
//! has no read-only form of its update check, so status makes its own: the
//! commit cargo recorded for each tool against what `git ls-remote` reports
//! for the branch `update` would build.

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
    /// Show the tools installed from github.com/scode, their versions, and which need an update
    Status,
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
                ScodeCommands::Status => scode_status(&Shell::new()?),
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
    /// The package version cargo shows, without the leading `v`.
    version: String,
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
            let version = rest.strip_prefix('v')?.split(' ').next()?;
            let source = rest
                .split_once(" (")
                .and_then(|(_, s)| s.strip_suffix(')'))
                .map(str::to_owned);
            Some(Installed {
                name: name.to_owned(),
                version: version.to_owned(),
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

/// Whether a git install is pinned to a tag or a commit. `update` reinstalls
/// from the recorded URL and branch only, so it would move a pinned tool to
/// its branch head and silently drop the pin. Such tools are skipped.
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
    /// Tools to update, in listing order, with their recorded source.
    update: Vec<(String, String)>,
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
            selection.update.push((i.name.clone(), source.to_owned()));
        }
    }
    selection
}

/// `cargo install` arguments that update one tool: the URL exactly as cargo
/// recorded it (so the recorded source stays the same) and its `?branch=`, if
/// any, as `--branch`, percent-decoded, since cargo records a branch such as
/// `topic/x` as `topic%2Fx`. No `--force`: cargo skips the tool when the branch has
/// not moved and rebuilds it when it has, which is exactly the update check.
/// `--locked` builds the dependency versions in the repository's committed
/// `Cargo.lock`, like `install`.
///
/// Only called for unpinned sources (see [`is_pinned`]), whose query holds at
/// most a `branch`.
fn update_args(name: &str, source: &str) -> Vec<String> {
    let (url, branch) = recorded_url_and_branch(source);
    let mut args = vec![
        "install".to_owned(),
        "--locked".to_owned(),
        "--git".to_owned(),
        url.to_owned(),
    ];
    if let Some(branch) = branch {
        args.push("--branch".to_owned());
        args.push(branch);
    }
    args.push(name.to_owned());
    args
}

/// The URL and branch a git install tracks, as cargo recorded them: the URL
/// exactly as given at install time, without its query and `#commit`, and
/// the `?branch=` value percent-decoded (`None` for the default branch).
/// Shared by `update`, which reinstalls from them, and `status`, which asks
/// git for the commit they currently point at, so the two cannot disagree
/// about which branch a tool follows.
fn recorded_url_and_branch(source: &str) -> (&str, Option<String>) {
    let without_commit = source.split('#').next().unwrap_or_default();
    let (url, query) = without_commit
        .split_once('?')
        .unwrap_or((without_commit, ""));
    let branch = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("branch="))
        .map(percent_decode);
    (url, branch)
}

/// Undo the percent-encoding cargo applies to a query value in a recorded
/// source. A `%` not followed by two hex digits is kept as is, and so is the
/// whole input if the decoded bytes are not UTF-8 (no valid branch name
/// produces that).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// Set so that installing and updating tools from private github.com/scode
/// repositories, which require authentication, works. It is cargo's switch
/// for fetching git sources with the `git` command, which uses the
/// credentials the user's git is already configured with, instead of
/// cargo's bundled libgit2, which supports some credential setups but fails
/// with others (it failed with "failed to acquire username/password" for a
/// private repository `git ls-remote` read fine).
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

/// Make a cargo command that fetches from GitHub use the
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

/// The install options cargo recorded for one package in `.crates2.json`,
/// limited to those that decide whether a plain `cargo install` counts as
/// current (`is_up_to_date` in cargo's install tracking; the set of binaries
/// is the other one, and a change there is a real update).
#[derive(Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
struct InstallOptions {
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    profile: String,
    target: Option<String>,
    /// `rustc -vV` output at install time; its `host:` line is the target a
    /// plain `cargo install` builds for.
    rustc: Option<String>,
}

impl InstallOptions {
    /// What differs from the options `update` passes (none: default features,
    /// the release profile, the host target), or `None` if nothing does.
    ///
    /// cargo rebuilds an install whose recorded options differ from the
    /// requested ones even when its branch has not moved, so `update` would
    /// silently turn e.g. a `--features extra` or `--profile dev` install into
    /// a default one. Such tools are skipped instead, as pinned ones are.
    fn non_default(&self) -> Option<String> {
        let mut differences = Vec::new();
        if !self.features.is_empty() {
            differences.push(format!("--features {}", self.features.join(",")));
        }
        if self.all_features {
            differences.push("--all-features".to_owned());
        }
        if self.no_default_features {
            differences.push("--no-default-features".to_owned());
        }
        if !self.profile.is_empty() && self.profile != "release" {
            differences.push(format!("--profile {}", self.profile));
        }
        let host = self
            .rustc
            .as_deref()
            .and_then(|r| r.lines().find_map(|l| l.strip_prefix("host: ")));
        if let (Some(target), Some(host)) = (self.target.as_deref(), host)
            && target != host
        {
            differences.push(format!("--target {target}"));
        }
        (!differences.is_empty()).then(|| differences.join(" "))
    }
}

/// Parse cargo's `.crates2.json` into install options by package name. Keys
/// look like `name version (source)`; cargo keeps at most one install per
/// package name, so the name alone identifies the entry.
fn parse_crates2(text: &str) -> anyhow::Result<std::collections::HashMap<String, InstallOptions>> {
    #[derive(serde::Deserialize)]
    struct Crates2 {
        #[serde(default)]
        installs: std::collections::HashMap<String, InstallOptions>,
    }
    let crates2: Crates2 = serde_json::from_str(text).context("parsing .crates2.json")?;
    Ok(crates2
        .installs
        .into_iter()
        .filter_map(|(key, opts)| Some((key.split(' ').next()?.to_owned(), opts)))
        .collect())
}

/// Where `cargo install` keeps `.crates2.json`: `CARGO_INSTALL_ROOT`, else an
/// absolute `install.root` from `$CARGO_HOME/config.toml` (or `config`), else
/// `$CARGO_HOME`, which defaults to `~/.cargo`.
///
/// This covers the setups kd creates. It deliberately does not reproduce
/// cargo's full lookup (a relative `install.root`, or one from a project's
/// `.cargo/config.toml` in the current directory); there the file may not be
/// found, and `update` warns that it could not check install options.
fn install_root(sh: &Shell) -> Option<std::path::PathBuf> {
    let non_empty = |name: &str| sh.var_os(name).filter(|v| !v.is_empty());
    if let Some(root) = non_empty("CARGO_INSTALL_ROOT") {
        return Some(root.into());
    }
    let cargo_home: std::path::PathBuf = match non_empty("CARGO_HOME") {
        Some(home) => home.into(),
        None => std::path::PathBuf::from(non_empty("HOME")?).join(".cargo"),
    };
    let configured = ["config.toml", "config"]
        .iter()
        .find_map(|f| std::fs::read_to_string(cargo_home.join(f)).ok())
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            let root = table.get("install")?.get("root")?.as_str()?;
            let root = std::path::PathBuf::from(root);
            root.is_absolute().then_some(root)
        });
    Some(configured.unwrap_or(cargo_home))
}

/// Install options for the installed packages, or `None` with a reason when
/// `.crates2.json` cannot be found or read.
fn installed_options(
    sh: &Shell,
) -> Result<std::collections::HashMap<String, InstallOptions>, String> {
    let path = install_root(sh)
        .ok_or_else(|| "neither CARGO_INSTALL_ROOT, CARGO_HOME nor HOME is set".to_owned())?
        .join(".crates2.json");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse_crates2(&text).map_err(|e| format!("{}: {e:#}", path.display()))
}

/// Update every unpinned tool from a scode repository by rerunning
/// `cargo install` for it (see [`update_args`] and the module docs for why not
/// cargo-update). Tools installed with non-default options are skipped (see
/// [`InstallOptions::non_default`]). One tool failing does not stop the
/// others; the command fails at the end if any did.
fn scode_update(sh: &Shell, dry_run: bool) -> anyhow::Result<()> {
    let mut selection = select(&installed_tools(sh)?);
    for (name, source) in &selection.pinned {
        println!("skipping {name}: pinned to a tag or commit ({source})");
    }
    match installed_options(sh) {
        Ok(options) => selection.update.retain(|(name, _)| {
            match options.get(name).and_then(InstallOptions::non_default) {
                Some(differences) => {
                    println!(
                        "skipping {name}: installed with {differences}, which updating would replace with the defaults"
                    );
                    false
                }
                None => true,
            }
        }),
        Err(reason) => eprintln!(
            "warning: could not check how the tools were installed ({reason}); a tool installed with non-default features, profile or target would be rebuilt with the defaults"
        ),
    }
    if selection.update.is_empty() {
        println!("no tools from github.com/scode to update");
        return Ok(());
    }
    let names: Vec<&str> = selection.update.iter().map(|(n, _)| n.as_str()).collect();
    println!("tools from github.com/scode: {}", names.join(", "));
    if dry_run {
        for (name, source) in &selection.update {
            println!(
                "dry run: would run `cargo {}`",
                update_args(name, source).join(" ")
            );
        }
        return Ok(());
    }
    // cargo's own output streams through (xshell echoes each command first).
    // For a tool that is already current, cargo reports "Ignored package ...
    // is already installed, use --force to override", which reads like a
    // problem but is the no-update-needed case. Updating kd itself while
    // this kd runs is fine: cargo installs by writing a new file and renaming
    // it into place, and the running process keeps its old inode.
    println!(
        "(cargo reports a tool whose branch has not moved as \"already installed\"; that is expected)"
    );
    let mut failed = Vec::new();
    for (name, source) in &selection.update {
        let args = update_args(name, source);
        if let Err(err) = with_git_cli(sh, cmd!(sh, "cargo {args...}")).run() {
            eprintln!("error: updating {name} failed: {err}");
            failed.push(name.as_str());
        }
    }
    if !failed.is_empty() {
        bail!("updating failed for: {}", failed.join(", "));
    }
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

/// Install one tool from github.com/scode/NAME. Every existing install of
/// NAME is decided by [`install_plan`] first; the only case that proceeds is
/// "not installed", so this command never replaces anything.
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
    if dry_run {
        println!("dry run: would run `cargo {}`", install.join(" "));
        return Ok(());
    }
    with_git_cli(sh, cmd!(sh, "cargo {install...}"))
        .run()
        .with_context(|| format!("installing {name}"))?;
    Ok(())
}

/// Uninstall one tool, but only one that came from a github.com/scode
/// repository: the command's name promises that scope, and a same-named
/// crates.io or other-owner install is not this command's to remove.
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

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// The commit cargo recorded for a git install: the `#` fragment of its
/// source, which `cargo install --list` shows abbreviated (8 hex digits as
/// of cargo 1.9x). `None` when there is no fragment to compare against.
fn installed_commit(source: &str) -> Option<&str> {
    source
        .split_once('#')
        .map(|(_, commit)| commit)
        .filter(|commit| !commit.is_empty())
}

/// The ref `status` asks `git ls-remote` for: the recorded branch, or `HEAD`
/// for an install that tracks the default branch, which is what a plain
/// `cargo install --git` without `--branch` builds.
fn tracked_ref(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("refs/heads/{branch}"),
        None => "HEAD".to_owned(),
    }
}

/// The commit `git ls-remote <url> <wanted>` reports for exactly `wanted`.
/// git matches the pattern against the tail of each ref name, so a branch
/// `main` would also match `refs/heads/x/refs/heads/main`; only the line
/// naming `wanted` itself counts. `None` when the ref does not exist.
fn parse_ls_remote(text: &str, wanted: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (commit, name) = line.split_once('\t')?;
        (name == wanted && commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| commit.to_owned())
    })
}

/// The ssh command `status` gives git when the user has not chosen one (see
/// [`user_picks_ssh_command`]). `BatchMode=yes` makes ssh fail instead of
/// asking for a passphrase or a password, and `ConnectTimeout` makes an
/// unreachable host fail that tool's check instead of making `status` look
/// hung.
const SSH_BATCH_COMMAND: &str = "ssh -o BatchMode=yes -o ConnectTimeout=15";

/// Whether the user chose how git runs ssh: a non-empty `GIT_SSH_COMMAND` or
/// `GIT_SSH`, or `core.sshCommand` in git's config. `status` then leaves ssh
/// alone. git gives `GIT_SSH_COMMAND` precedence over `core.sshCommand`, so
/// setting it would silently replace, say, the key the user selects there,
/// and `status` would fail where `update` works. An empty variable counts as
/// unset, as kd treats empty variables elsewhere in this module.
fn user_picks_ssh_command(sh: &Shell) -> bool {
    let set = |name: &str| sh.var_os(name).is_some_and(|v| !v.is_empty());
    if set("GIT_SSH_COMMAND") || set("GIT_SSH") {
        return true;
    }
    cmd!(sh, "git config --get core.sshCommand")
        .quiet()
        .ignore_status()
        .ignore_stderr()
        .read()
        .is_ok_and(|value| !value.trim().is_empty())
}

/// Why `ls-remote` failed, from git's stderr: the last two lines that are
/// not noise, joined with "; ". Over ssh, git prints ssh's own error
/// ("Permission denied (publickey).", "Connection refused") and then the
/// same generic `fatal: Could not read from remote repository.` every time,
/// so the line before the `fatal:` is the one that tells a missing key from
/// a firewall. Noise is ssh's `Warning:` lines, `remote:` banners, and
/// git's "Please make sure you have the correct access rights / and the
/// repository exists." trailer.
fn failure_reason(stderr: &str) -> String {
    let noise = |line: &str| {
        line.starts_with("Warning:")
            || line.starts_with("remote:")
            || line.starts_with("Please make sure you have")
            || line == "and the repository exists."
    };
    let kept: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !noise(l))
        .collect();
    if kept.is_empty() {
        return "no error output".to_owned();
    }
    kept[kept.len().saturating_sub(2)..].join("; ")
}

/// Ask the repository which commit the tool's branch points at now, the
/// commit `update` would install.
///
/// Uses the `git` command, for the same reason the fetching commands do (see
/// [`GIT_FETCH_WITH_CLI`]): its credentials, including credential helpers,
/// are the ones that work for private repositories. What it must not do is
/// stop and wait for someone to type something, so every way git asks is
/// turned off: `GIT_TERMINAL_PROMPT=0` for terminal prompts, an empty
/// `GIT_ASKPASS` for askpass programs (an empty value makes git skip
/// `core.askPass` and `SSH_ASKPASS` too; VS Code's terminal, for one, sets
/// `GIT_ASKPASS` to a dialog), and [`SSH_BATCH_COMMAND`] for ssh unless the
/// user chose their own. A transfer that stays under 1 KB/s for 20 seconds
/// is aborted. A failure comes back as the reason to show, never as an
/// error, so one unreachable repository does not hide the others.
///
/// git runs from `/`, not from wherever `status` was started, so a
/// repository the user happens to stand in cannot change the answer through
/// its local config: `update`'s fetch runs in cargo's own directory, where
/// only global and system config apply, and `status` must see the same.
fn latest_commit(sh: &Shell, source: &str) -> Result<String, String> {
    if !git_available(sh) {
        return Err("cannot check: git is not on PATH".to_owned());
    }
    let _neutral = sh.push_dir("/");
    let (url, branch) = recorded_url_and_branch(source);
    let wanted = tracked_ref(branch.as_deref());
    let mut ls_remote = cmd!(
        sh,
        "git -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=20 ls-remote {url} {wanted}"
    )
    .quiet()
    .ignore_status()
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_ASKPASS", "");
    if !user_picks_ssh_command(sh) {
        ls_remote = ls_remote.env("GIT_SSH_COMMAND", SSH_BATCH_COMMAND);
    }
    let output = ls_remote
        .output()
        .map_err(|e| format!("cannot check: running git failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cannot check: git ls-remote failed: {}",
            failure_reason(&stderr)
        ));
    }
    parse_ls_remote(&String::from_utf8_lossy(&output.stdout), &wanted)
        .ok_or_else(|| format!("cannot check: {url} has no {wanted}"))
}

/// One line of `status` output.
#[derive(Debug, PartialEq)]
struct StatusRow {
    name: String,
    version: String,
    /// The installed commit as cargo shows it, or `?` when it recorded none.
    installed: String,
    /// The branch head, abbreviated to the installed commit's length but to
    /// no fewer than 8 hex digits, or `-` when it was not checked or could
    /// not be.
    latest: String,
    status: String,
    /// Whether running `update` would change this tool. Drives the closing
    /// line, so a tool `update` skips never counts.
    update_would_change: bool,
    /// Whether the check that should have run could not (git missing or
    /// failing, the ref missing, no recorded commit). A pinned tool is not
    /// checked on purpose and does not count.
    check_failed: bool,
}

/// Decide one tool's status. Pure, so every combination is testable.
///
/// `latest` is `None` for a pinned tool, which is not checked: `update`
/// skips it whatever its branch does. A tool installed with non-default
/// options is checked, so the user can see that it fell behind, but it is
/// not counted as something `update` will fix, since `update` skips it too.
/// A commit match is a prefix match, because cargo shows the installed
/// commit abbreviated.
fn status_row(
    installed: &Installed,
    source: &str,
    non_default: Option<&str>,
    latest: Option<&Result<String, String>>,
) -> StatusRow {
    let commit = installed_commit(source);
    let mut row = StatusRow {
        name: installed.name.clone(),
        version: installed.version.clone(),
        installed: commit.unwrap_or("?").to_owned(),
        latest: "-".to_owned(),
        status: String::new(),
        update_would_change: false,
        check_failed: false,
    };
    let Some(latest) = latest else {
        row.status = "pinned to a tag or commit; `update` skips it".to_owned();
        return row;
    };
    let latest = match latest {
        Ok(latest) => latest,
        Err(reason) => {
            row.status = reason.clone();
            row.check_failed = true;
            return row;
        }
    };
    // `latest` is at least 40 hex digits (see `parse_ls_remote`), so the
    // clamp keeps the slice in bounds and on a char boundary.
    row.latest = latest[..commit.map_or(8, str::len).clamp(8, latest.len())].to_owned();
    let Some(commit) = commit else {
        row.status = "cannot compare: cargo recorded no installed commit".to_owned();
        row.check_failed = true;
        return row;
    };
    let current = latest.starts_with(&commit.to_ascii_lowercase());
    row.status = match (current, non_default) {
        (true, None) => "up to date".to_owned(),
        (false, None) => {
            row.update_would_change = true;
            "update available".to_owned()
        }
        (true, Some(options)) => format!("up to date (installed with {options})"),
        (false, Some(options)) => format!(
            "update available, but `update` skips it: installed with {options}, which it would replace with the defaults"
        ),
    };
    row
}

/// Status of every tool from a scode repository, in listing order, checking
/// each unpinned one against its repository (see [`latest_commit`]).
fn status_rows(sh: &Shell) -> anyhow::Result<Vec<StatusRow>> {
    let installed = installed_tools(sh)?;
    // Unlike `update`, an unreadable `.crates2.json` only loses the
    // non-default-options note; status still reports everything else.
    let options = installed_options(sh)
        .inspect_err(|reason| {
            eprintln!("warning: could not check how the tools were installed ({reason})")
        })
        .unwrap_or_default();
    Ok(installed
        .iter()
        .filter_map(|i| {
            let source = i.source.as_deref().filter(|s| is_scode_source(s))?;
            let non_default = options.get(&i.name).and_then(InstallOptions::non_default);
            let latest = (!is_pinned(source)).then(|| latest_commit(sh, source));
            Some(status_row(
                i,
                source,
                non_default.as_deref(),
                latest.as_ref(),
            ))
        })
        .collect())
}

/// Render the rows as aligned columns, then the closing lines: which tools
/// could not be checked, if any, and what running `update` would change.
/// "Nothing to do" is only claimed for the tools that were checked, since an
/// unchecked tool may well be behind.
fn render_status(rows: &[StatusRow]) -> String {
    if rows.is_empty() {
        return "no tools from github.com/scode installed".to_owned();
    }
    let header = ["TOOL", "VERSION", "INSTALLED", "LATEST", "STATUS"];
    let cells: Vec<[&str; 5]> = rows
        .iter()
        .map(|r| [&*r.name, &*r.version, &*r.installed, &*r.latest, &*r.status])
        .collect();
    let width = |col: usize| {
        cells
            .iter()
            .map(|c| c[col].chars().count())
            .chain([header[col].len()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..4).map(width).collect();
    let line = |c: &[&str; 5]| {
        let mut out = String::new();
        for (col, w) in widths.iter().enumerate() {
            out.push_str(&format!("{:<w$}  ", c[col], w = w));
        }
        out.push_str(c[4]);
        out
    };
    let mut lines = vec![line(&header)];
    lines.extend(cells.iter().map(line));
    let names = |keep: fn(&StatusRow) -> bool| -> Vec<&str> {
        rows.iter()
            .filter(|r| keep(r))
            .map(|r| r.name.as_str())
            .collect()
    };
    let behind = names(|r| r.update_would_change);
    let failed = names(|r| r.check_failed);
    lines.push(String::new());
    if !failed.is_empty() {
        lines.push(format!("could not check: {}", failed.join(", ")));
    }
    lines.push(match (behind.is_empty(), failed.is_empty()) {
        (false, _) => format!(
            "`kd cargo scode update` would update: {}",
            behind.join(", ")
        ),
        (true, true) => "nothing for `kd cargo scode update` to do".to_owned(),
        (true, false) => {
            "nothing for `kd cargo scode update` to do among the tools that could be checked"
                .to_owned()
        }
    });
    lines.join("\n")
}

/// `kd cargo scode status`: what is installed from github.com/scode, at
/// which version and commit, and what `update` would change. Read-only: it
/// runs `cargo install --list` and one `git ls-remote` per unpinned tool,
/// and never builds or installs anything. A tool whose check fails is shown
/// with the reason and the others are still checked; the command then exits
/// non-zero, so a script cannot mistake an incomplete answer for "nothing to
/// update".
fn scode_status(sh: &Shell) -> anyhow::Result<()> {
    let rows = status_rows(sh)?;
    println!("{}", render_status(&rows));
    if rows.iter().any(|r| r.check_failed) {
        bail!("some tools could not be checked; see above");
    }
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
                version: "22.1.1".to_owned(),
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
    /// reported separately, because reinstalling from the branch would
    /// silently unpin them.
    #[test]
    fn selects_scode_repositories_and_skips_pins() {
        let selection = select(&parse_install_list(LISTING));
        let update: Vec<&str> = selection.update.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(update, vec!["kd", "voice", "viassh"]);
        let pinned: Vec<&str> = selection.pinned.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(pinned, vec!["pinned", "revved"]);
    }

    /// A branch is not a pin: `update` reinstalls from the recorded branch.
    #[test]
    fn branch_is_not_a_pin() {
        assert!(!is_pinned("https://github.com/scode/x?branch=main#1"));
        assert!(!is_pinned("https://github.com/scode/x#tag"));
        assert!(is_pinned("https://github.com/scode/x?rev=1#1"));
        assert!(is_pinned("https://github.com/scode/x?branch=a&tag=b#1"));
    }

    /// The update reruns `cargo install` with the source exactly as cargo
    /// recorded it, so the recorded source does not change: the URL as given
    /// (a `.git` suffix, owner case and SSH form included), the branch as
    /// `--branch`, and neither the query nor the `#commit`. It builds
    /// `--locked` and never passes `--force`, since cargo skipping an
    /// unchanged tool is the update check.
    #[test]
    fn update_reinstalls_from_the_recorded_source() {
        assert_eq!(
            update_args("kd", "https://github.com/scode/kd#59c1f5e4"),
            vec![
                "install",
                "--locked",
                "--git",
                "https://github.com/scode/kd",
                "kd"
            ]
        );
        assert_eq!(
            update_args(
                "voice",
                "https://github.com/Scode/voice.git?branch=main#def456"
            ),
            vec![
                "install",
                "--locked",
                "--git",
                "https://github.com/Scode/voice.git",
                "--branch",
                "main",
                "voice"
            ]
        );
        assert_eq!(
            update_args("viassh", "ssh://git@github.com/scode/viassh#ccc333"),
            vec![
                "install",
                "--locked",
                "--git",
                "ssh://git@github.com/scode/viassh",
                "viassh"
            ]
        );
        assert!(
            !update_args("kd", "https://github.com/scode/kd#1").contains(&"--force".to_owned())
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
        let (log, result) =
            run_with_failing_stub_cargo(listing, fetch_with_cli, git, "", EMPTY_CRATES2, run);
        result.unwrap();
        log
    }

    /// [`run_with_stub_cargo`], except that the stub `cargo` fails any call
    /// whose last argument is `fail` (a package name; `""` fails nothing),
    /// `crates2` is the `.crates2.json` in the install root (the stub
    /// directory, via `CARGO_INSTALL_ROOT`), and the command's result is
    /// returned rather than unwrapped.
    fn run_with_failing_stub_cargo(
        listing: &str,
        fetch_with_cli: &str,
        git: bool,
        fail: &str,
        crates2: &str,
        run: impl FnOnce(&Shell) -> anyhow::Result<()>,
    ) -> (Vec<String>, anyhow::Result<()>) {
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
            "#!/bin/sh\nprintf '%s|%s\\n' \"${CARGO_NET_GIT_FETCH_WITH_CLI-unset}\" \"$*\" >> \"$STUB_LOG\"\nif [ \"$1 $2\" = 'install --list' ]; then while IFS= read -r l; do printf '%s\\n' \"$l\"; done < \"$STUB_LISTING\"; fi\nfor last; do :; done\nif [ -n \"$STUB_FAIL\" ] && [ \"$last\" = \"$STUB_FAIL\" ]; then exit 1; fi\nexit 0\n",
        );
        if git {
            // Logs `<GIT_TERMINAL_PROMPT>|<GIT_ASKPASS>|<GIT_SSH_COMMAND>|<args>`
            // to `$STUB_GIT_LOG`, answers `config --get core.sshCommand` from
            // `$STUB_CORE_SSH` and `ls-remote` from `$STUB_LS_REMOTE` (each
            // set only by a test that needs it), and fails `ls-remote` the
            // way git does for a URL containing "broken", with an ssh warning
            // ahead of the `fatal:` line, so `status` sees both outcomes.
            exe(
                "git",
                r#"#!/bin/sh
if [ -n "$STUB_GIT_LOG" ]; then
  printf '%s|%s|%s|%s\n' "${GIT_TERMINAL_PROMPT-unset}" "${GIT_ASKPASS-unset}" "${GIT_SSH_COMMAND-unset}" "$*" >> "$STUB_GIT_LOG"
fi
case "$*" in
  'config --get core.sshCommand')
    if [ -n "$STUB_CORE_SSH" ]; then printf '%s\n' "$STUB_CORE_SSH"; exit 0; fi
    exit 1;;
  *ls-remote*broken*)
    echo 'Warning: Permanently added the host key' >&2
    echo 'fatal: repository not found' >&2
    exit 128;;
  *ls-remote*)
    if [ -n "$STUB_LS_REMOTE" ]; then while IFS= read -r l; do printf '%s\n' "$l"; done < "$STUB_LS_REMOTE"; fi;;
esac
exit 0
"#,
            );
        }
        let sh = Shell::new().unwrap();
        sh.set_var("PATH", dir.path());
        sh.set_var("STUB_LOG", &log);
        sh.set_var("STUB_LISTING", &list);
        sh.set_var("STUB_FAIL", fail);
        std::fs::write(dir.path().join(".crates2.json"), crates2).unwrap();
        sh.set_var("CARGO_INSTALL_ROOT", dir.path());
        sh.set_var(GIT_FETCH_WITH_CLI, fetch_with_cli);
        let result = run(&sh);
        let log = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        (log, result)
    }

    const EMPTY_CRATES2: &str = r#"{"installs":{}}"#;
    const KD_LISTING: &str = "kd v0.1.0 (https://github.com/scode/kd#abc):\n    kd\n";
    const KD_UPDATE: &str = "install --locked --git https://github.com/scode/kd kd";
    const TOOL_INSTALL: &str = "install --locked --git https://github.com/scode/tool tool";

    /// Private scode repositories only install and update when cargo fetches
    /// with the `git` command (whose credentials work),
    /// so with git on PATH and no explicit setting, both fetching commands
    /// must get the variable.
    #[test]
    fn fetching_commands_use_the_git_cli_when_git_exists() {
        let log = run_with_stub_cargo(KD_LISTING, "", true, |sh| scode_update(sh, false));
        assert!(log.contains(&format!("true|{KD_UPDATE}")), "{log:?}");
        let log = run_with_stub_cargo("", "", true, |sh| scode_install(sh, "tool", false));
        assert!(log.contains(&format!("true|{TOOL_INSTALL}")), "{log:?}");
    }

    /// Without git the CLI cannot be used at all; public repositories must
    /// keep working through cargo's built-in fetching, so the variable is
    /// not set (and an empty one is removed, not passed on).
    #[test]
    fn without_git_cargo_keeps_its_builtin_fetching() {
        let log = run_with_stub_cargo(KD_LISTING, "", false, |sh| scode_update(sh, false));
        assert!(log.contains(&format!("unset|{KD_UPDATE}")), "{log:?}");
        let log = run_with_stub_cargo("", "", false, |sh| scode_install(sh, "tool", false));
        assert!(log.contains(&format!("unset|{TOOL_INSTALL}")), "{log:?}");
    }

    /// An explicit user setting, even `false`, is passed through untouched
    /// for both commands, git or not.
    #[test]
    fn explicit_setting_wins() {
        for git in [true, false] {
            let log = run_with_stub_cargo(KD_LISTING, "false", git, |sh| scode_update(sh, false));
            assert!(log.contains(&format!("false|{KD_UPDATE}")), "{log:?}");
            let log = run_with_stub_cargo("", "false", git, |sh| scode_install(sh, "tool", false));
            assert!(log.contains(&format!("false|{TOOL_INSTALL}")), "{log:?}");
        }
    }

    /// One tool failing to update must not stop the others, and the command
    /// must still fail at the end, naming it.
    #[test]
    fn update_continues_past_a_failing_tool_and_then_fails() {
        let listing = "kd v0.1.0 (https://github.com/scode/kd#abc):\n    kd\nvoice v0.2.0 (https://github.com/scode/voice#def):\n    voice\n";
        let (log, result) =
            run_with_failing_stub_cargo(listing, "", true, "kd", EMPTY_CRATES2, |sh| {
                scode_update(sh, false)
            });
        assert!(log.iter().any(|l| l.ends_with("install --locked --git https://github.com/scode/voice voice")), "{log:?}");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("kd") && !err.contains("voice"), "{err}");
    }

    /// cargo records `--branch topic/x` as `?branch=topic%2Fx`; passing that
    /// back literally names a branch that does not exist, so it is decoded.
    #[test]
    fn update_decodes_the_recorded_branch() {
        let listing = parse_install_list(
            "tool v0.1.0 (https://github.com/scode/tool?branch=topic%2Fslash#abc):\n    tool\n",
        );
        let (name, source) = &select(&listing).update[0];
        assert_eq!(
            update_args(name, source),
            vec![
                "install",
                "--locked",
                "--git",
                "https://github.com/scode/tool",
                "--branch",
                "topic/slash",
                "tool"
            ]
        );
        assert_eq!(percent_decode("a%2Fb%2fc"), "a/b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    /// Shape of real `.crates2.json` entries (bins and version_req omitted
    /// where irrelevant). Any recorded option a plain `cargo install` would
    /// not reproduce must be reported, and default installs must not be.
    #[test]
    fn detects_non_default_install_options() {
        let entry = |name: &str, overrides: serde_json::Value| {
            let mut e = serde_json::json!({
                "features": [],
                "all_features": false,
                "no_default_features": false,
                "profile": "release",
                "target": "x86_64-unknown-linux-gnu",
                "rustc": "rustc 1.98.1\nhost: x86_64-unknown-linux-gnu\n",
            });
            for (k, v) in overrides.as_object().unwrap() {
                e[k] = v.clone();
            }
            (
                format!("{name} 0.1.0 (git+https://github.com/scode/{name}#abc)"),
                e,
            )
        };
        let installs: serde_json::Map<String, serde_json::Value> = [
            entry("plain", serde_json::json!({})),
            entry(
                "featured",
                serde_json::json!({"features": ["extra"], "no_default_features": true}),
            ),
            entry("dev", serde_json::json!({"profile": "dev"})),
            entry(
                "cross",
                serde_json::json!({"target": "aarch64-unknown-linux-gnu"}),
            ),
            entry("everything", serde_json::json!({"all_features": true})),
        ]
        .into_iter()
        .collect();
        let text = serde_json::json!({ "installs": installs }).to_string();
        let options = parse_crates2(&text).unwrap();
        assert_eq!(options["plain"].non_default(), None);
        assert_eq!(
            options["featured"].non_default().as_deref(),
            Some("--features extra --no-default-features")
        );
        assert_eq!(
            options["dev"].non_default().as_deref(),
            Some("--profile dev")
        );
        assert_eq!(
            options["cross"].non_default().as_deref(),
            Some("--target aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            options["everything"].non_default().as_deref(),
            Some("--all-features")
        );
    }

    /// A tool installed with non-default options is skipped, since cargo would
    /// rebuild it with the defaults even at an unchanged commit; the others
    /// are still updated.
    #[test]
    fn update_skips_tools_installed_with_non_default_options() {
        let listing = "kd v0.1.0 (https://github.com/scode/kd#abc):\n    kd\nvoice v0.2.0 (https://github.com/scode/voice#def):\n    voice\n";
        let crates2 = r#"{"installs": {"voice 0.2.0 (git+https://github.com/scode/voice#def)": {"features": ["extra"], "profile": "release"}}}"#;
        let (log, result) = run_with_failing_stub_cargo(listing, "", true, "", crates2, |sh| {
            scode_update(sh, false)
        });
        result.unwrap();
        assert!(log.iter().any(|l| l.ends_with(KD_UPDATE)), "{log:?}");
        assert!(!log.iter().any(|l| l.contains("scode/voice")), "{log:?}");
    }

    /// `.crates2.json` lives in cargo's install root: `CARGO_INSTALL_ROOT`,
    /// else an absolute `install.root` in `$CARGO_HOME/config.toml`, else
    /// `$CARGO_HOME`. The test's Shell sets the variables, so the test
    /// process's environment is untouched.
    #[test]
    fn install_root_follows_cargo_for_the_common_setups() {
        let home = tempfile::tempdir().unwrap();
        let sh = Shell::new().unwrap();
        sh.set_var("CARGO_INSTALL_ROOT", "");
        sh.set_var("CARGO_HOME", home.path());
        assert_eq!(install_root(&sh), Some(home.path().to_owned()));
        std::fs::write(
            home.path().join("config.toml"),
            "[install]\nroot = \"/opt/tools\"\n",
        )
        .unwrap();
        assert_eq!(install_root(&sh), Some("/opt/tools".into()));
        std::fs::write(
            home.path().join("config.toml"),
            "[install]\nroot = \"relative\"\n",
        )
        .unwrap();
        assert_eq!(install_root(&sh), Some(home.path().to_owned()));
        sh.set_var("CARGO_INSTALL_ROOT", "/elsewhere");
        assert_eq!(install_root(&sh), Some("/elsewhere".into()));
    }

    /// Nothing that looks unlike a header may become a tool name.
    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_install_list("warning: something\n\nnot a header\n").is_empty());
    }

    // ── status ───────────────────────────────────────────────────────────

    const HEAD_KD: &str = "aaaaaaaa11111111111111111111111111111111";
    const MAIN_VOICE: &str = "bbbbbbbb22222222222222222222222222222222";

    /// git matches an `ls-remote` pattern against the tail of each ref, so
    /// asking for `refs/heads/main` can also return
    /// `refs/heads/x/refs/heads/main`. Only the exact ref may decide the
    /// latest commit, and a missing ref is reported as missing rather than
    /// read from a neighbouring line.
    #[test]
    fn ls_remote_takes_only_the_exact_ref() {
        let text =
            format!("{MAIN_VOICE}\trefs/heads/x/refs/heads/main\n{HEAD_KD}\trefs/heads/main\n");
        assert_eq!(
            parse_ls_remote(&text, "refs/heads/main").as_deref(),
            Some(HEAD_KD)
        );
        assert_eq!(parse_ls_remote(&text, "HEAD"), None);
        assert_eq!(parse_ls_remote("", "HEAD"), None);
        assert_eq!(parse_ls_remote("nothex\tHEAD\n", "HEAD"), None);
    }

    /// A default-branch install tracks the remote `HEAD`; one with a
    /// recorded branch tracks that branch, decoded as `update` decodes it.
    #[test]
    fn status_asks_for_the_ref_update_would_build() {
        let (_, branch) = recorded_url_and_branch("https://github.com/scode/kd#abc");
        assert_eq!(tracked_ref(branch.as_deref()), "HEAD");
        let (url, branch) =
            recorded_url_and_branch("https://github.com/scode/tool.git?branch=topic%2Fx#abc");
        assert_eq!(url, "https://github.com/scode/tool.git");
        assert_eq!(tracked_ref(branch.as_deref()), "refs/heads/topic/x");
    }

    fn tool(name: &str) -> Installed {
        Installed {
            name: name.to_owned(),
            version: "0.1.0".to_owned(),
            source: None,
        }
    }

    /// Every status a tool can have. Only a tool that is behind and that
    /// `update` will actually rebuild counts as "would change": a pinned
    /// tool or one installed with non-default options is skipped by
    /// `update`, and claiming otherwise would send the user to a command
    /// that does nothing for it.
    #[test]
    fn status_row_covers_every_case() {
        let src = "https://github.com/scode/kd#aaaaaaaa";
        let current: Result<String, String> = Ok(HEAD_KD.to_owned());
        let moved: Result<String, String> = Ok(MAIN_VOICE.to_owned());

        let row = status_row(&tool("kd"), src, None, Some(&current));
        assert_eq!(
            (row.status.as_str(), row.update_would_change),
            ("up to date", false)
        );
        assert_eq!(
            (row.installed.as_str(), row.latest.as_str()),
            ("aaaaaaaa", "aaaaaaaa")
        );

        let row = status_row(&tool("kd"), src, None, Some(&moved));
        assert_eq!(
            (row.status.as_str(), row.update_would_change),
            ("update available", true)
        );
        assert_eq!(row.latest, "bbbbbbbb");

        let row = status_row(&tool("kd"), src, Some("--features x"), Some(&moved));
        assert!(row.status.contains("skips it") && row.status.contains("--features x"));
        assert!(!row.update_would_change);

        let row = status_row(&tool("kd"), src, Some("--features x"), Some(&current));
        assert!(row.status.starts_with("up to date"), "{}", row.status);

        let row = status_row(
            &tool("kd"),
            "https://github.com/scode/kd?tag=v1#aaaaaaaa",
            None,
            None,
        );
        assert!(row.status.starts_with("pinned"), "{}", row.status);
        assert!(!row.update_would_change);
        assert_eq!(row.latest, "-");

        let failed: Result<String, String> = Err("cannot check: nope".to_owned());
        let row = status_row(&tool("kd"), src, None, Some(&failed));
        assert_eq!(
            (row.status.as_str(), row.update_would_change),
            ("cannot check: nope", false)
        );

        let row = status_row(
            &tool("kd"),
            "https://github.com/scode/kd",
            None,
            Some(&moved),
        );
        assert_eq!(row.installed, "?");
        assert!(row.status.starts_with("cannot compare"), "{}", row.status);
        assert!(!row.update_would_change);
    }

    /// The closing line names exactly the tools `update` would change, and
    /// says plainly when there are none.
    #[test]
    fn render_status_names_what_update_would_change() {
        let src = "https://github.com/scode/kd#aaaaaaaa";
        let moved: Result<String, String> = Ok(MAIN_VOICE.to_owned());
        let current: Result<String, String> = Ok(HEAD_KD.to_owned());
        let behind = status_row(&tool("kd"), src, None, Some(&moved));
        let fine = status_row(&tool("voice"), src, None, Some(&current));
        let text = render_status(&[behind, fine]);
        assert!(text.starts_with("TOOL"), "{text}");
        assert!(
            text.ends_with("`kd cargo scode update` would update: kd"),
            "{text}"
        );

        let fine = status_row(&tool("voice"), src, None, Some(&current));
        assert!(render_status(&[fine]).ends_with("nothing for `kd cargo scode update` to do"));
        assert_eq!(
            render_status(&[]),
            "no tools from github.com/scode installed"
        );
    }

    /// End to end against stub cargo and git: only scode tools are listed,
    /// each unpinned one is checked against the ref it tracks, a repository
    /// git cannot read is reported on its line while the others are still
    /// checked (the command's exit status is covered separately), and git is
    /// run with prompts turned off so a status check never stops for a
    /// password or passphrase.
    #[test]
    fn status_checks_each_unpinned_scode_tool() {
        let listing = "\
kd v0.1.0 (https://github.com/scode/kd#aaaaaaaa):
    kd
voice v0.2.0 (https://github.com/scode/voice?branch=main#cccccccc):
    voice
pinned v0.3.0 (https://github.com/scode/pinned?tag=v0.3.0#dddddddd):
    pinned
broken v0.1.0 (https://github.com/scode/broken#eeeeeeee):
    broken
other v1.0.0 (https://github.com/someone/other#abc123):
    other
";
        let scratch = tempfile::tempdir().unwrap();
        let ls_remote = scratch.path().join("ls-remote");
        std::fs::write(
            &ls_remote,
            format!("{HEAD_KD}\tHEAD\n{MAIN_VOICE}\trefs/heads/main\n"),
        )
        .unwrap();
        let git_log = scratch.path().join("git-log");
        let mut rows = Vec::new();
        let (_, result) = run_with_failing_stub_cargo(listing, "", true, "", EMPTY_CRATES2, |sh| {
            sh.set_var("STUB_LS_REMOTE", &ls_remote);
            sh.set_var("STUB_GIT_LOG", &git_log);
            // The developer's own ssh choice must not leak into the test;
            // an empty value counts as unset.
            sh.set_var("GIT_SSH_COMMAND", "");
            sh.set_var("GIT_SSH", "");
            rows = status_rows(sh)?;
            Ok(())
        });
        result.unwrap();
        let summary: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.status.as_str()))
            .collect();
        assert_eq!(summary[0], ("kd", "up to date"));
        assert_eq!(summary[1], ("voice", "update available"));
        assert!(
            summary[2].0 == "pinned" && summary[2].1.starts_with("pinned"),
            "{summary:?}"
        );
        assert!(
            summary[3].0 == "broken" && summary[3].1.ends_with("fatal: repository not found"),
            "{summary:?}"
        );
        assert!(rows[3].check_failed && !rows[2].check_failed, "{rows:?}");
        assert_eq!(rows.len(), 4, "{summary:?}");

        let git_log = std::fs::read_to_string(&git_log).unwrap();
        assert!(
            git_log.contains(&format!(
                "0||{SSH_BATCH_COMMAND}|-c http.lowSpeedLimit=1000 -c http.lowSpeedTime=20 ls-remote https://github.com/scode/kd HEAD"
            )),
            "{git_log}"
        );
        assert!(
            git_log.contains("ls-remote https://github.com/scode/voice refs/heads/main"),
            "{git_log}"
        );
        assert!(!git_log.contains("pinned"), "{git_log}");
    }

    /// Without git there is nothing to ask; every unpinned tool says why on
    /// its own line, rather than the listing failing before anything is
    /// shown.
    #[test]
    fn status_without_git_reports_on_each_line() {
        let mut rows = Vec::new();
        let (_, result) =
            run_with_failing_stub_cargo(KD_LISTING, "", false, "", EMPTY_CRATES2, |sh| {
                rows = status_rows(sh)?;
                Ok(())
            });
        result.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].status.contains("git is not on PATH"),
            "{}",
            rows[0].status
        );
    }

    /// A user who chose how git runs ssh keeps that choice: `status` must
    /// not swap in its batch-mode command, or it would fail with a key
    /// error where `update` works. Covers the variable and
    /// `core.sshCommand`, which git lets `GIT_SSH_COMMAND` override.
    #[test]
    fn status_keeps_the_users_ssh_command() {
        for (var, core) in [("myssh -i key", ""), ("", "myssh -i key")] {
            let scratch = tempfile::tempdir().unwrap();
            let git_log = scratch.path().join("git-log");
            let (_, result) =
                run_with_failing_stub_cargo(KD_LISTING, "", true, "", EMPTY_CRATES2, |sh| {
                    sh.set_var("STUB_GIT_LOG", &git_log);
                    sh.set_var("GIT_SSH_COMMAND", var);
                    sh.set_var("GIT_SSH", "");
                    sh.set_var("STUB_CORE_SSH", core);
                    status_rows(sh).map(|_| ())
                });
            result.unwrap();
            let git_log = std::fs::read_to_string(&git_log).unwrap();
            let ls_remote = git_log.lines().find(|l| l.contains("ls-remote")).unwrap();
            // git must get exactly the user's variable, or none at all so
            // its `core.sshCommand` applies.
            assert!(ls_remote.starts_with(&format!("0||{var}|")), "{ls_remote}");
        }
    }

    /// The reason shown for a failed check keeps what tells failures apart:
    /// over ssh, the line before git's generic `fatal:` is ssh's own error,
    /// and dropping it would make a missing key look like a firewall.
    /// Warnings, `remote:` banners and git's access-rights trailer are
    /// noise. The ssh input is real git 2.43 output for a refused
    /// connection.
    #[test]
    fn failure_reason_keeps_the_cause() {
        let refused = "ssh: connect to host 127.0.0.1 port 1: Connection refused\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n";
        assert_eq!(
            failure_reason(refused),
            "ssh: connect to host 127.0.0.1 port 1: Connection refused; fatal: Could not read from remote repository."
        );
        assert_eq!(
            failure_reason("Warning: Permanently added x\nfatal: nope\n\n"),
            "fatal: nope"
        );
        assert_eq!(
            failure_reason("remote: hi\nsomething broke\n"),
            "something broke"
        );
        assert_eq!(failure_reason(""), "no error output");
    }

    /// When a check fails, the closing lines say which tools went unchecked
    /// and claim "nothing to do" only for the rest, and the command exits
    /// non-zero, so neither a reader nor a script takes an incomplete answer
    /// for a complete one.
    #[test]
    fn failed_checks_are_named_and_fail_the_command() {
        let src = "https://github.com/scode/kd#aaaaaaaa";
        let current: Result<String, String> = Ok(HEAD_KD.to_owned());
        let failed: Result<String, String> = Err("cannot check: offline".to_owned());
        let rows = [
            status_row(&tool("kd"), src, None, Some(&current)),
            status_row(&tool("voice"), src, None, Some(&failed)),
        ];
        let text = render_status(&rows);
        assert!(text.contains("\ncould not check: voice\n"), "{text}");
        assert!(
            text.ends_with("to do among the tools that could be checked"),
            "{text}"
        );

        let (_, result) =
            run_with_failing_stub_cargo(KD_LISTING, "", false, "", EMPTY_CRATES2, scode_status);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("could not be checked")
        );
    }
}
