use anyhow::Result;

use crate::github::client::GitHubClient;
use crate::github::types::{CreateReviewRequest, IssueComment, PullRequestReview, ReviewComment};

pub async fn create_review(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
    request: &CreateReviewRequest,
) -> Result<()> {
    client.create_review(owner, repo, number, request).await
}

pub async fn create_issue_comment(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
    body: &str,
) -> Result<()> {
    client.create_issue_comment(owner, repo, number, body).await
}

pub async fn get_existing_reviews(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Vec<PullRequestReview>> {
    client.get_existing_reviews(owner, repo, number).await
}

pub async fn get_review_comments(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
    since: Option<&str>,
) -> Result<Vec<ReviewComment>> {
    client.get_review_comments(owner, repo, number, since).await
}

pub async fn get_issue_comments(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Vec<IssueComment>> {
    client.get_issue_comments(owner, repo, number).await
}

pub async fn reply_to_review_comment(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
    comment_id: u64,
    body: &str,
) -> Result<()> {
    client
        .reply_to_review_comment(owner, repo, number, comment_id, body)
        .await
}
