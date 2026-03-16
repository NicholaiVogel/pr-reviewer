<p align="center">
  <img src="public/logo.png" alt="pr-reviewer" width="280" />
</p>

<h1 align="center">pr-reviewer</h1>

<p align="center">
  Open-source, self-hosted PR review tool. Like Greptile or CodeRabbit, but runs on your machine.
</p>

---

Watches your GitHub repositories for pull requests, spawns a local AI coding CLI to review them, and posts structured comments back to GitHub automatically.

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
# encrypt and store (prompted for token + optional passphrase)
pr-reviewer config set-token --passphrase

# or pipe from stdin
echo "ghp_..." | pr-reviewer config set-token --stdin

# or store in Signet (if installed)
pr-reviewer config set-token --signet

# check what's configured
pr-reviewer config token-status
```

### 3. Add a repository

```bash
# auto-clones to ~/.config/pr-reviewer/repos/ (recommended)
pr-reviewer add owner/repo \
  --harness claude-code \
  --model claude-sonnet-4-6

# or point to an existing local clone
pr-reviewer add owner/repo --path /path/to/local/clone
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
# local_path is optional — omit to use auto-managed clone at ~/.config/pr-reviewer/repos/
# local_path = "/mnt/work/dev/openmarketui"
harness = "codex"
model = "gpt-5.3-codex"
fork_policy = "ignore"
ignore_paths = ["*.lock", "dist/**"]
custom_instructions = "This project uses a custom ORM. Watch for SQL injection."
gitnexus = true
```

## Documentation

- [Troubleshooting](docs/troubleshooting.md) — diagnosing common errors
- [Deployment](docs/deployment.md) — systemd, log management, production setup
- [Configuration reference](docs/configuration.md) — all config options explained

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
# auto-clones to managed directory
pr-reviewer add <owner/repo> \
  [--harness claude-code|opencode|codex] \
  [--model <model>] \
  [--fork-policy ignore|limited|full]

# or use an existing local clone
pr-reviewer add <owner/repo> --path <local_path> [...]

pr-reviewer add --scan <directory>
pr-reviewer add --org <org>   # currently not implemented
```

### `remove`
```bash
pr-reviewer remove <owner/repo>
pr-reviewer remove <owner/repo> --purge   # also deletes managed clone
```

### `cleanup`
```bash
pr-reviewer cleanup   # removes orphaned managed clones
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

# Token management
pr-reviewer config set-token [--passphrase] [--signet] [--stdin] [TOKEN]
pr-reviewer config remove-token
pr-reviewer config token-status
```

Examples:
```bash
pr-reviewer config set harness.default codex
pr-reviewer config set daemon.poll_interval_secs 60
pr-reviewer config set-token --passphrase
pr-reviewer config token-status
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
- **Managed repo clones**: repos are auto-cloned to `~/.config/pr-reviewer/repos/`, keeping your working repos isolated from review agents
- **Encrypted token storage**: GitHub tokens encrypted at rest with double-layer AES-256-GCM (machine-bound keyfile + passphrase/machine-derived key)
- **Environment scrubbing**: `HOME`, `SSH_AUTH_SOCK`, `GH_TOKEN`, `AWS_*`, `ANTHROPIC_API_KEY`, and other sensitive variables are stripped from the harness environment
- **Fork policy**: configurable per-repo (`ignore`, `limited`, `full`) with trusted author allowlists
- **Path validation**: canonicalization and containment checks reject traversal attacks and symlink escapes
- **Signet bypass**: `SIGNET_NO_HOOKS=1` prevents hook invocation in spawned processes
- **Optional Signet secrets**: tokens can be stored in Signet's encrypted secret store for unified credential management

## Token security

GitHub tokens are never stored in plain text by default. The `set-token` command encrypts tokens with two independent AES-256-GCM layers:

1. **Machine-bound key**: a random 32-byte key stored at `~/.config/pr-reviewer/keyfile` (permissions `0600`) -- this is the primary protection layer; anyone who can read this file can decrypt the token
2. **Passphrase or machine identity**: if `--passphrase` is used, the inner layer is derived via Argon2id from your passphrase, providing strong protection even if the keyfile is compromised. Without `--passphrase`, the inner layer uses `/etc/machine-id` + username (world-readable inputs, so the keyfile is your real security boundary)

Token resolution order:
1. Signet secret store (if Signet is installed and has the secret)
2. Encrypted config (`github.encrypted_token`)
3. Plain-text config (`github.token`) -- legacy, emits a warning
4. `GITHUB_TOKEN` environment variable

For daemon mode with passphrase-protected tokens, set `PR_REVIEWER_PASSPHRASE` as an environment variable.

Run `pr-reviewer config token-status` to see which source is active.

## GitNexus integration

If [GitNexus](https://github.com/abhigyanpatwari/GitNexus) is installed and a repo has been indexed (`.gitnexus/` directory exists), pr-reviewer enriches review context with:

- Impact analysis (blast radius of changed symbols)
- Call chain tracing (upstream/downstream dependencies)
- Community context (which functional area of the codebase is affected)

To index a repo: `pr-reviewer index owner/repo` or `gitnexus analyze /path/to/repo`.

## License

[MIT](LICENSE)
