//! Authentication and credentials management.
//!
//! Handles loading and parsing Claude API credentials from the
//! OAuth credentials file (`~/.claude/.credentials.json`).
//!
//! ## Credentials File Format
//!
//! The credentials file uses a nested structure with a `claudeAiOauth` wrapper:
//!
//! ```json
//! {
//!   "claudeAiOauth": {
//!     "accessToken": "sk-ant-...",
//!     ...
//!   }
//! }
//! ```
//!
//! Key fields:
//! - `accessToken`: OAuth access token (format: `sk-ant-...`)
//!
//! ## Token Security
//!
//! Tokens are never logged in any form for security reasons.
//!
//! ## Platform Notes
//!
//! - **Windows**: Uses `%USERPROFILE%\\.claude\\.credentials.json`
//! - **Linux/macOS**: Uses `~/.claude/.credentials.json`

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// Auth context error types
#[derive(thiserror::Error, Debug)]
pub enum AuthContextError {
    #[error("Credentials file not found at {0}. Please run 'claude auth login' first.")]
    NotFound(PathBuf),

    #[error("Failed to read credentials: {0}")]
    ReadError(String),

    #[error("Failed to parse credentials JSON: {0}")]
    ParseError(String),

    #[error("Missing required field: {0}")]
    MissingField(&'static str),
}

/// CRITICAL: Nested structure matching actual credentials file format
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: CredentialsInner,
}

/// Internal struct for deserialization
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialsInner {
    access_token: String,
}

/// Credentials loaded from Claude's `~/.claude/.credentials.json` file.
///
/// Note: Clone is intentionally not derived to prevent accidental token duplication.
pub struct Credentials {
    access_token: SecretString,
}

impl Credentials {
    /// Access the token securely - only expose when actually needed for API calls
    pub fn access_token(&self) -> &str {
        self.access_token.expose_secret()
    }

    /// Create test credentials for integration testing.
    ///
    /// # WARNING
    ///
    /// This should ONLY be used in tests. Never use real tokens in tests.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing(token: &str) -> Self {
        Self {
            access_token: SecretString::from(token.to_string()),
        }
    }
}

/// Custom Debug implementation that redacts sensitive tokens
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"[REDACTED]")
            .finish()
    }
}

/// Manual Clone implementation to ensure intentional duplication of sensitive data.
/// This makes credential cloning explicit rather than using `#[derive(Clone)]`.
impl Clone for Credentials {
    fn clone(&self) -> Self {
        Self {
            access_token: SecretString::from(self.access_token.expose_secret().to_owned()),
        }
    }
}

/// Load credentials from the standard Claude credentials file
#[must_use = "this returns credentials that should be used or error handled"]
pub fn load_credentials() -> Result<Credentials, AuthContextError> {
    let path = get_credentials_path().map_err(|e| AuthContextError::ReadError(e.to_string()))?;

    if !path.exists() {
        return Err(AuthContextError::NotFound(path));
    }

    // Check file permissions (warn if world-readable)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        const WORLD_READABLE_BIT: u32 = 0o004;
        let metadata =
            fs::metadata(&path).map_err(|e| AuthContextError::ReadError(e.to_string()))?;
        let mode = metadata.permissions().mode();
        if mode & WORLD_READABLE_BIT != 0 {
            log::warn!(
                "Credentials file is world-readable (mode {:o})",
                mode & 0o777
            );
        }
    }

    let content =
        fs::read_to_string(&path).map_err(|e| AuthContextError::ReadError(e.to_string()))?;

    // CRITICAL: Parse with nested claudeAiOauth wrapper
    let creds_file: CredentialsFile =
        serde_json::from_str(&content).map_err(|e| AuthContextError::ParseError(e.to_string()))?;

    let inner = creds_file.claude_ai_oauth;

    // Validate required fields
    if inner.access_token.is_empty() {
        return Err(AuthContextError::MissingField("accessToken"));
    }

    let creds = Credentials {
        access_token: SecretString::from(inner.access_token),
    };

    log::debug!("Successfully loaded credentials");

    Ok(creds)
}

/// Get the path to the credentials file (platform-specific)
pub fn get_credentials_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let userprofile =
            std::env::var("USERPROFILE").context("USERPROFILE environment variable not set")?;
        Ok(PathBuf::from(userprofile)
            .join(".claude")
            .join(".credentials.json"))
    }
    #[cfg(not(windows))]
    {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        Ok(home.join(".claude").join(".credentials.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_credentials_parsing() {
        // Test happy path: valid credentials JSON is parsed correctly
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-test123"
            }
        }"#;

        let result = serde_json::from_str::<CredentialsFile>(json);
        assert!(
            result.is_ok(),
            "Valid credentials JSON should parse successfully"
        );

        let creds_file = result.unwrap();
        assert_eq!(creds_file.claude_ai_oauth.access_token, "sk-ant-test123");
    }
}
