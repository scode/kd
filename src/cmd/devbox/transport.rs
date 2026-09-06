//! The one way `kd devbox` talks to a remote box: run a bash script there.
//!
//! Every remote action in `backup`, `resume`, and `bootstrap` goes through
//! [`Transport`]. It has two backings (plain `ssh`, and `tailscale ssh` for
//! tailnet peers) and three ways of handling the child's stdio: inherit it
//! so the user watches the agent work, capture it for checks, or stream
//! remote stdout into a local file for the one large download (the Hermes
//! archive). Data always travels on stdin; the script always travels in
//! argv under `bash -lc`. That split is load-bearing: `bash -lc` is what
//! puts `~/.cargo/bin` and Linuxbrew on `PATH` in a non-interactive session,
//! and stdin being free is what lets a file push be
//! `cat local | install -D -m 0600 /dev/stdin remote` with nothing buffered.
//!
//! Because the script is in argv, a `pgrep -f` pattern inside it is visible
//! in the argv of the shell running it. Scripts must use the `[h]ermes`
//! bracket form for such patterns; SPEC_impl.md explains why.
//!
//! No retry, no connection multiplexing, no host-key handling beyond the
//! options the caller passes: the known-hosts story is decided per command
//! (SPEC_impl.md "Transport") and expressed as extra `-o` options.

use anyhow::{Context, bail};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// How to reach the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    /// Plain `ssh` with the user's own config and known_hosts, plus whatever
    /// `-o` options the caller adds (the per-run known-hosts file on a real
    /// bootstrap). The destination may be `user@host` or an
    /// `ssh://user@host:port` URI, both of which OpenSSH accepts.
    Ssh { options: Vec<String> },
    /// `tailscale ssh`, which wraps the system ssh with a tailnet-verified
    /// host key. Used for `--target` hosts that are tailnet peers.
    TailscaleSsh,
}

/// A remote box plus the way to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    pub destination: String,
    pub backing: Backing,
}

/// Captured result of a remote script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Keepalive every ssh invocation carries, so a laptop lid-close or a quiet
/// hour of `apt` does not get the session dropped by a NAT in the middle.
const SERVER_ALIVE_OPTION: &str = "ServerAliveInterval=30";

impl Transport {
    /// Plain ssh to `destination` with the user's own ssh config.
    pub fn ssh(destination: impl Into<String>) -> Self {
        Transport {
            destination: destination.into(),
            backing: Backing::Ssh {
                options: Vec::new(),
            },
        }
    }

    /// `tailscale ssh` to a tailnet peer.
    pub fn tailscale(destination: impl Into<String>) -> Self {
        Transport {
            destination: destination.into(),
            backing: Backing::TailscaleSsh,
        }
    }

    /// Add an `-o key=value` option. Only meaningful for the plain-ssh
    /// backing; `tailscale ssh` manages its own host-key options and the
    /// call is a no-op there, which is deliberate rather than an error so a
    /// caller can build the transport uniformly.
    pub fn with_option(mut self, option: impl Into<String>) -> Self {
        if let Backing::Ssh { options } = &mut self.backing {
            options.push(option.into());
        }
        self
    }

    /// Pin host verification to a specific known-hosts file, strict. This is
    /// the real-run bootstrap case where the user's own `known_hosts` still
    /// holds the pre-reinstall key.
    pub fn with_known_hosts_file(self, path: &Path) -> Self {
        self.with_option(format!("UserKnownHostsFile={}", path.display()))
            .with_option("StrictHostKeyChecking=yes")
    }

    /// The argv that runs `script` remotely under `bash -lc`. Pure, so the
    /// exact command lines are unit-tested; every execution path below
    /// starts from this.
    pub fn argv(&self, script: &str) -> Vec<OsString> {
        let remote = format!("bash -lc {}", shell_quote(script));
        let mut argv: Vec<OsString> = Vec::new();
        match &self.backing {
            Backing::Ssh { options } => {
                argv.push("ssh".into());
                argv.push("-o".into());
                argv.push(SERVER_ALIVE_OPTION.into());
                for option in options {
                    argv.push("-o".into());
                    argv.push(option.into());
                }
                argv.push("--".into());
                argv.push(self.destination.clone().into());
                argv.push(remote.into());
            }
            Backing::TailscaleSsh => {
                argv.push("tailscale".into());
                argv.push("ssh".into());
                argv.push(self.destination.clone().into());
                argv.push(remote.into());
            }
        }
        argv
    }

    fn command(&self, script: &str) -> Command {
        let argv = self.argv(script);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd
    }

    /// Run `script` with the child's stdio inherited, so the user sees it
    /// live. This is the mode for agent phases, `tailscale up`, and anything
    /// else a person watches. Fails on nonzero exit.
    pub fn run(&self, script: &str) -> anyhow::Result<()> {
        let status = self
            .command(script)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("failed to start remote command on {}", self.destination))?;
        if !status.success() {
            bail!(
                "remote command on {} exited with {status}",
                self.destination
            );
        }
        Ok(())
    }

    /// Like [`Transport::run`] but with `input` fed to the remote script's
    /// stdin. This is how the Codex prompt reaches `codex exec`, and how a
    /// file push works (the script being `install -D -m 0600 /dev/stdin
    /// <path>`). Output is inherited.
    pub fn run_with_stdin(&self, script: &str, input: &[u8]) -> anyhow::Result<()> {
        let mut child = self
            .command(script)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start remote command on {}", self.destination))?;
        // Write then drop the handle so the remote side sees EOF; a script
        // that reads to EOF (`install` does) would otherwise hang.
        {
            let mut stdin = child.stdin.take().context("child stdin missing")?;
            stdin
                .write_all(input)
                .context("failed to write to remote stdin")?;
        }
        let status = child.wait().context("failed to wait for remote command")?;
        if !status.success() {
            bail!(
                "remote command on {} exited with {status}",
                self.destination
            );
        }
        Ok(())
    }

    /// Run `script` and capture its output. Does not fail on nonzero exit,
    /// because most callers are checks whose "no" answer is the interesting
    /// one; inspect [`Captured::success`].
    pub fn capture(&self, script: &str) -> anyhow::Result<Captured> {
        let output = self
            .command(script)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to start remote command on {}", self.destination))?;
        Ok(Captured {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Run `script` and stream its stdout into `local`, created with mode
    /// 0600. Stderr is inherited. This exists only for the Hermes archive
    /// pull, which can be hundreds of megabytes and must not be buffered.
    pub fn pull_stdout(&self, script: &str, local: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(local)
            .with_context(|| format!("failed to create {}", local.display()))?;
        let status = self
            .command(script)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .status()
            .with_context(|| format!("failed to start remote command on {}", self.destination))?;
        if !status.success() {
            bail!(
                "remote command on {} exited with {status}",
                self.destination
            );
        }
        Ok(())
    }

    /// Push `contents` to `$HOME/<home_relative>` on the box as a mode-0600
    /// file, creating parent directories. Streams; nothing is logged.
    ///
    /// The path is relative to the remote user's home on purpose, resolved
    /// by the remote shell: the ssh username is not always the unix user
    /// (a tunnelled sandbox logs in as one name and lands in another's
    /// home), so kd never builds an absolute home path itself.
    pub fn push_secret(&self, contents: &[u8], home_relative: &str) -> anyhow::Result<()> {
        self.run_with_stdin(&push_secret_script(home_relative), contents)
            .with_context(|| format!("failed to push ~/{home_relative} to {}", self.destination))
    }
}

/// The remote side of [`Transport::push_secret`]: `$HOME` expands in the
/// remote shell, the relative part is quoted literally.
fn push_secret_script(home_relative: &str) -> String {
    format!(
        "install -D -m 0600 /dev/stdin \"$HOME\"/{}",
        shell_quote(home_relative)
    )
}

/// Single-quote `s` for a POSIX shell: the only quoting form that makes
/// every byte literal, at the cost of the `'\''` dance for embedded quotes.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(argv: &[OsString]) -> Vec<&str> {
        argv.iter().map(|s| s.to_str().unwrap()).collect()
    }

    /// The plain-ssh command line is the contract with the user's ssh
    /// config: keepalive on, caller options in order, `--` before the
    /// destination so a destination can never be parsed as a flag, and the
    /// script as one argv element under `bash -lc`.
    #[test]
    fn plain_ssh_argv_shape() {
        let t = Transport::ssh("scode@1.2.3.4").with_known_hosts_file(Path::new("/tmp/kh"));
        assert_eq!(
            strs(&t.argv("echo hi")),
            [
                "ssh",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "UserKnownHostsFile=/tmp/kh",
                "-o",
                "StrictHostKeyChecking=yes",
                "--",
                "scode@1.2.3.4",
                "bash -lc 'echo hi'",
            ]
        );
    }

    /// `tailscale ssh` gets no `-o` options at all: it manages host keys
    /// itself, and the known-hosts option must not leak into that path.
    #[test]
    fn tailscale_argv_ignores_options() {
        let t = Transport::tailscale("scode@worker").with_known_hosts_file(Path::new("/tmp/kh"));
        assert_eq!(
            strs(&t.argv("true")),
            ["tailscale", "ssh", "scode@worker", "bash -lc 'true'"]
        );
    }

    /// A script with single quotes must survive the trip through `bash -lc`
    /// as one argument: this is what protects every pgrep pattern and
    /// every quoted path the callers build.
    #[test]
    fn shell_quote_handles_embedded_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        let t = Transport::ssh("u@h");
        let argv = t.argv("pgrep -f '[h]ermes'");
        assert_eq!(
            strs(&argv).last().copied().unwrap(),
            "bash -lc 'pgrep -f '\\''[h]ermes'\\'''"
        );
    }

    /// The URI destination form is passed through untouched; it is how a
    /// tunnelled sandbox on a local port is addressed.
    #[test]
    fn uri_destination_passes_through() {
        let t = Transport::ssh("ssh://tl-user@localhost:2299");
        assert!(strs(&t.argv("true")).contains(&"ssh://tl-user@localhost:2299"));
    }

    /// A push must create parents (`-D`, a fresh home has none of the CLI
    /// config directories), set the mode atomically, and let `$HOME` expand
    /// remotely while the relative part stays literal.
    #[test]
    fn push_secret_script_creates_parents_with_mode() {
        assert_eq!(
            push_secret_script(".codex/auth.json"),
            "install -D -m 0600 /dev/stdin \"$HOME\"/'.codex/auth.json'"
        );
    }
}
