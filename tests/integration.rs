//! Integration tests for the Claude Usage Tracker.
//!
//! These tests verify the full system works together by mocking the Claude API.
//! They cover only end-to-end scenarios that unit tests cannot: actual HTTP
//! requests with mocked servers, retry behavior, and error handling.
//!
//! NOTE: These tests use the #[serial] attribute for automatic serialization
//! and can be run with: cargo test --test integration

use std::sync::Arc;

use serial_test::serial;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use claude_usage_tracker::api::{build_http_client, fetch_usage, ApiError};
use claude_usage_tracker::auth::Credentials;
use claude_usage_tracker::config::{Config, Percentage};
use claude_usage_tracker::AppState;

/// Environment variable name for overriding the API endpoint
const HTTP_ENDPOINT_ENV_VAR: &str = "CLAUDE_USAGE_API_ENDPOINT";

/// RAII guard for environment variables. Cleans up the env var on drop,
/// ensuring cleanup even if the test panics.
struct EnvGuard {
    key: &'static str,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        std::env::set_var(key, value);
        Self { key }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.key);
    }
}

/// Helper to create test credentials
fn make_test_credentials(token: &str) -> Credentials {
    Credentials::for_testing(token)
}

/// Helper to create a test config
fn make_test_config() -> Config {
    Config {
        warning_threshold: Percentage::new(75.0).unwrap(),
        critical_threshold: Percentage::new(90.0).unwrap(),
        reset_threshold: Percentage::new(50.0).unwrap(),
        polling_interval_minutes: 1,
        notification_cooldown_minutes: 5,
    }
}

/// Integration test: Successful API fetch returns valid usage data.
///
/// Verifies the complete happy path: mock server -> HTTP client -> parsed response.
#[tokio::test]
#[serial]
async fn test_successful_fetch_returns_valid_usage() {
    let mock_server = MockServer::start().await;

    let usage_json = r#"{
        "five_hour": {"utilization": 65.0, "resets_at": "2025-12-30T12:00:00Z"},
        "seven_day": {"utilization": 45.0, "resets_at": "2025-12-31T00:00:00Z"},
        "seven_day_opus": {"utilization": 30.0},
        "seven_day_sonnet": {"utilization": 20.0}
    }"#;

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::from_str::<serde_json::Value>(usage_json).unwrap()),
        )
        .mount(&mock_server)
        .await;

    let _env_guard = EnvGuard::set(HTTP_ENDPOINT_ENV_VAR, &mock_server.uri());

    let config = make_test_config();
    let http_client = build_http_client();
    let cancel_token = CancellationToken::new();
    let state = Arc::new(AppState::new(config, http_client, cancel_token));

    let creds = make_test_credentials("test-token-123");
    *state.credentials.lock().unwrap() = Some(Arc::new(creds));

    let stored_creds = state.credentials.lock().unwrap().as_ref().unwrap().clone();
    let result = fetch_usage(stored_creds.access_token(), &state.http_client).await;

    assert!(
        result.is_ok(),
        "API fetch should succeed with mocked response"
    );

    let usage = result.unwrap();
    assert_eq!(usage.five_hour.as_ref().unwrap().utilization.as_f64(), 65.0);
    assert_eq!(usage.seven_day.as_ref().unwrap().utilization.as_f64(), 45.0);
}

/// Integration test: API 401 Unauthorized returns correct error type.
///
/// Verifies that expired/invalid tokens are properly detected.
#[tokio::test]
#[serial]
async fn test_api_401_returns_unauthorized_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized"
        })))
        .mount(&mock_server)
        .await;

    let _env_guard = EnvGuard::set(HTTP_ENDPOINT_ENV_VAR, &mock_server.uri());

    let config = make_test_config();
    let http_client = build_http_client();
    let cancel_token = CancellationToken::new();
    let state = Arc::new(AppState::new(config, http_client, cancel_token));

    let creds = make_test_credentials("expired-token");
    *state.credentials.lock().unwrap() = Some(Arc::new(creds));

    let stored_creds = state.credentials.lock().unwrap().as_ref().unwrap().clone();
    let result = fetch_usage(stored_creds.access_token(), &state.http_client).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ApiError::Unauthorized));
}

/// Integration test: API 429 Rate Limited returns error with Retry-After value.
///
/// Verifies that rate limiting is properly detected and Retry-After header is parsed.
#[tokio::test]
#[serial]
async fn test_api_429_returns_rate_limited_with_retry_after() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "60")
                .set_body_json(serde_json::json!({"error": "rate_limited"})),
        )
        .mount(&mock_server)
        .await;

    let _env_guard = EnvGuard::set(HTTP_ENDPOINT_ENV_VAR, &mock_server.uri());

    let config = make_test_config();
    let http_client = build_http_client();
    let cancel_token = CancellationToken::new();
    let state = Arc::new(AppState::new(config, http_client, cancel_token));

    let creds = make_test_credentials("any-token");
    *state.credentials.lock().unwrap() = Some(Arc::new(creds));

    let stored_creds = state.credentials.lock().unwrap().as_ref().unwrap().clone();
    let result = fetch_usage(stored_creds.access_token(), &state.http_client).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        ApiError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(60));
        }
        _ => panic!("Expected ApiError::RateLimited"),
    }
}

/// Integration test: Server 500 errors trigger automatic retry and succeed.
///
/// Verifies the HTTP client's built-in retry logic for transient server errors.
#[tokio::test]
#[serial]
#[ignore] // Slow test due to retry delays
async fn test_api_500_retries_and_succeeds() {
    let mock_server = MockServer::start().await;

    // First two requests fail with 500
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "error": "internal_server_error"
        })))
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;

    // Third request succeeds
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "five_hour": {"utilization": 50.0}
        })))
        .mount(&mock_server)
        .await;

    let _env_guard = EnvGuard::set(HTTP_ENDPOINT_ENV_VAR, &mock_server.uri());

    let config = make_test_config();
    let http_client = build_http_client();
    let cancel_token = CancellationToken::new();
    let state = Arc::new(AppState::new(config, http_client, cancel_token));

    let creds = make_test_credentials("any-token");
    *state.credentials.lock().unwrap() = Some(Arc::new(creds));

    let stored_creds = state.credentials.lock().unwrap().as_ref().unwrap().clone();
    let result = fetch_usage(stored_creds.access_token(), &state.http_client).await;

    assert!(result.is_ok(), "API fetch should succeed after retries");
    assert!(result.unwrap().five_hour.is_some());
}
