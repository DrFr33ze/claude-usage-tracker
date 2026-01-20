//! API client for Claude usage endpoint.
//!
//! Handles HTTP communication with the Anthropic OAuth usage API,
//! including retry logic with exponential backoff and jitter.
//!
//! ## Type Safety Strategy
//!
//! This module uses [`Utilization`] for API response values:
//!
//! - **[`Utilization`]**: Clamps invalid values to [0, 100] range. Used for API responses
//!   where we must handle potentially malformed data gracefully without failing.
//!
//! In contrast, [`crate::config::Percentage`] is used for configuration:
//!
//! - **`Percentage`**: Rejects invalid values, returning `None`. Used in configuration
//!   to catch errors early during deserialization.
//!
//! This distinction ensures:
//! - Config errors are caught at startup (fail fast)
//! - API quirks don't crash the application (defensive handling)
//!
//! ## Usage
//!
//! ```rust,ignore
//! use api::{build_http_client, fetch_usage};
//!
//! let client = build_http_client();
//! let result = fetch_usage(&token, &client).await;
//! ```
//!
//! ## Endpoint Configuration
//!
//! The endpoint URL can be overridden via the `CLAUDE_USAGE_API_ENDPOINT`
//! environment variable for testing purposes.
//!
//! ## Retry Behavior
//!
//! - Uses exponential backoff: 1s, 2s, 4s, ... capped at 60s
//! - Adds random jitter (0-25%) to prevent thundering herd
//! - Retries on server errors (5xx) and network failures
//! - Does NOT retry on client errors (4xx) except 429

use chrono::{DateTime, Utc};
use serde::Deserialize;
#[cfg(not(test))]
use std::sync::OnceLock;
use tokio::time::{sleep, Duration};

// HTTP client configuration constants
const HTTP_DEFAULT_USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const HTTP_ENDPOINT_ENV_VAR: &str = "CLAUDE_USAGE_API_ENDPOINT";
const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;

// Retry logic constants for exponential backoff
const RETRY_MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;
const RETRY_MAX_DELAY_MS: u64 = 60_000;
const RETRY_JITTER_PERCENT: u8 = 25;
const RETRY_MAX_SHIFT_BITS: u32 = 63;

/// Cached endpoint URL (initialized once on first access, production only)
#[cfg(not(test))]
static ENDPOINT: OnceLock<String> = OnceLock::new();

// ============================================================================
// Structs & Enums
// ============================================================================

/// API error types
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Failed to parse JSON response: {0}")]
    ParseError(String),

    #[error("HTTP error: {status} - {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("Authentication failed (401). Token may be expired.")]
    Unauthorized,

    #[error("Rate limited (429). Retry after {retry_after:?} seconds.")]
    RateLimited {
        /// Server-specified retry delay in seconds, if provided.
        retry_after: Option<u64>,
    },

    #[error("Server error ({0}). Will retry...")]
    ServerError(u16),
}

/// API response types matching the Anthropic OAuth usage endpoint
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct UsageResponse {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
}

/// A single usage window from the API response.
///
/// # Utilization Scale
///
/// The Claude API returns `utilization` as a 0-100 percentage (e.g., `75.0` for 75%).
/// This matches the threshold comparison logic in `config.rs`.
///
/// # Timestamp Parsing
///
/// The `resets_at` field is automatically parsed from RFC3339 format during deserialization,
/// avoiding repeated parsing at display time.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Usage percentage (0-100 scale, validated via Utilization newtype)
    pub utilization: Utilization,

    /// Reset timestamp in UTC (parsed from RFC3339 during deserialization)
    #[serde(default, deserialize_with = "deserialize_reset_time")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// A validated utilization percentage in the range [0, 100].
///
/// Provides type safety for utilization values and ensures validity invariants.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Utilization(f64);

impl Utilization {
    /// Creates a new Utilization, clamping to valid range [0, 100].
    ///
    /// NaN values are treated as zero.
    #[must_use]
    pub fn new(value: f64) -> Self {
        if value.is_nan() {
            return Self(0.0);
        }
        Self(value.clamp(0.0, 100.0))
    }

    /// Returns the underlying f64 value.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl Default for Utilization {
    fn default() -> Self {
        Self(0.0)
    }
}

impl<'de> serde::Deserialize<'de> for Utilization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

// ============================================================================
// Functions
// ============================================================================

/// Get the API endpoint URL (can be overridden via `CLAUDE_USAGE_API_ENDPOINT` env var)
///
/// In production, uses `OnceLock` to cache the value on first access, returning a clone.
/// In tests, reads the env var each time to support per-test mock servers.
#[cfg(not(test))]
fn get_usage_endpoint() -> String {
    ENDPOINT
        .get_or_init(|| {
            std::env::var(HTTP_ENDPOINT_ENV_VAR)
                .unwrap_or_else(|_| HTTP_DEFAULT_USAGE_ENDPOINT.to_string())
        })
        .clone()
}

/// Test version: reads env var each time to support per-test mock servers
#[cfg(test)]
fn get_usage_endpoint() -> String {
    std::env::var(HTTP_ENDPOINT_ENV_VAR).unwrap_or_else(|_| HTTP_DEFAULT_USAGE_ENDPOINT.to_string())
}

/// Calculate delay with exponential backoff, max cap, and jitter
///
/// Formula: `min(base * 2^attempt, max) + random_jitter`
fn calculate_retry_delay(attempt: u32) -> u64 {
    // Exponential backoff: base * 2^(attempt-1), capped at max
    let exp_delay = RETRY_BASE_DELAY_MS
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(RETRY_MAX_SHIFT_BITS));
    let capped_delay = exp_delay.min(RETRY_MAX_DELAY_MS);

    // Add jitter (0 to jitter_percent% of delay)
    if RETRY_JITTER_PERCENT > 0 {
        let jitter_range = capped_delay * u64::from(RETRY_JITTER_PERCENT) / 100;
        let jitter = fastrand::u64(0..=jitter_range);
        capped_delay.saturating_add(jitter)
    } else {
        capped_delay
    }
}

/// Custom deserializer for RFC3339 timestamp strings
///
/// Parses the `resets_at` field from the API response, converting RFC3339 timestamp strings
/// to `DateTime<Utc>` during deserialization. This avoids repeated parsing at display time
/// and ensures consistent timezone handling (always UTC).
///
/// Lenient: Invalid/malformed timestamps are logged with a cooldown (1 hour)
/// and treated as `None`, allowing the API fetch to succeed despite malformed timestamps.
/// Re-logging occurs after the cooldown period to ensure users are aware of ongoing issues.
fn deserialize_reset_time<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::UNIX_EPOCH;

    /// Cooldown period before re-logging invalid timestamps (1 hour)
    const LOG_COOLDOWN_SECS: u64 = 3600;

    /// Stores the timestamp (seconds since UNIX_EPOCH) of the last log
    static LAST_LOGGED_TS: AtomicU64 = AtomicU64::new(0);

    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(s) => {
            match DateTime::parse_from_rfc3339(&s) {
                Ok(dt) => Ok(Some(dt.with_timezone(&Utc))),
                Err(_) => {
                    // Check if enough time has passed since the last log
                    let now = std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let last_logged = LAST_LOGGED_TS.load(Ordering::Relaxed);

                    if now.saturating_sub(last_logged) >= LOG_COOLDOWN_SECS {
                        // Attempt to claim the log right using compare_exchange
                        if LAST_LOGGED_TS
                            .compare_exchange(last_logged, now, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                        {
                            log::warn!("Invalid reset timestamp '{s}' - treating as no reset");
                        }
                    }
                    Ok(None)
                }
            }
        }
    }
}

/// Build an HTTP client configured for API requests.
///
/// Creates a client with connection pooling enabled, custom user agent,
/// and configured timeouts. Called once at startup.
///
/// # Panics
/// Panics if HTTP client cannot be created with required timeout configuration.
#[must_use]
pub fn build_http_client() -> reqwest::Client {
    let user_agent = format!("claude-usage-tracker/{}", env!("CARGO_PKG_VERSION"));
    let timeout = Duration::from_secs(HTTP_TIMEOUT_SECS);
    let connect_timeout = Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS);

    // Build client with required timeouts
    let client = reqwest::Client::builder()
        .user_agent(&user_agent)
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .build();

    match client {
        Ok(c) => c,
        Err(e) => {
            // Fall back to client with just timeout (no custom user agent)
            log::warn!("Failed to create HTTP client with user agent: {e}, trying minimal config");
            reqwest::Client::builder()
                .timeout(timeout)
                .connect_timeout(connect_timeout)
                .build()
                .expect("Failed to create HTTP client with required timeouts")
        }
    }
}

/// Fetch usage data with retry logic and exponential backoff
///
/// Uses exponential backoff with jitter to prevent thundering herd:
/// - Base delay doubles each attempt (1s, 2s, 4s, ...)
/// - Delay is capped at 60 seconds
/// - Random jitter (0 to 25%) is added to each delay
#[must_use = "this returns the usage data which should be processed"]
pub async fn fetch_usage(
    access_token: &str,
    client: &reqwest::Client,
) -> Result<UsageResponse, ApiError> {
    let endpoint = get_usage_endpoint();
    for attempt in 1..=RETRY_MAX_RETRIES {
        match client
            .get(&endpoint)
            .header("Authorization", format!("Bearer {access_token}"))
            // Required for OAuth token authentication (vs API key)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();

                if status.is_success() {
                    match response.json().await {
                        Ok(usage) => {
                            log::info!("Successfully fetched usage data");
                            log::debug!("Usage response: {usage:?}");
                            return Ok(usage);
                        }
                        Err(e) => {
                            log::error!("Failed to parse usage response: {e}");
                            return Err(ApiError::ParseError(e.to_string()));
                        }
                    }
                }

                // Handle specific error codes
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    log::error!("Authentication failed (401) - token may be expired");
                    return Err(ApiError::Unauthorized);
                }

                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    log::warn!(
                        "Rate limited (429) - retry after {} seconds",
                        retry_after.map_or("default".to_string(), |s| s.to_string())
                    );

                    return Err(ApiError::RateLimited { retry_after });
                }

                if status.is_server_error() {
                    if attempt < RETRY_MAX_RETRIES {
                        let body = response.text().await.unwrap_or_default();
                        let delay = calculate_retry_delay(attempt);
                        log::warn!(
                            "Server error {}, retrying ({}/{}) after {}ms: {}",
                            status.as_u16(),
                            attempt,
                            RETRY_MAX_RETRIES,
                            delay,
                            body
                        );
                        sleep(Duration::from_millis(delay)).await;
                        continue;
                    }
                    // Server error after exhausting retries
                    log::error!(
                        "Server error {} after {} retries",
                        status.as_u16(),
                        RETRY_MAX_RETRIES
                    );
                    return Err(ApiError::ServerError(status.as_u16()));
                }

                // Non-retryable client error (4xx other than 401, 429)
                let body = response.text().await.unwrap_or_default();
                log::error!("HTTP error {status}: {body}");
                return Err(ApiError::Http { status, body });
            }
            Err(e) if attempt < RETRY_MAX_RETRIES => {
                let delay = calculate_retry_delay(attempt);
                log::warn!(
                    "Request failed, retrying ({attempt}/{RETRY_MAX_RETRIES}) after {delay}ms: {e}"
                );
                sleep(Duration::from_millis(delay)).await;
            }
            Err(e) => {
                log::error!("Request failed after {RETRY_MAX_RETRIES} retries: {e}");
                return Err(ApiError::Network(e));
            }
        }
    }

    // SAFETY: This is unreachable because:
    // 1. MAX_RETRIES is >= 1 (const = 3)
    // 2. The loop always executes at least once since 1..=MAX_RETRIES is non-empty
    // 3. Every match arm either returns or continues (only when attempt < MAX_RETRIES)
    // 4. On the final iteration (attempt == MAX_RETRIES), all arms return
    unreachable!("All retry loop iterations return before reaching this point")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod utilization_tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < f64::EPSILON
    }

    #[test]
    fn test_utilization_clamps_negative() {
        assert!(approx_eq(Utilization::new(-10.0).as_f64(), 0.0));
    }

    #[test]
    fn test_utilization_clamps_over_100() {
        assert!(approx_eq(Utilization::new(150.0).as_f64(), 100.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < f64::EPSILON
    }

    #[test]
    fn test_usage_response_parsing_with_resets_at() {
        // Test complete usage response with resets_at field
        let json = r#"{
            "five_hour": {"utilization": 75.0, "resets_at": "2025-12-30T12:00:00Z"},
            "seven_day": {"utilization": 50.0},
            "seven_day_opus": {"utilization": 25.0, "resets_at": "2025-12-31T00:00:00Z"},
            "seven_day_sonnet": {"utilization": 10.0}
        }"#;

        let usage: UsageResponse = serde_json::from_str(json).unwrap();
        assert!(approx_eq(
            usage.five_hour.as_ref().unwrap().utilization.as_f64(),
            75.0
        ));
        assert!(usage.five_hour.as_ref().unwrap().resets_at.is_some());
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.unwrap().year(),
            2025
        );
    }

    #[test]
    fn test_invalid_reset_timestamp_returns_none() {
        // Invalid timestamp should be handled gracefully, returning None
        let json = r#"{
            "five_hour": {"utilization": 75.0, "resets_at": "invalid-timestamp"},
            "seven_day": {"utilization": 50.0},
            "seven_day_opus": {"utilization": 25.0},
            "seven_day_sonnet": {"utilization": 10.0}
        }"#;

        let usage: UsageResponse = serde_json::from_str(json).unwrap();
        assert!(approx_eq(
            usage.five_hour.as_ref().unwrap().utilization.as_f64(),
            75.0
        ));
        // Invalid timestamp results in None for resets_at
        assert!(usage.five_hour.as_ref().unwrap().resets_at.is_none());
    }

    #[test]
    fn test_valid_reset_timestamp_parses_correctly() {
        let json = r#"{
            "five_hour": {"utilization": 75.0, "resets_at": "2025-12-30T12:00:00Z"},
            "seven_day": {"utilization": 50.0},
            "seven_day_opus": {"utilization": 25.0},
            "seven_day_sonnet": {"utilization": 10.0}
        }"#;

        let usage: UsageResponse = serde_json::from_str(json).unwrap();
        assert!(usage.five_hour.as_ref().unwrap().resets_at.is_some());
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.unwrap().year(),
            2025
        );
    }

    #[test]
    fn test_missing_reset_timestamp_field() {
        // Missing resets_at field should result in None
        let json = r#"{
            "five_hour": {"utilization": 75.0},
            "seven_day": {"utilization": 50.0},
            "seven_day_opus": {"utilization": 25.0},
            "seven_day_sonnet": {"utilization": 10.0}
        }"#;

        let usage: UsageResponse = serde_json::from_str(json).unwrap();
        assert!(usage.five_hour.as_ref().unwrap().resets_at.is_none());
    }

    #[test]
    fn test_malformed_json() {
        let json = r#"{"five_hour": not_valid_json}"#;
        let result: Result<UsageResponse, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Should fail on malformed JSON");
    }
}
