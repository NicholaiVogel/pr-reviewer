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

## Prerequisites

- **Rust toolchain** -- install via [rustup](https://rustup.rs/)
- **GitHub Personal Access Token** -- fine-grained PAT with `pull_requests:read+write` and `contents:read` scopes, or a classic PAT with `repo` scope
- **At least one AI CLI tool** installed and authenticated:
  - [Claude Code](https://docs.anthropic.com/en/docs/claude-code) (`claude`) -- requires a Pro/Max subscription
  - [OpenCode](https://github.com/opencode-ai/opencode) (`opencode`)
  - [Codex](https://github.com/openai/codex) (`codex`)
- **GitNexus** (optional) -- `npm install -g gitnexus` or use `npx gitnexus`. Enriches reviews with codebase impact analysis

## Installation

```bash
# clone and build
git clone https://github.com/NicholaiVogel/pr-reviewer.git
cd pr-reviewer
cargo build --release

# the binary is at ./target/release/pr-reviewer
# optionally install it to your PATH
cargo install --path .
```

## Getting started

### 1. Initialize

```bash
pr-reviewer init
```

This creates `~/.config/pr-reviewer/config.toml` and the SQLite state database.

### 2. Set your GitHub token

```bash
pr-reviewer config set github.token ghp_your_token_here
```

### 3. Add a repository

```bash
pr-reviewer add owner/repo --path /path/to/local/clone \
  --harness claude-code \
  --model claude-sonnet-4-6
```

If GitNexus is installed, this automatically runs `gitnexus analyze` to index the repo.

### 4. Test with a dry run

```bash
pr-reviewer review owner/repo#42 --dry-run
```

This runs the full review pipeline but logs the output instead of posting to GitHub.

### 5. Review for real

```bash
pr-reviewer review owner/repo#42
```

### 6. Start the daemon

```bash
# foreground (see logs in terminal)
pr-reviewer start

# background (writes PID file, stop with `pr-reviewer stop`)
pr-reviewer start --daemon

# check status
pr-reviewer status
```

The daemon polls GitHub for new/updated PRs and automatically reviews them. It uses ETag caching and adaptive backoff to stay well within GitHub's rate limits.

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

```
pr-reviewer <COMMAND> [OPTIONS]
```

### Global help
```bash
pr-reviewer --help
pr-reviewer <command> --help
```

### `init`
```bash
pr-reviewer init
```
Creates config and SQLite state in `~/.config/pr-reviewer/`.

### `add`
```bash
pr-reviewer add <owner/repo> --path <local_path> \
  [--harness claude-code|opencode|codex] \
  [--model <model>] \
  [--fork-policy ignore|limited|full]

pr-reviewer add --scan <directory>
pr-reviewer add --org <org>   # currently not implemented
```

### `remove`
```bash
pr-reviewer remove <owner/repo>
```

### `list`
```bash
pr-reviewer list
```

### `index`
```bash
pr-reviewer index <owner/repo>
pr-reviewer index --all
```
Runs `gitnexus analyze` for the target repo(s).

### `review`
```bash
pr-reviewer review <owner/repo>#<pr_number> \
  [--dry-run] \
  [--harness claude-code|opencode|codex] \
  [--model <model>]
```

### `start`
```bash
pr-reviewer start
pr-reviewer start --daemon
```
Starts polling loop in foreground or background.

### `stop`
```bash
pr-reviewer stop
```

### `status`
```bash
pr-reviewer status
```
Shows daemon/process state and recent polling metadata.

### `logs`
```bash
pr-reviewer logs \
  [--repo <owner/repo>] \
  [--since <datetime>] \
  [--model <model>] \
  [--harness <harness>] \
  [--limit <n>]
```

### `stats`
```bash
pr-reviewer stats [--repo <owner/repo>] [--since <datetime>]
```

### `config`
```bash
pr-reviewer config list
pr-reviewer config get <key>
pr-reviewer config set <key> <value>
```

Examples:
```bash
pr-reviewer config set harness.default codex
pr-reviewer config set daemon.poll_interval_secs 60
pr-reviewer config get github.token
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

If [GitNexus](https://github.com/abhigyanpatwari/GitNexus) is installed and a repo has been indexed (`.gitnexus/` directory exists), pr-reviewer enriches review context with:

- Impact analysis (blast radius of changed symbols)
- Call chain tracing (upstream/downstream dependencies)
- Community context (which functional area of the codebase is affected)

To index a repo: `pr-reviewer index owner/repo` or `gitnexus analyze /path/to/repo`.

## License

[MIT](LICENSE)
