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
    check(
        "timezone",
        "[ \"$(timedatectl show -p Timezone --value 2>/dev/null || cat /etc/timezone)\" = America/Los_Angeles ]",
    );
    check("gh auth", "gh auth status >/dev/null 2>&1");
    check(
        "repo count",
        &format!(
            "[ \"$(ls -d \"$HOME\"/git/*/ 2>/dev/null | wc -l | tr -d ' ')\" = {expected_repos} ]"
        ),
    );
    check("ssh localhost", "ssh -o BatchMode=yes localhost true");
    check("docker", "docker ps >/dev/null 2>&1");
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
    check(
        "codex request",
        "codex exec --skip-git-repo-check 'reply ok' >/dev/null 2>&1",
    );
    check("claude request", "claude -p ok >/dev/null 2>&1");
    check("opencode request", "opencode run ok >/dev/null 2>&1");
    check("muse request", "muse exec ok >/dev/null 2>&1");
    s
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
