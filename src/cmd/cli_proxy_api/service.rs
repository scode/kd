//! `kd cli-proxy-api monitor enable|disable`: install or remove the systemd
//! user unit that keeps `monitor run` running across logouts and reboots.
//!
//! The unit runs the same `kd` binary that enabled it, by absolute path, with
//! no flags, so the daemon always uses the default key file and the fixed log
//! path `overview` reads. Enabling always rewrites the unit and restarts it,
//! which is also how an upgraded `kd` binary takes effect. Linger is turned
//! on so the user manager, and with it the monitor, starts at boot rather
//! than at first login.
//!
//! Every external command goes through [`Host`], so the sequence of
//! `systemctl` and `loginctl` calls is tested without touching the host's
//! systemd.

use anyhow::{Context, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unit name, also what `journalctl --user -u` takes.
pub const UNIT: &str = "kd-cli-proxy-api-monitor.service";

/// Restart delay. systemd's default (100 ms) combined with its start-rate
/// limit (five starts in ten seconds) would leave a unit that fails at
/// startup permanently failed within a second; a minute keeps it retrying
/// indefinitely, which is what "keeps running" needs.
const RESTART_SEC: u32 = 60;

/// Result of one external command: success and trimmed stdout. Stderr is
/// folded into the error message on failure.
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs external commands. [`System`] is the real implementation; tests
/// record calls instead.
pub trait Host {
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Ran>;
}

/// Runs commands on this machine, inheriting the environment, which is what
/// lets `systemctl --user` find the user manager's bus.
pub struct System;

impl Host for System {
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Ran> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("running {program}"))?;
        Ok(Ran {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Run a command that must succeed.
fn must(host: &dyn Host, program: &str, args: &[&str]) -> anyhow::Result<String> {
    let ran = host.run(program, args)?;
    if !ran.ok {
        bail!("{program} {} failed: {}", args.join(" "), ran.stderr);
    }
    Ok(ran.stdout)
}

/// Where the unit file lives: the standard per-user unit directory.
pub fn unit_path(home: &Path) -> PathBuf {
    home.join(".config").join("systemd").join("user").join(UNIT)
}

/// The unit file text for `exe`.
pub fn unit_text(exe: &Path) -> String {
    format!(
        "# Written by `kd cli-proxy-api monitor enable`; rewritten on every enable.\n\
         [Unit]\n\
         Description=kd CLIProxyAPI monitor (Claude account priorities and usage log)\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={} cli-proxy-api monitor run\n\
         Restart=always\n\
         RestartSec={RESTART_SEC}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_quote(&exe.to_string_lossy())
    )
}

/// Quote the `ExecStart` program path. systemd splits on whitespace,
/// unescapes backslashes and quotes inside double quotes, and expands `%`
/// specifiers even there, so those three are escaped. `$` is deliberately
/// left alone: systemd expands variables only in the arguments, never in
/// the program path, so `$$` would reach `execve` literally. Always quoting
/// keeps the rule simple; a plain path reads the same either way.
fn exec_quote(word: &str) -> String {
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Fail with an explanation unless a systemd user manager is reachable.
/// The two usual causes need different remedies, so the message names both
/// and quotes what systemctl said: on macOS or in a container there is no
/// systemd and `monitor run` needs another supervisor, while a `su` or
/// `sudo -u` shell on a systemd host merely lacks the user's session bus
/// and works from a real login (SSH) as that user.
fn require_user_manager(host: &dyn Host) -> anyhow::Result<()> {
    let detail = match host.run("systemctl", &["--user", "show", "--property=Version"]) {
        Ok(r) if r.ok => return Ok(()),
        Ok(r) => r.stderr,
        Err(err) => format!("{err:#}"),
    };
    bail!(
        "no systemd user manager is reachable ({detail}); from a `su` or `sudo -u` shell, log in as the user \
         directly instead (for example over SSH); without systemd, run `kd cli-proxy-api monitor run` under \
         another supervisor"
    )
}

/// Install or refresh the unit, enable it, (re)start it, and make sure
/// linger is on. Safe to repeat: every step is idempotent, and the restart
/// is what makes a rebuilt binary take effect.
pub fn enable(host: &dyn Host, home: &Path, exe: &Path) -> anyhow::Result<String> {
    require_user_manager(host)?;

    // Without linger the user manager stops at logout and only starts at
    // login, which would take the monitor down with every closed SSH session
    // and keep it down after a reboot. Done before anything is installed, so
    // a linger failure cannot leave a monitor running that silently dies at
    // logout. Checked first so an already-lingering user never triggers a
    // polkit prompt.
    let uid = must(host, "id", &["-u"])?;
    let linger = must(
        host,
        "loginctl",
        &["show-user", &uid, "--property=Linger", "--value"],
    )?;
    let linger_note = if linger == "yes" {
        "linger already on"
    } else {
        must(host, "loginctl", &["enable-linger"])?;
        "linger turned on"
    };

    let path = unit_path(home);
    write_atomically(&path, &unit_text(exe))
        .with_context(|| format!("writing {}", path.display()))?;
    must(host, "systemctl", &["--user", "daemon-reload"])?;
    must(host, "systemctl", &["--user", "enable", UNIT])?;
    // With `Type=exec` a restart fails when the binary cannot be executed
    // (moved, or a deleted build directory), instead of reporting success
    // and leaving the unit in a restart loop.
    must(host, "systemctl", &["--user", "restart", UNIT])?;
    Ok(format!(
        "enabled {UNIT} running {} ({linger_note})\nunit file: {}\nlogs: journalctl --user -u {UNIT} -f\n",
        exe.display(),
        path.display()
    ))
}

/// Stop and disable the unit and remove its file. Linger is left alone:
/// other user services may rely on it, and it does nothing by itself.
pub fn disable(host: &dyn Host, home: &Path) -> anyhow::Result<String> {
    let path = unit_path(home);
    if !path.exists() {
        return Ok(format!("{UNIT} is not installed; nothing to do\n"));
    }
    require_user_manager(host)?;
    must(host, "systemctl", &["--user", "disable", "--now", UNIT])?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    must(host, "systemctl", &["--user", "daemon-reload"])?;
    Ok(format!("disabled and removed {UNIT}\n"))
}

/// Write via a temporary file in the same directory and a rename, so
/// systemd never reads a half-written unit.
fn write_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("unit path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every command and answers from a table: commands whose
    /// joined text starts with a key in `fail` fail, `stdout` gives canned
    /// output.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        fail: Vec<&'static str>,
        stdout: Vec<(&'static str, &'static str)>,
    }

    impl Host for Fake {
        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Ran> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            let stdout = self
                .stdout
                .iter()
                .find(|(k, _)| line.starts_with(k))
                .map_or("", |(_, v)| v);
            Ok(Ran {
                ok: !self.fail.iter().any(|k| line.starts_with(k)),
                stdout: stdout.to_owned(),
                stderr: "boom".to_owned(),
            })
        }
    }

    fn lingering() -> Fake {
        Fake {
            stdout: vec![("id -u", "1000"), ("loginctl show-user", "yes")],
            ..Fake::default()
        }
    }

    /// The unit runs the enabling binary by absolute path with no flags, and
    /// restarts slowly enough that systemd's start-rate limit can never give
    /// up on it.
    #[test]
    fn unit_runs_this_binary_with_slow_restarts() {
        let text = unit_text(Path::new("/opt/bin/kd"));
        assert!(text.contains("Type=exec\nExecStart=\"/opt/bin/kd\" cli-proxy-api monitor run\n"));
        assert!(text.contains("Restart=always\nRestartSec=60\n"));
        assert!(text.contains("WantedBy=default.target"));
    }

    /// Characters systemd would interpret inside ExecStart must reach the
    /// command line literally.
    #[test]
    fn exec_paths_are_quoted_for_systemd() {
        assert_eq!(
            exec_quote(r#"/a b/100%/$x/"q"\z"#),
            r#""/a b/100%%/$x/\"q\"\\z""#
        );
    }

    /// Enable writes the unit, reloads, enables, and restarts (so a new
    /// binary takes effect), and leaves linger alone when it is already on.
    #[test]
    fn enable_installs_and_restarts() {
        let home = tempfile::tempdir().unwrap();
        let host = lingering();
        let report = enable(&host, home.path(), Path::new("/opt/bin/kd")).unwrap();
        assert!(report.contains("linger already on"), "{report}");
        assert_eq!(
            *host.calls.borrow(),
            vec![
                "systemctl --user show --property=Version",
                "id -u",
                "loginctl show-user 1000 --property=Linger --value",
                "systemctl --user daemon-reload",
                "systemctl --user enable kd-cli-proxy-api-monitor.service",
                "systemctl --user restart kd-cli-proxy-api-monitor.service",
            ]
        );
        let text = std::fs::read_to_string(unit_path(home.path())).unwrap();
        assert_eq!(text, unit_text(Path::new("/opt/bin/kd")));
    }

    /// Without linger the monitor would stop at logout, so enable turns it
    /// on.
    #[test]
    fn enable_turns_on_linger() {
        let home = tempfile::tempdir().unwrap();
        let host = Fake {
            stdout: vec![("id -u", "1000"), ("loginctl show-user", "no")],
            ..Fake::default()
        };
        let report = enable(&host, home.path(), Path::new("/kd")).unwrap();
        assert!(report.contains("linger turned on"));
        assert_eq!(host.calls.borrow()[3], "loginctl enable-linger");
    }

    /// A linger failure stops enable before anything is installed, so it
    /// cannot leave a monitor running that dies at the next logout.
    #[test]
    fn linger_failure_installs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let host = Fake {
            stdout: vec![("id -u", "1000"), ("loginctl show-user", "no")],
            fail: vec!["loginctl enable-linger"],
            ..Fake::default()
        };
        assert!(enable(&host, home.path(), Path::new("/kd")).is_err());
        assert!(!unit_path(home.path()).exists());
        assert!(!host.calls.borrow().iter().any(|c| c.contains("restart")));
    }

    /// Without a user manager there is nothing to install into: refuse
    /// before writing anything.
    #[test]
    fn enable_refuses_without_systemd() {
        let home = tempfile::tempdir().unwrap();
        let host = Fake {
            fail: vec!["systemctl --user show"],
            ..Fake::default()
        };
        let err = enable(&host, home.path(), Path::new("/kd")).unwrap_err();
        assert!(
            err.to_string()
                .contains("no systemd user manager is reachable (boom)"),
            "{err}"
        );
        assert!(!unit_path(home.path()).exists());
    }

    /// A machine without `systemctl` at all (macOS) gets the same refusal,
    /// carrying the spawn error.
    #[test]
    fn enable_refuses_without_systemctl() {
        struct Missing;
        impl Host for Missing {
            fn run(&self, program: &str, _: &[&str]) -> anyhow::Result<Ran> {
                anyhow::bail!("running {program}: No such file or directory")
            }
        }
        let home = tempfile::tempdir().unwrap();
        let err = enable(&Missing, home.path(), Path::new("/kd")).unwrap_err();
        assert!(
            err.to_string().contains("No such file or directory"),
            "{err}"
        );
        assert!(!unit_path(home.path()).exists());
    }

    /// A failing systemctl step stops enable and reports its stderr.
    #[test]
    fn enable_reports_failed_steps() {
        let home = tempfile::tempdir().unwrap();
        let host = Fake {
            fail: vec!["systemctl --user restart"],
            ..lingering()
        };
        let err = enable(&host, home.path(), Path::new("/kd")).unwrap_err();
        assert!(err.to_string().contains("restart"), "{err}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    /// Disable stops and removes the unit, and is a no-op when nothing is
    /// installed (so it never needs systemd just to say so).
    #[test]
    fn disable_removes_the_unit() {
        let home = tempfile::tempdir().unwrap();
        let host = Fake::default();
        assert!(
            disable(&host, home.path())
                .unwrap()
                .contains("not installed")
        );
        assert!(host.calls.borrow().is_empty());

        enable(&lingering(), home.path(), Path::new("/kd")).unwrap();
        let unreachable = Fake {
            fail: vec!["systemctl --user show"],
            ..Fake::default()
        };
        assert!(disable(&unreachable, home.path()).is_err());
        assert!(
            unit_path(home.path()).exists(),
            "kept when it cannot be stopped"
        );
        let host = Fake::default();
        disable(&host, home.path()).unwrap();
        assert!(!unit_path(home.path()).exists());
        assert_eq!(
            *host.calls.borrow(),
            vec![
                "systemctl --user show --property=Version",
                "systemctl --user disable --now kd-cli-proxy-api-monitor.service",
                "systemctl --user daemon-reload",
            ]
        );
    }
}
