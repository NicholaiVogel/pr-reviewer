# pr-reviewer

A self-hosted PR review daemon written in Rust. It watches your GitHub repositories for pull requests, spawns a local AI coding CLI to review them, and posts comments back to GitHub automatically.

No API keys required -- pr-reviewer uses your existing CLI subscriptions (Claude Code, OpenCode, Codex) instead of calling AI APIs directly.

## How it works

1. You configure which repos to watch and which CLI harness to use
2. The daemon polls GitHub for new or updated pull requests
3. For each PR, it assembles context (diff, changed files, optional GitNexus impact analysis)
4. It spawns your chosen CLI tool with a review prompt
5. The response is parsed into structured comments and posted as a GitHub PR review
6. If someone replies to a review comment, the daemon responds in-thread

## Features

- **Multi-harness support** -- Claude Code, OpenCode, Codex, or any CLI that accepts a prompt
- **No API keys** -- uses your existing Pro/Max subscription through the CLI
- **Multi-repo** -- watch as many repos as you want, each with its own harness and model
- **GitNexus integration** -- optional codebase indexing for impact analysis and call chain context
- **Safety first** -- fork policy enforcement, path traversal protection, environment scrubbing, sandboxed execution
- **Idempotent** -- deduplicated reviews with atomic claim locks, no double-posting on restarts
- **Adaptive polling** -- ETag caching, backoff, jitter, and rate budget tracking
- **CLI-first** -- every setting is controllable via CLI commands, designed for agent management
- **Signet compatible** -- runs with `SIGNET_NO_HOOKS=1` to avoid memory pollution and statistical bias
- **Dry-run mode** -- log reviews without posting to GitHub

## Installation

```bash
# from source
cargo install --path .

# or just build
cargo build --release
```

## Quick start

```bash
# initialize config and state database
pr-reviewer init

# add a repo (runs gitnexus analyze if gitnexus is installed)
pr-reviewer add nicholai/signetai --path ~/signet/signetai --harness claude-code --model claude-sonnet-4-6

# test with a dry run on a specific PR
pr-reviewer review nicholai/signetai#42 --dry-run

# review for real
pr-reviewer review nicholai/signetai#42

# start the daemon
pr-reviewer start

# or run in the background
pr-reviewer start --daemon
```

## Configuration

Config lives at `~/.config/pr-reviewer/config.toml`. All settings can be managed via CLI:

```bash
pr-reviewer config list                        # show all config
pr-reviewer config set harness.default codex   # change default harness
pr-reviewer config set harness.model gpt-5.3-codex
pr-reviewer config get daemon.poll_interval_secs
```

See [config.example.toml](config.example.toml) for the full configuration reference.

### Per-repo overrides

Each repo can override the global harness, model, fork policy, and review instructions:

```toml
[[repos]]
owner = "nicholai"
name = "openmarketui"
local_path = "/mnt/work/dev/openmarketui"
harness = "codex"
model = "gpt-5.3-codex"
fork_policy = "ignore"
ignore_paths = ["*.lock", "dist/**"]
custom_instructions = "This project uses a custom ORM. Watch for SQL injection."
gitnexus = true
```

## CLI reference

### Repo management
```
pr-reviewer add <owner/repo> --path <local>   add a repo
  [--harness <name>] [--model <model>]        set harness and model
  [--fork-policy <policy>]                    ignore | limited | full
pr-reviewer remove <owner/repo>               remove a repo
pr-reviewer list                              list all configured repos
pr-reviewer index <owner/repo>                re-run gitnexus analyze
```

### Reviewing
```
pr-reviewer review <owner/repo>#<number>      review a specific PR
  [--dry-run]                                 log without posting
  [--harness <name>] [--model <model>]        override for this review
```

### Daemon
```
pr-reviewer start [--daemon]                  start polling (--daemon for background)
pr-reviewer stop                              stop backgrounded daemon
pr-reviewer status                            daemon state, rate limits, recent reviews
```

### Usage tracking
```
pr-reviewer logs [--repo <r>] [--since <d>]   review history
pr-reviewer stats [--repo <r>] [--since <d>]  usage summary by repo, model, harness
```

## Supported harnesses

| Harness | CLI command | Model flag |
|---------|-------------|------------|
| claude-code | `claude --model <m> --dangerously-skip-permissions -p "..."` | `--model` |
| opencode | `opencode run --model <m> --format json "..."` | `--model` |
| codex | `codex exec --model <m> --skip-git-repo-check --json "..."` | `--model` |

Model resolution: per-repo config > global `harness.model` > harness default.

## Safety model

pr-reviewer runs AI tools against potentially untrusted code. The safety model includes:

- **Sandboxed execution**: harnesses run in a temporary directory, not the repo root
- **Environment scrubbing**: `HOME`, `SSH_AUTH_SOCK`, `GH_TOKEN`, `AWS_*`, `ANTHROPIC_API_KEY`, and other sensitive variables are stripped from the harness environment
- **Fork policy**: configurable per-repo (`ignore`, `limited`, `full`) with trusted author allowlists
- **Path validation**: canonicalization and containment checks reject traversal attacks and symlink escapes
- **Signet bypass**: `SIGNET_NO_HOOKS=1` prevents hook invocation in spawned processes

## GitNexus integration

If [GitNexus](https://github.com/nicholai/gitnexus) is installed and a repo has been indexed (`.gitnexus/` directory exists), pr-reviewer enriches review context with:

- Impact analysis (blast radius of changed symbols)
- Call chain tracing (upstream/downstream dependencies)
- Community context (which functional area of the codebase is affected)

To index a repo: `pr-reviewer index owner/repo` or `gitnexus analyze /path/to/repo`.

## License

[MIT](LICENSE)
