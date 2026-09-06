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

/// The two repos the dotfiles installer depends on, cloned whether or not
/// the manifest lists them. Deliberately compiled in; see SPEC.md.
pub const ALWAYS_CLONED: [&str; 2] = ["scode/voice", "scode/dotfiles"];

/// Home-relative paths kd places before the user-space run; the prompt
/// names them so the agent knows where to look.
pub const GITHUB_TOKEN_FILE: &str = ".kd-github-token";
pub const HERMES_ARCHIVE_FILE: &str = ".kd-hermes-backup.zip";

/// The user-space phase: everything that needs a secret, run after kd has
/// placed the credentials and the Hermes archive.
pub fn user_space_phase(user: &str, repos: &[String], rehearsal: bool, hermes: bool) -> String {
    let manifest: Vec<String> = repos.iter().map(|r| format!("  - {r}")).collect();
    let archive_note = if hermes {
        format!(", `{HERMES_ARCHIVE_FILE}` (a Hermes backup archive)")
    } else {
        String::new()
    };
    let hermes_start = if rehearsal {
        "--no-start-now --no-start-on-login (this is a REHEARSAL: the gateway and dashboard must not be enabled or started, so they never compete with the real box)"
    } else {
        "--start-now --start-on-login"
    };
    let dashboard_action = if rehearsal {
        "Write the unit but do not enable or start it."
    } else {
        "Enable and start it, and run `loginctl enable-linger` so it survives logout."
    };
    let tailscale = if rehearsal {
        "Do not install or touch Tailscale."
    } else {
        "Install Tailscale with its official install script if the `tailscale` binary is missing. Do NOT run `tailscale up`; the caller does that."
    };
    let hermes_steps = if hermes {
        format!(
            "6. Hermes: install with `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash` if `hermes` is missing. If a Hermes gateway is running, stop it (`hermes gateway stop`). Then `hermes import ~/{HERMES_ARCHIVE_FILE} --force`, then delete `~/{HERMES_ARCHIVE_FILE}` with `unlink` (it contains credentials; `rm -f` is rejected by your command policy). Do NOT run `hermes setup`; it is interactive. Then `hermes gateway install {hermes_start}`.\n7. Hermes dashboard: a user systemd unit at `~/.config/systemd/user/hermes-dashboard.service` running `hermes dashboard --host 127.0.0.1 --port 9119 --no-open` with `EnvironmentFile=-%h/.hermes/.env`, `Restart=always`, `WantedBy=default.target`, and a PATH that can find `hermes` and `npm`. {dashboard_action}"
        )
    } else {
        "6. Hermes: skip entirely on this run. Do not install or configure it.".to_owned()
    };
    format!(
        r#"You are configuring a development machine over a non-interactive session as user `{user}`, who has passwordless sudo. The system phase (packages, toolchains, Docker) is already done. Nobody will answer questions: decide yourself and keep going. Every step must be idempotent, because this whole prompt may be run again after a failure. Toolchains installed earlier may need `~/.cargo/env`, Homebrew's shellenv, or a similar file sourced; find and source what the previous phase left.

This is the USER-SPACE PHASE of a devbox bootstrap. Files kd placed for you, all under your home: `{GITHUB_TOKEN_FILE}` (a GitHub token, present only if `gh` was not already logged in){archive_note}, and the agent CLIs' credential files at their native locations (`~/.codex/auth.json`, `~/.claude/.credentials.json`, `~/.local/share/opencode/auth.json`, `~/.config/muse/auth.json`). Never print any of their contents.

Do these, in order:

1. CLIs, using the package-source preference below: gh, jj (Jujutsu), cargo-dist, git-cliff, sccache, trunk, dioxus-cli, dprint, herdr (Homebrew core), vercel, Claude Code (its own installer), OpenCode (its own installer), Muse Code (`muse`, its own installer). Homebrew is at `/home/linuxbrew/.linuxbrew`; `brew` may need its shellenv sourced first. Every CLI must end up on the login shell's PATH; verify each with `command -v` in a fresh `bash -lc`.
2. GitHub: if `gh auth status` fails, `gh auth login --with-token < ~/{GITHUB_TOKEN_FILE}`. Then delete `~/{GITHUB_TOKEN_FILE}` if it exists (use `unlink`; your command policy rejects `rm -f`), whether or not you used it. Then `gh auth setup-git`.
3. Clone `scode/voice` and then `scode/dotfiles` into `~/git/<name>` over HTTPS (skip clones that already exist). Then run `cargo run -p dotfiles -- install` from `~/git/dotfiles` twice; the second run must report zero failures. `voice` goes first because the dotfiles installer links the voice skill only when `~/git/voice` exists.
4. Clone every repo below into `~/git/<name>` over HTTPS, skipping ones that already exist, then run `jj git init --colocate` in each `~/git/*` that is not already a jj repo (including voice and dotfiles):
{manifest}
5. `ssh localhost` without a password. The contract is that plain `ssh -o BatchMode=yes localhost true` succeeds with no extra flags, so a script or tool can rely on it. Mechanism: generate `~/.ssh/id_ed25519_localhost` with no passphrase if missing, append its public key to `~/.ssh/authorized_keys` if absent, add a `Host localhost` stanza to `~/.ssh/config` that names that IdentityFile (and `AddressFamily inet` if IPv6 gets in the way), add localhost's host key to `~/.ssh/known_hosts` with ssh-keyscan, fix modes, and verify with exactly that plain command.
{hermes_steps}
9. {tailscale}

Package sources, in order of preference: apt for slow-moving system tools; Cargo for Rust tools; Homebrew for fast-moving CLIs; a tool's own upstream installer where that is the supported path; npm only when nothing else is supported.

If this machine has no systemd (no `systemctl`; a container or a cloud sandbox), still write the unit files but skip enabling or starting anything and skip `loginctl`, and list each skipped item in your final report. Do the same for any step that turns out impossible here: work around it if you can, skip it if you cannot, and report it.

Rules: do not modify the contents of any git repository beyond cloning and `jj git init`. Do not read files under the home directory that you do not need. Do not print secrets or credentials. Do not touch anything outside this machine.

Your final message must end with a section titled `## Workarounds` listing every step that failed, was skipped, or needed a workaround, with what you did. If there were none, that section is the single line `no workarounds`."#,
        manifest = manifest.join("\n"),
    )
}

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
9. Toolchains, installed as `{user}` (not root): rustup with the stable toolchain, a current Node.js LTS with npm, Bun, uv, and python3 with venv support, all under that user's home. Homebrew is the exception: install it with its official installer at the standard Linux prefix `/home/linuxbrew/.linuxbrew` (owned by `{user}`), not under the home directory, because only the standard prefix gets prebuilt bottles and everything else builds from source. Make sure `{user}`'s login shell picks up `brew shellenv`.

Package sources, in order of preference: apt for slow-moving system tools; Cargo for Rust tools; Homebrew for fast-moving CLIs; a tool's own upstream installer where that is the supported path; npm only when nothing else is supported.

If this machine has no systemd (no `systemctl`; a container or a cloud sandbox), skip service enablement, the firewall, and anything reboot-related, and list each skipped item in your final report. Do the same for any step that turns out impossible here: work around it if you can, skip it if you cannot, and report it.

Rules: do not modify the contents of any git repository. Do not read files under the home directory that you do not need. Do not print secrets or credentials. Do not touch anything outside this machine.

Your final message must end with a section titled `## Workarounds` listing every step that failed, was skipped, or needed a workaround, with what you did. If there were none, that section is the single line `no workarounds`."#
    )
}
