//! The probe: the only success signal bootstrap has.
//!
//! One shell script, rendered per run because it carries expected values,
//! run as the user, printing one line per check as `<name>: ok` or
//! `<name>: FAIL (exit N)`. It is a report, never a gate: every check runs,
//! nothing is fatal, and bootstrap exits 0 once it has run. The agent's
//! own final messages are printed after it; between the two, a person can
//! see what the box actually is.
//!
//! It deliberately hardcodes one real request per agent CLI. That is drift
//! kd accepts (see SPEC_impl.md): a rotted probe line shows up as a failed
//! probe item, never as a failed run.

use super::transport::{Transport, shell_quote};
use std::path::Path;

/// Render the probe for this run. `expected_repos` is the manifest
/// deduplicated with the always-cloned repos. Rehearsal expects restored
/// services stopped; Tailscale is checked only when enrollment was requested.
pub fn script(
    hostname: &str,
    expected_repos: usize,
    rehearsal: bool,
    hermes: bool,
    enroll_tailscale: bool,
) -> String {
    let mut s = String::from(PRELUDE);
    let mut check = |name: &str, cmd: &str| {
        s.push_str(&format!(
            "check {} {}\n",
            shell_quote(name),
            shell_quote(cmd)
        ));
    };
    check(
        "hostname",
        &format!("[ \"$(hostname)\" = {} ]", shell_quote(hostname)),
    );
    for (name, command) in timezone_checks(
        Path::new("/etc"),
        Path::new("/usr/share/zoneinfo"),
        Path::new("/run/systemd/system"),
    ) {
        check(name, &command);
    }
    check("gh auth", "gh auth status >/dev/null 2>&1");
    check(
        "repo count",
        &format!(
            "[ \"$(ls -d \"$HOME\"/git/*/ 2>/dev/null | wc -l | tr -d ' ')\" = {expected_repos} ]"
        ),
    );
    check("ssh localhost", "ssh -o BatchMode=yes localhost true");
    check("docker", "docker ps >/dev/null 2>&1");
    // Installation is useful without cloud credentials; do not turn this
    // availability check into a login or a billable sandbox operation.
    check("tensorlake CLI", "tl --version >/dev/null 2>&1");
    // Check names must not spell out what the pgrep pattern matches: the
    // whole script is in the login shell's argv, so a name like "hermes
    // gateway stopped" would match `[h]ermes.*gateway` and fail every time.
    if hermes && rehearsal {
        check(
            "no gateway process (rehearsal)",
            "! pgrep -f '[h]ermes.*gateway' >/dev/null",
        );
    } else if hermes {
        check("gateway process", "pgrep -f '[h]ermes.*gateway' >/dev/null");
        check(
            "dashboard",
            "curl -fsS 127.0.0.1:9119/api/status >/dev/null",
        );
    }
    if enroll_tailscale {
        check("tailscale", "tailscale status >/dev/null 2>&1");
    }
    // A working package-manager copy can still lack standalone-only features.
    // Compare file identity so symlinks are accepted but PATH shadowing is not.
    check(
        "codex installation",
        "test \"$(command -v codex)\" -ef \"$HOME/.local/bin/codex\"",
    );
    check(
        "codex request",
        "codex exec --skip-git-repo-check 'reply ok' >/dev/null 2>&1",
    );
    check("claude request", "claude -p ok >/dev/null 2>&1");
    check(
        "claude onboarding",
        "jq -e '.hasCompletedOnboarding == true' \"$HOME/.claude.json\" >/dev/null",
    );
    check("opencode request", "opencode run ok >/dev/null 2>&1");
    check("muse request", "muse exec ok >/dev/null 2>&1");
    s
}

/// Check each timezone source independently: a correct systemd label must
/// not hide stale legacy metadata, and legacy metadata must not stand in for
/// the actual zoneinfo used by libc on hosts without systemd. Comparing zone
/// files covers daylight-saving rules rather than just today's UTC offset.
/// Paths are supplied explicitly so tests can reproduce conflicting files
/// without touching the controller's timezone or process environment.
fn timezone_checks(
    etc: &Path,
    zoneinfo: &Path,
    systemd_runtime: &Path,
) -> [(&'static str, String); 4] {
    let quote = |path: &Path| shell_quote(&path.to_string_lossy());
    let localtime = quote(&etc.join("localtime"));
    let legacy = quote(&etc.join("timezone"));
    let expected = quote(&zoneinfo.join("America/Los_Angeles"));
    [
        ("timezone data", format!("cmp -s {localtime} {expected}")),
        (
            "timezone metadata",
            format!(
                "if [ -e {legacy} ] || [ -L {legacy} ]; then [ \"$(cat {legacy})\" = America/Los_Angeles ]; fi"
            ),
        ),
        (
            "timezone systemd",
            format!(
                "if [ -d {} ]; then zone=$(timedatectl show -p Timezone --value) && [ \"$zone\" = America/Los_Angeles ]; fi",
                quote(systemd_runtime),
            ),
        ),
        (
            "timezone environment",
            "[ -z \"${TZ+x}\" ] || [ \"$TZ\" = America/Los_Angeles ]".to_owned(),
        ),
    ]
}

/// `check NAME CMD` runs CMD under `bash -c` and prints the verdict line.
/// Every check gets a timeout so a hung CLI cannot stall the report.
const PRELUDE: &str = r#"
check() {
  if timeout 180 bash -c "$2" >/dev/null 2>&1; then
    printf '%s: ok\n' "$1"
  else
    printf '%s: FAIL (exit %s)\n' "$1" "$?"
  fi
}
"#;

/// Run the probe and return its output. The exit status is ignored on
/// purpose; the lines are the result.
pub fn run(t: &Transport, script: &str) -> anyhow::Result<String> {
    let out = t.capture(script)?;
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    /// Evaluate the real probe commands against a miniature filesystem. The
    /// fake timedatectl lets failures be tested even on non-systemd macOS;
    /// bash and cmp remain real so file mismatches are not mocked away.
    fn timezone_results(
        localtime: Option<&str>,
        metadata: Option<&str>,
        systemd: Option<(i32, &str)>,
        tz: Option<&str>,
    ) -> Vec<bool> {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        let zoneinfo = dir.path().join("zoneinfo");
        let runtime = dir.path().join("systemd");
        let bin = dir.path().join("bin");
        for path in [&etc, &zoneinfo.join("America"), &bin] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(zoneinfo.join("America/Los_Angeles"), "zone data").unwrap();
        if let Some(data) = localtime {
            fs::write(etc.join("localtime"), data).unwrap();
        }
        if let Some(data) = metadata {
            fs::write(etc.join("timezone"), data).unwrap();
        }
        let (status, zone) = systemd.unwrap_or((99, "must not run"));
        if systemd.is_some() {
            fs::create_dir(&runtime).unwrap();
        }
        let command = bin.join("timedatectl");
        fs::write(
            &command,
            format!(
                "#!/bin/sh\nprintf '%s\\n' {}\nexit {status}\n",
                shell_quote(zone)
            ),
        )
        .unwrap();
        fs::set_permissions(command, fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        timezone_checks(&etc, &zoneinfo, &runtime)
            .into_iter()
            .map(|(_, script)| {
                let mut command = Command::new("bash");
                command
                    .args(["-c", &script])
                    .env("PATH", &path)
                    .env_remove("TZ");
                if let Some(tz) = tz {
                    command.env("TZ", tz);
                }
                command.output().unwrap().status.success()
            })
            .collect()
    }

    /// Ubuntu 24.04 can report the desired systemd timezone while keeping old
    /// /etc/timezone contents. Later releases may omit that legacy file;
    /// neither case may obscure the actual localtime data.
    #[test]
    fn timezone_sources_cannot_mask_each_other() {
        let running = Some((0, "America/Los_Angeles"));
        assert_eq!(
            timezone_results(Some("zone data"), Some("Europe/Berlin\n"), running, None),
            [true, false, true, true]
        );
        assert_eq!(
            timezone_results(
                Some("wrong data"),
                Some("America/Los_Angeles\n"),
                None,
                None
            ),
            [false, true, true, true]
        );
        assert_eq!(
            timezone_results(None, Some("America/Los_Angeles\n"), None, None),
            [false, true, true, true]
        );
        assert_eq!(
            timezone_results(Some("zone data"), None, None, None),
            [true; 4]
        );
        assert_eq!(
            timezone_results(
                Some("zone data"),
                Some("America/Los_Angeles\n"),
                running,
                None
            ),
            [true; 4]
        );
    }

    /// A broken systemd query must not fall back to a correct text file, and
    /// an inherited TZ override can defeat correct host files. Empty TZ means
    /// UTC on libc, so it must not be mistaken for an unset variable.
    #[test]
    fn timezone_runtime_failures_and_overrides_are_visible() {
        for systemd in [Some((1, "")), Some((0, "Europe/Berlin"))] {
            assert_eq!(
                timezone_results(
                    Some("zone data"),
                    Some("America/Los_Angeles"),
                    systemd,
                    None
                ),
                [true, true, false, true]
            );
        }
        for tz in ["", "UTC", "Europe/Berlin", "PST8"] {
            assert_eq!(
                timezone_results(Some("zone data"), None, None, Some(tz)),
                [true, true, true, false]
            );
        }
        assert_eq!(
            timezone_results(Some("zone data"), None, None, Some("America/Los_Angeles")),
            [true; 4]
        );
    }

    /// A rehearsal must not probe what only a real run sets up, and must
    /// expect the gateway stopped; a real run is the other way round.
    #[test]
    fn rehearsal_and_real_probe_different_hermes_and_network_checks() {
        let r = script("devbox", 25, true, true, false);
        assert!(r.contains("no gateway process"));
        assert!(!r.contains("'tailscale'"));
        assert!(!r.contains("'dashboard'"));
        let real = script("devbox", 25, false, true, true);
        assert!(real.contains("'gateway process'"));
        assert!(real.contains("'tailscale'"));
        assert!(real.contains("'dashboard'"));
    }

    /// The script text itself must never contain the literal the pgrep
    /// pattern matches, or the probe matches its own shell. This guards the
    /// check names above against a careless rename.
    #[test]
    fn script_never_contains_the_literal_it_greps_for() {
        for rehearsal in [true, false] {
            let s = script("devbox", 1, rehearsal, true, false).to_ascii_lowercase();
            assert!(!s.contains("hermes gateway"), "{s}");
        }
    }

    /// Scratch runs have no stateful checks; enrollment is independent of
    /// restoring an archive, so both network choices must work from scratch.
    #[test]
    fn no_hermes_drops_gateway_and_dashboard_checks() {
        for enroll in [true, false] {
            let s = script("devbox", 1, false, false, enroll);
            assert!(!s.contains("gateway"));
            assert!(!s.contains("'dashboard'"));
            assert_eq!(s.contains("'tailscale'"), enroll);
        }
    }

    /// Expected values are rendered in, which is why the script is a
    /// template rather than a constant.
    #[test]
    fn expected_values_are_rendered() {
        let s = script("box-1", 7, true, true, false);
        assert!(s.contains("box-1"));
        assert!(s.contains("= 7 ]"));
    }
}
