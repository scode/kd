# AGENTS.md

This project is either public now, or may become public in the future. No content in this project should contain
personal information such as personal usernames, hostnames, details about the local environments, etc.

Requires ImageMagick (`magick` command) at runtime for image operations.

## Before finishing work

Run these checks before considering work complete or creating a PR. The commands match `.github/workflows/ci.yml`; all
must pass:

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
dprint check
bash -n install.sh
shellcheck install.sh
```

Use the stable Rust toolchain with rustfmt and clippy available. CI refreshes it with
`rustup update stable && rustup default stable`. Install ImageMagick and make sure `magick` is on `PATH` before testing:
CI installs Ubuntu's `imagemagick` package and links `/usr/bin/convert` to `/usr/local/bin/magick`. On a machine with
ImageMagick 7, the native `magick` command already satisfies that requirement; do not replace it. Without ImageMagick,
the image tests may skip themselves, which does not reproduce CI's coverage. `shellcheck` and `dprint` must also be
installed; CI runs the latter through `dprint/check@v2.3`.

The separate PR Base workflow checks the GitHub event's base branch rather than local files, so it has no local check
command. It currently rejects PRs based on anything other than `main`, including stacked PRs.

Agents must conform to `SPEC.md` and `SPEC_impl.md`. If implementation and either file disagree, treat that as a bug or
explicitly update the file in the same change. `SPEC.md` records user-facing behavior only; implementation choices that
are deliberate and easy to "fix" into something worse go in `SPEC_impl.md`.

## Style

Comment generously — the codebase should be easy to skim for intent and functionality. Focus on _why_ and _what the
purpose is_, not restating the code.

## Conventional Commits

All commit messages and PR titles must use Conventional Commit format: `<type>: <short summary>`

Allowed types: `feat`, `fix`, `docs`, `perf`, `refactor`, `style`, `test`, `chore`, `ci`, `revert`.

Append `!` after the type for breaking changes (e.g. `feat!: remove legacy endpoint`). Scope is optional.

Rules:

- Type reflects the user-visible effect, not the implementation activity. A bug fix that requires heavy refactoring is
  `fix`, not `refactor`. A new CLI flag is `feat`, not `chore`.
- The summary after the colon is lowercase, imperative mood, no trailing period.
- Keep the first line under 72 characters.
