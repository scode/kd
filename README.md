# kd

Small personal toolbox. The name means nothing; it is just designed to be easy to type and not clash with other tools.

## Install

Works on macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/scode/kd/main/install.sh | bash
```

It clones into `~/git/kd` (failing rather than touching a checkout that's already there) and runs `cargo install` on the
checkout. It needs `git` and a C toolchain already installed — it checks and says so if they're missing. If cargo isn't
working (missing, or a toolchain-less rustup shim), it asks whether to install rust via homebrew or rustup first. Read
[`install.sh`](install.sh) before piping it if you (sensibly) don't run shell scripts off the internet blind.

### Uninstall

Deletes the checkout and the installed binary; rust itself is left alone. (With a custom `CARGO_HOME`, the binary is
under `$CARGO_HOME/bin` instead.)

```sh
rm -f ~/.cargo/bin/kd
rm -rf ~/git/kd
```

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
```

## Command Notes

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
