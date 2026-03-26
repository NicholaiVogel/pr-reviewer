use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    ClaudeCode,
    Opencode,
    Codex,
}

impl HarnessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Codex => "codex",
        }
    }
}

impl Display for HarnessKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for HarnessKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::Opencode),
            "codex" => Ok(Self::Codex),
            _ => Err(anyhow!("unsupported harness: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
        }
    }
}

impl Display for ReasoningEffort {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::Xhigh),
            _ => Err(anyhow!("unsupported reasoning effort: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ForkPolicy {
    Ignore,
    Limited,
    Full,
}

impl Default for ForkPolicy {
    fn default() -> Self {
        Self::Ignore
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonMode {
    Poll,
    Webhook,
}

impl Default for DaemonMode {
    fn default() -> Self {
        Self::Poll
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "default_harness")]
    pub default: HarnessKind,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitHubConfig {
    /// Plain-text token (legacy — triggers deprecation warning at runtime).
    #[serde(default)]
    pub token: Option<String>,
    /// Base64-encoded double-encrypted token blob.
    #[serde(default)]
    pub encrypted_token: Option<String>,
    /// Whether a user passphrase was used for the inner encryption layer.
    #[serde(default)]
    pub passphrase_protected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_poll_interval")]
    pub max_poll_interval_secs: u64,
    #[serde(default)]
    pub mode: DaemonMode,
    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_reviews: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueTriageConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub create_missing_labels: bool,
    #[serde(default = "default_issue_triage_context_bytes")]
    pub max_context_bytes: usize,
    #[serde(default = "default_issue_triage_max_labels_to_create")]
    pub max_labels_to_create: usize,
    #[serde(default = "default_issue_triage_label_prefixes")]
    pub allowed_new_label_prefixes: Vec<String>,
    #[serde(default = "default_issue_triage_label_name_max_chars")]
    pub max_new_label_name_chars: usize,
    #[serde(default = "default_issue_triage_label_description_max_chars")]
    pub max_new_label_description_chars: usize,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_auto_review")]
    pub auto_review: bool,
    #[serde(default)]
    pub review_drafts: bool,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: usize,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: usize,
    #[serde(default = "default_bot_name")]
    pub bot_name: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_prompt_limit")]
    pub max_prompt_bytes: usize,
    #[serde(default = "default_true")]
    pub skip_docs_only: bool,
}

/// Conditions that must be met for a workflow step to run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowConditions {
    /// Only run if the PR touches files matching any of these globs.
    #[serde(default)]
    pub file_patterns: Vec<String>,
    /// Only run if the diff has at least this many lines.
    #[serde(default)]
    pub min_diff_lines: Option<usize>,
    /// Skip draft PRs.
    #[serde(default)]
    pub skip_drafts: bool,
    /// Only run if the PR has at least one of these labels.
    #[serde(default)]
    pub label_patterns: Vec<String>,
}

/// A single step in a repo's review workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Unique identifier (UUID or user-defined slug).
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: String,
    /// Override the harness for this step. Falls back to repo/global default.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// Override the model for this step. Falls back to repo/global default.
    #[serde(default)]
    pub model: Option<String>,
    /// Additional instructions injected into the prompt for this step.
    #[serde(default)]
    pub custom_instructions: Option<String>,
    /// Conditions that must be met for this step to run.
    #[serde(default)]
    pub conditions: WorkflowConditions,
    /// Whether this step is active. Disabled steps are skipped.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
    /// Local path override. When `None`, uses managed clone at `~/.config/pr-reviewer/repos/{owner}/{name}`.
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub fork_policy: ForkPolicy,
    #[serde(default)]
    pub trusted_authors: Vec<String>,
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    #[serde(default)]
    pub custom_instructions: Option<String>,
    #[serde(default = "default_gitnexus")]
    pub gitnexus: bool,
    #[serde(default)]
    pub issue_triage: IssueTriageConfig,
    /// Optional multi-step review workflow. When non-empty the daemon runs
    /// each enabled step in order instead of the single-step legacy behaviour.
    #[serde(default)]
    pub workflow: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub harness: HarnessConfig,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub serve: ServeConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServeConfig {
    #[serde(default)]
    pub enabled: bool,
}

fn default_harness() -> HarnessKind {
    HarnessKind::ClaudeCode
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn default_timeout_secs() -> u64 {
    1800
}

fn default_poll_interval() -> u64 {
    30
}

fn default_max_poll_interval() -> u64 {
    300
}

fn default_webhook_port() -> u16 {
    3847
}

fn default_concurrency() -> usize {
    2
}

fn default_auto_review() -> bool {
    true
}

fn default_max_files() -> usize {
    50
}

fn default_max_diff_lines() -> usize {
    3000
}

fn default_max_file_size() -> usize {
    100_000
}

fn default_bot_name() -> String {
    "pr-reviewer".to_string()
}

fn default_prompt_limit() -> usize {
    200 * 1024
}

fn default_gitnexus() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_issue_triage_context_bytes() -> usize {
    64 * 1024
}

fn default_issue_triage_max_labels_to_create() -> usize {
    3
}

fn default_issue_triage_label_prefixes() -> Vec<String> {
    vec![
        "bug".to_string(),
        "documentation".to_string(),
        "enhancement".to_string(),
        "question".to_string(),
        "spec".to_string(),
        "spec:".to_string(),
        "priority".to_string(),
        "priority: ".to_string(),
        "area".to_string(),
        "area:".to_string(),
        "bucket".to_string(),
        "bucket:".to_string(),
    ]
}

fn default_issue_triage_label_name_max_chars() -> usize {
    50
}

fn default_issue_triage_label_description_max_chars() -> usize {
    256
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            default: default_harness(),
            model: default_model(),
            reasoning_effort: None,
            timeout_secs: default_timeout_secs(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            max_poll_interval_secs: default_max_poll_interval(),
            mode: DaemonMode::default(),
            webhook_port: default_webhook_port(),
            max_concurrent_reviews: default_concurrency(),
        }
    }
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            auto_review: default_auto_review(),
            review_drafts: false,
            max_files: default_max_files(),
            max_diff_lines: default_max_diff_lines(),
            max_file_size_bytes: default_max_file_size(),
            bot_name: default_bot_name(),
            dry_run: false,
            max_prompt_bytes: default_prompt_limit(),
            skip_docs_only: true,
        }
    }
}

impl Default for IssueTriageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            create_missing_labels: false,
            max_context_bytes: default_issue_triage_context_bytes(),
            max_labels_to_create: default_issue_triage_max_labels_to_create(),
            allowed_new_label_prefixes: default_issue_triage_label_prefixes(),
            max_new_label_name_chars: default_issue_triage_label_name_max_chars(),
            max_new_label_description_chars: default_issue_triage_label_description_max_chars(),
            instructions: None,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            harness: HarnessConfig::default(),
            github: GitHubConfig::default(),
            daemon: DaemonConfig::default(),
            defaults: DefaultsConfig::default(),
            repos: Vec::new(),
            serve: ServeConfig::default(),
        }
    }
}

impl RepoConfig {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Resolve the effective local path for this repo.
    /// Returns `local_path` if set, otherwise the managed clone path.
    pub fn effective_local_path(&self) -> Result<PathBuf> {
        crate::repo_manager::resolve_local_path(self)
    }

    /// Whether this repo uses a managed (auto-cloned) path vs. a manual override.
    pub fn is_managed(&self) -> bool {
        self.local_path.is_none()
    }

    pub fn resolved_harness(&self, config: &AppConfig) -> HarnessKind {
        self.harness.unwrap_or(config.harness.default)
    }

    pub fn resolved_model<'a>(&'a self, config: &'a AppConfig) -> &'a str {
        self.model
            .as_deref()
            .unwrap_or(config.harness.model.as_str())
    }

    pub fn resolved_reasoning_effort(&self, config: &AppConfig) -> Option<ReasoningEffort> {
        self.reasoning_effort.or(config.harness.reasoning_effort)
    }

    pub fn issue_triage_enabled(&self) -> bool {
        self.issue_triage.enabled
    }
}

impl AppConfig {
    pub fn serve_enabled(&self) -> bool {
        self.serve.enabled
    }

    pub fn config_dir() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("PR_REVIEWER_CONFIG_DIR") {
            return Ok(PathBuf::from(path));
        }
        let base = dirs::config_dir().context("unable to resolve config dir")?;
        Ok(base.join("pr-reviewer"))
    }

    pub fn config_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn db_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("state.db"))
    }

    pub fn pid_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("daemon.pid"))
    }

    pub fn load() -> Result<Self> {
        let file = Self::config_file()?;
        let data = fs::read_to_string(&file)
            .with_context(|| format!("failed to read config file at {}", file.display()))?;
        let config: AppConfig = toml::from_str(&data).context("failed to parse config TOML")?;
        Ok(config)
    }

    pub fn load_or_default() -> Result<Self> {
        let file = Self::config_file()?;
        if file.exists() {
            Self::load()
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let file = Self::config_file()?;
        let toml = toml::to_string_pretty(self).context("failed to serialize config")?;
        let tmp = file.with_extension("toml.tmp");
        fs::write(&tmp, toml)
            .with_context(|| format!("failed to write config to {}", tmp.display()))?;
        fs::rename(&tmp, &file)
            .with_context(|| format!("failed to rename tmp config to {}", file.display()))?;
        Ok(())
    }

    pub fn init() -> Result<Self> {
        let cfg = Self::default();
        cfg.save()?;
        Ok(cfg)
    }

    pub fn get_repo(&self, full_name: &str) -> Option<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| r.full_name().eq_ignore_ascii_case(full_name))
    }

    pub fn get_repo_mut(&mut self, full_name: &str) -> Option<&mut RepoConfig> {
        self.repos
            .iter_mut()
            .find(|r| r.full_name().eq_ignore_ascii_case(full_name))
    }

    pub fn add_repo(&mut self, repo: RepoConfig) -> Result<()> {
        if self
            .repos
            .iter()
            .any(|r| r.full_name().eq_ignore_ascii_case(&repo.full_name()))
        {
            return Err(anyhow!("repo already configured: {}", repo.full_name()));
        }
        self.repos.push(repo);
        Ok(())
    }

    pub fn remove_repo(&mut self, full_name: &str) -> bool {
        let before = self.repos.len();
        self.repos
            .retain(|r| !r.full_name().eq_ignore_ascii_case(full_name));
        before != self.repos.len()
    }

    pub fn set_key(&mut self, key: &str, raw_value: &str) -> Result<()> {
        let mut value =
            toml::Value::try_from(self.clone()).context("failed to project config to TOML")?;
        set_toml_path(&mut value, key, parse_toml_value(raw_value)?)?;
        *self = value
            .try_into()
            .context("invalid config after applying key")?;
        Ok(())
    }

    pub fn get_key(&self, key: &str) -> Result<Option<String>> {
        let value =
            toml::Value::try_from(self.clone()).context("failed to project config to TOML")?;
        Ok(get_toml_path(&value, key).map(|v| match v {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        }))
    }

    pub fn list_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self).context("failed to serialize config")?)
    }
}

pub fn parse_repo_full_name(input: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(anyhow!("invalid repo format, expected owner/repo"));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn parse_toml_value(raw: &str) -> Result<toml::Value> {
    let wrapped = format!("v = {}", raw);
    if let Ok(table) = wrapped.parse::<toml::Table>() {
        if let Some(v) = table.get("v") {
            return Ok(v.clone());
        }
    }
    Ok(toml::Value::String(raw.to_string()))
}

fn set_toml_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<()> {
    let mut parts = key.split('.').peekable();
    let mut current = root;

    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let table = current
                .as_table_mut()
                .ok_or_else(|| anyhow!("key path is not a table at {part}"))?;
            table.insert(part.to_string(), value);
            return Ok(());
        }

        let table = current
            .as_table_mut()
            .ok_or_else(|| anyhow!("key path is not a table at {part}"))?;

        if !table.contains_key(part) {
            table.insert(part.to_string(), toml::Value::Table(Default::default()));
        }

        current = table
            .get_mut(part)
            .ok_or_else(|| anyhow!("failed to descend into key path"))?;
    }

    Err(anyhow!("invalid config key"))
}

fn get_toml_path<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for part in key.split('.') {
        let table = current.as_table()?;
        current = table.get(part)?;
    }
    Some(current)
}

/// Validate that a manually-specified repo path exists.
pub fn ensure_repo_path_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("repo path does not exist: {}", path.display()));
    }
    Ok(())
}
