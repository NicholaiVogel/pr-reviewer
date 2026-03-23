use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::Serialize;
use tokio::sync::Mutex;

use axum::http::HeaderMap;

use crate::config::{AppConfig, WorkflowStep};

// The dashboard HTML is embedded at compile time.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Hard limits for user-supplied strings to keep prompt context bounded.
const MAX_STEP_ID_LEN: usize = 64;
const MAX_STEP_NAME_LEN: usize = 256;
const MAX_MODEL_LEN: usize = 256;
const MAX_INSTRUCTIONS_LEN: usize = 8_192;
const MAX_PATTERN_LEN: usize = 512;
const MAX_PATTERNS: usize = 64;
const MAX_STEPS: usize = 32;

// ── Shared state ─────────────────────────────────────────────────────────────

type SharedConfig = Arc<Mutex<AppConfig>>;

// ── Error wrapper ─────────────────────────────────────────────────────────────

/// Typed API error that maps to HTTP status codes.
enum ApiError {
    NotFound(String),
    BadRequest(String),
    Forbidden(String),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::BadRequest(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            ApiError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

// ── Handler: dashboard ────────────────────────────────────────────────────────

async fn dashboard_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
}

// ── Handler: list repos ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct RepoSummary {
    owner: String,
    name: String,
    harness: Option<String>,
    model: Option<String>,
    workflow: Vec<WorkflowStep>,
}

async fn list_repos(State(state): State<SharedConfig>) -> ApiResult<impl IntoResponse> {
    let cfg = state.lock().await;
    let repos: Vec<RepoSummary> = cfg
        .repos
        .iter()
        .map(|r| RepoSummary {
            owner: r.owner.clone(),
            name: r.name.clone(),
            harness: r.harness.map(|h| h.as_str().to_string()),
            model: r.model.clone(),
            workflow: r.workflow.clone(),
        })
        .collect();
    Ok(Json(repos))
}

// ── Path validation ───────────────────────────────────────────────────────────

fn validate_path_segment(seg: &str) -> Result<(), ApiError> {
    if seg.contains('/') || seg.contains('\\') || seg.contains("..") {
        return Err(ApiError::BadRequest(format!("invalid path segment: {seg}")));
    }
    Ok(())
}

// ── Handler: get workflow ─────────────────────────────────────────────────────

async fn get_workflow(
    State(state): State<SharedConfig>,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    validate_path_segment(&owner)?;
    validate_path_segment(&name)?;
    let cfg = state.lock().await;
    let repo = cfg
        .repos
        .iter()
        .find(|r| r.owner.eq_ignore_ascii_case(&owner) && r.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| ApiError::NotFound(format!("repo not found: {owner}/{name}")))?;
    Ok(Json(repo.workflow.clone()))
}

// ── Handler: put workflow ─────────────────────────────────────────────────────

async fn put_workflow(
    State(state): State<SharedConfig>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(steps): Json<Vec<WorkflowStep>>,
) -> ApiResult<impl IntoResponse> {
    validate_path_segment(&owner)?;
    validate_path_segment(&name)?;

    // CSRF: require custom header on mutating requests.
    match headers
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
    {
        Some("pr-reviewer") => {}
        _ => {
            return Err(ApiError::Forbidden(
                "missing or invalid X-Requested-With header".into(),
            ))
        }
    }

    validate_workflow(&steps)?;

    // Clone the config for saving outside the lock to avoid blocking I/O
    // while holding the async mutex.
    let config_to_save = {
        let mut cfg = state.lock().await;
        let repo = cfg
            .repos
            .iter_mut()
            .find(|r| r.owner.eq_ignore_ascii_case(&owner) && r.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| ApiError::NotFound(format!("repo not found: {owner}/{name}")))?;
        repo.workflow = steps;
        cfg.clone()
    };
    config_to_save.save()?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Validation ────────────────────────────────────────────────────────────────

fn validate_workflow(steps: &[WorkflowStep]) -> Result<(), ApiError> {
    if steps.len() > MAX_STEPS {
        return Err(ApiError::BadRequest(format!(
            "too many steps: {}, max is {MAX_STEPS}",
            steps.len()
        )));
    }

    // Duplicate ID check.
    let mut seen_ids = HashSet::new();
    for step in steps {
        if step.id.is_empty() {
            return Err(ApiError::BadRequest("step id must not be empty".into()));
        }
        if step.id.len() > MAX_STEP_ID_LEN {
            return Err(ApiError::BadRequest(format!(
                "step id too long ({} chars, max {MAX_STEP_ID_LEN})",
                step.id.len()
            )));
        }
        if !seen_ids.insert(step.id.clone()) {
            return Err(ApiError::BadRequest(format!(
                "duplicate step id: {}",
                step.id
            )));
        }

        if step.name.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "step '{}' has an empty name",
                step.id
            )));
        }
        if step.name.len() > MAX_STEP_NAME_LEN {
            return Err(ApiError::BadRequest(format!(
                "step '{}' name too long ({} chars, max {MAX_STEP_NAME_LEN})",
                step.id,
                step.name.len()
            )));
        }

        if let Some(ref model) = step.model {
            if model.len() > MAX_MODEL_LEN {
                return Err(ApiError::BadRequest(format!(
                    "step '{}' model too long ({} chars, max {MAX_MODEL_LEN})",
                    step.id,
                    model.len()
                )));
            }
        }

        if let Some(ref instr) = step.custom_instructions {
            if instr.len() > MAX_INSTRUCTIONS_LEN {
                return Err(ApiError::BadRequest(format!(
                    "step '{}' custom_instructions too long ({} chars, max {MAX_INSTRUCTIONS_LEN})",
                    step.id,
                    instr.len()
                )));
            }
        }

        let cond = &step.conditions;
        if cond.file_patterns.len() > MAX_PATTERNS {
            return Err(ApiError::BadRequest(format!(
                "step '{}' has too many file_patterns (max {MAX_PATTERNS})",
                step.id
            )));
        }
        for pat in &cond.file_patterns {
            if pat.len() > MAX_PATTERN_LEN {
                return Err(ApiError::BadRequest(format!(
                    "step '{}' file_pattern too long (max {MAX_PATTERN_LEN} chars)",
                    step.id
                )));
            }
        }
        if cond.label_patterns.len() > MAX_PATTERNS {
            return Err(ApiError::BadRequest(format!(
                "step '{}' has too many label_patterns (max {MAX_PATTERNS})",
                step.id
            )));
        }
        for pat in &cond.label_patterns {
            if pat.len() > MAX_PATTERN_LEN {
                return Err(ApiError::BadRequest(format!(
                    "step '{}' label_pattern too long (max {MAX_PATTERN_LEN} chars)",
                    step.id
                )));
            }
        }
    }

    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the workflow builder UI server on the given port.
/// Binds only to 127.0.0.1 — not accessible from outside the local machine.
pub async fn start(config: AppConfig, port: u16) -> Result<()> {
    let shared = Arc::new(Mutex::new(config));

    // No CORS layer: the server is local-only (127.0.0.1) and the dashboard
    // is served from the same origin, so cross-origin requests are blocked by
    // the browser's same-origin policy without any extra work.
    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/repos", get(list_repos))
        .route("/api/repos/:owner/:name/workflow", get(get_workflow))
        .route("/api/repos/:owner/:name/workflow", put(put_workflow))
        .with_state(shared);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("pr-reviewer UI running at http://localhost:{port}");
    println!("Press Ctrl-C to stop.");

    axum::serve(listener, app).await?;
    Ok(())
}
