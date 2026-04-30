use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use reqwest::header::{HeaderMap, ACCEPT, ETAG, IF_NONE_MATCH, LINK, USER_AGENT};
use reqwest::{Client, Method};

use crate::github::types::{
    ContentResponse, CreateIssueCommentRequest, CreatePullRequestRequest,
    CreatePullRequestResponse, CreateRefRequest, CreateReplyRequest, CreateReviewRequest,
    GitRefResponse, IssueComment, PrFile, PullRequest, PullRequestReview, ReviewComment,
    UpdateContentRequest, UpdateIssueCommentRequest,
};

#[derive(Debug, Clone, Default)]
pub struct RateState {
    pub limit: Option<u32>,
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
        complete: bool,
    },
}

const OPEN_PRS_PAGE_SIZE: u32 = 100;
const OPEN_PRS_MAX_PAGES: u32 = 10;

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    token: String,
    base_url: String,
    rate_state: Arc<Mutex<RateState>>,
}

impl GitHubClient {
    /// Returns a reference to the raw token for use in git operations.
    pub fn token(&self) -> &str {
        &self.token
    }

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
        let mut all_prs = Vec::new();
        let mut new_etag = etag.map(ToString::to_string);
        let mut page = 1u32;

        loop {
            let path = format!(
                "/repos/{owner}/{repo}/pulls?state=open&sort=updated&direction=desc&per_page={OPEN_PRS_PAGE_SIZE}&page={page}"
            );
            let mut req = self.request(Method::GET, &path);
            if page == 1 {
                if let Some(tag) = etag {
                    req = req.header(IF_NONE_MATCH, tag);
                }
            }

            let resp = req.send().await.context("GitHub list PR request failed")?;
            self.update_rate_state(resp.headers());

            if page == 1 {
                new_etag = resp
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string)
                    .or_else(|| etag.map(ToString::to_string));

                if resp.status().as_u16() == 304 {
                    return Ok(ListPullsResult::NotModified { etag: new_etag });
                }
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("GitHub list PRs failed ({status}): {body}"));
            }

            let has_next = response_has_next_page(resp.headers());
            let batch = resp
                .json::<Vec<PullRequest>>()
                .await
                .context("failed to decode PR list")?;
            let batch_empty = batch.is_empty();
            all_prs.extend(batch);

            let truncated = has_next && !batch_empty && page >= OPEN_PRS_MAX_PAGES;
            if truncated {
                tracing::warn!(
                    owner = owner,
                    repo = repo,
                    pr_count = all_prs.len(),
                    "open PR pagination hit page cap; open-set finalization checks will stay conservative"
                );
            }

            if !has_next || batch_empty || page >= OPEN_PRS_MAX_PAGES {
                return Ok(ListPullsResult::Updated {
                    prs: all_prs,
                    etag: new_etag,
                    complete: !truncated,
                });
            }

            page += 1;
        }
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

        if resp.status().as_u16() == 406 {
            tracing::warn!(
                owner = owner,
                repo = repo,
                number = number,
                "diff endpoint returned 406 (too large), falling back to files endpoint"
            );
            let files = self.get_pr_files(owner, repo, number).await?;
            return Ok(synthesize_diff_from_files(&files));
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub get diff failed ({status}): {body}"));
        }

        Ok(resp.text().await.context("failed to read diff response")?)
    }

    pub async fn get_pr_files(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<PrFile>> {
        let mut all_files = Vec::new();
        let mut page = 1u32;
        loop {
            let path =
                format!("/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}");
            let resp = self
                .request(Method::GET, &path)
                .send()
                .await
                .context("GitHub get PR files failed")?;
            self.update_rate_state(resp.headers());

            let has_next = response_has_next_page(resp.headers());

            let batch: Vec<PrFile> = handle_json(resp).await?;
            let batch_empty = batch.is_empty();
            all_files.extend(batch);

            let truncated = has_next && !batch_empty && page >= 10;
            if truncated {
                tracing::warn!(
                    owner = owner,
                    repo = repo,
                    number = number,
                    file_count = all_files.len(),
                    "PR files pagination hit 10-page cap; review may have partial coverage"
                );
            }
            if !has_next || batch_empty || page >= 10 {
                break;
            }
            page += 1;
        }

        Ok(all_files)
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

    pub async fn get_file_content_with_sha(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        reference: &str,
    ) -> Result<Option<(String, String)>> {
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

        let sha = payload
            .sha
            .ok_or_else(|| anyhow!("GitHub content payload missing sha for {path}"))?;
        let cleaned = payload.content.replace('\n', "");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .context("failed to decode base64 file payload")?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(Some((text, sha)))
    }

    pub async fn get_branch_head_sha(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<String> {
        let encoded_branch = urlencoding::encode(branch);
        let path = format!("/repos/{owner}/{repo}/git/ref/heads/{encoded_branch}");
        let resp = self
            .request(Method::GET, &path)
            .send()
            .await
            .context("GitHub get branch ref failed")?;
        self.update_rate_state(resp.headers());
        let payload: GitRefResponse = handle_json(resp).await?;
        Ok(payload.object.sha)
    }

    pub async fn create_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/git/refs");
        let payload = CreateRefRequest {
            ref_name: format!("refs/heads/{branch}"),
            sha: sha.to_string(),
        };
        let resp = self
            .request(Method::POST, &path)
            .json(&payload)
            .send()
            .await
            .context("GitHub create branch failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub create branch failed ({status}): {body}"));
        }

        Ok(())
    }

    pub async fn update_file_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        branch: &str,
        message: &str,
        content: &str,
        sha: Option<&str>,
    ) -> Result<()> {
        let encoded_path = path
            .split('/')
            .map(urlencoding::encode)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("/");
        let endpoint = format!("/repos/{owner}/{repo}/contents/{encoded_path}");
        let payload = UpdateContentRequest {
            message: message.to_string(),
            content: base64::engine::general_purpose::STANDARD.encode(content),
            branch: branch.to_string(),
            sha: sha.map(ToString::to_string),
        };
        let resp = self
            .request(Method::PUT, &endpoint)
            .json(&payload)
            .send()
            .await
            .context("GitHub update file content failed")?;
        self.update_rate_state(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GitHub update file content failed ({status}): {body}"
            ));
        }

        Ok(())
    }

    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<String> {
        let payload = CreatePullRequestRequest {
            title: title.to_string(),
            body: body.to_string(),
            head: head.to_string(),
            base: base.to_string(),
            draft: false,
        };
        let payload = self
            .create_pull_request_with_options(owner, repo, &payload)
            .await?;
        Ok(payload.html_url)
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
    ) -> Result<IssueComment> {
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

        handle_json(resp).await
    }

    pub async fn update_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}");
        let payload = UpdateIssueCommentRequest {
            body: body.to_string(),
        };
        let resp = self
            .request(Method::PATCH, &path)
            .json(&payload)
            .send()
            .await
            .context("GitHub update issue comment failed")?;
        self.update_rate_state(resp.headers());
        handle_json(resp).await
    }

    pub async fn create_pull_request_with_options(
        &self,
        owner: &str,
        repo: &str,
        request: &CreatePullRequestRequest,
    ) -> Result<CreatePullRequestResponse> {
        let path = format!("/repos/{owner}/{repo}/pulls");
        let resp = self
            .request(Method::POST, &path)
            .json(request)
            .send()
            .await
            .context("GitHub create pull request failed")?;
        self.update_rate_state(resp.headers());
        handle_json(resp).await
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

            let has_next = response_has_next_page(resp.headers());

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
                path.push_str(&urlencoding::encode(since));
            }
            let resp = self
                .request(Method::GET, &path)
                .send()
                .await
                .context("GitHub get review comments failed")?;
            self.update_rate_state(resp.headers());

            let has_next = response_has_next_page(resp.headers());

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

    pub async fn get_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<IssueComment>> {
        let mut all_comments = Vec::new();
        let mut page = 1u32;
        loop {
            let path = format!(
                "/repos/{owner}/{repo}/issues/{pr_number}/comments?per_page=100&page={page}"
            );
            let resp = self
                .request(Method::GET, &path)
                .send()
                .await
                .context("GitHub get issue comments failed")?;
            self.update_rate_state(resp.headers());

            let has_next = response_has_next_page(resp.headers());

            let batch: Vec<IssueComment> = handle_json(resp).await?;
            let batch_empty = batch.is_empty();
            all_comments.extend(batch);

            // Cap at 10 pages (consistent with other paginated endpoints)
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
        state.limit = headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());
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

pub fn synthesize_diff_from_files(files: &[PrFile]) -> String {
    let mut out = String::new();
    for file in files {
        let old_path = match &file.previous_filename {
            Some(prev) => prev.as_str(),
            None => file.filename.as_str(),
        };
        let new_path = file.filename.as_str();

        out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));

        match file.status.as_str() {
            "added" => {
                out.push_str("--- /dev/null\n");
                out.push_str(&format!("+++ b/{new_path}\n"));
            }
            "removed" => {
                out.push_str(&format!("--- a/{old_path}\n"));
                out.push_str("+++ /dev/null\n");
            }
            _ => {
                out.push_str(&format!("--- a/{old_path}\n"));
                out.push_str(&format!("+++ b/{new_path}\n"));
            }
        }

        match &file.patch {
            Some(patch) => {
                out.push_str(patch);
                if !patch.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => {
                // Binary or too-large file — emit an empty hunk so the file
                // still appears in ParsedDiff.files for content fetching.
            }
        }
    }
    out
}

fn response_has_next_page(headers: &HeaderMap) -> bool {
    headers
        .get(LINK)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("rel=\"next\""))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::diff_parser::{parse_unified_diff, DiffSide};

    fn make_file(
        filename: &str,
        status: &str,
        patch: Option<&str>,
        previous_filename: Option<&str>,
    ) -> PrFile {
        PrFile {
            filename: filename.to_string(),
            status: status.to_string(),
            additions: 0,
            deletions: 0,
            changes: 0,
            patch: patch.map(|s| s.to_string()),
            previous_filename: previous_filename.map(|s| s.to_string()),
        }
    }

    #[test]
    fn detects_next_page_link_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            r#"<https://api.github.com/repositories/1/pulls?page=2>; rel="next", <https://api.github.com/repositories/1/pulls?page=10>; rel="last""#
                .parse()
                .expect("valid link header"),
        );

        assert!(response_has_next_page(&headers));
    }

    #[test]
    fn no_next_page_without_next_relation() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            r#"<https://api.github.com/repositories/1/pulls?page=1>; rel="prev""#
                .parse()
                .expect("valid link header"),
        );

        assert!(!response_has_next_page(&headers));
    }

    #[test]
    fn synthesize_mixed_statuses_parses_correctly() {
        let files = vec![
            make_file(
                "src/new.rs",
                "added",
                Some("@@ -0,0 +1,3 @@\n+line1\n+line2\n+line3\n"),
                None,
            ),
            make_file(
                "src/lib.rs",
                "modified",
                Some("@@ -1,3 +1,4 @@\n line1\n-line2\n+line2 changed\n line3\n+line4\n"),
                None,
            ),
            make_file(
                "src/old.rs",
                "removed",
                Some("@@ -1,2 +0,0 @@\n-goodbye\n-world\n"),
                None,
            ),
        ];

        let diff = synthesize_diff_from_files(&files);
        let parsed = parse_unified_diff(&diff).expect("parse synthesized diff");

        assert_eq!(parsed.files.len(), 3);
        assert_eq!(parsed.files[0].new_path, "src/new.rs");
        assert_eq!(parsed.files[0].old_path, "/dev/null");
        assert_eq!(parsed.files[1].new_path, "src/lib.rs");
        assert_eq!(parsed.files[2].old_path, "src/old.rs");
        assert_eq!(parsed.files[2].new_path, "/dev/null");

        // Verify position mapping works for the modified file
        let pos = parsed
            .position_for("src/lib.rs", 2, DiffSide::Right)
            .expect("modified line should have position");
        assert!(pos.diff_position >= 1);
    }

    #[test]
    fn synthesize_rename_maps_paths() {
        let files = vec![make_file(
            "src/renamed.rs",
            "renamed",
            Some("@@ -1,2 +1,2 @@\n-old content\n+new content\n"),
            Some("src/original.rs"),
        )];

        let diff = synthesize_diff_from_files(&files);
        let parsed = parse_unified_diff(&diff).expect("parse renamed file diff");

        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].old_path, "src/original.rs");
        assert_eq!(parsed.files[0].new_path, "src/renamed.rs");
    }

    #[test]
    fn synthesize_missing_patch_does_not_panic() {
        let files = vec![
            make_file("binary.png", "modified", None, None),
            make_file(
                "src/ok.rs",
                "modified",
                Some("@@ -1 +1 @@\n-old\n+new\n"),
                None,
            ),
        ];

        let diff = synthesize_diff_from_files(&files);
        let parsed = parse_unified_diff(&diff).expect("parse diff with missing patch");

        // The binary file may or may not produce a FileDiff entry (no hunks),
        // but the valid file must parse correctly.
        let ok_file = parsed
            .files
            .iter()
            .find(|f| f.new_path == "src/ok.rs")
            .expect("src/ok.rs should be in parsed files");
        assert_eq!(ok_file.hunks.len(), 1);
    }
}
