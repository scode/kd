#!/usr/bin/env bash
# kd installer for macOS and Linux. Intended to be piped from GitHub:
#
#   curl -fsSL https://raw.githubusercontent.com/scode/kd/main/install.sh | bash
#
# What it does: preflight-check git and a C toolchain (clear errors beat a
# cryptic failure at the final build step), reserve ~/git/kd (refusing to
# touch anything that already exists there), offer to install rust via
# homebrew or rustup if it's missing — asking interactively via /dev/tty,
# which works even though the script itself arrives on stdin — then clone
# this repo into ~/git/kd and `cargo install` it, warning at the end if the
# installed binary isn't on PATH.
#
# Uninstalling is two deletions; see the Install section of README.md.
set -euo pipefail

# $HOME underpins every path below. Without this, an unset HOME (stripped
# container, service account) dies with nounset's raw "unbound variable"
# instead of an actionable message like every other preflight here.
: "${HOME:?HOME is not set; export HOME and retry}"

# Preflight: the clone happens partway through, but check git up front —
# failing after installing rust would waste that whole step, and a raw
# "git: command not found" from deep in the script is a worse error than
# this one.
command -v git >/dev/null 2>&1 || {
  echo "error: git not found in PATH; install git first" >&2
  exit 1
}

# cargo needs a C linker at build time even for pure-rust dependencies, and
# hitting that at the final step — after cloning and possibly installing
# rust — is the worst place to find out. On macOS, /usr/bin/cc exists as a
# stub even without the developer tools, so probe xcode-select instead.
case "$(uname -s)" in
  Darwin)
    xcode-select -p >/dev/null 2>&1 || {
      echo "error: Xcode Command Line Tools not installed; run: xcode-select --install" >&2
      exit 1
    }
    ;;
  *)
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 || {
      echo "error: no C compiler/linker in PATH; install one (e.g. apt install build-essential)" >&2
      exit 1
    }
    ;;
esac

# Reserve the target directory atomically. `mkdir` without -p both checks
# and claims in one step, so there's no check-then-act gap for something to
# slip a directory (or symlink — mkdir refuses dangling symlinks too, which
# a plain -e test would miss) into ~/git/kd during the potentially long
# rust-install detour below.
dir="$HOME/git/kd"
mkdir -p "$HOME/git" 2>/dev/null || {
  echo "error: could not create $HOME/git (permissions on \$HOME? is $HOME/git a file?)" >&2
  exit 1
}
if ! mkdir "$dir" 2>/dev/null; then
  # mkdir can fail for reasons other than EEXIST (permissions, disk full);
  # only claim "already exists" when that's actually what happened, so the
  # remediation hint never points at the wrong problem. The hint quotes the
  # path: it's meant to be copy-pasted, and an unquoted path with a space in
  # $HOME would word-split into an rm -rf of the wrong thing.
  if [ -e "$dir" ] || [ -L "$dir" ]; then
    echo "error: $dir already exists; not touching it" >&2
    echo "       (a leftover from a previously failed install can be removed with: rm -rf \"$dir\")" >&2
  else
    echo "error: could not create $dir (permissions on $HOME/git? disk full?)" >&2
  fi
  exit 1
fi
# If we die before the clone populates the reserved directory, remove it so
# a rerun isn't refused over an empty husk. rmdir refuses to delete
# non-empty directories, so once the clone lands this trap is a no-op and a
# real checkout can never be swept up by it.
trap 'rmdir "$dir" 2>/dev/null || true' EXIT

# `cargo --version` rather than `command -v cargo`: a rustup shim with no
# default toolchain resolves on PATH but can't build anything, and it's
# better to catch that here — where rust installation is on offer — than at
# the final build step.
if ! cargo --version >/dev/null 2>&1; then
  # No terminal means no way to ask which installer to use. Check before
  # the prompt so a headless run (container, CI) gets a real message
  # instead of a raw redirection error from the printf below.
  if ! { : </dev/tty; } 2>/dev/null; then
    echo "error: rust is missing and no terminal is available to ask how to install it;" >&2
    echo "       install rust first, then re-run" >&2
    exit 1
  fi
  printf 'rust not found in PATH. install via homebrew or rustup? [homebrew/rustup] ' >/dev/tty
  # Guard the read: EOF on a tty that opened but has no input would
  # otherwise kill the script (set -e) with zero output. Lowercase the
  # answer so "Homebrew"/"RUSTUP" count as valid answers.
  read -r choice </dev/tty || {
    echo "error: could not read a response from the terminal" >&2
    exit 1
  }
  choice=$(printf '%s' "$choice" | tr '[:upper:]' '[:lower:]')
  case "$choice" in
    homebrew | brew)
      command -v brew >/dev/null 2>&1 || {
        echo "error: brew not found in PATH" >&2
        exit 1
      }
      # </dev/tty for symmetry with the other interactive spots: our stdin
      # is the drained script pipe, and if brew ever decides to prompt it
      # should reach a terminal, not EOF.
      brew install rust </dev/tty
      ;;
    rustup)
      # curl is a given when this script itself arrived via curl | bash,
      # but not when it's run from a downloaded copy.
      command -v curl >/dev/null 2>&1 || {
        echo "error: curl not found in PATH" >&2
        exit 1
      }
      # rustup's own installer reconnects /dev/tty the same way when piped,
      # so it stays interactive here. pipefail (set at the top) is what
      # makes a failed download abort this line instead of handing empty
      # input to sh and "succeeding".
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
      # rustup honors a custom CARGO_HOME, so source the env file from
      # wherever it actually landed.
      # shellcheck source=/dev/null
      . "${CARGO_HOME:-$HOME/.cargo}/env"
      ;;
    *)
      echo "error: answer 'homebrew' or 'rustup'" >&2
      exit 1
      ;;
  esac

  # Two subtleties make this recheck necessary. First, bash caches command
  # lookups: the failed `cargo` probe above can leave a stale (broken) path
  # hashed, and the brew branch never reassigns PATH, which is what would
  # implicitly flush it — so flush explicitly. Second, insisting cargo
  # actually works now keeps a failed rust install from surfacing later as
  # a cryptic error after the clone.
  hash -r
  cargo --version >/dev/null 2>&1 || {
    echo "error: rust was installed but cargo still isn't usable in this shell;" >&2
    echo "       open a new shell and re-run" >&2
    exit 1
  }
fi

git clone https://github.com/scode/kd.git "$dir"
# --force: overwrite a stale kd binary from an earlier install (e.g. a
# partial uninstall that removed the checkout but not the binary). Without
# it cargo refuses, and the resulting error loop points at the checkout —
# the wrong thing — as the blocker.
cargo install --force --path "$dir"

# cargo puts the binary in $CARGO_HOME/bin (~/.cargo/bin by default). rustup
# wires that into new shells via profile edits; `brew install rust` does
# not, so make the missing-PATH case a banner the user can't miss instead of
# letting a "successful" install end in a quiet `kd: command not found`.
# Strip any trailing slash from a custom CARGO_HOME so the string compare
# below can't be defeated by a cosmetic double slash.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_bin="${cargo_home%/}/bin"
# Compare the resolved path, not mere existence: a different `kd` earlier
# on PATH would otherwise shadow the fresh install silently, and the banner
# is exactly what should catch that. (command -v failing is fine here — a
# command substitution in an if-condition doesn't trip errexit, and empty
# output correctly compares unequal.)
if [ "$(command -v kd 2>/dev/null)" != "$cargo_bin/kd" ]; then
  echo "" >&2
  echo "==================================================================" >&2
  echo "  ACTION NEEDED: kd was installed to $cargo_bin," >&2
  echo "  but typing 'kd' does not resolve there. Please add the" >&2
  echo "  following line to your ~/.bashrc or ~/.zshrc or equivalent:" >&2
  echo "" >&2
  echo "      export PATH=\"\$PATH:$cargo_bin\"" >&2
  echo "" >&2
  echo "==================================================================" >&2
fi
