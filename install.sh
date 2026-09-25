#!/usr/bin/env bash
# kd installer for macOS and Linux. Intended to be piped from GitHub:
#
#   curl -fsSL https://raw.githubusercontent.com/scode/kd/main/install.sh | bash
#
# What it does: preflight-check for a C toolchain (clear errors beat a
# cryptic failure at the final build step), install rust with the official
# rustup one-liner if cargo isn't usable, make sure the toolchain is new
# enough for kd, then install kd straight from GitHub the same way
# `kd cargo scode install kd` does, and warn at the end if the installed
# binary isn't what `kd` resolves to. Specified in SPEC.md (`## install.sh`).
#
# Installing from the repository rather than a local checkout matters: it
# records https://github.com/scode/kd as kd's source, which is what lets
# `kd cargo scode update` (and cargo-update) update it later.
#
# Uninstalling: `kd cargo scode uninstall kd` or plain `cargo uninstall kd`.
set -euo pipefail

# Oldest rust that builds kd: it uses edition 2024 (Rust 1.85). Keep in step
# with `rust-version` in Cargo.toml.
min_rust_minor=85

# Set once rustup has been installed by this run; the EXIT trap uses it to
# tell the user their current shell still lacks rust, whether or not the
# rest of the install succeeded.
installed_rustup=no

rust_env_hint() {
  if [ "$installed_rustup" = yes ]; then
    local home="${CARGO_HOME:-$HOME/.cargo}"
    echo "" >&2
    echo "rust was installed by this script. Your current shell does not have it on PATH yet:" >&2
    echo "open a new shell, or run: . \"${home%/}/env\"   (fish: source \"${home%/}/env.fish\")" >&2
  fi
}

# Prints the rustc minor version (the 85 in 1.85.0), or nothing if rustc
# can't be run or its output isn't understood. Only 1.x exists today.
rustc_minor() {
  rustc --version 2>/dev/null | sed -n 's/^rustc 1\.\([0-9][0-9]*\)\..*/\1/p'
}

main() {
  # $HOME underpins every path below. Without this, an unset HOME (stripped
  # container, service account) dies with nounset's raw "unbound variable"
  # instead of an actionable message like every other preflight here.
  : "${HOME:?HOME is not set; export HOME and retry}"

  trap rust_env_hint EXIT

  # cargo needs a C linker at build time even for pure-rust dependencies,
  # and rustup does not provide one. Hitting that at the final step, after
  # possibly installing rust, is the worst place to find out. On macOS,
  # /usr/bin/cc exists as a stub even without the developer tools, so probe
  # xcode-select instead. On Linux rustc invokes `cc` specifically, so a
  # box with only `gcc` or `clang` under those names would still fail to
  # link; require `cc` itself.
  case "$(uname -s)" in
    Darwin)
      xcode-select -p >/dev/null 2>&1 || {
        echo "error: Xcode Command Line Tools not installed; run: xcode-select --install" >&2
        exit 1
      }
      ;;
    *)
      command -v cc >/dev/null 2>&1 || {
        echo "error: no \`cc\` in PATH, which rust uses as its linker; install a C toolchain" >&2
        echo "       (e.g. apt install build-essential), or link cc to your gcc/clang" >&2
        exit 1
      }
      ;;
  esac

  # `cargo --version` rather than `command -v cargo`: a rustup shim with no
  # default toolchain resolves on PATH but can't build anything, and that
  # is exactly the case the rustup installer below fixes (with no default
  # toolchain, rustup-init installs stable even over an existing rustup).
  if ! cargo --version >/dev/null 2>&1; then
    # curl is a given when this script itself arrived via curl | bash, but
    # not when it's run from a downloaded copy.
    command -v curl >/dev/null 2>&1 || {
      echo "error: cargo is not usable and curl is not in PATH to install rust with" >&2
      exit 1
    }
    echo "cargo not usable; installing rust with rustup (https://rustup.rs)" >&2
    # The official rustup.rs one-liner, with `-y` so the whole kd install is
    # one unattended copy and paste: it accepts rustup's defaults (stable
    # toolchain, default profile, PATH added to shell profiles). pipefail
    # (set at the top) is what makes a failed download abort this line
    # instead of handing empty input to sh and "succeeding".
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    installed_rustup=yes
    # rustup's profile edits only affect new shells; load its environment
    # into this one. rustup honors a custom CARGO_HOME, so source the env
    # file from wherever it actually landed.
    # shellcheck source=/dev/null
    . "${CARGO_HOME:-$HOME/.cargo}/env"
    # bash caches command lookups, and the failed `cargo` probe above can
    # leave a stale path hashed; flush it. Then insist cargo works now, so a
    # failed rust install stops here rather than as a cryptic build error.
    hash -r
    cargo --version >/dev/null 2>&1 || {
      echo "error: rust was installed but cargo still isn't usable in this shell;" >&2
      echo "       open a new shell and re-run" >&2
      exit 1
    }
  fi

  # A working but old cargo (a distro package, a rustup stable nobody has
  # updated in years) would otherwise fail deep in the build with an
  # edition error. With rustup, install a current stable just for this
  # build rather than changing the user's default toolchain; without it,
  # say what's needed.
  local toolchain=()
  local minor
  minor=$(rustc_minor)
  if [ -z "$minor" ] || [ "$minor" -lt "$min_rust_minor" ]; then
    if command -v rustup >/dev/null 2>&1; then
      echo "rust is older than 1.$min_rust_minor; installing current stable with rustup for this build" >&2
      rustup toolchain install stable --profile minimal
      toolchain=(+stable)
    else
      echo "error: kd needs rust 1.$min_rust_minor or newer, found: $(rustc --version 2>/dev/null || echo unknown)" >&2
      echo "       update rust (for example by installing it with rustup from https://rustup.rs) and re-run" >&2
      exit 1
    fi
  fi

  # The equivalent of `kd cargo scode install kd` (see SPEC.md): same
  # recorded source (plain HTTPS URL, no `.git`, branch, tag or commit, so
  # kd tracks the default branch and later updates recognise it), and
  # `--locked` so the build uses the dependency versions in the committed
  # Cargo.lock. Two deliberate differences: `--force`, because this is the
  # kd installer and replacing an existing kd binary (typically one from the
  # older checkout-based installer, which cargo would otherwise refuse to
  # overwrite) is the point of running it, so a rerun rebuilds kd from the
  # current default branch; and no cargo-update lock setting, since
  # cargo-update is usually not installed here and `kd cargo scode update`
  # applies that setting itself before it updates.
  # `${arr[@]+...}`: bash 3.2 (macOS /bin/bash) treats expanding an empty
  # array under `set -u` as an unbound variable, and `toolchain` is empty
  # on the normal path.
  cargo ${toolchain[@]+"${toolchain[@]}"} install --locked --force --git https://github.com/scode/kd kd

  # cargo puts the binary in $CARGO_INSTALL_ROOT/bin if set, else
  # $CARGO_HOME/bin (~/.cargo/bin by default). An `install.root` set in a
  # cargo config file also moves it, which this does not detect. Strip a
  # trailing slash so the string compare below can't be defeated by a
  # cosmetic double slash.
  local install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
  local bin_dir="${install_root%/}/bin"
  # Compare the resolved path, not mere existence: a different `kd` earlier
  # on PATH would otherwise shadow the fresh install silently. (command -v
  # failing is fine here: a command substitution in an assignment doesn't
  # trip errexit, and empty output is handled below.)
  local resolved
  resolved=$(command -v kd 2>/dev/null || true)
  if [ "$resolved" != "$bin_dir/kd" ]; then
    echo "" >&2
    echo "==================================================================" >&2
    echo "  ACTION NEEDED: kd was installed to $bin_dir," >&2
    if [ -n "$resolved" ]; then
      echo "  but 'kd' resolves to $resolved, which comes earlier on PATH." >&2
      echo "  Remove that one, or put $bin_dir before it on PATH:" >&2
    else
      echo "  but typing 'kd' does not resolve there. Add this line to your" >&2
      echo "  ~/.bashrc or ~/.zshrc or equivalent:" >&2
    fi
    echo "" >&2
    echo "      export PATH=\"$bin_dir:\$PATH\"" >&2
    echo "" >&2
    echo "==================================================================" >&2
  fi
}

# Everything runs from main, called on the last line: a download cut short
# by a network failure defines at most part of a function and runs nothing,
# instead of executing whatever prefix of the script arrived.
main "$@"
