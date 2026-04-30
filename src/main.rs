mod auto_fix;
mod config;
mod context;
mod daemon;
mod github;
mod harness;
mod repo_manager;
mod review;
mod safety;
mod serve;
mod store;
mod token;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::{
    parse_repo_full_name, AppConfig, ForkPolicy, HarnessKind, ReasoningEffort, RepoConfig,
};
use github::client::GitHubClient;
use review::engine::{ReviewEngine, ReviewOptions};
use serde::Serialize;
use store::db::{format_stats, Database, LogsFilter, WorkQueueEntry};

#[derive(Parser, Debug)]
#[command(name = "pr-reviewer", version, about = "Self-hosted PR review daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Init,
    Add(AddArgs),
    Remove {
        repo: String,
        #[arg(long)]
        purge: bool,
    },
    Cleanup,
    List,
    Index {
        repo: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Start {
        #[arg(long)]
        daemon: bool,
    },
    Stop,
    Review {
        target: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        harness: Option<HarnessKind>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        reasoning_effort: Option<ReasoningEffort>,
        /// Bypass the dedupe check by clearing any stale or failed claim for this SHA.
        /// Has no effect on completed reviews — those entries are preserved.
        #[arg(long)]
        force: bool,
        /// Intentionally rerun this exact PR/SHA/harness mode even if it already completed.
        /// Also bypasses the GitHub already-posted guard for operator dogfooding.
        #[arg(long)]
        rerun_completed: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    Logs {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Stats {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Start the local workflow builder UI.
    Serve {
        /// Port to listen on (default: 3848).
        #[arg(long, default_value_t = 3848)]
        port: u16,
    },
}

#[derive(Args, Debug)]
struct AddArgs {
    repo: Option<String>,
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long)]
    harness: Option<HarnessKind>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    reasoning_effort: Option<ReasoningEffort>,
    #[arg(long, default_value = "ignore")]
    fork_policy: String,
    #[arg(long)]
    org: Option<String>,
    #[arg(long)]
    scan: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Set {
        key: String,
        value: String,
    },
    Get {
        key: String,
    },
    List,
    /// Encrypt and store a GitHub token
    SetToken {
        /// Read token from stdin instead of prompting
        #[arg(long)]
        stdin: bool,
        /// Protect with a passphrase (prompted interactively)
        #[arg(long)]
        passphrase: bool,
        /// Store in Signet secret store instead of config file
        #[arg(long)]
        signet: bool,
    },
    /// Remove stored GitHub token
    RemoveToken,
    /// Show which token source is active
    TokenStatus,
}

#[derive(Subcommand, Debug)]
enum QueueCommand {
    List {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    Retry {
        id: i64,
    },
    Cancel {
        id: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            let cfg = AppConfig::init()?;
            let db = Database::new(AppConfig::db_file()?).await?;
            db.migrate().await?;
            write_config_example(&cfg)?;
            println!(
                "initialized config at {}",
                AppConfig::config_file()?.display()
            );
            println!("initialized db at {}", db.path().display());
        }
        Commands::Add(args) => {
            let mut cfg = AppConfig::load_or_default()?;
            if let Some(org) = args.org {
                return Err(anyhow!(
                    "org discovery is not implemented yet; use `pr-reviewer add owner/repo --path <local>` for now (requested org: {org})"
                ));
            }

            if let Some(scan_dir) = args.scan {
                add_scanned_repos(&mut cfg, &scan_dir)?;
                cfg.save()?;
                println!("scan completed and config updated");
                return Ok(());
            }

            let repo = args
                .repo
                .ok_or_else(|| anyhow!("repo argument required (owner/repo)"))?;

            let (owner, name) = parse_repo_full_name(&repo)?;

            // If --path is provided, validate it exists. Otherwise, auto-clone.
            let local_path = if let Some(ref path) = args.path {
                config::ensure_repo_path_exists(path)?;
                Some(path.clone())
            } else {
                // Auto-clone: need a token to authenticate the clone
                let token_cfg = AppConfig::load_or_default()?;
                let (tok, _) = token::resolve_github_token(&token_cfg).await?;
                let clone_path = repo_manager::ensure_cloned(&owner, &name, &tok).await?;
                println!("cloned to {}", clone_path.display());
                None // managed path, not stored in config
            };

            let fork_policy = parse_fork_policy(&args.fork_policy)?;
            let repo_cfg = RepoConfig {
                owner,
                name,
                local_path,
                harness: args.harness,
                model: args.model,
                reasoning_effort: args.reasoning_effort,
                fork_policy,
                trusted_authors: vec![],
                ignore_paths: vec![
                    "*.lock".to_string(),
                    "vendor/**".to_string(),
                    "dist/**".to_string(),
                ],
                custom_instructions: None,
                gitnexus: true,
                workflow: vec![],
                auto_fix: Default::default(),
            };
            let repo_name = repo_cfg.full_name();
            cfg.add_repo(repo_cfg.clone())?;
            cfg.save()?;
            println!("added repo {repo_name}");

            if repo_cfg.gitnexus {
                if let Ok(effective_path) = repo_cfg.effective_local_path() {
                    match context::gitnexus::run_analyze(&effective_path).await {
                        Ok(_) => println!("gitnexus index built for {repo_name}"),
                        Err(err) => {
                            println!("warning: gitnexus analyze failed for {repo_name}: {err}")
                        }
                    }
                }
            }
        }
        Commands::Remove { repo, purge } => {
            let mut cfg = AppConfig::load_or_default()?;
            let was_managed = cfg.get_repo(&repo).map(|r| r.is_managed()).unwrap_or(false);
            if cfg.remove_repo(&repo) {
                cfg.save()?;
                println!("removed {repo}");
                if purge && was_managed {
                    let (owner, name) = parse_repo_full_name(&repo)?;
                    match repo_manager::purge(&owner, &name).await {
                        Ok(true) => println!("purged managed clone for {repo}"),
                        Ok(false) => println!("no managed clone found for {repo}"),
                        Err(err) => println!("warning: failed to purge clone: {err}"),
                    }
                }
            } else {
                println!("repo not found: {repo}");
            }
        }
        Commands::Cleanup => {
            let cfg = AppConfig::load_or_default()?;
            let removed = repo_manager::cleanup(&cfg.repos).await?;
            if removed.is_empty() {
                println!("no orphaned managed clones found");
            } else {
                for r in &removed {
                    println!("removed orphaned clone: {r}");
                }
                println!("cleaned up {} orphaned clone(s)", removed.len());
            }
        }
        Commands::List => {
            let cfg = AppConfig::load_or_default()?;
            if cfg.repos.is_empty() {
                println!("no repos configured");
            } else {
                for repo in &cfg.repos {
                    let harness = repo.resolved_harness(&cfg);
                    let model = repo.resolved_model(&cfg);
                    let reasoning_effort = repo
                        .resolved_reasoning_effort(&cfg)
                        .map(|effort| effort.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let path = repo
                        .effective_local_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "(unresolved)".to_string());
                    let managed = if repo.is_managed() { " [managed]" } else { "" };
                    println!(
                        "{}  path={}{managed}  harness={}  model={}  reasoning={}  fork_policy={:?}",
                        repo.full_name(),
                        path,
                        harness,
                        model,
                        reasoning_effort,
                        repo.fork_policy
                    );
                }
            }
        }
        Commands::Index { repo, all } => {
            let cfg = AppConfig::load_or_default()?;
            if all {
                for repo in &cfg.repos {
                    println!("indexing {}...", repo.full_name());
                    match repo.effective_local_path() {
                        Ok(path) => match context::gitnexus::run_analyze(&path).await {
                            Ok(_) => println!("  ok"),
                            Err(err) => println!("  failed: {err}"),
                        },
                        Err(err) => println!("  failed to resolve path: {err}"),
                    }
                }
            } else {
                let target = repo.ok_or_else(|| anyhow!("provide owner/repo or --all"))?;
                let repo_cfg = cfg
                    .get_repo(&target)
                    .ok_or_else(|| anyhow!("repo not configured: {target}"))?;
                let path = repo_cfg.effective_local_path()?;
                context::gitnexus::run_analyze(&path).await?;
                println!("index refreshed for {target}");
            }
        }
        Commands::Start { daemon: daemonize } => {
            let cfg = AppConfig::load_or_default()?;
            let db = Database::new(AppConfig::db_file()?).await?;
            let gh = github_client_from_config(&cfg).await?;
            daemon::start(cfg, db, gh, daemonize).await?;
        }
        Commands::Stop => {
            daemon::stop()?;
        }
        Commands::Review {
            target,
            dry_run,
            harness,
            model,
            reasoning_effort,
            force,
            rerun_completed,
        } => {
            let cfg = Arc::new(AppConfig::load_or_default()?);
            let db = Database::new(AppConfig::db_file()?).await?;
            let gh = github_client_from_config(&cfg).await?;

            let (repo_name, pr_number) = parse_target(&target)?;
            let repo_cfg = cfg
                .get_repo(&repo_name)
                .cloned()
                .ok_or_else(|| anyhow!("repo not configured: {repo_name}"))?;

            let mut engine = ReviewEngine::new(cfg.clone(), gh, db);
            engine.init().await;
            let result = engine
                .review_pr(
                    &repo_cfg,
                    pr_number,
                    ReviewOptions {
                        dry_run,
                        harness,
                        model,
                        reasoning_effort,
                        force,
                        rerun_completed,
                    },
                )
                .await?;

            println!(
                "{}#{} {} verdict={:?} comments={}",
                result.repo,
                result.pr_number,
                result.status,
                result.verdict,
                result.comments_posted
            );
        }
        Commands::Status { json } => {
            let cfg = AppConfig::load_or_default()?;
            let db = Database::new(AppConfig::db_file()?).await?;
            let status = daemon::collect_status(&db, &cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("{}", daemon::format_status(&status));
            }
        }
        Commands::Queue { command } => {
            let db = Database::new(AppConfig::db_file()?).await?;
            match command {
                QueueCommand::List {
                    repo,
                    status,
                    limit,
                    json,
                } => {
                    let entries = db
                        .list_work_queue(repo.as_deref(), status.as_deref(), limit)
                        .await?;
                    if json {
                        let output = entries
                            .iter()
                            .map(QueueEntryOutput::from)
                            .collect::<Vec<_>>();
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else if entries.is_empty() {
                        println!("no queued work found");
                    } else {
                        for entry in entries {
                            println!("{}", format_queue_entry_line(&entry));
                        }
                    }
                }
                QueueCommand::Show { id, json } => {
                    let Some(entry) = db.get_work_item(id).await? else {
                        return Err(anyhow!("work item not found: {id}"));
                    };
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&QueueEntryOutput::from(&entry))?
                        );
                    } else {
                        println!("{}", format_queue_entry_detail(&entry));
                    }
                }
                QueueCommand::Retry { id } => {
                    if db.retry_work_item(id).await? {
                        println!("requeued work item #{id}");
                    } else {
                        println!("work item #{id} was not retryable");
                    }
                }
                QueueCommand::Cancel { id } => {
                    if db.cancel_work_item(id).await? {
                        println!("canceled work item #{id}");
                    } else {
                        println!("work item #{id} was not cancelable");
                    }
                }
            }
        }
        Commands::Logs {
            repo,
            since,
            model,
            harness,
            limit,
        } => {
            let db = Database::new(AppConfig::db_file()?).await?;
            let logs = db
                .list_logs(LogsFilter {
                    repo,
                    since,
                    harness,
                    model,
                    limit,
                })
                .await?;

            if logs.is_empty() {
                println!("no review logs found");
            } else {
                for row in logs {
                    println!(
                        "#{} {} pr#{} sha={} {} {} comments={:?} verdict={:?} duration={:?} gitnexus_used={:?} gitnexus_latency_ms={:?} gitnexus_hits={:?} error={:?}",
                        row.id,
                        row.created_at,
                        row.pr_number,
                        short_sha(&row.sha),
                        row.repo,
                        row.status,
                        row.comments_posted,
                        row.verdict,
                        row.duration_secs,
                        row.gitnexus_used,
                        row.gitnexus_latency_ms,
                        row.gitnexus_hit_count,
                        row.error_message,
                    );
                }
            }
        }
        Commands::Stats { since, repo } => {
            let db = Database::new(AppConfig::db_file()?).await?;
            let stats = db.usage_stats(since.as_deref(), repo.as_deref()).await?;
            println!("{}", format_stats(&stats));
        }
        Commands::Serve { port } => {
            eprintln!("The `serve` command is experimental and not yet ready for use.");
            eprintln!("To enable it, run: pr-reviewer config set serve.enabled true");
            let cfg = AppConfig::load_or_default()?;
            if !cfg.serve_enabled() {
                return Err(anyhow!(
                    "serve is disabled; enable with: pr-reviewer config set serve.enabled true"
                ));
            }
            serve::start(cfg, port).await?;
        }
        Commands::Config { command } => {
            let mut cfg = AppConfig::load_or_default()?;
            match command {
                ConfigCommand::Set { key, value } => {
                    cfg.set_key(&key, &value)?;
                    cfg.save()?;
                    println!("updated {key}");
                }
                ConfigCommand::Get { key } => {
                    if let Some(value) = cfg.get_key(&key)? {
                        println!("{value}");
                    } else {
                        println!("key not found: {key}");
                    }
                }
                ConfigCommand::List => {
                    println!("{}", cfg.list_toml()?);
                }
                ConfigCommand::SetToken {
                    stdin,
                    passphrase,
                    signet,
                } => {
                    if stdin && passphrase {
                        return Err(anyhow!(
                            "--stdin and --passphrase are mutually exclusive; \
                             use PR_REVIEWER_PASSPHRASE env var with --stdin instead"
                        ));
                    }

                    // Read the token from stdin, existing config, or interactive prompt (no echo)
                    let raw_token = if stdin {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                            .context("failed to read token from stdin")?;
                        buf.trim().to_string()
                    } else if let Some(ref existing) = cfg.github.token {
                        // Migrate existing plain-text token
                        println!("migrating existing plain-text token to encrypted storage");
                        existing.clone()
                    } else {
                        // Interactive prompt with no echo
                        rpassword::prompt_password("GitHub token: ")
                            .context("failed to read token")?
                            .trim()
                            .to_string()
                    };

                    if raw_token.is_empty() {
                        return Err(anyhow!("token cannot be empty"));
                    }

                    // Validate format (warn but don't block)
                    if let Err(warn) = token::crypto::validate_token_format(&raw_token) {
                        eprintln!("warning: {warn}");
                    }

                    if signet {
                        token::signet::store_token(&raw_token).await?;
                        // Clear any config-stored tokens
                        cfg.github.token = None;
                        cfg.github.encrypted_token = None;
                        cfg.github.passphrase_protected = false;
                        cfg.save()?;
                        println!("token stored in Signet secret store");
                    } else {
                        let pp = if passphrase {
                            let pp = rpassword::prompt_password("passphrase: ")
                                .context("failed to read passphrase")?
                                .trim()
                                .to_string();
                            if pp.is_empty() {
                                return Err(anyhow!("passphrase cannot be empty"));
                            }
                            Some(pp)
                        } else {
                            None
                        };

                        let encrypted = token::crypto::encrypt_token(&raw_token, pp.as_deref())?;

                        cfg.github.encrypted_token = Some(encrypted);
                        cfg.github.passphrase_protected = pp.is_some();
                        cfg.github.token = None; // remove legacy plain text
                        cfg.save()?;

                        let method = if pp.is_some() {
                            "passphrase-protected"
                        } else {
                            "machine-bound"
                        };
                        println!("token encrypted ({method}) and saved to config");
                    }
                }
                ConfigCommand::RemoveToken => {
                    cfg.github.token = None;
                    cfg.github.encrypted_token = None;
                    cfg.github.passphrase_protected = false;
                    cfg.save()?;
                    println!("token removed from config");

                    // Also attempt to remove from Signet
                    if let Err(err) = token::signet::delete_token().await {
                        tracing::debug!(error = %err, "signet token deletion failed");
                    }
                    println!("done");
                }
                ConfigCommand::TokenStatus => match token::token_status(&cfg).await {
                    Some((source, preview)) => {
                        println!("source: {source}");
                        println!("preview: {preview}");
                    }
                    None => {
                        println!("no GitHub token configured");
                    }
                },
            }
        }
    }

    Ok(())
}

fn write_config_example(config: &AppConfig) -> Result<()> {
    let example = toml::to_string_pretty(config)?;
    std::fs::write("config.example.toml", example).context("failed writing config.example.toml")?;
    Ok(())
}

fn parse_target(target: &str) -> Result<(String, u64)> {
    let (repo, number) = target
        .rsplit_once('#')
        .ok_or_else(|| anyhow!("invalid target format, expected owner/repo#number"))?;
    let pr_number = number
        .parse::<u64>()
        .with_context(|| format!("invalid PR number: {number}"))?;
    Ok((repo.to_string(), pr_number))
}

fn parse_fork_policy(raw: &str) -> Result<ForkPolicy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ignore" => Ok(ForkPolicy::Ignore),
        "limited" => Ok(ForkPolicy::Limited),
        "full" => Ok(ForkPolicy::Full),
        _ => Err(anyhow!("invalid fork policy: {raw}")),
    }
}

#[derive(Debug, Serialize)]
struct QueueEntryOutput<'a> {
    id: i64,
    repo: &'a str,
    pr_number: i64,
    head_sha: &'a str,
    task_kind: &'a str,
    dedupe_key: &'a str,
    source_comment_id: Option<i64>,
    status: &'a str,
    attempts: i64,
    error_message: Option<&'a str>,
    created_at: &'a str,
    claimed_at: Option<&'a str>,
    completed_at: Option<&'a str>,
    payload: &'a str,
}

impl<'a> From<&'a WorkQueueEntry> for QueueEntryOutput<'a> {
    fn from(entry: &'a WorkQueueEntry) -> Self {
        Self {
            id: entry.id,
            repo: &entry.repo,
            pr_number: entry.pr_number,
            head_sha: &entry.head_sha,
            task_kind: &entry.task_kind,
            dedupe_key: &entry.dedupe_key,
            source_comment_id: entry.source_comment_id,
            status: &entry.status,
            attempts: entry.attempts,
            error_message: entry.error_message.as_deref(),
            created_at: &entry.created_at,
            claimed_at: entry.claimed_at.as_deref(),
            completed_at: entry.completed_at.as_deref(),
            payload: &entry.payload,
        }
    }
}

fn format_queue_entry_line(entry: &WorkQueueEntry) -> String {
    let source = entry
        .source_comment_id
        .map(|id| format!(" comment={id}"))
        .unwrap_or_default();
    let error = entry
        .error_message
        .as_deref()
        .map(|msg| format!(" error={}", truncate_one_line(msg, 80)))
        .unwrap_or_default();
    format!(
        "#{id} {status} {task} {repo}#{pr} sha={sha} attempts={attempts}{source}{error}",
        id = entry.id,
        status = entry.status,
        task = entry.task_kind,
        repo = entry.repo,
        pr = entry.pr_number,
        sha = short_sha(&entry.head_sha),
        attempts = entry.attempts,
    )
}

fn format_queue_entry_detail(entry: &WorkQueueEntry) -> String {
    let mut out = String::new();
    out.push_str(&format_queue_entry_line(entry));
    out.push('\n');
    out.push_str(&format!("  dedupe: {}\n", entry.dedupe_key));
    out.push_str(&format!("  created: {}\n", entry.created_at));
    if let Some(claimed_at) = entry.claimed_at.as_deref() {
        out.push_str(&format!("  claimed: {claimed_at}\n"));
    }
    if let Some(completed_at) = entry.completed_at.as_deref() {
        out.push_str(&format!("  completed: {completed_at}\n"));
    }
    if let Some(error) = entry.error_message.as_deref() {
        out.push_str(&format!("  error: {error}\n"));
    }
    out.push_str("  payload:\n");
    out.push_str(&indent_lines(&entry.payload, "    "));
    out
}

fn truncate_one_line(value: &str, max_chars: usize) -> String {
    let mut out = value.replace(['\n', '\r'], " ");
    if out.len() > max_chars {
        out.truncate(max_chars.saturating_sub(3));
        out.push_str("...");
    }
    out
}

fn indent_lines(value: &str, prefix: &str) -> String {
    let mut out = String::new();
    for line in value.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str(prefix);
        out.push('\n');
    }
    out
}

async fn github_client_from_config(config: &AppConfig) -> Result<GitHubClient> {
    let (token, source) = token::resolve_github_token(config).await?;
    tracing::info!(source = %source, "GitHub token resolved");
    GitHubClient::new(token)
}

fn add_scanned_repos(cfg: &mut AppConfig, scan_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(scan_dir)
        .with_context(|| format!("failed to read directory {}", scan_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(".git").exists() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // owner is unknown from local scan; use placeholder and let user edit if needed.
        let repo = RepoConfig {
            owner: "local".to_string(),
            name: name.to_string(),
            local_path: Some(path),
            harness: None,
            model: None,
            reasoning_effort: None,
            fork_policy: ForkPolicy::Ignore,
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: true,
            workflow: vec![],
            auto_fix: Default::default(),
        };

        if cfg
            .repos
            .iter()
            .any(|existing| existing.full_name().eq_ignore_ascii_case(&repo.full_name()))
        {
            continue;
        }

        cfg.repos.push(repo);
    }
    Ok(())
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 8 {
        sha
    } else {
        &sha[..8]
    }
}
