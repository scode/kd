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

use super::prompts::CARGO_GIT_PACKAGES;
use super::routers::{CLAUDE_NATIVE_SETTINGS, CLIPROXY_PORT, CODEX_LB_PORT, CODEX_NATIVE_OVERRIDE};
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
    // Installed from the repository, not a same-named crates.io or Homebrew
    // package: only a git install is what `cargo install-update -a -g`
    // updates from the default branch. `cargo install --list` prints
    // `name vX.Y.Z (https://github.com/owner/name#commit):` for those.
    for repo in CARGO_GIT_PACKAGES {
        let name = repo.rsplit('/').next().unwrap_or(repo);
        check(
            &format!("{name} from github"),
            &format!(
                "cargo install --list | grep -q {}",
                shell_quote(&format!("^{name} v[^ ]* (https://github.com/{repo}#"))
            ),
        );
        // cargo-update keeps per-package settings in `.install_config.toml`
        // in its install directory (`$CARGO_INSTALL_ROOT`, else
        // `$CARGO_HOME`, else `~/.cargo`); `--enforce-lock` sets
        // `enforce_lock = true` in the package's table. Without it, updates
        // would build unlocked even though the first install was locked.
        check(
            &format!("{name} updates locked"),
            &format!(
                "python3 -c 'import os, tomllib; c = tomllib.load(open(os.path.join(os.environ.get(\"CARGO_INSTALL_ROOT\") or os.environ.get(\"CARGO_HOME\") or os.path.expanduser(\"~/.cargo\"), \".install_config.toml\"), \"rb\")); assert c[\"{name}\"][\"enforce_lock\"] is True'"
            ),
        );
    }
    check(
        "cargo install-update",
        "cargo install-update --help >/dev/null 2>&1",
    );
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
    // Requests bypass the routers: on a fresh box they have no accounts yet,
    // so these lines check the credentials kd copied, not the routers.
    check(
        "codex request",
        &format!(
            "codex exec --skip-git-repo-check {CODEX_NATIVE_OVERRIDE} 'reply ok' >/dev/null 2>&1"
        ),
    );
    check(
        "claude request",
        &format!(
            "claude --settings {} -p ok >/dev/null 2>&1",
            shell_quote(CLAUDE_NATIVE_SETTINGS)
        ),
    );
    check(
        "claude onboarding",
        "jq -e '.hasCompletedOnboarding == true' \"$HOME/.claude.json\" >/dev/null",
    );
    // No OpenCode request: OpenCode picks its own default model from the
    // copied credentials, and on recent scratch bootstraps that model was
    // one its provider rejected, a failure unrelated to the box. It kept
    // every report red without saying anything about bootstrap, so it was
    // dropped; OpenCode is still installed, just not probed.
    check("muse request", "muse exec ok >/dev/null 2>&1");
    for (name, command) in router_checks() {
        check(name, &command);
    }
    s
}

/// Router checks cover what bootstrap can promise before any login: both
/// services answer, nothing listens beyond loopback, CLIProxyAPI rejects
/// requests without its client key and accepts them with it, and both CLIs
/// are wired to the routers. None of them needs an account; whether the
/// routers hold any is the user's post-bootstrap login, not a probe item.
fn router_checks() -> [(&'static str, String); 5] {
    [
        (
            "codex-lb health",
            format!("curl -fsS http://127.0.0.1:{CODEX_LB_PORT}/health"),
        ),
        (
            // The key goes to curl as a config file on stdin, never argv.
            "cliproxy key gate",
            format!(
                "url=http://127.0.0.1:{CLIPROXY_PORT}/v1/models; \
                 [ \"$(curl -s -o /dev/null -w '%{{http_code}}' \"$url\")\" = 401 ] && \
                 key=$(sed -n 's/^CLIPROXY_CLIENT_KEY=//p' \"$HOME/.config/cliproxy/secrets.env\") && \
                 [ -n \"$key\" ] && \
                 printf 'header = \"Authorization: Bearer %s\"\\n' \"$key\" | curl -fsS -K - \"$url\""
            ),
        ),
        (
            // A router on a public interface would be reachable from the
            // internet; that must be a visible failure, not a silent pass.
            "router listeners loopback-only",
            format!(
                "out=$(ss -ltnH '( sport = :{CODEX_LB_PORT} or sport = :{CLIPROXY_PORT} )') && \
                 [ \"$(printf '%s\\n' \"$out\" | grep -c .)\" -ge 2 ] && \
                 ! printf '%s\\n' \"$out\" | awk '{{print $4}}' | grep -qv '^127\\.0\\.0\\.1:'"
            ),
        ),
        (
            // Both keys: a stale token (say, secrets.env regenerated) would
            // otherwise pass here while every real request gets a 401. The
            // key reaches jq through the environment, not argv.
            "claude uses cliproxy",
            format!(
                "KD_KEY=$(sed -n 's/^CLIPROXY_CLIENT_KEY=//p' \"$HOME/.config/cliproxy/secrets.env\") && \
                 [ -n \"$KD_KEY\" ] && export KD_KEY && \
                 jq -e '.env.ANTHROPIC_BASE_URL == \"http://127.0.0.1:{CLIPROXY_PORT}\" and .env.ANTHROPIC_AUTH_TOKEN == $ENV.KD_KEY' \"$HOME/.claude/settings.json\""
            ),
        ),
        (
            // An active `profile` can override the top-level provider, and
            // a wrong base_url routes past codex-lb; check what Codex will
            // actually use, not just the key kd wrote.
            "codex uses codex-lb",
            format!(
                "python3 -c 'import tomllib, os
c = tomllib.load(open(os.path.expanduser(\"~/.codex/config.toml\"), \"rb\"))
profile = c.get(\"profiles\", {{}}).get(c.get(\"profile\"), {{}})
assert profile.get(\"model_provider\", c.get(\"model_provider\")) == \"codex-lb\"
assert c[\"model_providers\"][\"codex-lb\"][\"base_url\"] == \"http://127.0.0.1:{CODEX_LB_PORT}/backend-api/codex\"'"
            ),
        ),
    ]
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
    use std::path::Path;
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

    /// Router checks are hand-escaped shell inside Rust strings; a quoting
    /// slip there would turn into a probe line that always fails. Parse the
    /// whole rendered probe, and each router check as the inner `bash -c`
    /// will, before any box ever runs them.
    #[test]
    fn rendered_probe_and_router_checks_are_valid_bash() {
        let parses = |script: &str| {
            Command::new("bash")
                .args(["-n", "-c", script])
                .status()
                .unwrap()
                .success()
        };
        assert!(parses(&script("devbox", 1, false, true, true)));
        for (name, command) in router_checks() {
            assert!(parses(&command), "{name}: {command}");
        }
    }

    /// The loopback check exists to catch a router exposed on a public
    /// interface, and a check that cannot fail proves nothing. Feed it fake
    /// `ss` output: both routers on loopback passes; a wildcard or public
    /// listener, a missing router, or an `ss` failure must all fail.
    #[test]
    fn loopback_check_fails_on_public_or_missing_listeners() {
        let (_, command) = router_checks()
            .into_iter()
            .find(|(name, _)| *name == "router listeners loopback-only")
            .unwrap();
        let run = |listeners: &str, status: i32| {
            let dir = tempfile::tempdir().unwrap();
            let ss = dir.path().join("ss");
            fs::write(
                &ss,
                format!(
                    "#!/bin/sh
printf '%s' {}
exit {status}
",
                    shell_quote(listeners)
                ),
            )
            .unwrap();
            fs::set_permissions(&ss, fs::Permissions::from_mode(0o700)).unwrap();
            let path = std::env::join_paths(
                std::iter::once(dir.path().to_owned())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            Command::new("bash")
                .args(["-c", &command])
                .env("PATH", path)
                .status()
                .unwrap()
                .success()
        };
        let line = |addr: &str| format!("LISTEN 0 4096 {addr} 0.0.0.0:*\n");
        let both = line("127.0.0.1:2455") + &line("127.0.0.1:8317");
        assert!(run(&both, 0));
        assert!(!run(&(line("127.0.0.1:2455") + &line("0.0.0.0:8317")), 0));
        assert!(!run(&(both.clone() + &line("[::]:8317")), 0));
        assert!(!run(&line("127.0.0.1:2455"), 0));
        assert!(!run(&both, 1));
    }

    /// Run one named router check with `HOME` pointed at a fixture home.
    fn router_check_passes(name: &str, home: &Path) -> bool {
        let (_, command) = router_checks()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap();
        Command::new("bash")
            .args(["-c", &command])
            .env("HOME", home)
            .status()
            .unwrap()
            .success()
    }

    /// The wiring checks must fail when the wiring is subtly wrong, not only
    /// when it is absent: a stale Claude token, a Codex profile overriding
    /// the provider, or a provider pointing somewhere other than codex-lb.
    #[test]
    fn wiring_checks_catch_stale_tokens_profiles_and_wrong_urls() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for sub in [".config/cliproxy", ".claude", ".codex"] {
            fs::create_dir_all(home.join(sub)).unwrap();
        }
        fs::write(
            home.join(".config/cliproxy/secrets.env"),
            "CLIPROXY_CLIENT_KEY=sk-cpa-live\n",
        )
        .unwrap();
        let claude = |token: &str| {
            fs::write(
                home.join(".claude/settings.json"),
                format!(
                    r#"{{"env":{{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8317","ANTHROPIC_AUTH_TOKEN":"{token}"}}}}"#
                ),
            )
            .unwrap();
            router_check_passes("claude uses cliproxy", home)
        };
        assert!(claude("sk-cpa-live"));
        assert!(!claude("sk-cpa-stale"));

        let wired = "model_provider = \"codex-lb\"\n[model_providers.codex-lb]\nbase_url = \"http://127.0.0.1:2455/backend-api/codex\"\n";
        let codex = |config: &str| {
            fs::write(home.join(".codex/config.toml"), config).unwrap();
            router_check_passes("codex uses codex-lb", home)
        };
        assert!(codex(wired));
        assert!(codex(&format!(
            "profile = \"p\"\n{wired}[profiles.p]\nmodel = \"m\"\n"
        )));
        assert!(!codex(&format!(
            "profile = \"p\"\n{wired}[profiles.p]\nmodel_provider = \"openai\"\n"
        )));
        assert!(!codex(&wired.replace("2455", "9999")));
        assert!(!codex(&wired.replace("\"codex-lb\"\n[", "\"openai\"\n[")));
    }

    /// The git-install check must accept exactly the line `cargo install
    /// --list` prints for a git install of the repository, and reject a
    /// crates.io install of the same name or a fork.
    #[test]
    fn git_install_check_matches_only_the_repository_source() {
        let s = script("devbox", 1, false, false, false);
        let line = s
            .lines()
            .find(|l| l.starts_with("check 'kd from github'"))
            .expect("kd check present");
        let command = line.strip_prefix("check 'kd from github' ").unwrap();
        // Unwrap the single-quoted argument the script passes to `check`.
        let inner = Command::new("bash")
            .args(["-c", &format!("printf '%s' {command}")])
            .output()
            .unwrap();
        let inner = String::from_utf8(inner.stdout).unwrap();
        let run = |listing: &str| {
            Command::new("bash")
                .args([
                    "-c",
                    &inner.replace(
                        "cargo install --list",
                        &format!("printf '%s\\n' {}", shell_quote(listing)),
                    ),
                ])
                .status()
                .unwrap()
                .success()
        };
        assert!(run(
            "kd v0.1.0 (https://github.com/scode/kd#8e691195):\n    kd"
        ));
        assert!(!run("kd v0.1.0:\n    kd"));
        assert!(!run("kd v0.1.0 (https://github.com/someone/kd#1234):"));
        assert!(s.contains("'cargo install-update'"));
    }

    /// Updates must stay locked: the check passes only when cargo-update's
    /// config sets `enforce_lock = true` for the package, and honours
    /// `CARGO_HOME`. Run with a fixture home instead of the real one. The
    /// check needs Python 3.11+ (`tomllib`), which every supported target
    /// has; a controller whose `python3` is older skips this test with a
    /// note rather than failing it.
    #[test]
    fn locked_update_check_reads_cargo_update_config() {
        let has_tomllib = Command::new("python3")
            .args(["-c", "import tomllib"])
            .status()
            .is_ok_and(|s| s.success());
        if !has_tomllib {
            eprintln!("skipping: python3 without tomllib (needs 3.11+)");
            return;
        }
        let command = script("devbox", 1, false, false, false)
            .lines()
            .find_map(|l| l.strip_prefix("check 'kd updates locked' "))
            .unwrap()
            .to_owned();
        let inner = Command::new("bash")
            .args(["-c", &format!("printf '%s' {command}")])
            .output()
            .unwrap();
        let inner = String::from_utf8(inner.stdout).unwrap();
        let run = |config: Option<&str>| {
            let home = tempfile::tempdir().unwrap();
            let cargo = home.path().join("cargo");
            fs::create_dir_all(&cargo).unwrap();
            if let Some(config) = config {
                fs::write(cargo.join(".install_config.toml"), config).unwrap();
            }
            Command::new("bash")
                .args(["-c", &inner])
                .env("CARGO_HOME", &cargo)
                .env_remove("CARGO_INSTALL_ROOT")
                .status()
                .unwrap()
                .success()
        };
        assert!(run(Some("[kd]\nenforce_lock = true\n")));
        assert!(!run(Some("[kd]\nenforce_lock = false\n")));
        assert!(!run(Some("[other]\nenforce_lock = true\n")));
        assert!(!run(None));
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
