//! Top-level command dispatch. Each submodule owns a domain of functionality.

pub mod devbox;
pub mod gh;
pub mod ubiworker;
pub mod yt;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Disposable remote development box: backup, resume, bootstrap
    Devbox {
        #[command(subcommand)]
        cmd: devbox::Commands,
    },
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
            Commands::Devbox { cmd } => cmd.run(),
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
    /// Mirrors `main.rs`'s `Cli` shape closely enough to matter: `verbose`
    /// and `quiet` are included (as `global = true`, matching `main.rs`)
    /// specifically so the `ubiworker ssh` tests below can assert that a
    /// forwarded ssh flag like `-v` was *not* captured by kd's own global
    /// verbosity flag — that ambiguity is exactly what bug B2 was, and a
    /// `TestCli` without these fields couldn't observe it either way.
    ///
    /// These tests are deliberately narrow: they exercise argv *wiring*
    /// (does this subcommand shape parse, does clap reject an unknown one,
    /// and — for `ssh` — which tokens end up on which side of the
    /// name/ssh-args split) rather than command *behavior*, which each
    /// command's own module tests cover.
    #[derive(Parser)]
    struct TestCli {
        #[arg(short, long, action = clap::ArgAction::Count, global = true)]
        verbose: u8,

        #[arg(short, long, action = clap::ArgAction::Count, global = true)]
        quiet: u8,

        #[command(subcommand)]
        command: Commands,
    }

    fn parses(args: &[&str]) -> bool {
        TestCli::try_parse_from(args).is_ok()
    }

    /// Parse `args` as a `kd ubiworker ssh ...` invocation and return the
    /// raw `SshArgs::args` vector (as `&str`s, for assertion convenience)
    /// alongside the parsed `verbose` count — the pair every test below
    /// needs to check both "did the split land where expected" and "did kd's
    /// own global flag get stolen instead."
    ///
    /// Panics on a parse failure or on any `Commands` shape other than
    /// `Ubiworker { cmd: ubiworker::Commands::Ssh(_) }`: every caller
    /// constructs `args` to be a valid `ssh` invocation, so either failure
    /// mode means the test itself is wrong, not the code under test.
    fn parse_ssh(args: &[&str]) -> (Vec<String>, u8) {
        let cli = TestCli::try_parse_from(args).expect("expected a successful parse");
        let Commands::Ubiworker {
            cmd: ubiworker::Commands::Ssh(ssh_args),
        } = cli.command
        else {
            panic!("expected `ubiworker ssh` args, got a different subcommand");
        };
        let args: Vec<String> = ssh_args
            .args
            .iter()
            .map(|a| a.to_str().expect("test args are UTF-8").to_string())
            .collect();
        (args, cli.verbose)
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

    #[test]
    fn ubiworker_ssh_no_args_parses() {
        assert!(parses(&["kd", "ubiworker", "ssh"]));
    }

    #[test]
    fn ubiworker_ssh_with_name_parses() {
        let (args, _) = parse_ssh(&["kd", "ubiworker", "ssh", "myname"]);
        assert_eq!(args, vec!["myname"]);
    }

    /// Regression test for bug B2: `myname -v` used to have `-v` stolen by
    /// kd's own global `--verbose` flag instead of reaching `ssh_args`,
    /// because the old two-positional shape (`name` then a separately
    /// trailing `ssh_args`) gave clap a point to hand `-v` to kd before the
    /// trailing capture "started". The single-positional `SshArgs::args`
    /// (see its docs) removes that handoff point: once anything after `ssh`
    /// starts matching, every token — flag-shaped or not — lands in the one
    /// vec. Assert both halves: the flag must be *in* the parsed args, and
    /// kd's own verbosity count must stay zero.
    #[test]
    fn ubiworker_ssh_name_then_flag_forwards_flag_not_kd_verbosity() {
        let (args, verbose) = parse_ssh(&["kd", "ubiworker", "ssh", "myname", "-v"]);
        assert_eq!(args, vec!["myname", "-v"]);
        assert_eq!(verbose, 0, "-v must not be consumed as kd's own flag");
    }

    /// A forwarded flag (as opposed to a remote command) must survive
    /// parsing appended after the name, unreordered — this is the
    /// port-forwarding use case (`kd ubiworker ssh name -L 8080:localhost:80`).
    #[test]
    fn ubiworker_ssh_with_name_and_flag_style_ssh_args_parses() {
        let (args, _) = parse_ssh(&[
            "kd",
            "ubiworker",
            "ssh",
            "myname",
            "-L",
            "8080:localhost:80",
        ]);
        assert_eq!(args, vec!["myname", "-L", "8080:localhost:80"]);
    }

    #[test]
    fn ubiworker_ssh_with_name_and_remote_command_parses() {
        let (args, _) = parse_ssh(&["kd", "ubiworker", "ssh", "myname", "uptime"]);
        assert_eq!(args, vec!["myname", "uptime"]);
    }

    /// Regression test for bug B1: `-L 8080:x` with neither a name nor `--`
    /// used to be rejected outright by the old two-positional shape (the
    /// `name` positional couldn't accept a hyphen-leading token, and clap
    /// had no other place to route it). Empirically, the single-positional
    /// shape (`trailing_var_arg` + `allow_hyphen_values` on the *first and
    /// only* positional) does accept it — clap has nothing else competing
    /// for a hyphen-leading token in first position once inside `ssh_args`'s
    /// grabbing range, so it's captured as a value rather than treated as an
    /// unrecognized flag. This is *not* the same as saying every leading
    /// flag is safe without `--`: a token that collides with one of kd's own
    /// registered flags (`-v`/`-q`/`-h`) is still claimed by kd first, since
    /// clap resolves its own declared flags before falling through to a
    /// value positional (see
    /// `ubiworker_ssh_leading_dash_v_is_claimed_by_kd_not_forwarded` below)
    /// — `--` remains the documented, unambiguous way to route any
    /// ssh flag through untouched.
    #[test]
    fn ubiworker_ssh_leading_hyphen_flag_without_separator_is_captured() {
        let (args, _) = parse_ssh(&["kd", "ubiworker", "ssh", "-L", "8080:localhost:80"]);
        assert_eq!(args, vec!["-L", "8080:localhost:80"]);
    }

    /// Sharpens the previous test: a leading token that happens to collide
    /// with one of kd's own *registered* flags is claimed by kd, not
    /// forwarded — unlike an arbitrary unregistered flag like `-L`. `-v`
    /// bumps kd's own verbosity count to 1 and does not appear in the parsed
    /// ssh args at all. This is exactly the case the `SshArgs::args`
    /// doc-comment and the README point at `--` for.
    #[test]
    fn ubiworker_ssh_leading_dash_v_is_claimed_by_kd_not_forwarded() {
        let (args, verbose) = parse_ssh(&["kd", "ubiworker", "ssh", "-v"]);
        assert!(args.is_empty());
        assert_eq!(verbose, 1);
    }

    /// `--` is the documented escape hatch for passing ssh flags with no
    /// worker name: everything after it must land in the parsed args
    /// (`split_target` then reads the first one as "no name" because it
    /// starts with `-`) rather than being parsed as kd's own arguments —
    /// this is what makes `-v`/`-q`/`-h` forwardable at all despite the
    /// previous test.
    #[test]
    fn ubiworker_ssh_leading_hyphen_after_separator_parses() {
        let (args, verbose) =
            parse_ssh(&["kd", "ubiworker", "ssh", "--", "-L", "8080:localhost:80"]);
        assert_eq!(args, vec!["-L", "8080:localhost:80"]);
        assert_eq!(verbose, 0);
    }

    /// `kd ubiworker ssh --` (empty trailing args): must parse to an empty
    /// vec, not an error — this is the "sole worker, no ssh args" form.
    #[test]
    fn ubiworker_ssh_bare_separator_parses_to_empty_args() {
        let (args, _) = parse_ssh(&["kd", "ubiworker", "ssh", "--"]);
        assert!(args.is_empty());
    }

    /// Same B2 regression as above, but with `-q` positioned *before* the
    /// subcommand rather than after the name — this is the acceptance-test
    /// shape from the review (`kd -q ubiworker ssh myname -v`), which used
    /// to fail with kd's own verbose/quiet conflict error because `-v` was
    /// captured as kd's flag while `-q` was already set. Confirms the fix
    /// holds regardless of where the *other* global flag was given.
    #[test]
    fn ubiworker_ssh_global_quiet_before_subcommand_does_not_conflict_with_forwarded_v() {
        let cli = TestCli::try_parse_from(["kd", "-q", "ubiworker", "ssh", "myname", "-v"])
            .expect("expected a successful parse");
        assert_eq!(cli.quiet, 1);
        assert_eq!(
            cli.verbose, 0,
            "-v must be forwarded, not counted as kd's flag"
        );
        let Commands::Ubiworker {
            cmd: ubiworker::Commands::Ssh(ssh_args),
        } = cli.command
        else {
            panic!("expected ubiworker ssh");
        };
        let args: Vec<String> = ssh_args
            .args
            .iter()
            .map(|a| a.to_str().unwrap().to_string())
            .collect();
        assert_eq!(args, vec!["myname", "-v"]);
    }
}
