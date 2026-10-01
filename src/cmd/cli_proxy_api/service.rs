//! `kd cli-proxy-api monitor enable|disable|restart`: install, remove, or
//! restart the systemd user unit that keeps `monitor run` running across
//! logouts and reboots.
//!
//! The unit runs the same `kd` binary that enabled it, by absolute path, with
//! no flags, so the daemon always uses the default key file and the fixed log
//! path `overview` reads. Enabling always rewrites the unit and restarts it.
//! After an in-place upgrade of `kd`, `restart` alone makes the running
//! monitor pick up the new binary. Linger is turned on so the user manager,
//! and with it the monitor, starts at boot rather than at first login.
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

/// The `ExecStart=` line for `exe`. Kept separate so `restart` can tell
/// which binary an installed unit runs without comparing the rest of the
/// file, which changes across kd versions and may be edited by hand.
fn exec_start(exe: &Path) -> String {
    format!(
        "ExecStart={} cli-proxy-api monitor run",
        exec_quote(&exe.to_string_lossy())
    )
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
         {}\n\
         Restart=always\n\
         RestartSec={RESTART_SEC}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_start(exe)
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

/// Restart the enabled unit, so a monitor running an upgraded binary
/// picks up the new code: `kd cargo scode update` replaces the binary at
/// the same path, and a running process keeps the old one until restarted.
///
/// It refuses unless the unit is installed and enabled, rather than
/// starting a monitor that was never set up or that the user switched off;
/// the refusal names the unit path, since a wrong `HOME` (sudo, another
/// user) looks the same as "not enabled". It reloads the user manager
/// first, so a unit file edited since the last load is what restarts, and
/// the binary check below describes what actually runs.
///
/// The unit is not rewritten. When its `ExecStart` runs a different binary
/// than `exe` (it was enabled from another path), the report says so and
/// points at `enable`, which repoints it; when the restart fails, the same
/// hint is attached to the error, since a binary that no longer exists at
/// the unit's path is the usual cause. `exe` is only used for that hint,
/// so when the running binary cannot be located it is `None` and the hint
/// is skipped rather than the restart.
pub fn restart(host: &dyn Host, home: &Path, exe: Option<&Path>) -> anyhow::Result<String> {
    let path = unit_path(home);
    let not_enabled = || {
        anyhow::anyhow!(
            "the monitor is not enabled ({} is not an enabled unit); run `kd cli-proxy-api monitor enable`",
            path.display()
        )
    };
    let installed = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(not_enabled()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    require_user_manager(host)?;
    must(host, "systemctl", &["--user", "daemon-reload"])?;
    if !host
        .run("systemctl", &["--user", "is-enabled", "--quiet", UNIT])
        .is_ok_and(|r| r.ok)
    {
        return Err(not_enabled());
    }
    let hint = exe
        .filter(|exe| !installed.lines().any(|line| line.trim() == exec_start(exe)))
        .map(|exe| {
            format!(
                "the unit does not run {}; run `kd cli-proxy-api monitor enable` to switch it to this binary",
                exe.display()
            )
        });
    if let Err(err) = must(host, "systemctl", &["--user", "restart", UNIT]) {
        return Err(match &hint {
            Some(hint) => err.context(hint.clone()),
            None => err,
        });
    }
    let mut report = format!("restarted {UNIT}\n");
    if let Some(hint) = hint {
        report.push_str(&format!("note: {hint}\n"));
    }
    report.push_str(&format!("logs: journalctl --user -u {UNIT} -f\n"));
    Ok(report)
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

    /// Restart only works on an installed, enabled unit and a reachable
    /// user manager; it reloads first so an edited unit is what restarts,
    /// never re-enables or touches the file, and points at `enable` only
    /// when the unit's `ExecStart` runs another binary: a unit written by an
    /// older kd, or edited elsewhere, gets no false note.
    #[test]
    fn restart_acts_only_on_an_enabled_unit() {
        let home = tempfile::tempdir().unwrap();
        let kd = Path::new("/kd");
        let host = Fake::default();
        let err = restart(&host, home.path(), Some(kd)).unwrap_err();
        assert!(err.to_string().contains("not enabled"), "{err}");
        assert!(
            err.to_string()
                .contains("kd-cli-proxy-api-monitor.service is not an enabled unit"),
            "names the path it looked at: {err}"
        );
        assert!(host.calls.borrow().is_empty());

        enable(&lingering(), home.path(), kd).unwrap();
        let disabled = Fake {
            fail: vec!["systemctl --user is-enabled"],
            ..Fake::default()
        };
        let err = restart(&disabled, home.path(), Some(kd)).unwrap_err();
        assert!(err.to_string().contains("not enabled"), "{err}");
        assert!(
            !disabled
                .calls
                .borrow()
                .iter()
                .any(|c| c.contains("restart"))
        );

        let host = Fake::default();
        let report = restart(&host, home.path(), Some(kd)).unwrap();
        assert_eq!(
            report,
            "restarted kd-cli-proxy-api-monitor.service\nlogs: journalctl --user -u kd-cli-proxy-api-monitor.service -f\n"
        );
        assert_eq!(
            *host.calls.borrow(),
            vec![
                "systemctl --user show --property=Version",
                "systemctl --user daemon-reload",
                "systemctl --user is-enabled --quiet kd-cli-proxy-api-monitor.service",
                "systemctl --user restart kd-cli-proxy-api-monitor.service",
            ]
        );

        let report = restart(&Fake::default(), home.path(), Some(Path::new("/other/kd"))).unwrap();
        assert!(
            report.contains("note: the unit does not run /other/kd"),
            "{report}"
        );
        assert!(
            !restart(&Fake::default(), home.path(), None)
                .unwrap()
                .contains("note")
        );

        let path = unit_path(home.path());
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("RestartSec=60", "RestartSec=30");
        std::fs::write(&path, &edited).unwrap();
        assert!(
            !restart(&Fake::default(), home.path(), Some(kd))
                .unwrap()
                .contains("note")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            edited,
            "restart never rewrites the unit"
        );
    }

    /// A failed restart is an error, not "restarted"; when the unit runs
    /// another binary (typically one that no longer exists), the error
    /// carries the hint to repoint it with `enable`. An unreachable user
    /// manager is refused like for `enable`.
    #[test]
    fn restart_reports_failures() {
        let home = tempfile::tempdir().unwrap();
        enable(&lingering(), home.path(), Path::new("/gone/kd")).unwrap();
        let failing = Fake {
            fail: vec!["systemctl --user restart"],
            ..Fake::default()
        };
        let err = format!(
            "{:#}",
            restart(&failing, home.path(), Some(Path::new("/kd"))).unwrap_err()
        );
        assert!(err.contains("restart") && err.contains("boom"), "{err}");
        assert!(
            err.contains("monitor enable"),
            "the hint survives the failure: {err}"
        );
        let unreachable = Fake {
            fail: vec!["systemctl --user show"],
            ..Fake::default()
        };
        let err = restart(&unreachable, home.path(), None).unwrap_err();
        assert!(err.to_string().contains("no systemd user manager"), "{err}");
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
