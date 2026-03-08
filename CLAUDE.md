# CLAUDE.md

Requires ImageMagick (`magick` command) at runtime for image operations.

## Before finishing work

Run `cargo fmt`, `cargo clippy`, and `cargo test` before considering work complete or creating a PR. All must pass.

## Style

Comment generously — the codebase should be easy to skim for intent and functionality. Focus on *why* and *what the purpose is*, not restating the code.

## Conventional Commits

All commit messages and PR titles must use Conventional Commit format: `<type>: <short summary>`

Allowed types: `feat`, `fix`, `docs`, `perf`, `refactor`, `style`, `test`, `chore`, `ci`, `revert`.

Append `!` after the type for breaking changes (e.g. `feat!: remove legacy endpoint`). Scope is optional.

Rules:

- Type reflects the user-visible effect, not the implementation activity. A bug fix that requires heavy refactoring is
  `fix`, not `refactor`. A new CLI flag is `feat`, not `chore`.
- The summary after the colon is lowercase, imperative mood, no trailing period.
- Keep the first line under 72 characters.
