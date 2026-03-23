use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::Engine;

use crate::config::AppConfig;

const BLOB_VERSION: u8 = 0x01;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Returns the path to the machine-bound keyfile.
pub fn keyfile_path() -> Result<PathBuf> {
    Ok(AppConfig::config_dir()?.join("keyfile"))
}

/// Ensures the keyfile exists, creating it with a random 32-byte key if missing.
/// Uses O_EXCL (create_new) to avoid TOCTOU races between concurrent writers.
/// Returns the 32-byte key.
pub fn ensure_keyfile() -> Result<[u8; KEY_LEN]> {
    let path = keyfile_path()?;

    // Try to create the keyfile atomically first
    let mut key = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
    match write_keyfile_exclusive(&path, &key) {
        Ok(()) => return Ok(key),
        Err(err) => {
            // Only fall through to reading if the file actually exists now;
            // otherwise propagate the real error (permission denied, disk full, etc.)
            if !path.exists() {
                return Err(err);
            }
            let data = fs::read(&path).context("failed to read keyfile")?;
            check_keyfile_permissions(&path);
            if data.len() != KEY_LEN {
                return Err(anyhow!(
                    "keyfile is corrupt ({} bytes, expected {})",
                    data.len(),
                    KEY_LEN
                ));
            }
            key.copy_from_slice(&data);
            Ok(key)
        }
    }
}

/// Atomically create the keyfile using O_EXCL to prevent TOCTOU races.
/// Returns Err if the file already exists.
fn write_keyfile_exclusive(path: &Path, key: &[u8; KEY_LEN]) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create keyfile directory")?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_EXCL: fail if file exists
        .open(path)
        .context("failed to create keyfile (may already exist)")?;
    file.write_all(key).context("failed to write keyfile")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .context("failed to set keyfile permissions")?;
    }
    Ok(())
}

fn check_keyfile_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{:o}", mode),
                    "keyfile has relaxed permissions; expected 0600"
                );
            }
        }
    }
}

/// Derive a 32-byte key from a passphrase using Argon2id.
fn derive_passphrase_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let params = Params::new(65536, 3, 1, Some(KEY_LEN))
        .map_err(|e| anyhow!("failed to create argon2 params: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2 key derivation failed: {e}"))?;
    Ok(key)
}

/// Derive a 32-byte key from machine identity (fallback when no passphrase).
/// Uses /etc/machine-id (Linux) or IOPlatformUUID (macOS) combined with username.
fn derive_machine_key(salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let machine_id = read_machine_id()?;
    let username = whoami();
    let combined = format!("pr-reviewer:{}:{}", machine_id, username);
    derive_passphrase_key(&combined, salt)
}

fn read_machine_id() -> Result<String> {
    // Linux
    if let Ok(id) = fs::read_to_string("/etc/machine-id") {
        let trimmed = id.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    // macOS fallback
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .context("failed to run ioreg")?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if line.contains("IOPlatformUUID") {
                if let Some(uuid) = line.split('"').nth(3) {
                    return Ok(uuid.to_string());
                }
            }
        }
    }
    Err(anyhow!(
        "could not determine machine identity; use --passphrase for token encryption"
    ))
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Encrypt a token with double-layer AES-256-GCM.
///
/// Layer 2 (inner): passphrase-derived or machine-derived key.
/// Layer 1 (outer): machine-bound keyfile.
///
/// Returns a base64-encoded blob.
pub fn encrypt_token(token: &str, passphrase: Option<&str>) -> Result<String> {
    let keyfile_key = ensure_keyfile()?;

    // Generate random salt and nonces
    let mut salt = [0u8; SALT_LEN];
    let mut l2_nonce_bytes = [0u8; NONCE_LEN];
    let mut l1_nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut l2_nonce_bytes);
    OsRng.fill_bytes(&mut l1_nonce_bytes);

    // Layer 2: inner encryption
    let l2_key = match passphrase {
        Some(pp) => derive_passphrase_key(pp, &salt)?,
        None => derive_machine_key(&salt)?,
    };
    let l2_cipher = Aes256Gcm::new_from_slice(&l2_key)
        .map_err(|e| anyhow!("failed to create L2 cipher: {e}"))?;
    let l2_nonce = Nonce::from_slice(&l2_nonce_bytes);
    let l2_ciphertext = l2_cipher
        .encrypt(l2_nonce, token.as_bytes())
        .map_err(|e| anyhow!("L2 encryption failed: {e}"))?;

    // Layer 1: outer encryption
    let l1_cipher = Aes256Gcm::new_from_slice(&keyfile_key)
        .map_err(|e| anyhow!("failed to create L1 cipher: {e}"))?;
    let l1_nonce = Nonce::from_slice(&l1_nonce_bytes);
    let l1_ciphertext = l1_cipher
        .encrypt(l1_nonce, l2_ciphertext.as_ref())
        .map_err(|e| anyhow!("L1 encryption failed: {e}"))?;

    // Assemble blob: version + salt + l2_nonce + l1_nonce + ciphertext
    let mut blob = Vec::with_capacity(1 + SALT_LEN + NONCE_LEN * 2 + l1_ciphertext.len());
    blob.push(BLOB_VERSION);
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&l2_nonce_bytes);
    blob.extend_from_slice(&l1_nonce_bytes);
    blob.extend_from_slice(&l1_ciphertext);

    Ok(base64::engine::general_purpose::STANDARD.encode(&blob))
}

/// Decrypt a base64-encoded double-encrypted token blob.
pub fn decrypt_token(encoded: &str, passphrase: Option<&str>) -> Result<String> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("failed to decode encrypted token (invalid base64)")?;

    let min_len = 1 + SALT_LEN + NONCE_LEN * 2;
    if blob.len() < min_len {
        return Err(anyhow!("encrypted token blob too short"));
    }

    let version = blob[0];
    if version != BLOB_VERSION {
        return Err(anyhow!(
            "unsupported encrypted token version: {version} (expected {BLOB_VERSION})"
        ));
    }

    let salt = &blob[1..1 + SALT_LEN];
    let l2_nonce_bytes = &blob[1 + SALT_LEN..1 + SALT_LEN + NONCE_LEN];
    let l1_nonce_bytes = &blob[1 + SALT_LEN + NONCE_LEN..1 + SALT_LEN + NONCE_LEN * 2];
    let l1_ciphertext = &blob[1 + SALT_LEN + NONCE_LEN * 2..];

    // Layer 1: outer decryption
    let keyfile_key = ensure_keyfile()?;
    let l1_cipher = Aes256Gcm::new_from_slice(&keyfile_key)
        .map_err(|e| anyhow!("failed to create L1 cipher: {e}"))?;
    let l1_nonce = Nonce::from_slice(l1_nonce_bytes);
    let l2_ciphertext = l1_cipher
        .decrypt(l1_nonce, l1_ciphertext)
        .map_err(|_| anyhow!("L1 decryption failed (keyfile may have changed)"))?;

    // Layer 2: inner decryption
    let l2_key = match passphrase {
        Some(pp) => derive_passphrase_key(pp, salt)?,
        None => derive_machine_key(salt)?,
    };
    let l2_cipher = Aes256Gcm::new_from_slice(&l2_key)
        .map_err(|e| anyhow!("failed to create L2 cipher: {e}"))?;
    let l2_nonce = Nonce::from_slice(l2_nonce_bytes);
    let plaintext = l2_cipher
        .decrypt(l2_nonce, l2_ciphertext.as_ref())
        .map_err(|_| {
            if passphrase.is_some() {
                anyhow!("L2 decryption failed (wrong passphrase?)")
            } else {
                anyhow!("L2 decryption failed (machine identity may have changed)")
            }
        })?;

    String::from_utf8(plaintext).context("decrypted token is not valid UTF-8")
}

/// Validate that a token looks like a GitHub token.
pub fn validate_token_format(token: &str) -> Result<()> {
    let prefixes = ["ghp_", "gho_", "ghs_", "ghu_", "github_pat_"];
    if prefixes.iter().any(|p| token.starts_with(p)) {
        Ok(())
    } else {
        Err(anyhow!(
            "token does not match known GitHub token formats (ghp_, gho_, ghs_, ghu_, github_pat_); \
             use at your own risk or check that you copied the full token"
        ))
    }
}

/// Mask a token for display: show first 4 and last 4 characters.
pub fn mask_token(token: &str) -> String {
    if token.len() <= 8 {
        return "*".repeat(token.len());
    }
    format!("{}...{}", &token[..4], &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env var mutations must be serialized since they affect global state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn round_trip_with_passphrase() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PR_REVIEWER_CONFIG_DIR", tmp.path().to_str().unwrap());

        let token = "ghp_test1234567890abcdef";
        let encrypted = encrypt_token(token, Some("mypassphrase")).unwrap();
        let decrypted = decrypt_token(&encrypted, Some("mypassphrase")).unwrap();
        assert_eq!(token, decrypted);

        // Wrong passphrase should fail
        assert!(decrypt_token(&encrypted, Some("wrong")).is_err());

        std::env::remove_var("PR_REVIEWER_CONFIG_DIR");
    }

    #[test]
    fn round_trip_without_passphrase() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PR_REVIEWER_CONFIG_DIR", tmp.path().to_str().unwrap());

        let token = "ghp_machine_derived_test";
        let encrypted = encrypt_token(token, None).unwrap();
        let decrypted = decrypt_token(&encrypted, None).unwrap();
        assert_eq!(token, decrypted);

        std::env::remove_var("PR_REVIEWER_CONFIG_DIR");
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PR_REVIEWER_CONFIG_DIR", tmp.path().to_str().unwrap());

        let token = "ghp_nonce_test";
        let e1 = encrypt_token(token, Some("pass")).unwrap();
        let e2 = encrypt_token(token, Some("pass")).unwrap();
        assert_ne!(e1, e2);

        // Both should decrypt correctly
        assert_eq!(decrypt_token(&e1, Some("pass")).unwrap(), token);
        assert_eq!(decrypt_token(&e2, Some("pass")).unwrap(), token);

        std::env::remove_var("PR_REVIEWER_CONFIG_DIR");
    }

    #[test]
    fn validate_token_format_ok() {
        assert!(validate_token_format("ghp_abc123").is_ok());
        assert!(validate_token_format("github_pat_xyz").is_ok());
        assert!(validate_token_format("gho_test").is_ok());
    }

    #[test]
    fn validate_token_format_bad() {
        assert!(validate_token_format("not_a_token").is_err());
        assert!(validate_token_format("").is_err());
    }

    #[test]
    fn mask_token_works() {
        assert_eq!(mask_token("ghp_1234567890abcdef"), "ghp_...cdef");
        assert_eq!(mask_token("short"), "*****");
    }
}
