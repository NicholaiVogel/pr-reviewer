mod config;
mod context;
mod daemon;
mod github;
mod harness;
mod review;
mod safety;
mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::{parse_repo_full_name, AppConfig, ForkPolicy, HarnessKind, RepoConfig};
use github::client::GitHubClient;
use review::engine::{ReviewEngine, ReviewOptions};
use store::db::{format_stats, Database, LogsFilter};

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
    },
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
    },
    Status,
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
    #[arg(long, default_value = "ignore")]
    fork_policy: String,
    #[arg(long)]
    org: Option<String>,
    #[arg(long)]
    scan: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Set { key: String, value: String },
    Get { key: String },
    List,
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
            let path = args
                .path
                .ok_or_else(|| anyhow!("--path is required for add"))?;

            let (owner, name) = parse_repo_full_name(&repo)?;
            config::ensure_repo_path_exists(&path)?;

            let fork_policy = parse_fork_policy(&args.fork_policy)?;
            let repo_cfg = RepoConfig {
                owner,
                name,
                local_path: path.clone(),
                harness: args.harness,
                model: args.model,
                fork_policy,
                trusted_authors: vec![],
                ignore_paths: vec![
                    "*.lock".to_string(),
                    "vendor/**".to_string(),
                    "dist/**".to_string(),
                ],
                custom_instructions: None,
                gitnexus: true,
            };
            let repo_name = repo_cfg.full_name();
            cfg.add_repo(repo_cfg.clone())?;
            cfg.save()?;
            println!("added repo {repo_name}");

            if repo_cfg.gitnexus {
                match context::gitnexus::run_analyze(Path::new(&repo_cfg.local_path)).await {
                    Ok(_) => println!("gitnexus index built for {repo_name}"),
                    Err(err) => println!("warning: gitnexus analyze failed for {repo_name}: {err}"),
                }
            }
        }
        Commands::Remove { repo } => {
            let mut cfg = AppConfig::load_or_default()?;
            if cfg.remove_repo(&repo) {
                cfg.save()?;
                println!("removed {repo}");
            } else {
                println!("repo not found: {repo}");
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
                    println!(
                        "{}  path={}  harness={}  model={}  fork_policy={:?}",
                        repo.full_name(),
                        repo.local_path.display(),
                        harness,
                        model,
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
                    match context::gitnexus::run_analyze(Path::new(&repo.local_path)).await {
                        Ok(_) => println!("  ok"),
                        Err(err) => println!("  failed: {err}"),
                    }
                }
            } else {
                let target = repo.ok_or_else(|| anyhow!("provide owner/repo or --all"))?;
                let repo_cfg = cfg
                    .get_repo(&target)
                    .ok_or_else(|| anyhow!("repo not configured: {target}"))?;
                context::gitnexus::run_analyze(Path::new(&repo_cfg.local_path)).await?;
                println!("index refreshed for {target}");
            }
        }
        Commands::Start { daemon: daemonize } => {
            let cfg = AppConfig::load_or_default()?;
            let db = Database::new(AppConfig::db_file()?).await?;
            let gh = github_client_from_config(&cfg)?;
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
        } => {
            let cfg = Arc::new(AppConfig::load_or_default()?);
            let db = Database::new(AppConfig::db_file()?).await?;
            let gh = github_client_from_config(&cfg)?;

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
        Commands::Status => {
            let cfg = AppConfig::load_or_default()?;
            let db = Database::new(AppConfig::db_file()?).await?;
            let gh = github_client_from_config(&cfg)?;
            let status = daemon::status(&db).await?;
            let rate = gh.rate_state();
            println!("{status}");
            println!(
                "Current rate: remaining={:?}, reset_epoch={:?}",
                rate.remaining, rate.reset_epoch
            );
            println!("Configured repos: {}", cfg.repos.len());
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
                        "#{} {} pr#{} sha={} {} {} comments={:?} verdict={:?} duration={:?} error={:?}",
                        row.id,
                        row.created_at,
                        row.pr_number,
                        short_sha(&row.sha),
                        row.repo,
                        row.status,
                        row.comments_posted,
                        row.verdict,
                        row.duration_secs,
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

fn github_client_from_config(config: &AppConfig) -> Result<GitHubClient> {
    let token = config
        .github
        .token
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .ok_or_else(|| {
            anyhow!("GitHub token missing: set github.token in config or GITHUB_TOKEN env")
        })?;

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
            local_path: path,
            harness: None,
            model: None,
            fork_policy: ForkPolicy::Ignore,
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: true,
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
