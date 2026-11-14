//! End-to-end check that `kd boatworker ssh` really execs into `boat ssh`
//! with the resolved sandbox id and the forwarded arguments.
//!
//! The unit tests in `src/cmd/boatworker/ssh.rs` cover resolution and the
//! argv builder; nothing there proves `run` wires them together, addresses
//! the sandbox by id rather than by name, or replaces itself with `boat`
//! rather than spawning it. Those are the command's whole contract.
//!
//! Mechanism, as in `ubiworker_ssh_exec.rs`: run the built `kd` binary as a
//! child whose private `PATH` holds a fake `boat` script. Only the child's
//! environment is touched; the test process's environment never is (see
//! CLAUDE.md). The fake answers the one `boat list --all --json` call kd
//! makes with a canned listing, records the argv of the `boat ssh` it is
//! exec'd as, and exits with a distinctive status, which becomes kd's own
//! exit status only if kd exec'd into it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Write an executable shell script at `dir/name`.
fn fake_bin(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The listing holds the target plus an unowned sandbox, so resolution has
/// something to filter out. Any boat call other than the listing and the
/// final `ssh` is a test failure: the ssh path must not, say, create or
/// delete anything. The Tailscale credentials are set to prove kd strips
/// them before the exec (the fake records whether it still sees them).
#[test]
fn boatworker_ssh_execs_boat_ssh_with_id_and_forwarded_args() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let argv_log = dir.path().join("boat-argv");
    let env_log = dir.path().join("boat-env");

    let listing = r#"{"pageInfo":{"hasMore":false},"sandboxes":[{"id":"bx_other","name":"other","state":"idle","type":"small"},{"id":"bx_target","name":"boatworker-foo","state":"idle","type":"large"}]}"#;
    fake_bin(
        &bin,
        "boat",
        &format!(
            "case \"$1\" in\n  list) [ \"$*\" = 'list --all --json' ] || exit 98; printf '%s\\n' '{listing}';;\n  ssh) printf '%s\\n' \"$@\" > '{argv}'; printf '%s %s\\n' \"${{TS_API_CLIENT_ID:-unset}}\" \"${{TS_API_CLIENT_SECRET:-unset}}\" > '{env}'; exit 7;;\n  *) echo \"unexpected boat call: $*\" >&2; exit 99;;\nesac\n",
            argv = argv_log.display(),
            env = env_log.display(),
        ),
    );

    let status = Command::new(env!("CARGO_BIN_EXE_kd"))
        .args(["boatworker", "ssh", "foo", "-t", "printf", "%s\\n", "a b"])
        .env_clear()
        .env("PATH", &bin)
        .env("TS_API_CLIENT_ID", "test-id")
        .env("TS_API_CLIENT_SECRET", "test-secret")
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(7), "kd did not exec into boat");
    let argv = fs::read_to_string(&argv_log).expect("fake boat ssh was not invoked");
    assert_eq!(
        argv, "ssh\nbx_target\n-t\nprintf\n%s\\n\na b\n",
        "unexpected boat argv"
    );
    assert_eq!(fs::read_to_string(&env_log).unwrap(), "unset unset\n");
}
