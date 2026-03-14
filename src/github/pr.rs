use anyhow::Result;

use crate::github::client::{GitHubClient, ListPullsResult};
use crate::github::types::PullRequest;

pub async fn list_open_prs(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    etag: Option<&str>,
) -> Result<ListPullsResult> {
    client.list_open_prs(owner, repo, etag).await
}

pub async fn get_pull_request(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PullRequest> {
    client.get_pull_request(owner, repo, number).await
}

pub async fn get_diff(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<String> {
    client.get_pr_diff(owner, repo, number).await
}
