pub mod crypto;
pub mod signet;

use anyhow::{anyhow, Result};

use crate::config::AppConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Signet,
    Encrypted,
    PlainText,
    Environment,
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signet => write!(f, "signet"),
            Self::Encrypted => write!(f, "encrypted config"),
            Self::PlainText => write!(f, "plain text config"),
            Self::Environment => write!(f, "GITHUB_TOKEN env var"),
        }
    }
}

/// Resolve the GitHub token from available sources, in priority order:
///
/// 1. Signet secret store (if signet is installed and has the secret)
/// 2. Encrypted token in config (double-layer AES-256-GCM)
/// 3. Plain-text token in config (legacy, emits warning)
/// 4. GITHUB_TOKEN environment variable
pub async fn resolve_github_token(config: &AppConfig) -> Result<(String, TokenSource)> {
    // 1. Signet secret
    match signet::get_token().await {
        Ok(Some(token)) => {
            tracing::debug!("resolved GitHub token from Signet secret store");
            return Ok((token, TokenSource::Signet));
        }
        Ok(None) => {}
        Err(err) => {
            tracing::debug!(error = %err, "signet token lookup failed, falling through");
        }
    }

    // 2. Encrypted config
    if let Some(ref encrypted) = config.github.encrypted_token {
        let passphrase = if config.github.passphrase_protected {
            // In daemon mode we can't prompt interactively.
            // Require PR_REVIEWER_PASSPHRASE env var for passphrase-protected tokens.
            let pp = std::env::var("PR_REVIEWER_PASSPHRASE").map_err(|_| {
                anyhow!(
                    "token is passphrase-protected; set PR_REVIEWER_PASSPHRASE env var \
                     or re-encrypt without --passphrase"
                )
            })?;
            Some(pp)
        } else {
            None
        };
        let token = crypto::decrypt_token(encrypted, passphrase.as_deref())?;
        tracing::debug!("resolved GitHub token from encrypted config");
        return Ok((token, TokenSource::Encrypted));
    }

    // 3. Plain-text config (legacy)
    if let Some(ref token) = config.github.token {
        tracing::warn!(
            "GitHub token stored in plain text in config; \
             run `pr-reviewer config set-token` to encrypt it"
        );
        return Ok((token.clone(), TokenSource::PlainText));
    }

    // 4. Environment variable
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        tracing::debug!("resolved GitHub token from GITHUB_TOKEN env var");
        return Ok((token, TokenSource::Environment));
    }

    Err(anyhow!(
        "GitHub token not found. Set it with one of:\n  \
         pr-reviewer config set-token --passphrase\n  \
         pr-reviewer config set-token --signet\n  \
         export GITHUB_TOKEN=ghp_..."
    ))
}

/// Determine which token source would be used, without actually decrypting.
/// Returns (source, masked_preview) or None if no token is available.
pub async fn token_status(config: &AppConfig) -> Option<(TokenSource, String)> {
    // Check signet first
    if let Ok(Some(token)) = signet::get_token().await {
        return Some((TokenSource::Signet, crypto::mask_token(&token)));
    }

    // Encrypted
    if config.github.encrypted_token.is_some() {
        let pp_status = if config.github.passphrase_protected {
            "passphrase-protected"
        } else {
            "machine-bound"
        };
        return Some((TokenSource::Encrypted, format!("[{pp_status}]")));
    }

    // Plain text
    if let Some(ref token) = config.github.token {
        return Some((TokenSource::PlainText, crypto::mask_token(token)));
    }

    // Env
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        return Some((TokenSource::Environment, crypto::mask_token(&token)));
    }

    None
}
