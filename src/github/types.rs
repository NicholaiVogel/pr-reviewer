use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
    /// GitHub account type: "User", "Bot", or "Organization".
    #[serde(rename = "type", default)]
    pub account_type: Option<String>,
}

impl User {
    pub fn is_bot(&self) -> bool {
        self.account_type.as_deref() == Some("Bot")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoRef {
    pub full_name: String,
    #[serde(default)]
    pub fork: bool,
    pub owner: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestHead {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub repo: Option<RepoRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestBase {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub repo: RepoRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default = "default_pr_state")]
    pub state: String,
    pub user: User,
    pub head: PullRequestHead,
    pub base: PullRequestBase,
    pub html_url: Option<String>,
    pub updated_at: Option<String>,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestReview {
    pub id: u64,
    pub body: Option<String>,
    pub user: User,
    pub state: Option<String>,
    pub commit_id: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub user: User,
    pub in_reply_to_id: Option<u64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    pub user: User,
    #[serde(default)]
    pub author_association: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateReviewComment {
    pub path: String,
    pub line: u32,
    pub side: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateReviewRequest {
    pub body: String,
    pub event: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<CreateReviewComment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateIssueCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateIssueCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateReplyRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatePullRequestRequest {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub draft: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatePullRequestResponse {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentResponse {
    #[serde(default)]
    pub sha: Option<String>,
    pub content: String,
    pub encoding: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrFile {
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub changes: u32,
    pub patch: Option<String>,
    pub previous_filename: Option<String>,
}

/// Fallback state when `state` is absent from the API response.
/// Treating unknown state as "open" is the safest default — finalization
/// logic will simply skip the PR (no-op) rather than incorrectly archiving it.
fn default_pr_state() -> String {
    "open".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRefObject {
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRefResponse {
    pub object: GitRefObject,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRefRequest {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateContentRequest {
    pub message: String,
    pub content: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}
