# kd

Small personal toolbox. The name means nothing; it is just designed to be easy to type and not clash with other tools.

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
PR returned by `gh pr list`.

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
