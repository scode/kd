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

# List existing ubiworker VMs.
kd ubiworker list

# Destroy ubiworker VM(s) (no confirmation prompt; see SPEC.md).
# With no name, targets the sole existing ubiworker.
kd ubiworker destroy
kd ubiworker destroy myname
kd ubiworker destroy myname otherworker
kd ubiworker destroy --all
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

`kd gh repo main-protect` also uses `gh`, and it uses the same repo-root auto-detection when you omit `owner/repo`. It
ensures a ruleset named `main-protect` exists on the default branch, enforces linear history, blocks force-pushes, and
then lets you interactively choose required status checks from checks it finds on the default branch and a recent merged
PR returned by `gh pr list`. Existing required checks that are not rediscovered are preserved unless you select `none`.

`kd ubiworker` shells out to the `ubi` CLI (must be on `PATH`) and calls the Tailscale API directly. It needs:

- `UBI_TOKEN` set (read by `ubi` itself)
- `TS_API_CLIENT_ID` / `TS_API_CLIENT_SECRET` for a Tailscale OAuth client with the `auth_keys` scope, owning
  `tag:ubicloud`
- Ubicloud SSH keys registered under the names `laptop` and `devbox`

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
