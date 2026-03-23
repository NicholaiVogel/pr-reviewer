# pr-reviewer

Self-hosted PR review daemon in Rust. Spawns existing AI CLI tools (claude-code, opencode, codex) to review GitHub PRs — no API keys, uses your existing Pro/Max subscription through the CLI.

## Architecture

```
src/
├── main.rs              # CLI entry (clap) — init, add, remove, cleanup, review, start, stop, status, logs, stats, config
├── daemon.rs            # Polling loop with ETag caching, adaptive backoff, jitter, rate budgeting
├── config.rs            # TOML config (serde) at ~/.config/pr-reviewer/config.toml, CLI config commands
├── repo_manager.rs      # Managed clone lifecycle: clone, fetch, purge, cleanup under ~/.config/pr-reviewer/repos/
├── safety.rs            # Path canonicalization, symlink rejection, fork policy evaluation
├── token/
│   ├── mod.rs           # Token resolution chain: signet → encrypted → plain text → env var
│   ├── crypto.rs        # Double-layer AES-256-GCM encryption (keyfile + passphrase/machine-derived)
│   └── signet.rs        # Optional Signet CLI integration for secret storage
├── github/
│   ├── client.rs        # reqwest GitHub REST API client with rate-limit tracking (Arc<Mutex<RateState>>)
│   ├── types.rs         # GitHub API response/request types (serde)
│   ├── comments.rs      # Review posting, position validation, thread replies (thin wrappers around client)
│   └── pr.rs            # PR listing, diff fetching (thin wrappers around client)
├── context/
│   ├── diff_parser.rs   # Unified diff → Vec<FileDiff> with line→position mapping for GitHub API
│   ├── file_reader.rs   # Safe file reading with path validation and containment checks
│   ├── retriever.rs     # Assembles prompt context: diff + file contents + GitNexus + prior reviews
│   └── gitnexus.rs      # Spawns `gitnexus query` CLI for impact analysis (reads stderr — see note below)
├── harness/
│   ├── mod.rs           # Harness trait: build_command(), uses_stdin()
│   ├── spawn.rs         # Sandboxed process spawning with env scrubbing, timeout, output capture
│   ├── claude_code.rs   # claude --model <m> --dangerously-skip-permissions -p - (stdin piped)
│   ├── opencode.rs      # opencode run --model <m> --format json "<prompt>"
│   └── codex.rs         # codex exec --model <m> --skip-git-repo-check --json "<prompt>"
├── review/
│   ├── engine.rs        # Pipeline orchestration: claim → context → harness → parse → post → complete
│   ├── prompt.rs        # Prompt construction with schema, guidelines, and repo custom instructions
│   └── parser.rs        # JSON extraction (marked block → brace-match → raw fallback), schema validation
└── store/
    └── db.rs            # SQLite (rusqlite, WAL mode) — pr_state, review_log, daemon_status, migrations
```

## Key Patterns

### Review Pipeline (engine.rs)

The core flow for `review_existing_pr`:
1. Evaluate fork policy → skip if denied
2. Build dedupe key (`sha256(repo + pr + sha + harness)`) → `claim_review` (atomic INSERT, unique constraint)
3. Check existing GitHub reviews for this SHA → post in-progress comment only if not already reviewed
4. `run_review_pipeline`: fetch diff + file contents + GitNexus context + prior reviews → build prompt → spawn harness → parse output → validate comment positions → post review
5. On 422 (self-review), retry as COMMENT with inline comments folded into body
6. `complete_review` in DB with actual posted comment count

### Harness Spawning (harness/spawn.rs)

All harnesses run in a sandboxed environment:
- Working directory: tmpdir (NOT the repo root)
- `SIGNET_NO_HOOKS=1` — prevents Signet hook invocation
- Stripped env vars: `HOME`, `SSH_AUTH_SOCK`, `GH_TOKEN`, `AWS_*`, `ANTHROPIC_API_KEY`, `CLAUDECODE`
- `kill_on_drop(true)` on child process to prevent orphans on timeout
- claude-code uses stdin piping (`-p -`) because prompts exceed Linux `MAX_ARG_STRLEN` (~128KB)

### Token Resolution (token/mod.rs)

GitHub tokens are resolved in priority order: Signet secret → encrypted config → plain-text config (warns) → `GITHUB_TOKEN` env var. Encrypted tokens use double-layer AES-256-GCM: outer layer with a machine-bound keyfile at `~/.config/pr-reviewer/keyfile` (primary security boundary, `0600` permissions), inner layer with either an Argon2id-derived passphrase key (strong, recommended) or a machine-identity-derived key (weak — uses world-readable `/etc/machine-id` + username, so keyfile is the real protection). For daemon mode with passphrase-protected tokens, set `PR_REVIEWER_PASSPHRASE` env var (required, not optional — will error if missing).

### Managed Repo Clones (repo_manager.rs)

When `local_path` is omitted from a repo config, the repo is auto-cloned to `~/.config/pr-reviewer/repos/{owner}/{name}/` via `git clone --single-branch`. Auth uses `http.extraHeader` (token never stored in `.git/config`). Git hooks are disabled via `core.hooksPath=/dev/null`. `fetch_latest()` is called before each review to keep the GitNexus index fresh.

### GitNexus Integration (context/gitnexus.rs)

GitNexus outputs to **stderr** because KuzuDB captures stdout at OS level. The code checks both streams (preferring stderr) so it won't silently break if this behavior changes. Falls back to `None` if gitnexus CLI isn't installed or the repo isn't indexed.

### Confidence (parser.rs)

Simple `Confidence { level: ConfidenceLevel, justification: String }` where level is High/Medium/Low. Displayed as a single line in the review body: `**Confidence:** {level} - {justification}`. No numeric scoring or dimensional breakdown.

### Idempotency

- DB claim lock prevents concurrent duplicate reviews (unique constraint on dedupe_key)
- GitHub-side check prevents re-posting if DB is wiped (queries existing reviews for bot + SHA match)
- Stale claims swept on daemon startup (older than harness timeout + 30s)

### Self-Review Detection (engine.rs)

Cached `authenticated_user` at engine init. Falls back to live API call if cache is empty. **Fail-closed**: if auth lookup fails entirely, verdict downgrades to COMMENT (logged as warning). This prevents APPROVE/REQUEST_CHANGES on your own PRs (GitHub rejects these with 422).

## Conventions

- **Error handling**: `anyhow::Result` throughout, `context()` on all fallible operations
- **Async**: tokio with `spawn_blocking` for SQLite operations
- **Logging**: `tracing` crate (`tracing::info!`, `tracing::warn!`, `tracing::error!`)
- **Config resolution**: per-repo field > global config field > hardcoded default (see `resolved_harness()`, `resolved_model()`)
- **GitHub API**: all requests go through `GitHubClient::request()` which sets auth, user-agent, API version headers and tracks rate limits automatically
- **Pagination**: paginated endpoints cap at 10 pages (1000 items) with `Link` header `rel="next"` detection

## Safety Rules

- Never checkout the PR branch locally — context is assembled from GitHub API responses
- All file paths canonicalized and checked for containment within repo root
- Symlinks resolved and rejected if target is outside repo
- Fork PRs handled per-repo policy: `ignore` (default), `limited` (diff only), `full` (trusted orgs)
- Environment scrubbed before spawning any harness process
- GitHub tokens encrypted at rest with double-layer AES-256-GCM; never stored in plain text by default
- Managed repo clones use `core.hooksPath=/dev/null` and strip auth from stored remote URLs
- Token never embedded in git clone URLs stored in `.git/config`

## State

- Config: `~/.config/pr-reviewer/config.toml`
- Database: `~/.config/pr-reviewer/state.db` (SQLite, WAL mode)
- PID file: `~/.config/pr-reviewer/daemon.pid`
- Keyfile: `~/.config/pr-reviewer/keyfile` (32-byte AES key, permissions `0600`)
- Managed repos: `~/.config/pr-reviewer/repos/{owner}/{name}/`
- Tables: `pr_state`, `review_log`, `daemon_status`, `repo_etags`, `schema_version`

## User-facing documentation (`docs/`)

- [docs/troubleshooting.md](docs/troubleshooting.md) — token errors, rate limits, harness failures, daemon issues, GitNexus problems
- [docs/deployment.md](docs/deployment.md) — systemd unit file, passphrase env var handling, log management, backup/restore, upgrades
- [docs/configuration.md](docs/configuration.md) — full config reference with explanations, fork policy, confidence scoring, multi-step workflows

## Testing

```bash
cargo test                                    # unit tests (parser, diff_parser, safety, db dedupe)
cargo check                                   # type checking
pr-reviewer review owner/repo#N --dry-run     # e2e dry run (no GitHub posting)
pr-reviewer review owner/repo#N               # e2e live review
```

## Common Tasks

```bash
# One-shot review
pr-reviewer review owner/repo#42 --dry-run

# Start daemon (foreground for debugging)
RUST_LOG=info pr-reviewer start

# Check daemon health
pr-reviewer status

# View review history
pr-reviewer logs --repo owner/repo --limit 10

# Re-index a repo for GitNexus
pr-reviewer index owner/repo
```

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **pr-reviewer** (2520 symbols, 6775 relationships, 195 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## When Debugging

1. `gitnexus_query({query: "<error or symptom>"})` — find execution flows related to the issue
2. `gitnexus_context({name: "<suspect function>"})` — see all callers, callees, and process participation
3. `READ gitnexus://repo/pr-reviewer/process/{processName}` — trace the full execution flow step by step
4. For regressions: `gitnexus_detect_changes({scope: "compare", base_ref: "main"})` — see what your branch changed

## When Refactoring

- **Renaming**: MUST use `gitnexus_rename({symbol_name: "old", new_name: "new", dry_run: true})` first. Review the preview — graph edits are safe, text_search edits need manual review. Then run with `dry_run: false`.
- **Extracting/Splitting**: MUST run `gitnexus_context({name: "target"})` to see all incoming/outgoing refs, then `gitnexus_impact({target: "target", direction: "upstream"})` to find all external callers before moving code.
- After any refactor: run `gitnexus_detect_changes({scope: "all"})` to verify only expected files changed.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Tools Quick Reference

| Tool | When to use | Command |
|------|-------------|---------|
| `query` | Find code by concept | `gitnexus_query({query: "auth validation"})` |
| `context` | 360-degree view of one symbol | `gitnexus_context({name: "validateUser"})` |
| `impact` | Blast radius before editing | `gitnexus_impact({target: "X", direction: "upstream"})` |
| `detect_changes` | Pre-commit scope check | `gitnexus_detect_changes({scope: "staged"})` |
| `rename` | Safe multi-file rename | `gitnexus_rename({symbol_name: "old", new_name: "new", dry_run: true})` |
| `cypher` | Custom graph queries | `gitnexus_cypher({query: "MATCH ..."})` |

## Impact Risk Levels

| Depth | Meaning | Action |
|-------|---------|--------|
| d=1 | WILL BREAK — direct callers/importers | MUST update these |
| d=2 | LIKELY AFFECTED — indirect deps | Should test |
| d=3 | MAY NEED TESTING — transitive | Test if critical path |

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/pr-reviewer/context` | Codebase overview, check index freshness |
| `gitnexus://repo/pr-reviewer/clusters` | All functional areas |
| `gitnexus://repo/pr-reviewer/processes` | All execution flows |
| `gitnexus://repo/pr-reviewer/process/{name}` | Step-by-step execution trace |

## Self-Check Before Finishing

Before completing any code modification task, verify:
1. `gitnexus_impact` was run for all modified symbols
2. No HIGH/CRITICAL risk warnings were ignored
3. `gitnexus_detect_changes()` confirms changes match expected scope
4. All d=1 (WILL BREAK) dependents were updated

## CLI

- Re-index: `npx gitnexus analyze`
- Check freshness: `npx gitnexus status`
- Generate docs: `npx gitnexus wiki`

<!-- gitnexus:end -->
