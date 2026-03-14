# Contributing to pr-reviewer

Thanks for your interest in contributing. This document covers how to get set up, how to submit changes, and what we expect from contributions.

## Getting started

### Prerequisites

- Rust 1.75+ (stable)
- A GitHub personal access token with `pull_requests:rw`, `contents:read`, `issues:read`
- At least one supported CLI harness installed (claude-code, opencode, or codex)

### Building

```bash
git clone https://github.com/nicholai/pr-reviewer.git
cd pr-reviewer
cargo build
```

### Running tests

```bash
cargo test
```

### Linting

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

## Code style

- Run `cargo fmt` before committing
- No clippy warnings (treated as errors in CI)
- Maximum 700 lines per file (soft limit)
- Maximum 3 levels of indentation
- Line width: 100 soft, 120 hard
- No `unwrap()` or `expect()` in non-test code -- use `anyhow::Context` for error propagation
- Prefer `thiserror` for library-style error types, `anyhow` for application-level errors

## Architecture

```
src/
  main.rs          CLI entry point (clap)
  config.rs        TOML config loading, validation, CLI config commands
  daemon.rs        Polling loop, PID management, signal handling
  safety.rs        Sandbox policy, path validation, fork PR rules
  github/          GitHub REST API client, PR fetching, review posting
  context/         Diff parsing, file reading, GitNexus integration, context assembly
  harness/         CLI process spawning (claude-code, opencode, codex)
  review/          Prompt construction, output parsing, pipeline orchestration
  store/           SQLite state management, dedupe, logging
```

### Key patterns

- **Harness trait**: each CLI tool implements `Harness` with `build_command()` and `parse_output()`. Adding a new harness means implementing this trait.
- **Dedupe claims**: reviews use atomic INSERT with a unique dedupe key to prevent double-posting. Check `store/db.rs` for the claim/complete/fail lifecycle.
- **Context assembly**: `context/retriever.rs` builds the prompt content with truncation policies. If you're changing what context the reviewer sees, this is where to look.

## Submitting changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run `cargo fmt`, `cargo clippy`, and `cargo test`
4. Write a clear commit message explaining what and why
5. Open a PR against `main`

### Commit messages

Use conventional commits:

```
feat: add webhook mode for instant PR detection
fix: prevent UTF-8 panic in output truncation
docs: add harness configuration examples
```

### What makes a good PR

- Focused on a single change
- Tests for new functionality
- No unrelated formatting changes
- Clear description of what changed and why

## Adding a new harness

1. Create `src/harness/my_harness.rs`
2. Implement the `Harness` trait
3. Add the variant to `HarnessKind` in `src/harness/mod.rs`
4. Wire it up in `create_harness()` in `src/harness/mod.rs`
5. Add a test that verifies the command construction
6. Update the README harness table

## Reporting bugs

Open an issue with:

- What you expected
- What actually happened
- Steps to reproduce
- Your `pr-reviewer status` output
- Relevant log output

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
