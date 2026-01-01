# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```sh
cargo build              # build
cargo test               # run all tests
cargo test resize        # run tests matching "resize"
cargo clippy             # lint
cargo fmt                # format
```

Requires ImageMagick (`magick` command) for image operations.

## Architecture

This is a Rust CLI tool using clap with derive macros for argument parsing. Commands are organized in a nested subcommand hierarchy:

```
kd <global-flags> <command> <subcommand> ...
```

**Command structure** (`src/cmd/`):
- Each command group is a module with its own `Commands` enum implementing `Subcommand`
- Each enum variant dispatches to a `run()` function
- Pattern: `src/cmd/<group>/mod.rs` defines the enum, `src/cmd/<group>/<action>.rs` implements the logic

**Current hierarchy**: `kd yt thumb resize <file>`

**Adding a new command**: Create a module following the existing pattern—add a `Commands` enum with `#[derive(Subcommand)]`, implement `run()`, and wire it into the parent module's enum.

**Logging**: Uses tracing. Global `-v`/`-q` flags control level (see README.md).
