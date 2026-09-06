# SPEC_impl.md

This file records implementation choices that are deliberate and easy to "fix" into something worse. `SPEC.md` is the
user-facing contract; this is the how and the why behind it. Both are binding on agents working in this repo.

## kd devbox

The terms controller, devbox, and target are defined at the top of the `kd devbox` section in `SPEC.md` and mean the
same thing here. `--target` always names the destination. "Rehearsal" means `--restore PROFILE --rehearsal`; "real run"
means a bootstrap without `--rehearsal`, whether from scratch or restoring. The intended user comes from the explicit
target user or `[bootstrap].user`, never from the source profile. `--enroll-tailscale` is independent.

NOTE: The governing constraint is size. The whole of `kd` is a few thousand lines and `kd devbox` should not double
that. Anything that hardcodes an installer command, a package name, or a distro detail into Rust is a maintenance
liability that rots the next time Ubuntu or an upstream installer changes. The Codex agent absorbs that drift; Rust owns
only what an agent cannot or must not do.

### What Rust owns and what the agent owns

Rust owns: profile parsing, SSH transport, placing secrets, phase ordering, the reboot between phases, the live-devbox
guard, prompts, and the probe script. The agent owns everything else: package installs, hardening, toolchains, cloning
and `jj git init --colocate`, the dotfiles installer, `ssh localhost` setup, Hermes install and import, the dashboard
unit, and diagnosing whatever breaks along the way. The line is "does this touch a secret, cross a reboot, or decide
whether it's safe to write to this machine?". If not, it goes in the prompt.

Two exceptions, both accepted because there is no agent yet or because they are a few lines of shell in a script Rust
already owns. First, Rust installs Codex during seeding: a generated script run on the target fetches
`https://github.com/openai/codex/releases/latest/download/<name>-$(uname -m)-unknown-linux-musl.tar.gz` for the two
names `codex` and `codex-code-mode-host`, which GitHub redirects to the newest release without any API call or JSON
parsing, and places the one binary inside each at `~/.local/bin/<name>`. Both are needed: since Codex 0.153 the command
runner is the separate `codex-code-mode-host` executable that `codex` looks for beside itself, and the feature is on by
default and fails closed, so a lone `codex` can run no commands at all. `curl` and `tar` are assumed present on a
minimal Ubuntu. That is the only release layout Rust knows about; when it drifts, update it here and in the code
together. Second, the probe script hardcodes one real request per agent CLI (see "Probe"). A rotted probe line shows up
as a failed probe item, never as a failed run.

The agent may not: modify repository contents (repos are data, not things to fix), touch controller state, read secrets
it doesn't need, or decide the target is safe. It reports failures and workarounds in its final message; it does not
declare success. The probe script is the only success signal.

### Agent invocation

Codex runs on the target as the user, not on the controller, so a yolo agent's filesystem reach is the machine it is
supposed to configure. Invocation, with the prompt on stdin:

```text
codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
  -m <model> -c model_reasoning_effort=<effort> -o ~/.kd-agent-last-message.md
```

`--skip-git-repo-check` is required because a fresh `$HOME` is not a git repo. Model and effort are two constants
(initially `gpt-6-astra` and `medium`) so a rename is a one-line change; a fallback model is a config change, never a
silent retry.

There are exactly two agent runs per bootstrap: the system phase followed by a kd-driven reboot when
`/var/run/reboot-required` exists, and the user-space phase. The reboot sits between them because the agent cannot
survive it. kd places the remaining secrets between the two runs, so the system phase sees no secret beyond Codex's own
auth file. Both prompts run as the user and use `sudo` for system changes. Agent output streams to the controller's
terminal as it happens.

Each prompt states the goal, where the agent may write (its own home, and system paths only through `sudo` for the items
listed), the package-source preference (apt for slow-moving system tools; Homebrew for any CLI with a Linux bottle, Rust
tools included; `cargo install` for Rust tools without a bottle or marked cargo; an upstream installer where that is the
supported path; npm only when nothing else is supported), that the agent must not modify repository contents, and that
its final message must end with a section listing every step that failed and how it was worked around, or "no
workarounds" if none. After each run kd reads `~/.kd-agent-last-message.md` back and keeps it to print whole after the
probe; there is no section parsing. Nonzero exit is phase failure: kd prints that file if it exists, then the error, and
stops.

### Phase contents

These lists are what the two prompts ask for. They are preferences, not identity, so they live in code; when the
inventory changes, edit the prompt and this section together. Exact package names and install commands are the agent's
problem, which is the point.

**System phase**, as the user with `sudo`:

- `apt update` and `apt dist-upgrade`, waiting on the dpkg lock rather than failing if a boot-time upgrade is still
  running (ubiworkers do this).
- Hostname from `--hostname`, falling back to the restore profile; timezone `America/Los_Angeles`.
- SSH: key-only, `PermitRootLogin no`, `PasswordAuthentication no`. Validate with `sshd -t` before restarting.
- UFW: default deny inbound, allow OpenSSH, `ufw allow in on tailscale0`, enable.
- Unattended security upgrades with automatic reboot disabled.
- Base packages: build tools, curl, git, jq, ripgrep, tmux, shellcheck, unzip, ca-certificates, `emacs-nox`.
- Media and browser dependencies: ImageMagick, ffmpeg, Xvfb, Playwright's system dependencies, GTK/WebKit runtime
  packages.
- Docker Engine and the Compose plugin, with the user in the `docker` group.
- Toolchains: rustup with stable, Node and npm, Bun, uv, Python 3, under the user's home; Homebrew at its standard Linux
  prefix `/home/linuxbrew/.linuxbrew`, because only that prefix gets prebuilt bottles (the first rehearsal's agent put
  it under `~` and everything brew touched built from source).

**User-space phase**, as the user, after kd has placed the secrets:

- CLIs: `gh`, `jj`, `cargo-dist`, `git-cliff`, `sccache`, `trunk`, `dioxus`, `dprint`, `herdr`, `vercel`, Claude Code
  (its own installer), OpenCode, Muse. The Rust tools come from Homebrew bottles where a formula exists (`cargo-dist`,
  `git-cliff`, `sccache`, `trunk`, `dprint`), not `cargo install`: the first ubiworker rehearsals spent about 20 of the
  user-space phase's 22 minutes compiling them from source. `dioxus` has no Homebrew formula (checked 2026-09-05; an
  agent guessing `dioxus-cli` fails too) and is `cargo install dioxus-cli`. Cargo stays the fallback for any Rust tool
  without a bottle, or one deliberately moved back to cargo later.
- `gh auth login --with-token < ~/.kd-github-token`, then delete the token file with `unlink`, then `gh auth setup-git`.
  Skip the login if `gh auth status` already passes. `unlink` rather than `rm -f` because Codex's built-in command
  policy rejects any command containing `rm -f`, even under full-access mode.
- Clone `scode/voice` and `scode/dotfiles` into `~/git/<name>` (always, whether or not the manifest lists them), then
  run `cargo run -p dotfiles -- install` from `~/git/dotfiles` twice; the second run must report zero failures. `voice`
  goes first because the dotfiles installer links the voice skill only when `~/git/voice` exists.
- Clone every repo in the shared bootstrap manifest into `~/git/<name>` over HTTPS, then `jj git init --colocate` in
  each (including `voice` and `dotfiles`). Skip clones that already exist.
- `ssh localhost`: the contract is that plain `ssh -o BatchMode=yes localhost true` succeeds. Mechanism: generate
  `~/.ssh/id_ed25519_localhost` with no passphrase, append its public key to `~/.ssh/authorized_keys`, a
  `Host localhost` stanza in `~/.ssh/config` naming that IdentityFile, `ssh-keyscan` into `~/.ssh/known_hosts`, fix
  modes. The first rehearsal's agent verified only with `-i` and the plain command failed, hence the stanza.
- Hermes, only with `--restore`: official installer; stop the gateway first if one is running (a real-run rerun can find
  one); then `hermes import ~/.kd-hermes-backup.zip --force`, then `rm ~/.kd-hermes-backup.zip` since it contains
  `.hermes/.env`; then `hermes gateway install`. Pass the start flags explicitly, because the headless default is to
  start and enable, which a rehearsal must not do: on a real run `--start-now --start-on-login`, on a rehearsal
  `--no-start-now --no-start-on-login`.
- Dashboard, only with `--restore`: a user systemd unit at `~/.config/systemd/user/hermes-dashboard.service` that runs
  `hermes dashboard --host 127.0.0.1 --port 9119 --no-open`, loads `~/.hermes/.env`, restarts on failure, and starts at
  boot via `loginctl enable-linger`. That is the contract; the unit body is the agent's, because `hermes dashboard` may
  try to build the web UI on start and needs a PATH a bare unit does not have, and the right incantation is a
  Hermes-version detail. The unit on the current devbox is a working reference. On a real run, enable and start it; on a
  rehearsal, only write it.
- Tailscale, only with `--enroll-tailscale`: install with the official script if `tailscale` is missing. Enrollment
  itself stays with kd (see the sequence), because it needs the login URL relayed to the terminal.

### Transport

One function runs a script on a destination with optional stdin. Output is inherited (streamed to the terminal) by
default; a capture variant returns it for checks such as `gh auth status`, `tailscale status`, the guard, and the
preflight script; a third variant streams remote stdout into a local file and exists only for the archive pull, so the
zip is never held in memory. The script itself goes in argv under `bash -lc`; stdin is reserved for data (file pushes,
the Codex prompt). That means a `pgrep -f` pattern inside a script is visible in the argv of the shell running it, so
every such pattern uses the `[h]ermes` bracket form, which matches the process but not its own literal text.

`backup`, `suspend` and `resume` talk to the source with plain `ssh` and the user's own config and `known_hosts`. A
non-rehearsal restore over plain SSH may reuse an address with a new host key, so it uses
`-o UserKnownHostsFile=<per-run temp file> -o StrictHostKeyChecking=yes`. That file is created in the controller's temp
directory for the run, filled from the `ssh-keyscan` line the user confirmed, and discarded afterwards. The user's
`~/.ssh/known_hosts` is never touched, which is why `SPEC.md` has the `ssh-keygen -R` reminder. Every ssh invocation
passes `-o ServerAliveInterval=30`.

For `--target` the transport is `tailscale ssh` when the target host appears as a peer in `tailscale status --json`
(matched against each peer's `HostName` and `DNSName`), for the same reasons `kd ubiworker ssh` uses it (see `SPEC.md`):
the tailnet authenticates the host, so there is nothing to prompt for. Otherwise, or when `--plain-ssh` is given, it is
plain `ssh` with the user's own config and `known_hosts`, which is how a non-tailnet box such as a cloud sandbox reached
through a local tunnel becomes a target. A missing `tailscale` binary counts as "not a peer". Selection does not imply
rehearsal or enrollment. `ssh -G` resolves the hostname and port before a restore's fingerprint scan; `ssh-keyscan`
still needs direct access to that endpoint (it does not use SSH proxies).

Remote commands run under `bash -lc` so tools under `~/.cargo/bin` and Linuxbrew are on `PATH` in a non-interactive
session. Files are pushed as `umask 077 && mkdir -p <dir> && cat > <path> && chmod 0600 <path>` fed on stdin over the
same transport, streamed rather than buffered since the Hermes archive can be large. `cat` works with both OpenSSH's
pipe stdin and Tailscale SSH's socket stdin; reopening `/dev/stdin` with `install` fails on the latter. No scp. A hash
check is only done on the archive pull in `backup`; ssh's transport is the integrity check for pushes.

### Secrets

Bootstrap preflight resolves the four agent auth sources without printing them and fails before connecting if one is
missing. Backup and service control need no agent credentials:

- Codex: `~/.codex/auth.json`, file only. If it is missing the error says to set `cli_auth_credentials_store = "file"`
  in `~/.codex/config.toml` and log in again; Codex's keyring mode is not supported because it has never been observed
  on the controller and its Keychain item name is only documented, not verified.
- Claude: the Keychain item `Claude Code-credentials` first
  (`security find-generic-password -s
  "Claude Code-credentials" -w`; observed on the controller), and only if that
  lookup fails, `~/.claude/.credentials.json`. The order matters: Claude Code on macOS uses the Keychain whenever it is
  writable and only writes the file as a fallback, so a stale file can sit next to a live Keychain entry. The Keychain
  payload is the same JSON the Linux file holds. There is no setting that forces file storage on macOS.
- OpenCode: `~/.local/share/opencode/auth.json`.
- Muse: `~/.config/muse/auth.json`, but not verbatim on macOS. Muse 1.0 writes schema 1 on Linux (secrets inline) and
  schema 2 on macOS (`"storage": "keychain"`, secrets in the Keychain item `ai.meta.dev.credentials`, account `meta`, as
  a small JSON with `api_key` and `access_token`), and a Linux Muse rejects schema 2 outright. kd merges the two back
  into a schema-1 file for the target; a schema-1 file on the controller passes through unchanged.

Codex's file is placed during seeding because the agent needs it. The other three and, on restores, the Hermes archive
go to the target as `0600` files between the two agent runs. The GitHub token goes with them only when `gh auth status`
on the target fails, on real runs and rehearsals alike, and the agent consumes it with
`gh auth login --with-token < file && rm file`, so a rerun that skips the login never leaves a token file behind. The
token touches disk on the target briefly; it never appears in argv or logs anywhere.

### Paths and names

On the target, all under the user's home:

- `~/.codex/auth.json`, `~/.claude/.credentials.json`, `~/.local/share/opencode/auth.json`, `~/.config/muse/auth.json`:
  the four auth files, at their CLIs' native locations.
- `~/.kd-github-token`: the GitHub token, deleted by the agent once `gh` has it.
- `~/.kd-hermes-backup.zip`: the archive to import.
- `~/.kd-agent-last-message.md`: the agent's final message, one per phase, overwritten.
- `~/.config/systemd/user/hermes-dashboard.service`: the dashboard unit restore writes. `suspend` and `resume` use
  `hermes gateway stop` / `hermes gateway start` for the gateway and `hermes dashboard --stop` to stop the dashboard,
  because the source may have a hand-written unit from before kd existed. Suspend also stops the known dashboard unit
  when present so Restart=always does not undo the CLI stop. It then requires pgrep to report no gateway/dashboard
  processes in a separate SSH command after the stop shell exits. Combining those scripts makes pgrep match the stop
  command's own argv. A process-check error or surviving process fails suspension. It does not disable boot-time
  startup. Starting the dashboard uses `systemctl --user start hermes-dashboard` when that unit exists, else
  `setsid -f hermes dashboard --host 127.0.0.1 --port 9119 --no-open`, because `hermes dashboard` runs in the foreground
  and would otherwise die with the ssh session.
- The dashboard listens on `127.0.0.1:9119`; the probe curls `/api/status` there (endpoint observed on the current
  devbox).

On the devbox during `backup`: `hermes backup -o ~/hermes-backup-kd.zip`, deleted after a verified pull.

On the controller:

- The archive lands in the profile's `backup_dir` as `hermes-<hostname>-<YYYYMMDDTHHMMSSZ>.zip`. "Newest archive" in
  `bootstrap` means the newest by mtime among `hermes-<hostname>-*.zip` in that directory, so a shared directory never
  picks another box's archive.
- The per-run known-hosts file lives in the system temp directory and is removed when the run ends.

### Bootstrap sequence

1. Controller preflight, cheap and with no connections: shared settings parse; target user and hostname validate;
   `public_key` is readable; the four auth sources resolve. Only a restore resolves a named profile and selects its
   newest `hermes-<source-hostname>-*.zip`. A hostname override never changes archive selection. Scratch bootstrap does
   not read a backup directory or require any profile.
2. Non-rehearsal restore over plain SSH only: resolve `ssh -G <destination>`, then
   `ssh-keyscan -t ed25519 -p <port> <host>`, show the key's fingerprint in both SHA256 and MD5 forms (`ssh-keygen -lf`
   and `ssh-keygen -E md5 -lf`, consoles vary), ask the user to confirm it matches the console. On yes, write the
   keyscan line to the per-run known-hosts file; on no, exit.
3. Connect as the intended user; on a real run, if that fails, root at the same destination for seeding (a rehearsal has
   no root and its user must already exist). Guard: `pgrep -f '[h]ermes.*gateway'` over that first connection (any user;
   `hermes` itself may not be on PATH yet). If it matches, a non-rehearsal restore asks before continuing; scratch
   bootstrap and rehearsals refuse outright. Running, not enabled, is the test, and directories are never checked,
   because a rerun after a partial bootstrap must work: on a real run the previous attempt may already have restored
   Hermes, and on a rehearsal Hermes is installed without being started.
4. Seed, idempotently, since a rerun finds everything already in place: create the user with `/bin/bash` if missing,
   install the public key if missing, passwordless sudo, prove a second connection as that user. From here on every
   connection is as the user; the root connection, if there was one, is not used again. On a ubiworker the user exists
   and there is no root, so seeding reduces to `sudo -n true` and the public key. Then, as the user, install Codex (the
   one Rust-owned installer, see above) and place its auth file.
5. Decide whether a GitHub token is needed: `gh auth status` as the user fails (no `gh` counts as failing). If so, a
   real run prompts for it (hidden entry on a terminal; one plain line from stdin when there is none, so a real run can
   be scripted) and a rehearsal takes the controller's `gh auth token`; otherwise no token is fetched or placed. With
   `--enroll-tailscale` only: Tailscale device prompt (free the hostname if replacing an old node, wait for Enter)
   unless `tailscale status --json` on the target reports `BackendState` of `Running` (no `tailscale` binary counts as
   not running).
6. Agent: system phase. Then, if `/var/run/reboot-required` exists and `systemctl` is available, `sudo systemctl reboot`
   over ssh, ignoring that command's own exit status since the connection drops, and poll SSH every 10 seconds for up to
   10 minutes; give up with an error after that. The per-run known-hosts file is reused because the host key survives a
   reboot.
7. Place secrets.
8. Agent: user-space phase.
9. Tailscale (only with `--enroll-tailscale`): the agent installed it in the user-space phase; kd runs
   `sudo tailscale up --timeout 10m` with output streamed so the login URL reaches the terminal. `tailscale up` blocks
   until the browser login completes or the timeout expires, which is the whole wait; a nonzero exit is an error. The
   hostname defaults to the OS hostname; no `--ssh`, no tags, not ephemeral. Skipped when `tailscale status --json`
   already reports `Running`: on an enrolled node `tailscale up` refuses unless every non-default flag from the original
   enrollment is repeated, which would fail every rerun.
10. Probe script over SSH, output printed as is, then both agents' final messages. Exit 0.

### Probe

One shell script, rendered per run because it carries expected values, run as the user, printing one line per check in
the form `<name>: ok` or `<name>: FAIL (exit N)`. Pass is exit status 0 unless stated. Checks: `hostname` equals the
resolved target hostname; `timedatectl show -p Timezone --value` equals `America/Los_Angeles`; `gh auth status`; the
count of `~/git/*` directories equals the size of the manifest deduplicated with `scode/voice` and `scode/dotfiles`;
`ssh -o BatchMode=yes localhost true`; `docker ps`; and, on restores, a gateway process check (absent on rehearsal,
present otherwise) plus `curl -fsS 127.0.0.1:9119/api/status` outside rehearsals. `tailscale status` is checked only
with `--enroll-tailscale`. One real request per agent CLI: `codex exec --skip-git-repo-check "reply ok"`,
`claude -p ok`, `opencode run ok`, `muse exec ok`. Every check is reported; none is fatal.

### Backup sequence

Controller preflight (profile parses, `backup_dir` created if missing; no agent credentials); read-only preflight script
on the devbox: `pgrep -af '[c]odex|[c]laude|[o]pencode|[m]use|[h]ermes'`, `tmux ls`, and the per-repo dirty and unpushed
check below; prompt unless `--yes`; `hermes backup -o ~/hermes-backup-kd.zip`; completeness check: exit status 0,
nonzero file size, and no line containing `incomplete` in stdout or stderr. The observed live backup was a ZIP-valid
archive that Hermes called incomplete because of sockets; that remains a failure, with the operator choosing whether to
suspend and retry. Then `sha256sum` on the source, pull, compare the local hash, mode `0600`, delete the remote copy,
and print the first line of `hermes --version`. Backup never calls service start or stop, on any path. Only `suspend`
and `resume` own those decisions.

The preflight script defines "dirty" as `git status --porcelain` non-empty and "unpushed" as
`git log --branches --not --remotes --oneline` non-empty, run in each `~/git/*` directory that has a `.git`. Both are
informational; colocated jj repos are just git repos for this purpose.

### Testing

Unit tests cover profile parsing, bootstrap plan selection, CLI constraints, transport command lines, service-control
scripts and conditional prompt/probe content. Full prompt snapshots are unnecessary; tests focus on mode boundaries.
Everything else is tested end to end against a ubiworker the user creates and hands over:

```text
kd devbox backup --profile <name> --yes
kd devbox bootstrap --target scode@<worker> --restore <name> --rehearsal
kd devbox bootstrap --target scode@<worker> --hostname <worker>
```

The restore rehearsal uses the controller's GitHub token; scratch runs prompt if the target is not authenticated.
Rerunning bootstrap against the same worker is the fast loop for iterating on a later phase; a fresh worker is the full
check. Neither touches the real devbox beyond reading it and writing one archive that is removed again.

### Known gaps

- The ubiworker image (Ubuntu 26.04) may be newer than the newest LTS image the real box's provider offers, so a
  rehearsal is not always the same distro. The agent absorbs the drift.
- A rehearsal changes the worker's OS hostname in the system phase. That does not rename the tailnet node mid-run only
  because `kd ubiworker create` pins `--hostname` at enrollment; if that ever changes, the rehearsal transport breaks.
- An hours-long SSH session from a laptop is the weakest link. `ServerAliveInterval` and running under `caffeinate -i`
  are the mitigation; idempotent reruns cover the rest. Running the agent under tmux with a done-file is not planned
  unless that proves insufficient.
- Copying OAuth state to a second machine is known to work for Claude and Codex; OpenCode and Muse are unverified.
- A rehearsal arrives over Tailscale SSH, which may not create a logind session. `hermes gateway install`,
  `systemctl --user`, and `loginctl enable-linger` all want `XDG_RUNTIME_DIR` and the user bus; if the first rehearsal
  shows them failing, the fix is `loginctl enable-linger` first and `XDG_RUNTIME_DIR=/run/user/$(id -u)` in the prompt's
  environment.
- `hermes dashboard` may try to build the web UI on every start when `npm` is on PATH and complain when it is not;
  whether the unit needs `--skip-build` or a PATH line is a first-rehearsal question, which is why the unit body is left
  to the agent.
- `tailscale up` flags and `hermes` non-interactive behavior after import were checked against Hermes 0.21 and current
  docs, not against a live run. The first rehearsal is where they get verified.
