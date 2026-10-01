# kd

Small personal toolbox. The name means nothing; it is just designed to be easy to type and not clash with other tools.

## Install

Works on macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/scode/kd/main/install.sh | bash
```

If cargo isn't usable (missing, or a toolchain-less rustup shim), it first installs rust with the official
[rustup](https://rustup.rs) one-liner, accepting rustup's defaults; if rust is older than 1.85 and rustup is present, it
installs current stable just for the build. Then it runs
`cargo install --locked --force --git https://github.com/scode/kd kd`, which records the same source
`kd cargo scode install kd` would, so `kd cargo scode update` can keep kd current from its default branch later.
Rerunning the one-liner rebuilds kd from the current default branch. It needs a C toolchain (Xcode Command Line Tools on
macOS; `cc`, e.g. from `build-essential`, on Linux) and says so if one is missing. Read [`install.sh`](install.sh)
before piping it if you (sensibly) don't run shell scripts off the internet blind.

Earlier versions of this installer cloned kd into `~/git/kd` and installed from that checkout. Rerunning the one-liner
replaces such an install; the old checkout is no longer used and can be deleted.

### Uninstall

`kd cargo scode uninstall kd`, or `cargo uninstall kd` for a kd installed by the older checkout-based installer (which
`kd cargo scode uninstall` does not recognise as a github.com/scode install). Rust itself is left alone.

## Commands TLDR

```sh
# Resize an image in place until it fits under YouTube's 2 MB thumbnail limit.
kd yt thumb resize image.png

# Apply my preferred merge settings to the repo in the current directory,
# or to an explicit owner/repo.
kd gh repo apply-preferred-settings
kd gh repo apply-preferred-settings scode/foo

# See what would change without touching anything.
kd gh repo apply-preferred-settings --dry-run
kd gh repo apply-preferred-settings --dry-run scode/foo

# Re-apply settings even if the repo already looks correct.
kd gh repo apply-preferred-settings --force scode/foo

# Apply the same settings to every non-fork, non-archived repo I own.
kd gh repo apply-preferred-settings --all
kd gh repo apply-preferred-settings --all --dry-run
kd gh repo apply-preferred-settings --all --yes

# Create or repair the main-protect ruleset, then interactively choose
# which CI checks should block merges.
kd gh repo main-protect
kd gh repo main-protect scode/foo

# List my PRs outside my own account's repos that are open or closed in
# the last week, most recent activity by others first.
kd gh pr list
# Same, including PRs to repos I own.
kd gh pr list --include-mine

# Create a disposable Ubicloud worker VM and enroll it into the tailnet.
# Defaults to a timestamped name; a custom name gets the ubiworker- prefix
# added automatically if it's missing.
kd ubiworker create
kd ubiworker create myname

# Request apt packages, installed asynchronously (with an apt-get
# dist-upgrade) after boot; see SPEC.md for what "asynchronously" means.
kd ubiworker create --pkg build-essential --pkg git

# List existing ubiworker VMs.
kd ubiworker list

# Destroy ubiworker VM(s) (no confirmation prompt; see SPEC.md).
# With no name, targets the sole existing ubiworker.
kd ubiworker destroy
kd ubiworker destroy myname
kd ubiworker destroy myname otherworker
kd ubiworker destroy --all

# Connect to a ubiworker over Tailscale SSH (kd execs into `tailscale ssh`,
# which verifies the host key via the tailnet, so your ~/.ssh/known_hosts is
# never touched; see SPEC.md). With no name, targets the sole existing
# ubiworker, like destroy.
kd ubiworker ssh
kd ubiworker ssh myname

# Everything after the worker name forwards straight to ssh: a remote
# command, or ssh's own flags (e.g. a port forward).
kd ubiworker ssh myname uptime
kd ubiworker ssh myname -L 8080:localhost:80

# The first argument is only treated as a worker name if it doesn't start
# with `-`, so an ssh flag kd doesn't itself recognize can be given with no
# name at all:
kd ubiworker ssh -L 8080:localhost:80

# But a leading flag that collides with one of kd's own (-v/-q/-h) is
# claimed by kd first, not forwarded -- use `--` to force it through to ssh
# instead. Under the same leading-argument rule, `--` followed by a bare
# word is a worker name, not a remote command run on the sole worker: `kd
# ubiworker ssh -- uptime` targets a worker literally named `uptime`
# (normalized to `ubiworker-uptime`), it does not run `uptime` anywhere.
kd ubiworker ssh -- -v
kd ubiworker ssh -- -L 8080:localhost:80

# Update kd and every other tool installed from a github.com/scode repository
# to the latest commit of its branch.
kd cargo scode update
kd cargo scode update --dry-run

# Install or uninstall a tool from github.com/scode/NAME (tracks the default branch, --locked).
kd cargo scode install kd
kd cargo scode uninstall kd

# Keep CLIProxyAPI's Claude accounts ordered so the quota that resets soonest
# is used first, logging usage every 15 minutes: install it as a systemd user
# service (survives logout and reboot), or run it in the foreground.
kd cli-proxy-api monitor enable
kd cli-proxy-api monitor disable
kd cli-proxy-api monitor run

# Drain one Claude account first until its weekly reset (for example before
# pressing a banked limit reset on it), then return to reset order.
kd cli-proxy-api burn someone@example.com
kd cli-proxy-api burn --clear

# Show the pool's state and each account's weekly-quota use across its
# current week (4-hour bars, rollovers and now marked), or the last hours in
# 15-minute bars.
kd cli-proxy-api overview
kd cli-proxy-api overview --recent
kd cli-proxy-api overview --privacy   # "Account 1" instead of emails
```

## Development environments and stateful instances

Configure a fresh Ubuntu machine over SSH, optionally restoring a named instance's backup. A disposable environment
needs no named profile and no backup. Set up shared defaults once in `~/.config/kd/devboxes.toml` (or
`$XDG_CONFIG_HOME/kd/devboxes.toml`):

```toml
[bootstrap]
user = "scode"
public_key = "~/.ssh/id_ed25519.pub"
repos = ["scode/kd", "scode/voice"]

# Only needed for a stateful instance:
[devbox.devbox]
host = "devbox.example.com"
hostname = "devbox"
backup_dir = "~/devbox-backups"
```

| Intent                                                 | Invocation                                                                    |
| ------------------------------------------------------ | ----------------------------------------------------------------------------- |
| Suspend Hermes before a consistent backup or migration | `kd devbox suspend --profile devbox`                                          |
| Back up Hermes without changing whether it is running  | `kd devbox backup --profile devbox`                                           |
| Restart Hermes on the original box after suspension    | `kd devbox resume --profile devbox`                                           |
| Bootstrap a replacement and restore/start Hermes       | `kd devbox bootstrap --target NEW_IP --restore devbox`                        |
| Rehearse the restore without starting Hermes services  | `kd devbox bootstrap --target scode@test-worker --restore devbox --rehearsal` |
| Bootstrap a fresh disposable environment               | `kd devbox bootstrap --target worker --hostname worker`                       |
| Bootstrap using a different Unix user                  | `kd devbox bootstrap --target alice@worker --hostname worker`                 |

`suspend → backup → resume` returns the services to operation and leaves a new archive. For a move, suspend and back up
the source, create or reinstall the target yourself with your SSH key, then bootstrap it with `--restore devbox`. Keep
the source suspended; bootstrap does not contact it. Suspension stops current services without disabling their startup
after reboot. After verifying the replacement, update the profile's `host` if its address changed. `resume` starts the
original services; it is not a step after a successful restore.

Hermes is currently the only application state backed up. Git repos are cloned from remotes, so push or otherwise save
local work before disposing of a machine. The backup preflight reports dirty/unpushed repos but does not save them.
Backup never stops or starts services, even on failure; suspend first for a consistent archive. A live backup may fail
if Hermes reports it incomplete, including because of live sockets.

Add `--enroll-tailscale` to bootstrap to install and enroll an unenrolled target; existing enrollment is preserved.
Without it, Tailscale enrollment is left alone. Transport automatically uses Tailscale SSH for a known peer; add
`--plain-ssh` to force ordinary SSH. SSH aliases and `ssh://USER@HOST:PORT` work too. A bare host uses the shared user.
Bootstrap first tries that user, then root if the login fails, including during a restore rehearsal. It creates the
account if missing, authorizes the configured key, and grants passwordless sudo. An existing user login needs
passwordless sudo to perform setup.

The controller must have logins for Codex, OpenCode and Muse. OpenCode's and Muse's are copied to the target. The Codex
login (a file-backed ChatGPT login with at least a day left before its access token expires; `codex login` gives a fresh
one) is only lent to the two remote Codex runs that do the setup, then taken back and deleted from the target, because
two live copies of one ChatGPT login break each other. Claude's login is not used. Bootstrap prints a probe report plus
each phase's workarounds. Probe failures do not change the command's exit status; inspect the report before relying on
the box. Destroy disposable targets yourself when done.

Keep interactive Claude and Codex sessions closed during bootstrap, on the target and on the controller. After wiring
Claude to its router, kd completes its first-run onboarding state so opening `claude` does not ask you to log in.
Existing settings and project trust decisions are preserved. `backup --yes` skips its confirmation, not the preflight
report. See [SPEC.md](SPEC.md#kd-devbox) for prompts, restore semantics, and migrating the old per-box configuration
format.

Bootstrap also runs two local subscription routers in Docker, bound to loopback only: codex-lb for Codex and CLIProxyAPI
for Claude. They are the only way `codex` and `claude` on the box reach a model, so Docker must work there or bootstrap
fails. They start with no accounts, and logging accounts in is a manual step after bootstrap (bootstrap prints the
tunnel command and where to go). Until then `codex` and `claude` on that box fail; there is no native login to fall back
to. Router accounts and history live in Docker volumes and are not backed up.

Bootstrap installs `kd` itself from this repository's default branch with `cargo install --locked --git`. On a
bootstrapped box, `kd cargo scode update` updates kd, and any other tool installed from a github.com/scode repository,
to the latest commit of its branch.

Bootstrap copies the controller's global Git `user.name` and `user.email` to the target user's global config. Both must
be configured before running it. Other Git settings and repository-specific identities are not copied.

## Command Notes

`kd cli-proxy-api monitor run` talks to CLIProxyAPI's management API, by default at `http://127.0.0.1:8317` with the key
from `~/.config/cliproxy/secrets.env` (where `kd devbox bootstrap` puts it). It gives the Claude account whose weekly
quota resets soonest the highest priority, so quota about to expire is spent before quota that is not at risk, and
re-checks every 15 minutes and shortly after each known window reset. Every wake appends a JSON line to
`~/.local/state/kd/cli-proxy-api-monitor.jsonl`, which is also where to look for accounts CLIProxyAPI keeps in cooldown
after Anthropic says their quota is back (`cooldown_outlives_reset`). If the management key is rejected, the monitor
stops calling CLIProxyAPI until the key file changes, because CLIProxyAPI bans an address from its management API for 30
minutes after five failed keys. `monitor enable` runs it as a systemd user service with linger, so it keeps running
after logout; rerun `enable` after upgrading kd to restart it on the new binary, and read its output with
`journalctl --user -u kd-cli-proxy-api-monitor -f`. Run `monitor disable` before uninstalling kd, or the unit keeps
trying to start a binary that is gone. From another machine, run `monitor run` by hand: forward the port with SSH and
pass `--key-file` with a file holding only the management key.

Timezone setup keeps `/etc/localtime` and any existing `/etc/timezone` consistent with `America/Los_Angeles`. The probe
checks them independently, along with systemd's timezone and any inherited `TZ` override; application and container
timezone settings remain separate.

`kd yt thumb resize` rewrites the file you pass it. If the image is already below 2 MB, it does nothing. This shells out
to ImageMagick, so you need `magick` installed.

`kd gh repo apply-preferred-settings` shells out to the GitHub CLI, so `gh` needs to be installed and authenticated. In
single-repo mode, if you omit `owner/repo`, run it from the repo root; it reads `.git/config` there and uses the
`origin` remote. The preferred settings are:

- squash merge enabled
- squash commit title set to `PR_TITLE`
- squash commit message set to `PR_BODY`
- merge commits disabled
- rebase merges disabled
- delete branch on merge enabled
- default Actions workflow token permissions set to read-only, with `can_approve_pull_request_reviews` disabled
- on public repos, fork pull-request workflows from external contributors (outside the repo and its org) require
  approval before running; `pull_request_target` workflows are not gated by this (`all_external_contributors`)
- on non-public repos, fork pull-request workflows are disabled entirely

The fork-workflow requirement is visibility-specific: public repos get the approval policy above; non-public repos get
fork-PR workflows disabled outright instead, since GitHub rejects the approval-policy endpoint on non-public repos. Each
is applied only where it's applicable, and the correct one lands automatically on the first run after a repo's
visibility changes.

`kd gh repo main-protect` also uses `gh`, and it uses the same repo-root auto-detection when you omit `owner/repo`. It
ensures a ruleset named `main-protect` exists on the default branch, enforces linear history, blocks force-pushes, and
then lets you interactively choose required status checks from checks it finds on the default branch and a recent merged
PR returned by `gh pr list`. Existing required checks that are not rediscovered are preserved unless you select `none`.

`kd gh pr list` also uses `gh`. It lists the PRs you authored that are open, or were closed or merged in the last 7
days, leaving out PRs to repositories your own account owns unless you pass `--include-mine` (organization repos are
always included). The first column is how long ago someone other than you last did something significant on the PR
(`25m`, `1d7h21m`), and the list is sorted by it, most recent first. `-` means nobody else has, and `?` means GitHub
would not return enough of the PR's history to tell. Comments, reviews (including review-thread replies), pushes,
merges, closes, label changes and similar count as significant; being mentioned does not, and neither does anything you
do yourself. Bots count like anyone else, so a bot that comments on every push keeps the age short. In a terminal, the
`owner/repo#N` column is a clickable link (if your terminal supports OSC 8 hyperlinks) and titles are cut to fit the
width; piped output instead is unpadded, with full titles and the URL at the end of each line.

`kd ubiworker` shells out to the `ubi` CLI (must be on `PATH`) and calls the Tailscale API directly. It needs:

- `UBI_TOKEN` set (read by `ubi` itself)
- `TS_API_CLIENT_ID` / `TS_API_CLIENT_SECRET` for a Tailscale OAuth client with the `auth_keys` scope, owning
  `tag:ubicloud`
- at least one SSH key registered in Ubicloud (`ubi sk create`); every registered key is installed on each worker
- a `tailscale` binary on `PATH` (with the machine joined to the same tailnet) plus an OpenSSH `ssh` binary, for
  `kd ubiworker ssh` — kd execs directly into `tailscale ssh`, which in turn wraps `ssh` (see `SPEC.md`). On macOS this
  means the standalone Tailscale distribution: the App Store and TestFlight builds refuse the `tailscale ssh`
  subcommand.
- the same local username on every machine you run `create` and `ssh` from: `create` provisions the account `id -un`
  reports (it must satisfy Ubicloud's `[a-z_][a-z0-9_-]{0,31}` rule and must not be `root`, so don't run kd under
  `sudo`), `ssh` logs in as whatever `id -un` reports where _it_ runs, and kd stores nothing about which user a worker
  was created with.
- a tailnet policy `ssh` rule allowing you to log in to `tag:ubicloud` as that username. Workers are tag-owned, so
  Tailscale's default "SSH to your own devices" rule does not cover them. `create` prints the exact rule object to use,
  with your own Tailscale login as `src` and your username under `users`; append it to the existing `ssh` array of the
  policy file (create the array if there is none):

  ```json
  { "action": "accept", "src": ["<your-tailscale-login>"], "dst": ["tag:ubicloud"], "users": ["<output of id -un>"] }
  ```

  Widen `src` (e.g. to `autogroup:member`) only if you really mean to let every tailnet member log in as you. The
  ordinary ACLs/grants must also allow you to reach `tag:ubicloud` on port 22.

Upgrading from a kd that predates Tailscale SSH: workers it created joined the tailnet without `--ssh` and have no
Tailscale SSH host key, so `kd ubiworker ssh` fails against them. Run `sudo tailscale set --ssh` on each one (over
whatever access you used before), or destroy and recreate them.

Every worker gets the same fixed shape: location `us-east-a2`, size `standard-4`, an 80 GiB disk, and the
`ubuntu-resolute` image — this isn't configurable via flags. See `SPEC.md` for the intentional behavior around
ownership, naming, and the minted tailscale key's lifetime.

## Logging

Default log level is INFO.

| Flag   | Level |
| ------ | ----- |
| `-v`   | DEBUG |
| `-vv`  | TRACE |
| `-q`   | WARN  |
| `-qq`  | ERROR |
| `-qqq` | OFF   |

```sh
kd -v yt thumb resize image.png   # debug output
kd -qq yt thumb resize image.png  # errors only
```
