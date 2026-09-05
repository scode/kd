//! The agent prompts: where every drift-prone preference lives.
//!
//! These are the "Phase contents" lists from SPEC_impl.md, phrased for an
//! agent that runs unattended. When the tool inventory changes, edit the
//! prompt and SPEC_impl.md together. Exact package names and installer
//! commands are deliberately the agent's problem: encoding them here would
//! be the maintenance liability the whole design exists to avoid.
//!
//! Every prompt carries the same frame: no one will answer questions, reruns
//! must be safe, systemd may be absent, repositories are data, secrets are
//! never printed, and the final message ends with a "Workarounds" section
//! kd prints verbatim.

/// The system phase: everything that needs `sudo` and nothing that needs a
/// secret. Followed by a kd-driven reboot when the upgrade asks for one.
pub fn system_phase(hostname: &str, user: &str) -> String {
    format!(
        r#"You are configuring a freshly installed Ubuntu machine over a non-interactive session as user `{user}`, who has passwordless sudo. Nobody will answer questions: decide yourself and keep going. Every step must be idempotent, because this whole prompt may be run again after a failure.

This is the SYSTEM PHASE of a devbox bootstrap. Do these, in order:

1. `apt-get update` and `apt-get full-upgrade`, non-interactively (DEBIAN_FRONTEND=noninteractive). If the dpkg lock is held by a boot-time upgrade, wait for it rather than failing. Do NOT reboot even if the upgrade asks for one; the caller handles reboots.
2. Set the hostname to `{hostname}` and the timezone to `America/Los_Angeles`.
3. SSH hardening via a file in /etc/ssh/sshd_config.d/: `PasswordAuthentication no`, `KbdInteractiveAuthentication no`, `PermitRootLogin no`. Run `sshd -t` before reloading sshd, and never do anything that could cut the current session.
4. Firewall with ufw: default deny incoming, allow OpenSSH, `ufw allow in on tailscale0`, enable it non-interactively.
5. Unattended security upgrades enabled, automatic reboot disabled.
6. Base packages: build-essential, curl, git, jq, ripgrep, tmux, shellcheck, unzip, ca-certificates, emacs-nox.
7. Media and browser dependencies: ImageMagick, ffmpeg, Xvfb, the system libraries Playwright needs on this Ubuntu release, and the GTK and WebKitGTK runtime packages.
8. Docker Engine and the Compose plugin from Docker's own apt repository; add `{user}` to the docker group.
9. Toolchains, installed as `{user}` (not root) under that user's home: rustup with the stable toolchain, Homebrew (Linuxbrew), a current Node.js LTS with npm, Bun, uv, and python3 with venv support.

Package sources, in order of preference: apt for slow-moving system tools; Cargo for Rust tools; Homebrew for fast-moving CLIs; a tool's own upstream installer where that is the supported path; npm only when nothing else is supported.

If this machine has no systemd (no `systemctl`; a container or a cloud sandbox), skip service enablement, the firewall, and anything reboot-related, and list each skipped item in your final report. Do the same for any step that turns out impossible here: work around it if you can, skip it if you cannot, and report it.

Rules: do not modify the contents of any git repository. Do not read files under the home directory that you do not need. Do not print secrets or credentials. Do not touch anything outside this machine.

Your final message must end with a section titled `## Workarounds` listing every step that failed, was skipped, or needed a workaround, with what you did. If there were none, that section is the single line `no workarounds`."#
    )
}
