//! End-to-end check that `kd ubiworker ssh` really execs into `tailscale
//! ssh` with the resolved `user@worker` destination and forwarded args.
//!
//! The unit tests in `src/cmd/ubiworker/ssh.rs` cover the argv *builder*;
//! nothing there proves `run` actually uses it, or that the process it
//! replaces itself with is `tailscale` rather than plain `ssh`. Since
//! "verified Tailscale SSH instead of host-key-bypassed OpenSSH" is the
//! whole point of the command, that boundary deserves a test of its own.
//!
//! Mechanism: spawn the built `kd` binary as a child with a private `PATH`
//! containing fake `id`, `ubi`, and `tailscale` scripts. Only the child's
//! environment is touched (the test process's env is never mutated — see
//! CLAUDE.md). The fake `tailscale` records its argv to a file and exits
//! with a distinctive status; since kd `exec`s into it, that status is
//! kd's own exit status, which is itself a useful assertion that the exec
//! (not a spawn-and-wrap) happened.

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

/// The fake `ubi` has to satisfy exactly one call: `ubi vm list -N -f
/// location,name,id,ip4`, which kd uses to resolve the worker. Anything
/// else is a test failure (kd must not, say, try to create a VM from the
/// ssh path).
#[test]
fn ubiworker_ssh_execs_tailscale_ssh_with_destination_and_forwarded_args() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let argv_log = dir.path().join("tailscale-argv");

    fake_bin(&bin, "id", "printf 'alice\\n'\n");
    fake_bin(
        &bin,
        "ubi",
        "case \"$*\" in\n  'vm list -N -f location,name,id,ip4') printf 'us-east-a2 ubiworker-foo vm-id 1.2.3.4\\n';;\n  *) echo \"unexpected ubi call: $*\" >&2; exit 99;;\nesac\n",
    );
    fake_bin(
        &bin,
        "tailscale",
        &format!("printf '%s\\n' \"$@\" > '{}'\nexit 7\n", argv_log.display()),
    );

    let status = Command::new(env!("CARGO_BIN_EXE_kd"))
        .args([
            "ubiworker",
            "ssh",
            "foo",
            "-L",
            "8080:localhost:80",
            "uptime",
        ])
        // Child-only environment: a private PATH so the fakes shadow the
        // real tools, and the token kd insists on before listing workers.
        .env_clear()
        .env("PATH", &bin)
        .env("UBI_TOKEN", "test-token")
        .status()
        .unwrap();

    // The exit status is the fake tailscale's: proves kd exec'd into it.
    assert_eq!(status.code(), Some(7), "kd did not exec into tailscale");
    let argv = fs::read_to_string(&argv_log).expect("fake tailscale was not invoked");
    assert_eq!(
        argv, "ssh\nalice@ubiworker-foo\n-L\n8080:localhost:80\nuptime\n",
        "unexpected tailscale argv"
    );
}
