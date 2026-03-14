use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::Serialize;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use crate::config::{AppConfig, WorkflowStep};

// The dashboard HTML is embedded at compile time.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

// ── Shared state ─────────────────────────────────────────────────────────────

type SharedConfig = Arc<Mutex<AppConfig>>;

// ── Error wrapper ─────────────────────────────────────────────────────────────

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

type ApiResult<T> = std::result::Result<T, AppError>;

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

// ── Handler: get workflow ─────────────────────────────────────────────────────

async fn get_workflow(
    State(state): State<SharedConfig>,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let cfg = state.lock().await;
    let repo = cfg
        .repos
        .iter()
        .find(|r| r.owner.eq_ignore_ascii_case(&owner) && r.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| anyhow!("repo not found: {owner}/{name}"))?;
    Ok(Json(repo.workflow.clone()))
}

// ── Handler: put workflow ─────────────────────────────────────────────────────

async fn put_workflow(
    State(state): State<SharedConfig>,
    Path((owner, name)): Path<(String, String)>,
    Json(steps): Json<Vec<WorkflowStep>>,
) -> ApiResult<impl IntoResponse> {
    let mut cfg = state.lock().await;
    let repo = cfg
        .repos
        .iter_mut()
        .find(|r| r.owner.eq_ignore_ascii_case(&owner) && r.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| anyhow!("repo not found: {owner}/{name}"))?;
    repo.workflow = steps;
    cfg.save()?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the workflow builder UI server on the given port.
pub async fn start(config: AppConfig, port: u16) -> Result<()> {
    let shared = Arc::new(Mutex::new(config));

    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/repos", get(list_repos))
        .route("/api/repos/:owner/:name/workflow", get(get_workflow))
        .route("/api/repos/:owner/:name/workflow", put(put_workflow))
        .layer(CorsLayer::permissive())
        .with_state(shared);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("pr-reviewer UI running at http://localhost:{port}");
    println!("Press Ctrl-C to stop.");

    axum::serve(listener, app).await?;
    Ok(())
}
