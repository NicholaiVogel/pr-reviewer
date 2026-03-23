use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
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
    pub user: User,
    pub head: PullRequestHead,
    pub base: PullRequestBase,
    pub html_url: Option<String>,
    pub updated_at: Option<String>,
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
pub struct CreateReplyRequest {
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentResponse {
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
