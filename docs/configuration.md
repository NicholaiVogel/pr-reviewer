# Configuration reference

All configuration lives at `~/.config/pr-reviewer/config.toml`. Every field can be inspected and set via CLI:

```bash
pr-reviewer config list
pr-reviewer config get harness.default
pr-reviewer config set daemon.poll_interval_secs 60
```

---

## Config precedence

Settings are resolved in this order, from highest to lowest priority:

```
per-repo config field
    > global config field
        > hardcoded default
```

For example, if a repo sets `harness = "codex"` but the global `[harness]` section sets `default = "claude-code"`, that repo uses Codex. A repo with no `harness` field falls back to the global default.

---

## `[harness]`

Controls which AI CLI tool runs reviews and how long to wait for a response.

```toml
[harness]
default = "claude-code"   # claude-code | opencode | codex
model = "claude-sonnet-4-6"
reasoning_effort = "low"  # optional, currently used by codex
timeout_secs = 600
```

| Field | Default | Description |
|-------|---------|-------------|
| `default` | `claude-code` | Harness to use when no per-repo override is set |
| `model` | `claude-sonnet-4-6` | Model identifier passed to the harness CLI |
| `reasoning_effort` | unset | Optional reasoning effort override. When using Codex, pr-reviewer passes this through as `-c model_reasoning_effort="..."` |
| `timeout_secs` | `600` | Seconds to wait for a harness response before treating the review as failed |

### Harness selection guide

| Harness | CLI | Best for |
|---------|-----|----------|
| `claude-code` | `claude` | Highest review quality; requires Claude Pro/Max subscription |
| `opencode` | `opencode` | Open-source alternative; supports multiple providers via config |
| `codex` | `codex` | OpenAI Codex; fastest option for high-volume repos |

Model names are passed directly to the harness CLI. Use whatever model identifier that CLI accepts — pr-reviewer does no validation.

For Codex, `reasoning_effort` can be `none`, `low`, `medium`, `high`, or `xhigh`. Leave it unset to use your normal Codex defaults.

---

## `[daemon]`

Controls polling behavior and concurrency.

```toml
[daemon]
poll_interval_secs = 30
max_poll_interval_secs = 300
mode = "poll"
webhook_port = 3847
max_concurrent_reviews = 2
```

| Field | Default | Description |
|-------|---------|-------------|
| `poll_interval_secs` | `30` | Base polling interval in seconds. Adaptive backoff starts here. |
| `max_poll_interval_secs` | `300` | Maximum backoff ceiling. The daemon will never wait longer than this between polls. |
| `mode` | `poll` | `poll` is the only production-ready mode; `webhook` is not yet complete |
| `webhook_port` | `3847` | Port for webhook mode (reserved for future use) |
| `max_concurrent_reviews` | `2` | Maximum number of harness processes running in parallel |

### Adaptive backoff

The daemon starts polling at `poll_interval_secs` and backs off exponentially (with jitter) when:

- All repos return `304 Not Modified` (nothing changed — back off)
- The GitHub rate limit is nearly exhausted

It resets to the base interval when a new PR is found. The ceiling is `max_poll_interval_secs`.

For repos with low PR volume, you can set both values higher to reduce unnecessary API calls:

```bash
pr-reviewer config set daemon.poll_interval_secs 120
pr-reviewer config set daemon.max_poll_interval_secs 600
```

### Concurrency

`max_concurrent_reviews` controls how many harness processes can run at the same time. Each harness invocation is CPU and memory intensive. On a machine with limited resources or a Claude Pro subscription (which has its own concurrency limits), keep this at `1` or `2`.

---

## `[defaults]`

Review behavior defaults applied to all repos unless overridden.

```toml
[defaults]
auto_review = true
review_drafts = false
max_files = 50
max_diff_lines = 3000
max_file_size_bytes = 100000
bot_name = "pr-reviewer"
dry_run = false
max_prompt_bytes = 204800
```

| Field | Default | Description |
|-------|---------|-------------|
| `auto_review` | `true` | Whether the daemon auto-reviews new PRs. Set to `false` to only allow manual `review` commands. |
| `review_drafts` | `false` | Whether to review draft PRs. |
| `max_files` | `50` | Maximum number of changed files to include in context. PRs with more files are truncated. |
| `max_diff_lines` | `3000` | Maximum diff lines to include. Large diffs are truncated at this limit. |
| `max_file_size_bytes` | `100000` | Maximum size of a single file to include as context. Larger files are skipped. |
| `bot_name` | `pr-reviewer` | GitHub username of the bot account. Used for self-review detection and deduplication. |
| `dry_run` | `false` | If `true`, run the full pipeline but log the output instead of posting to GitHub. |
| `max_prompt_bytes` | `204800` | Maximum prompt size sent to the harness (200KB). Context is trimmed to fit. |

### Tuning context limits

Smaller limits = faster reviews, lower cost, less risk of hitting model context windows.
Larger limits = more complete reviews for large PRs, but slower and potentially truncated by the model.

For repos with typically small PRs (e.g., docs, config changes):

```bash
pr-reviewer config set defaults.max_diff_lines 1000
pr-reviewer config set defaults.max_files 20
```

For repos with large, complex PRs:

```bash
pr-reviewer config set defaults.max_diff_lines 5000
pr-reviewer config set defaults.max_prompt_bytes 307200   # 300KB
```

---

## `[[repos]]`

Per-repo configuration. Each `[[repos]]` block in the TOML array adds one repo to the watch list. All fields except `owner` and `name` are optional and override the global defaults.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
local_path = "/optional/existing/clone"
harness = "claude-code"
model = "claude-sonnet-4-6"
fork_policy = "ignore"
trusted_authors = ["dependabot[bot]", "renovate[bot]"]
ignore_paths = ["*.lock", "dist/**", "vendor/**"]
custom_instructions = "This project uses a custom ORM. Watch for N+1 queries."
gitnexus = true

  [repos.issue_triage]
  enabled = true
  create_missing_labels = false
  max_context_bytes = 65536
  max_labels_to_create = 3
  allowed_new_label_prefixes = ["bug", "documentation", "enhancement", "question", "spec", "spec:", "priority", "priority: ", "area", "area:", "bucket", "bucket:"]
  max_new_label_name_chars = 50
  max_new_label_description_chars = 256
  instructions = "Prefer existing area:* labels."
```

| Field | Description |
|-------|-------------|
| `owner` | GitHub organization or username |
| `name` | Repository name |
| `local_path` | Path to an existing local clone. If omitted, the repo is auto-cloned to `~/.config/pr-reviewer/repos/{owner}/{name}/` |
| `harness` | Override the global harness for this repo |
| `model` | Override the global model for this repo |
| `fork_policy` | How to handle PRs from forks (see below) |
| `trusted_authors` | Authors whose fork PRs get full context regardless of `fork_policy` |
| `ignore_paths` | Glob patterns for files to exclude from context |
| `custom_instructions` | Free-text hints appended to the review prompt |
| `gitnexus` | Whether to include GitNexus impact analysis in context. Default: `true` (best-effort; gracefully skipped if unavailable) |
| `issue_triage.enabled` | Enable automatic issue sorting/labeling for newly opened issues in this repo |
| `issue_triage.create_missing_labels` | Allow pr-reviewer to create missing labels when the repo has no equivalent taxonomy. Default: `false` |
| `issue_triage.max_labels_to_create` | Hard cap on how many new labels can be proposed per triage run. Default: `3` |
| `issue_triage.allowed_new_label_prefixes` | Allowed prefixes for newly created labels. If empty, no new labels are allowed. |
| `issue_triage.max_new_label_name_chars` | Max new label name length in characters. Default: `50` |
| `issue_triage.max_new_label_description_chars` | Max new label description length in characters. Default: `256` |
| `issue_triage.max_context_bytes` | Max repo instruction/spec context loaded for issue triage. Default: `65536` |
| `issue_triage.instructions` | Extra issue-triage-specific guidance, separate from PR review instructions |

When enabled, pr-reviewer enriches context with:
- ranked GitNexus processes related to changed files
- top changed symbols with caller/callee context
- best-effort upstream impact snapshots (short timeout, non-blocking fallback)

### Auto-managed clones vs local_path

When `local_path` is omitted, pr-reviewer clones the repo to `~/.config/pr-reviewer/repos/{owner}/{name}/` and manages it automatically:

- Git hooks are disabled (`core.hooksPath=/dev/null`)
- Auth is passed via `http.extraHeader`, never stored in the remote URL
- `fetch_latest` runs before each review to keep the index fresh

Use `local_path` only if you already have a clone you want to reuse. Note that the review context is assembled from GitHub API responses, not the local clone — the clone is only used for GitNexus indexing.

### Issue triage

When `issue_triage.enabled = true`, the daemon also watches the repo's open issues feed. Newly seen issues are triaged once: pr-reviewer loads repo instructions/spec files from the local clone, fetches the repo's existing label catalog, asks the configured harness to classify the issue, then applies the proposed labels.

By default, pr-reviewer only applies labels that already exist in the repository. Set `issue_triage.create_missing_labels = true` if you want it to create missing labels as part of the triage pass.

On first run for a repo, pr-reviewer remembers the highest currently-open issue number and skips triaging those legacy issues. On subsequent polls, it only triages issues with a higher number than the stored high-water mark, then advances the mark forward.

Manual triage command:

```bash
pr-reviewer triage owner/repo#123 --dry-run
pr-reviewer triage owner/repo#123
```

---

## Fork policy

Controls what happens when pr-reviewer encounters a PR from a fork.

| Policy | Behavior |
|--------|----------|
| `ignore` | Skip fork PRs entirely. No review is posted. (default) |
| `limited` | Review with diff only. File contents are not included to avoid executing untrusted code paths in context assembly. |
| `full` | Full context, same as a non-fork PR. Use only for forks you trust (e.g., internal org forks). |

`trusted_authors` overrides the policy for specific GitHub usernames. A trusted author's PR always gets full context regardless of `fork_policy`:

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
fork_policy = "limited"
trusted_authors = ["dependabot[bot]", "known-external-contributor"]
```

---

## Confidence scoring

The review output includes 13 confidence dimensions, each rated 1–10 by the model:

| Dimension | Description |
|-----------|-------------|
| `style_maintainability` | Readability and future maintainability |
| `repo_convention_adherence` | Follows established project patterns |
| `merge_conflict_detection` | Risk of conflicts with concurrent changes |
| `security_vulnerability_detection` | Known vulnerability patterns |
| `injection_risk_detection` | SQL, command, and template injection |
| `attack_surface_risk_assessment` | New attack vectors introduced |
| `future_hardening_guidance` | Opportunities for future hardening |
| `scope_alignment` | Changes match stated PR intent |
| `duplication_awareness` | Reuse of existing utilities vs reinvention |
| `tooling_pattern_leverage` | Correct use of established tooling |
| `functional_completeness` | All cases handled, no obvious gaps |
| `pattern_correctness` | Correct application of design patterns |
| `documentation_coverage` | Docs match implementation |

These are averaged to a global confidence score. **If the average falls below 5, an `APPROVE` verdict is automatically downgraded to `COMMENT`.** The review still posts with its full analysis — the downgrade only affects the verdict type.

This is not configurable per-repo today. If a repo routinely triggers low-confidence reviews (e.g., highly domain-specific code), use `custom_instructions` to give the model more context about the domain.

---

## `ignore_paths`

Glob patterns for files to exclude from diff context. Matched files are dropped before the prompt is assembled. Use this to avoid spending context budget on generated or vendored files.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
ignore_paths = [
  "*.lock",           # lockfiles
  "dist/**",          # build output
  "vendor/**",        # vendored dependencies
  "*.min.js",         # minified assets
  "**/__generated__/**",  # codegen output
]
```

Patterns use glob syntax (same as `.gitignore` glob rules).

---

## `custom_instructions`

Free-text instructions appended to the review prompt for a specific repo. Use this to give the model domain context it can't infer from the code alone.

```toml
[[repos]]
owner = "your-org"
name = "payments-service"
custom_instructions = """
This service handles PCI-scoped payment processing. Any code touching card data or transaction
records should be reviewed against our PCI-DSS controls. Flag any logging of cardholder data
as a critical finding.
"""
```

**Security note:** `custom_instructions` is included verbatim in the prompt. It is not a security boundary — a malicious PR could attempt to override or contradict these instructions via prompt injection in code comments or commit messages. The harness runs in a sandboxed environment, but the model itself is not immune to prompt injection in untrusted input.

---

## Multi-step workflows

A repo can define an ordered sequence of review steps, each with its own harness, model, and conditions. Steps run in the order defined; a failed step does not block subsequent steps.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"

[[repos.workflow]]
id = "security-scan"
name = "Security review"
harness = "claude-code"
model = "claude-opus-4-6"
custom_instructions = "Focus exclusively on security vulnerabilities."
enabled = true
[repos.workflow.conditions]
file_patterns = ["src/**", "api/**"]
min_diff_lines = 5
skip_drafts = true
label_patterns = []

[[repos.workflow]]
id = "general-review"
name = "General code review"
harness = "claude-code"
model = "claude-sonnet-4-6"
enabled = true
[repos.workflow.conditions]
min_diff_lines = 10
skip_drafts = false
```

### Workflow conditions

| Field | Description |
|-------|-------------|
| `file_patterns` | Glob patterns — step runs only if at least one changed file matches. Empty list means always run. |
| `min_diff_lines` | Minimum number of changed lines required to trigger this step. |
| `skip_drafts` | If `true`, skip this step for draft PRs. |
| `label_patterns` | PR must have at least one label matching a pattern. Empty list means always run. |

When no workflow steps are defined, pr-reviewer runs a single default review using the repo's harness and model settings.
