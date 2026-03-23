# Deployment

How to run pr-reviewer reliably in production — as a systemd service, with secure token handling, proper logging, and a strategy for backups and upgrades.

---

## Token setup for production

For any long-running setup, use `--passphrase` when storing your token. This adds an Argon2id-derived key as the inner encryption layer, so even if your keyfile is compromised, the token is still protected.

```bash
pr-reviewer config set-token --passphrase
# Prompted: enter GitHub token, then passphrase
```

Without `--passphrase`, the inner layer uses `/etc/machine-id` + username — both world-readable, so the keyfile at `~/.config/pr-reviewer/keyfile` is your only real security boundary.

When the daemon starts with a passphrase-protected token, it reads the passphrase from the `PR_REVIEWER_PASSPHRASE` environment variable. If this variable is absent, the daemon will error on startup rather than silently fail to decrypt.

---

## Running as a systemd service

### 1. Create the unit file

```ini
# /etc/systemd/system/pr-reviewer.service
[Unit]
Description=pr-reviewer daemon
After=network.target

[Service]
Type=simple
User=nicholai
EnvironmentFile=/etc/pr-reviewer/secrets
ExecStart=/usr/local/bin/pr-reviewer start
Restart=on-failure
RestartSec=10s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

Replace `User=nicholai` with the user that has the pr-reviewer config in `~/.config/pr-reviewer/`.

### 2. Create the secrets file

```bash
sudo mkdir -p /etc/pr-reviewer
sudo tee /etc/pr-reviewer/secrets > /dev/null <<'EOF'
PR_REVIEWER_PASSPHRASE=your-passphrase-here
RUST_LOG=info
EOF
sudo chmod 600 /etc/pr-reviewer/secrets
sudo chown root:root /etc/pr-reviewer/secrets
```

The secrets file must be `0600`. If the service user needs to read it, adjust ownership accordingly:

```bash
sudo chown root:nicholai /etc/pr-reviewer/secrets
```

### 3. Enable and start

```bash
sudo systemctl daemon-reload
sudo systemctl enable pr-reviewer
sudo systemctl start pr-reviewer
sudo systemctl status pr-reviewer
```

---

## Environment variable reference

| Variable | Description |
|----------|-------------|
| `PR_REVIEWER_PASSPHRASE` | Passphrase for decrypting tokens stored with `--passphrase`. Required if token was encrypted with a passphrase. |
| `GITHUB_TOKEN` | Fallback token source if no encrypted token is configured. Lowest priority in the resolution chain. |
| `RUST_LOG` | Log level filter. Values: `error`, `warn`, `info`, `debug`, `trace`. Default: `error`. Use `info` for production, `debug` for diagnosis. |
| `SIGNET_NO_HOOKS` | Set to `1` to disable Signet hook invocation. pr-reviewer sets this automatically in spawned harness environments. |

For fine-grained log filtering:

```bash
# Log only pr-reviewer's own output at debug, everything else at warn
RUST_LOG=warn,pr_reviewer=debug pr-reviewer start
```

---

## Log management

### Viewing logs with journald (systemd)

```bash
# Follow live
journalctl -u pr-reviewer -f

# Last 100 lines
journalctl -u pr-reviewer -n 100

# Since a specific time
journalctl -u pr-reviewer --since "2026-03-15 10:00:00"

# Filter by log level (requires RUST_LOG=info or higher)
journalctl -u pr-reviewer -g "WARN\|ERROR"
```

### Writing to a log file instead

If you're not using systemd, redirect output:

```bash
RUST_LOG=info pr-reviewer start >> /var/log/pr-reviewer.log 2>&1 &
```

Or use a tool like `tee` to capture both to file and terminal:

```bash
RUST_LOG=info pr-reviewer start 2>&1 | tee /var/log/pr-reviewer.log
```

### Log rotation

If writing to a file, set up logrotate:

```
# /etc/logrotate.d/pr-reviewer
/var/log/pr-reviewer.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    postrotate
        # pr-reviewer has no SIGHUP handler for log-file reopen, so a full
        # restart is required. This causes a brief service interruption (~1s)
        # on each rotation. --no-block returns immediately; systemd handles
        # the restart asynchronously.
        systemctl restart pr-reviewer --no-block || true
    endscript
}
```

---

## Monitoring health

```bash
# Basic health check
pr-reviewer status

# Review history for a specific repo
pr-reviewer logs --repo owner/repo --limit 20

# Usage statistics
pr-reviewer stats --repo owner/repo
pr-reviewer stats --since 2026-03-01
```

`pr-reviewer status` shows:

- Whether the daemon process is running
- Uptime, start time, and last heartbeat
- The current review queue depth and active worker count
- Watched repos from config
- The last known GitHub rate limit snapshot

Add `--json` when you want the same snapshot in a machine-readable form.

Things to watch for in logs:

- Repeated `WARN: rate limit` — consider increasing `poll_interval_secs`
- `ERROR: harness timeout` — review pipeline failing; check harness auth or increase `timeout_secs`
- `WARN: failed to post review, retrying as COMMENT` — GitHub position validation issue; harmless, review still posts
- `ERROR: token decryption failed` — passphrase mismatch or corrupted keyfile

---

## State backup and restore

The state that matters:

| File | Description |
|------|-------------|
| `~/.config/pr-reviewer/config.toml` | All configuration including repo list |
| `~/.config/pr-reviewer/state.db` | SQLite database: review log, PR state, daemon status |
| `~/.config/pr-reviewer/keyfile` | 32-byte AES key for token decryption. **Critical — without this, encrypted tokens cannot be decrypted** |

Back up all three together:

```bash
tar czf pr-reviewer-backup-$(date +%Y%m%d).tar.gz \
  -C "$HOME" \
  .config/pr-reviewer/config.toml \
  .config/pr-reviewer/state.db \
  .config/pr-reviewer/keyfile
```

Store the backup somewhere the passphrase is not also stored. If the keyfile is backed up and the backup is compromised, the token is only protected by the passphrase (if `--passphrase` was used).

### Restore

```bash
# Stop the daemon first
pr-reviewer stop

# Restore files
tar xzf pr-reviewer-backup-20260315.tar.gz -C "$HOME"

# Restart
pr-reviewer start --daemon
```

If restoring to a new machine, the keyfile must match what was used to encrypt the token. The keyfile is machine-specific — if you lose it, re-run `pr-reviewer config set-token` to re-encrypt with the new machine's keyfile.

---

## Upgrading

```bash
# Stop the daemon
pr-reviewer stop

# Pull latest and rebuild
cd /path/to/pr-reviewer
git pull
cargo install --path .

# Schema migrations run automatically on next startup
pr-reviewer start --daemon
```

The database schema is versioned with a `schema_version` table and migrations apply automatically on startup. There is no manual migration step.

If something goes wrong after an upgrade:

1. Check `RUST_LOG=debug pr-reviewer status` for startup errors
2. Roll back: `git checkout <previous-tag> && cargo install --path .`
3. The database format is backward-compatible across patch versions; major version upgrades may require a migration note in the release
