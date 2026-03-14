use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use reqwest::header::{HeaderMap, ACCEPT, ETAG, IF_NONE_MATCH, USER_AGENT};
use reqwest::{Client, Method};

use crate::github::types::{
    ContentResponse, CreateIssueCommentRequest, CreateReplyRequest, CreateReviewRequest,
    PullRequest, PullRequestReview, ReviewComment,
};

#[derive(Debug, Clone, Default)]
pub struct RateState {
    pub remaining: Option<u32>,
    pub reset_epoch: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ListPullsResult {
    NotModified {
        etag: Option<String>,
    },
    Updated {
        prs: Vec<PullRequest>,
        etag: Option<String>,
    },
}

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    token: String,
    base_url: String,
    rate_state: Arc<Mutex<RateState>>,
}

impl GitHubClient {
    pub fn new(token: String) -> Result<Self> {
        Self::new_with_base(token, "https://api.github.com")
    }

    pub fn new_with_base(token: String, base_url: &str) -> Result<Self> {
        let client = Client::builder()
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            client,
            token,
            base_url: base_url.trim_end_matches('/').to_string(),
            rate_state: Arc::new(Mutex::new(RateState::default())),
        })
    }

    pub fn rate_state(&self) -> RateState {
        match self.rate_state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub async fn list_open_prs(
        &self,
        owner: &str,
        repo: &str,
        etag: Option<&str>,
    ) -> Result<ListPullsResult> {
        let path = format!(
            "/repos/{owner}/{repo}/pulls?state=open&sort=updated&direction=desc&per_page=50"
        );
        let mut req = self.request(Method::GET, &path);
        if let Some(tag) = etag {
            req = req.header(IF_NONE_MATCH, tag);
        }

        let resp = req.send().await.context("GitHub list PR request failed")?;
        self.update_rate_state(resp.headers());

        let new_etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string)
            .or_else(|| etag.map(ToString::to_string));

        if resp.status().as_u16() == 304 {
            return Ok(ListPullsResult::NotModified { etag: new_etag });
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub list PRs failed ({status}): {body}"));
        }

        let prs = resp
            .json::<Vec<PullRequest>>()
            .await
            .context("failed to decode PR list")?;

        Ok(ListPullsResult::Updated {
            prs,
            etag: new_etag,
        })
    }

    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        let resp = self
            .request(Method::GET, &path)
            .send()
            .await
            .context("GitHub get PR failed")?;
        self.update_rate_state(resp.headers());
        handle_json(resp).await
    }

    pub async fn get_pr_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        let resp = self
            .request(Method::GET, &path)
            .header(ACCEPT, "application/vnd.github.v3.diff")
            .send()
            .await
            .context("GitHub get diff failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub get diff failed ({status}): {body}"));
        }

        Ok(resp.text().await.context("failed to read diff response")?)
    }

    pub async fn get_compare_diff(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> Result<String> {
        let path = format!("/repos/{owner}/{repo}/compare/{base}...{head}");
        let resp = self
            .request(Method::GET, &path)
            .header(ACCEPT, "application/vnd.github.v3.diff")
            .send()
            .await
            .context("GitHub get compare diff failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub compare diff failed ({status}): {body}"));
        }

        Ok(resp
            .text()
            .await
            .context("failed to read compare diff response")?)
    }

    pub async fn get_file_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        reference: &str,
    ) -> Result<Option<String>> {
        let encoded_path = path
            .split('/')
            .map(urlencoding::encode)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("/");
        let endpoint = format!("/repos/{owner}/{repo}/contents/{encoded_path}?ref={reference}");
        let resp = self
            .request(Method::GET, &endpoint)
            .send()
            .await
            .context("GitHub get file content failed")?;
        self.update_rate_state(resp.headers());

        if resp.status().as_u16() == 404 {
            return Ok(None);
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub get file content failed ({status}): {body}"));
        }

        let payload = resp
            .json::<ContentResponse>()
            .await
            .context("invalid content payload")?;

        if payload.encoding != "base64" {
            return Ok(None);
        }

        let cleaned = payload.content.replace('\n', "");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .context("failed to decode base64 file payload")?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(Some(text))
    }

    pub async fn create_review(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        request: &CreateReviewRequest,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pr_number}/reviews");
        let resp = self
            .request(Method::POST, &path)
            .json(request)
            .send()
            .await
            .context("GitHub create review failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub create review failed ({status}): {body}"));
        }

        Ok(())
    }

    pub async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/issues/{pr_number}/comments");
        let payload = CreateIssueCommentRequest {
            body: body.to_string(),
        };
        let resp = self
            .request(Method::POST, &path)
            .json(&payload)
            .send()
            .await
            .context("GitHub create issue comment failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub issue comment failed ({status}): {text}"));
        }

        Ok(())
    }

    pub async fn reply_to_review_comment(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pr_number}/comments/{comment_id}/replies");
        let payload = CreateReplyRequest {
            body: body.to_string(),
        };
        let resp = self
            .request(Method::POST, &path)
            .json(&payload)
            .send()
            .await
            .context("GitHub reply to comment failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub reply failed ({status}): {text}"));
        }
        Ok(())
    }

    pub async fn get_existing_reviews(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<PullRequestReview>> {
        let mut all_reviews = Vec::new();
        let mut page = 1u32;
        loop {
            let path =
                format!("/repos/{owner}/{repo}/pulls/{pr_number}/reviews?per_page=100&page={page}");
            let resp = self
                .request(Method::GET, &path)
                .send()
                .await
                .context("GitHub get existing reviews failed")?;
            self.update_rate_state(resp.headers());

            let has_next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("rel=\"next\""));

            let batch: Vec<PullRequestReview> = handle_json(resp).await?;
            let batch_empty = batch.is_empty();
            all_reviews.extend(batch);

            if !has_next || batch_empty || page >= 10 {
                break;
            }
            page += 1;
        }
        Ok(all_reviews)
    }

    pub async fn get_review_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        since: Option<&str>,
    ) -> Result<Vec<ReviewComment>> {
        let mut all_comments = Vec::new();
        let mut page = 1u32;
        loop {
            let mut path = format!(
                "/repos/{owner}/{repo}/pulls/{pr_number}/comments?per_page=100&page={page}"
            );
            if let Some(since) = since {
                path.push_str("&since=");
                path.push_str(since);
            }
            let resp = self
                .request(Method::GET, &path)
                .send()
                .await
                .context("GitHub get review comments failed")?;
            self.update_rate_state(resp.headers());

            let has_next = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("rel=\"next\""));

            let batch: Vec<ReviewComment> = handle_json(resp).await?;
            let batch_empty = batch.is_empty();
            all_comments.extend(batch);

            if !has_next || batch_empty || page >= 10 {
                break;
            }
            page += 1;
        }
        Ok(all_comments)
    }

    pub async fn get_authenticated_user(&self) -> Result<String> {
        let resp = self
            .request(Method::GET, "/user")
            .send()
            .await
            .context("GitHub get authenticated user failed")?;
        self.update_rate_state(resp.headers());
        let payload: serde_json::Value = handle_json(resp).await?;
        payload["login"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no login in /user response"))
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.base_url, path))
            .header(USER_AGENT, "pr-reviewer/0.1")
            .header(ACCEPT, "application/vnd.github+json")
            .bearer_auth(&self.token)
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn update_rate_state(&self, headers: &HeaderMap) {
        let mut state = match self.rate_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());
        state.reset_epoch = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
    }
}

async fn handle_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub API failed ({status}): {body}"));
    }
    Ok(resp
        .json::<T>()
        .await
        .context("failed decoding GitHub JSON payload")?)
}
