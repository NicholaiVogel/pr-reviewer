# Troubleshooting

Common issues and how to fix them. Start with `RUST_LOG=debug pr-reviewer <command>` to get verbose output for any problem.

---

## Token errors

### "no GitHub token configured"

No token is available from any source. Set one:

```bash
pr-reviewer config set-token --passphrase
```

Or set the `GITHUB_TOKEN` environment variable as a fallback.

See `pr-reviewer config token-status` to confirm which source is active and what the resolution chain found.

### 401 Unauthorized from GitHub

The token is present but rejected. Likely causes:

- The token has expired (fine-grained PATs expire)
- The token is missing required scopes — needs `pull_requests:read+write` and `contents:read` (fine-grained), or `repo` (classic)
- The token was revoked

Generate a new PAT and re-run `pr-reviewer config set-token --passphrase`.

### "passphrase required but PR_REVIEWER_PASSPHRASE not set"

You encrypted the token with `--passphrase` and are running the daemon without the passphrase env var. Either:

```bash
# Set the env var before starting the daemon
export PR_REVIEWER_PASSPHRASE="your-passphrase"
pr-reviewer start --daemon
```

Or see [Deployment](deployment.md) for the systemd `EnvironmentFile=` pattern, which is the recommended approach for production.

### `set-token` fails silently or exits with error

- Make sure `~/.config/pr-reviewer/` exists (`pr-reviewer init` creates it)
- If using `--signet`, confirm Signet is installed and `signet secret set GITHUB_TOKEN` works independently
- If piping via `--stdin`, confirm the token has no trailing newline issues: `printf '%s' "$TOKEN" | pr-reviewer config set-token --stdin`

---

## GitHub rate limits

### "rate limit exceeded" or 403 responses from GitHub

The daemon automatically backs off when the rate limit is hit and waits until the reset window. Check the current state:

```bash
pr-reviewer status
```

This shows remaining API quota and the next reset time.

If you're hitting limits regularly:

- Increase `daemon.poll_interval_secs` in config (e.g., from 30 to 60)
- Reduce `daemon.max_concurrent_reviews` to serialize reviews instead of running them in parallel
- Reduce the number of watched repos

The daemon uses ETag caching on list endpoints — unchanged responses don't count against your quota. This is working correctly when you see `304 Not Modified` in debug logs.

---

## Harness not found

### "harness binary not found" or `No such file or directory`

The harness process spawns in a scrubbed environment, which strips `HOME`, `PATH` modifications from shell rc files, and version manager shims (`nvm`, `pyenv`, `rbenv`, etc.).

**The binary must be on the system PATH as installed by the package manager, not via a user-space version manager shim.**

```bash
# Confirm the binary is reachable without shell rc:
env -i PATH=/usr/local/bin:/usr/bin:/bin which claude
env -i PATH=/usr/local/bin:/usr/bin:/bin which opencode
env -i PATH=/usr/local/bin:/usr/bin:/bin which codex
```

If the binary is only accessible via a version manager:

1. Install the tool globally via `npm install -g` or `cargo install` to a system path
2. Or create a wrapper script at `/usr/local/bin/claude` that calls the version-manager-managed binary

### The harness runs but authentication fails

AI CLIs need to be authenticated separately. pr-reviewer strips `ANTHROPIC_API_KEY` and similar variables from the harness environment intentionally — the expectation is that Claude Code, OpenCode, or Codex are authenticated via their own login mechanisms (browser auth, CLI login), not environment variables.

Confirm the harness works outside of pr-reviewer:

```bash
echo "hello" | claude --dangerously-skip-permissions -p -
opencode run "hello"
codex exec "hello"
```

---

## Harness timeout or parse failure

### Review logged as failed with "timeout"

The default timeout is 600 seconds (10 minutes). Very large PRs or slow models can exceed this.

Increase it in config:

```bash
pr-reviewer config set harness.timeout_secs 900
```

Or per-repo via `config.toml`:

```toml
[[repos]]
owner = "your-org"
name = "large-repo"
# custom timeout not yet per-repo — use global harness.timeout_secs
```

### "failed to parse harness output" or review posted with no comments

The parser extracts JSON from the harness output using three strategies: marked code block → brace-match fallback → raw fallback. Parse failures usually mean the model returned malformed output.

Debug by running a dry-run and inspecting the raw output:

```bash
RUST_LOG=debug pr-reviewer review owner/repo#42 --dry-run 2>&1 | less
```

Look for `raw harness output` in the debug log. Common causes:

- Model hit a context limit and truncated mid-JSON
- Model added a preamble before the JSON block (the brace-match fallback handles this, but deeply nested truncation can still fail)
- `max_prompt_bytes` set too high, causing the model to time out before completing

Reduce context if needed:

```bash
pr-reviewer config set defaults.max_diff_lines 1500
pr-reviewer config set defaults.max_files 25
```

---

## Review not posting

### Review runs but nothing appears on GitHub

First check if it's a dry-run:

```bash
pr-reviewer config get defaults.dry_run
```

If `true`, set it back to `false`:

```bash
pr-reviewer config set defaults.dry_run false
```

### "skipping: PR is a draft"

Draft PRs are skipped by default. To review drafts:

```bash
pr-reviewer config set defaults.review_drafts true
```

Or per-repo in `config.toml`:

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
# add a workflow step with skip_drafts = false, or set globally:
```

### "skipping: fork PR, policy is ignore"

Fork PRs are skipped when `fork_policy = "ignore"` (the default). Options:

- `limited` — reviews the diff but does not include full file contents (safer for untrusted forks)
- `full` — full context, treat like a non-fork PR

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
fork_policy = "limited"
trusted_authors = ["known-contributor"]   # these get full context regardless
```

### Self-review: review posts as COMMENT instead of APPROVE/REQUEST_CHANGES

GitHub does not allow you to approve or request changes on your own PRs. If the authenticated user is the PR author, the verdict is automatically downgraded to `COMMENT` with inline feedback folded into the body. This is expected behavior, not a bug.

### 422 error: "pull request review thread not pullable"

GitHub rejected inline comment positions (happens when position mapping goes wrong on very old or re-based PRs). pr-reviewer automatically retries these as a `COMMENT` review with inline comments collapsed into the body. Check logs to confirm the retry succeeded.

---

## Duplicate reviews

### Two identical reviews posted on the same PR

The DB dedupe key (`sha256(repo + pr + sha + harness)`) prevents duplicates within a single install. If you wiped the database, the GitHub-side check (querying existing reviews for bot name + SHA match) acts as the second line of defense.

If duplicates are appearing:

1. Confirm you only have one instance of pr-reviewer running: `pr-reviewer status`
2. Confirm the `bot_name` in config matches what was used when the prior review was posted

### Review stuck in "in-progress" state after a crash

Stale claims older than `harness.timeout_secs + 30s` are automatically swept on daemon startup. If you need to clear them immediately:

```bash
# Not yet a CLI command — clear via sqlite3 directly

# Check how many stale claims exist first
sqlite3 ~/.config/pr-reviewer/state.db \
  "SELECT COUNT(*) FROM pr_state WHERE status = 'in_progress';"

# Clear them
sqlite3 ~/.config/pr-reviewer/state.db \
  "UPDATE pr_state SET status = 'pending' WHERE status = 'in_progress';"

# Verify the update took effect (should return 0)
sqlite3 ~/.config/pr-reviewer/state.db \
  "SELECT COUNT(*) FROM pr_state WHERE status = 'in_progress';"

pr-reviewer start
```

---

## Daemon issues

### Daemon won't start: "address already in use" or PID file exists

Check if the daemon is actually running:

```bash
pr-reviewer status
```

If it reports no daemon, the PID file is stale. Remove it:

```bash
rm ~/.config/pr-reviewer/daemon.pid
pr-reviewer start --daemon
```

### "database is locked"

Only one pr-reviewer process should access the state database at a time. If you see this:

1. Confirm no other pr-reviewer process is running: `pgrep -a pr-reviewer`
2. Kill any zombie processes: `pkill pr-reviewer`
3. The WAL mode journal files (`.db-wal`, `.db-shm`) left behind by a crashed process are safe to delete if no pr-reviewer process is running

### Daemon starts but immediately stops

Check the logs:

```bash
# If using systemd
journalctl -u pr-reviewer -n 50

# If started manually with --daemon
RUST_LOG=info pr-reviewer start   # run foreground first to see startup errors
```

Common causes: missing token, config parse error, binary not found.

---

## GitNexus issues

### GitNexus warnings in logs but reviews still work

GitNexus is fully optional. If it's not installed or the repo isn't indexed, pr-reviewer falls back to diff + file contents only — reviews still run. The warning is informational.

### "gitnexus: command not found"

Install it:

```bash
npm install -g gitnexus
# or use npx without installing:
# pr-reviewer uses `npx gitnexus query` as fallback
```

### GitNexus output is empty or stale

The index needs to be built for each repo before it provides useful context. Re-index:

```bash
pr-reviewer index owner/repo
# or directly:
npx gitnexus analyze /path/to/repo
```

The `.gitnexus/` directory in the repo root indicates it's been indexed. If the directory is missing, the index has never been built.

### GitNexus output appears on stderr, not stdout

This is expected — KuzuDB (GitNexus's graph database) captures stdout at the OS level, so GitNexus routes its output through stderr. pr-reviewer checks both streams, preferring stderr. You don't need to do anything; this is handled internally.
