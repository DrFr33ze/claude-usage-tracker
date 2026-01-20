//! Polling service layer.
//!
//! This module contains the core polling logic and notification state management.
//! It coordinates credential loading, API polling, error handling, and graceful shutdown.
//! This layer is framework-agnostic and communicates via events.

// Re-export types from crate::events for backward compatibility
pub use crate::events::{AppEvent, CredentialRefreshResult};

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::{fetch_usage, ApiError, UsageResponse};
use crate::auth::{load_credentials, Credentials};
use crate::config::Config;

// =============================================================================
// Constants
// =============================================================================

const TIMING_JITTER_RANGE_SECS: i64 = 30;
const TIMING_MIN_POLLING_INTERVAL_SECS: u64 = 60;
const TIMING_DEFAULT_RATE_LIMIT_BACKOFF_SECS: u64 = 300; // 5 minutes
const TIMING_SECONDS_PER_MINUTE: u64 = 60;
const AUTH_CHECK_INTERVAL_SECS: u64 = 30;

// =============================================================================
// Types
// =============================================================================

/// Per-window notification tracking state.
///
/// Tracks whether warning/critical alerts have been sent for a specific usage window
/// (5-hour or 7-day). This enables:
/// - **One-shot notifications**: Each threshold level triggers at most one notification
///   until usage drops below the reset threshold.
/// - **Cooldown enforcement**: Prevents notification spam by rate-limiting alerts.
/// - **State persistence**: Remembers what was sent so escalation logic can work correctly.
///
/// # State Transitions
/// ```text
/// Initial (both false) --[warning threshold]--> warned=true
///                      --[critical threshold]--> critical=true, warned=true (direct jump)
/// warned=true --[critical threshold]--> critical=true (escalation, bypasses cooldown)
/// Any state --[below reset threshold]--> Reset to initial
/// ```
#[derive(Default, Clone, Debug)]
pub struct WindowNotificationState {
    /// True if warning notification was sent for this window.
    /// Set when utilization >= warning_threshold and remains true until reset.
    pub warned: bool,
    /// True if critical notification was sent for this window.
    /// Set when utilization >= critical_threshold and remains true until reset.
    pub critical: bool,
    /// Timestamp of last notification sent. Used for cooldown calculation.
    /// Set to None on reset to allow immediate notification on next threshold breach.
    pub last_notified: Option<chrono::DateTime<chrono::Utc>>,
}

/// Aggregate notification state with independent per-window tracking.
///
/// Each usage window (5-hour, 7-day) has its own notification state because:
/// - Windows have different reset times and utilization rates
/// - A user may hit critical on 5-hour but only warning on 7-day
/// - Independent tracking prevents one window's alerts from affecting another
///
/// # Usage Pattern
/// The polling loop checks each window separately and may send multiple
/// notifications per poll cycle (one per window if both cross thresholds).
#[derive(Default, Clone, Debug)]
pub struct NotificationState {
    /// 5-hour window notification state (short-term usage bursts)
    pub five_hour: WindowNotificationState,
    /// 7-day window notification state (sustained usage patterns)
    pub seven_day: WindowNotificationState,
}

/// Action determined by the notification state machine.
///
/// Returned by `check_window_notification()` to separate decision logic from
/// side effects. The caller is responsible for:
/// - `SendWarning`/`SendCritical`: Display system notification to user, then call `apply_state_change()`
/// - `Reset`: Optionally log that usage returned to normal (state already applied)
/// - `None`: No action needed
///
/// This design keeps the state machine pure and testable.
///
/// # Important: State Update Protocol
/// For `SendWarning` and `SendCritical` actions, the state is NOT updated until
/// the caller explicitly calls `apply_state_change()`. This prevents race conditions
/// where a failed notification send would leave the state incorrectly marked as sent.
#[derive(Default, Debug, Clone)]
pub enum NotificationAction {
    /// No notification needed. Either:
    /// - Usage is in a "quiet zone" (above reset but below warning)
    /// - Notification was already sent and cooldown hasn't elapsed
    /// - Notification was already sent at this threshold level
    #[default]
    None,
    /// Usage dropped below reset threshold. Caller should clear any
    /// persistent UI state. The state machine has already cleared its flags.
    Reset,
    /// Send warning notification. Usage crossed warning_threshold.
    /// State will be updated when `apply_state_change()` is called.
    SendWarning,
    /// Send critical notification. Usage crossed critical_threshold.
    /// May be triggered via escalation bypass (see `check_window_notification`).
    /// State will be updated when `apply_state_change()` is called.
    SendCritical,
}

/// Pending state change to be applied after successful notification.
///
/// This struct captures the state changes that should be applied to
/// `WindowNotificationState` after a notification is successfully sent.
/// This two-phase approach prevents race conditions where a failed
/// notification would leave state incorrectly marked as sent.
#[derive(Debug, Clone)]
pub struct PendingStateChange {
    /// Whether to set the warned flag
    pub set_warned: bool,
    /// Whether to set the critical flag
    pub set_critical: bool,
    /// Timestamp to set for last_notified
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl PendingStateChange {
    /// Apply this pending change to the notification state.
    /// Should only be called after successful notification send.
    pub fn apply(&self, ns: &mut WindowNotificationState) {
        if self.set_warned {
            ns.warned = true;
        }
        if self.set_critical {
            ns.critical = true;
        }
        ns.last_notified = Some(self.timestamp);
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// Load credentials via spawn_blocking with graceful shutdown handling.
/// Returns None if the task was cancelled during shutdown.
async fn spawn_load_credentials() -> Option<Result<Credentials, crate::auth::AuthContextError>> {
    match tokio::task::spawn_blocking(load_credentials).await {
        Ok(result) => Some(result),
        Err(e) => {
            log::debug!("spawn_blocking cancelled during shutdown: {e}");
            None
        }
    }
}

// =============================================================================
// Public functions
// =============================================================================

/// Handle unauthorized error: attempt credential refresh and send event if needed.
///
/// Returns `CredentialRefreshResult` indicating if credentials changed/failed/unchanged.
pub async fn handle_unauthorized_error(
    sender: &mpsc::Sender<AppEvent>,
    credentials: &std::sync::Mutex<Option<Arc<Credentials>>>,
) -> CredentialRefreshResult {
    // Load credentials in blocking task (file I/O)
    let Some(creds_result) = spawn_load_credentials().await else {
        return CredentialRefreshResult::Failed;
    };

    // Attempt credential refresh within lock scope, then handle result outside
    // SAFETY: Mutex poisoning indicates a thread panicked while holding the lock.
    // State may be corrupted; crashing is safer than continuing with potentially inconsistent credentials.
    let refresh_result = {
        let mut guard = credentials
            .lock()
            .expect("credentials mutex should not be poisoned");

        match creds_result {
            Ok(new_creds) => {
                // Compare tokens to detect changes
                let new_token = new_creds.access_token();
                let changed = guard
                    .as_ref()
                    .map_or(true, |old| old.access_token() != new_token);
                *guard = Some(Arc::new(new_creds));
                if changed {
                    CredentialRefreshResult::Changed
                } else {
                    CredentialRefreshResult::Unchanged
                }
            }
            Err(refresh_err) => {
                log::error!("Failed to refresh credentials: {refresh_err}");
                CredentialRefreshResult::Failed
            }
        }
    };

    // Handle refresh result outside lock scope (single decision point)
    match refresh_result {
        CredentialRefreshResult::Changed => {
            log::debug!("Credentials changed - retrying immediately");
        }
        CredentialRefreshResult::Unchanged => {
            log::warn!(
                "Credentials unchanged after 401 - user needs to re-authenticate via Claude CLI"
            );
            if let Err(e) = sender.send(AppEvent::CredentialsExpired).await {
                log::error!("Failed to send CredentialsExpired event: {e}");
            }
        }
        CredentialRefreshResult::Failed => {
            // Already logged error above
            if let Err(e) = sender.send(AppEvent::CredentialsExpired).await {
                log::error!("Failed to send CredentialsExpired event: {e}");
            }
        }
    }

    refresh_result
}

/// Check a single usage window and determine what notification action to take.
///
/// This is the core notification state machine. It handles three key behaviors:
///
/// # 1. One-Shot Notifications
/// Each threshold (warning, critical) triggers at most one notification until
/// the user's usage drops below `reset_threshold`. This prevents spam when
/// usage hovers around a threshold.
///
/// # 2. Cooldown Rate-Limiting
/// Even for new threshold crossings, notifications are rate-limited by
/// `cooldown` duration to prevent rapid-fire alerts during volatile usage.
///
/// # 3. Escalation Bypass
/// **Critical feature**: When usage escalates from warning to critical level,
/// the cooldown is bypassed. This ensures users get timely critical alerts
/// even if they just received a warning. The rationale: critical situations
/// warrant immediate notification regardless of recent warning.
///
/// # State Machine Logic
/// ```text
/// 1. Below reset_threshold? -> Clear all flags, return Reset (if any were set)
/// 2. Above critical AND not yet sent critical?
///    - If cooldown elapsed OR escalating from warning -> SendCritical
/// 3. Above warning AND not yet sent warning AND cooldown elapsed? -> SendWarning
/// 4. Otherwise -> None
/// ```
///
/// # Arguments
/// * `ns` - Mutable notification state for this window (only modified for Reset)
/// * `utilization` - Current usage percentage (0.0-100.0)
/// * `config` - Threshold configuration
/// * `now` - Current timestamp for cooldown calculation
/// * `cooldown` - Minimum duration between notifications
///
/// # Returns
/// A tuple of (action, optional pending state change). For `SendWarning` and `SendCritical`,
/// the state is NOT updated immediately - the caller must call `pending.apply(ns)` after
/// successfully sending the notification. This prevents race conditions where failed
/// notification sends would leave state incorrectly marked.
///
/// For `Reset`, state is updated immediately since there's no external operation that can fail.
#[must_use = "notification action must be handled"]
pub fn check_window_notification(
    ns: &mut WindowNotificationState,
    utilization: f64,
    config: &Config,
    now: chrono::DateTime<chrono::Utc>,
    cooldown: chrono::Duration,
) -> (NotificationAction, Option<PendingStateChange>) {
    // Check if enough time has passed since last notification
    let can_send = ns
        .last_notified
        .map_or(true, |last| now.signed_duration_since(last) >= cooldown);

    // Priority 1: Reset if usage dropped below reset threshold
    // Clear all flags to allow fresh notifications on next threshold breach
    // This is applied immediately since reset has no external side effect that can fail
    if config.is_below_reset(utilization) {
        if ns.warned || ns.critical {
            ns.warned = false;
            ns.critical = false;
            ns.last_notified = None; // Allow immediate notification on next breach
            return (NotificationAction::Reset, None);
        }
        return (NotificationAction::None, None);
    }

    // Priority 2: Critical threshold (higher priority than warning)
    // ESCALATION BYPASS: If we already warned but haven't sent critical,
    // skip cooldown check. This ensures critical alerts are never delayed
    // just because a warning was recently sent.
    let is_escalation = ns.warned && !ns.critical;
    if config.is_above_critical(utilization) && !ns.critical && (can_send || is_escalation) {
        // Don't update state yet - return pending change for caller to apply after successful send
        let pending = PendingStateChange {
            set_warned: true,
            set_critical: true,
            timestamp: now,
        };
        return (NotificationAction::SendCritical, Some(pending));
    }

    // Priority 3: Warning threshold (only if not already warned)
    if config.is_above_warning(utilization) && !ns.warned && can_send {
        // Don't update state yet - return pending change for caller to apply after successful send
        let pending = PendingStateChange {
            set_warned: true,
            set_critical: false,
            timestamp: now,
        };
        return (NotificationAction::SendWarning, Some(pending));
    }

    (NotificationAction::None, None)
}

pub fn calculate_next_poll(config: &Config) -> Instant {
    let base_secs = u64::from(config.polling_interval_minutes) * TIMING_SECONDS_PER_MINUTE;

    // Clamp base to minimum FIRST, then apply symmetric jitter
    let clamped_base = base_secs.max(TIMING_MIN_POLLING_INTERVAL_SECS);

    // Calculate jitter range that won't violate minimum
    // If clamped_base is 60 and min is 60, max negative jitter is 0
    // If clamped_base is 120 and min is 60, max negative jitter is -30 (or less if 60 away)
    #[allow(clippy::cast_possible_wrap)]
    let max_negative_jitter = (clamped_base - TIMING_MIN_POLLING_INTERVAL_SECS)
        .min(TIMING_JITTER_RANGE_SECS as u64) as i64;

    // Use symmetric jitter range: [-max_negative, +max_negative]
    // This ensures average interval equals clamped_base (avoids polling drift over time)
    #[allow(clippy::cast_possible_wrap)]
    let jitter = if max_negative_jitter > 0 {
        // Generate random value in [0, 2*max] then shift to [-max, +max]
        let range = (max_negative_jitter * 2) as u64;
        (fastrand::u64(0..=range) as i64) - max_negative_jitter
    } else {
        0
    };

    let adjusted = clamped_base.saturating_add_signed(jitter);
    Instant::now() + Duration::from_secs(adjusted)
}

/// Calculate the next polling time respecting Retry-After header
///
/// When a 429 response includes a Retry-After header, use that value (in seconds)
/// as the base delay, with a small amount of jitter to prevent thundering herd.
pub fn calculate_next_poll_with_retry_after(retry_after: Option<u64>) -> Instant {
    if let Some(seconds) = retry_after {
        // Calculate jitter as 10% of the retry delay
        // Jitter only adds positive delay to respect Retry-After header
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let jitter_range = ((seconds as f64 * 0.1).floor() as u64).min(seconds / 2);
        let jitter = if jitter_range > 0 {
            fastrand::u64(0..=jitter_range)
        } else {
            0
        };
        let adjusted = seconds + jitter;
        Instant::now() + Duration::from_secs(adjusted)
    } else {
        // No retry_after value, use a default backoff
        log::warn!(
            "429 response without Retry-After header, using default backoff of {} seconds",
            TIMING_DEFAULT_RATE_LIMIT_BACKOFF_SECS
        );
        Instant::now() + Duration::from_secs(TIMING_DEFAULT_RATE_LIMIT_BACKOFF_SECS)
    }
}

/// Main polling loop - simple loop with tokio::select!
///
/// Coordinates credential loading, API polling, error handling, and graceful shutdown.
pub async fn polling_loop(
    sender: mpsc::Sender<AppEvent>,
    state: Arc<crate::AppState>,
    cancel: CancellationToken,
) {
    // Load initial credentials
    let mut has_credentials = load_initial_credentials(&sender, &state).await;

    // Track next poll time
    let mut next_poll = if has_credentials {
        Instant::now() // Poll immediately on startup
    } else {
        Instant::now() + Duration::from_secs(AUTH_CHECK_INTERVAL_SECS)
    };

    // Store notified future BEFORE entering loop to capture early signals
    let mut notified = std::pin::pin!(state.refresh_notify.notified());

    loop {
        let sleep_duration = next_poll.saturating_duration_since(Instant::now());

        tokio::select! {
            // Shutdown requested
            () = cancel.cancelled() => {
                log::debug!("Polling loop shutting down gracefully");
                break;
            }

            // Manual refresh requested
            () = &mut notified => {
                log::debug!("Manual refresh requested");
                // Re-arm for next signal
                notified.set(state.refresh_notify.notified());
                if has_credentials {
                    // Poll immediately
                    next_poll = do_poll(&sender, &state, &mut has_credentials).await;
                } else {
                    // Try to reload credentials
                    has_credentials = try_reload_credentials(&state).await;
                    if has_credentials {
                        next_poll = Instant::now(); // Poll immediately
                    }
                }
            }

            // Regular interval elapsed
            () = tokio::time::sleep(sleep_duration) => {
                if has_credentials {
                    next_poll = do_poll(&sender, &state, &mut has_credentials).await;
                } else {
                    // Waiting for auth - try to reload credentials periodically
                    has_credentials = try_reload_credentials(&state).await;
                    if has_credentials {
                        next_poll = Instant::now(); // Poll immediately
                    } else {
                        next_poll = Instant::now() + Duration::from_secs(AUTH_CHECK_INTERVAL_SECS);
                    }
                }
            }
        }
    }
}

// =============================================================================
// Private functions
// =============================================================================

/// Load initial credentials at startup.
/// Returns true if credentials were loaded successfully.
async fn load_initial_credentials(
    sender: &mpsc::Sender<AppEvent>,
    state: &Arc<crate::AppState>,
) -> bool {
    let Some(creds_result) = spawn_load_credentials().await else {
        return false;
    };

    match creds_result {
        Ok(creds) => {
            log::info!("Credentials loaded successfully");
            *state
                .credentials
                .lock()
                .expect("credentials mutex should not be poisoned during initialization") =
                Some(Arc::new(creds));
            true
        }
        Err(e) => {
            log::error!("Failed to load credentials: {e}");
            let _ = sender.send(AppEvent::AuthRequired).await;
            false
        }
    }
}

/// Try to reload credentials from disk.
/// Returns true if credentials were loaded successfully.
async fn try_reload_credentials(state: &Arc<crate::AppState>) -> bool {
    let Some(creds_result) = spawn_load_credentials().await else {
        return false;
    };

    match creds_result {
        Ok(new_creds) => {
            log::info!("Credentials reloaded successfully");
            *state
                .credentials
                .lock()
                .expect("credentials mutex should not be poisoned") = Some(Arc::new(new_creds));
            true
        }
        Err(_) => false,
    }
}

/// Perform a poll and return the next poll time.
async fn do_poll(
    sender: &mpsc::Sender<AppEvent>,
    state: &Arc<crate::AppState>,
    has_credentials: &mut bool,
) -> Instant {
    let result = do_fetch(sender, state).await;

    match result {
        Ok(()) => calculate_next_poll(&state.config),
        Err(ApiError::RateLimited { retry_after }) => {
            calculate_next_poll_with_retry_after(retry_after)
        }
        Err(ApiError::Unauthorized) => {
            // Try to refresh credentials
            let refresh_result = handle_unauthorized_error(sender, &state.credentials).await;
            match refresh_result {
                CredentialRefreshResult::Changed => {
                    // Retry immediately with new credentials
                    Instant::now()
                }
                CredentialRefreshResult::Unchanged | CredentialRefreshResult::Failed => {
                    // Need user to re-authenticate
                    *has_credentials = false;
                    Instant::now() + Duration::from_secs(AUTH_CHECK_INTERVAL_SECS)
                }
            }
        }
        Err(_) => {
            // Other errors - retry at normal interval
            calculate_next_poll(&state.config)
        }
    }
}

/// Perform usage fetch and update state.
async fn do_fetch(
    sender: &mpsc::Sender<AppEvent>,
    state: &Arc<crate::AppState>,
) -> Result<(), ApiError> {
    log::debug!("Fetching usage data...");

    // Get credentials from state
    let credentials = if let Some(c) = state
        .credentials
        .lock()
        .expect("credentials mutex should not be poisoned")
        .as_ref()
    {
        Arc::clone(c)
    } else {
        log::error!("No credentials available");
        return Err(ApiError::Unauthorized);
    };

    match fetch_usage(credentials.access_token(), &state.http_client).await {
        Ok(usage) => {
            handle_successful_fetch(sender, state, usage).await;
            Ok(())
        }
        Err(e) => {
            handle_fetch_error(sender, state, &e).await;
            Err(e)
        }
    }
}

/// Handle successful API fetch: update state and send event.
async fn handle_successful_fetch(
    sender: &mpsc::Sender<AppEvent>,
    state: &Arc<crate::AppState>,
    usage: UsageResponse,
) {
    let usage = Arc::new(usage);

    // Update async state
    *state.latest_usage.write().await = Some(Arc::clone(&usage));
    *state.last_error.write().await = None;

    // Update last_checked
    *state
        .last_checked
        .lock()
        .expect("last_checked lock should not be poisoned") = Some(chrono::Utc::now());

    // Send usage updated event
    if let Err(e) = sender.send(AppEvent::UsageUpdated(usage)).await {
        log::error!("Failed to send UsageUpdated event: {e}");
    }

    log::debug!("Usage updated successfully");
}

/// Handle fetch error: log appropriately, update error state, and send event.
async fn handle_fetch_error(
    sender: &mpsc::Sender<AppEvent>,
    state: &Arc<crate::AppState>,
    error: &ApiError,
) {
    if matches!(error, ApiError::Unauthorized) {
        log::warn!("Failed to fetch usage (401) - will attempt credential refresh");
        // Don't send ErrorOccurred for 401 - it's handled specially in do_poll()
        return;
    } else if matches!(error, ApiError::RateLimited { .. }) {
        log::warn!("Rate limited (429) - will retry after backoff");
    } else {
        log::error!("Failed to fetch usage: {error}");
    }

    let error_msg = format!("{error}");

    // Update error state
    {
        *state.last_error.write().await = Some(error_msg.clone());
    }

    // Send error event
    if let Err(e) = sender.send(AppEvent::ErrorOccurred(error_msg)).await {
        log::error!("Failed to send ErrorOccurred event: {e}");
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Percentage;
    use chrono::Utc;

    fn make_test_config() -> Config {
        Config {
            warning_threshold: Percentage::new(75.0).unwrap(),
            critical_threshold: Percentage::new(90.0).unwrap(),
            reset_threshold: Percentage::new(50.0).unwrap(),
            polling_interval_minutes: 2,
            notification_cooldown_minutes: 5,
        }
    }

    // =========================================================================
    // Polling tests
    // =========================================================================

    #[test]
    fn test_calculate_next_poll_respects_interval() {
        let config = make_test_config();
        let before = Instant::now();
        let next = calculate_next_poll(&config);

        // With 2 minute interval (120s) and symmetric +-30s jitter, expect 90-150 seconds
        // Jitter is symmetric around base, so average equals configured interval
        let duration = next.saturating_duration_since(before);
        assert!(duration >= Duration::from_secs(90), "Duration too short");
        assert!(duration <= Duration::from_secs(150), "Duration too long");
    }

    // =========================================================================
    // check_window_notification tests
    // =========================================================================

    #[test]
    fn test_check_window_at_warning_sends_warning() {
        let mut ns = WindowNotificationState::default();
        let config = make_test_config();
        let now = Utc::now();
        let cooldown = chrono::Duration::minutes(5);

        let (action, pending) = check_window_notification(&mut ns, 75.0, &config, now, cooldown);
        assert!(matches!(action, NotificationAction::SendWarning));
        // State NOT updated yet - pending change returned
        assert!(!ns.warned);
        assert!(!ns.critical);
        // Apply pending change (simulating successful send)
        let pending = pending.expect("should have pending change for SendWarning");
        pending.apply(&mut ns);
        assert!(ns.warned);
        assert!(!ns.critical);
    }

    #[test]
    fn test_check_window_at_critical_sends_critical_and_sets_both_flags() {
        let mut ns = WindowNotificationState::default();
        let config = make_test_config();
        let now = Utc::now();
        let cooldown = chrono::Duration::minutes(5);

        let (action, pending) = check_window_notification(&mut ns, 90.0, &config, now, cooldown);
        assert!(matches!(action, NotificationAction::SendCritical));
        // State NOT updated yet
        assert!(!ns.warned);
        assert!(!ns.critical);
        // Apply pending change (simulating successful send)
        let pending = pending.expect("should have pending change for SendCritical");
        pending.apply(&mut ns);
        assert!(ns.warned);
        assert!(ns.critical);
        assert_eq!(ns.last_notified, Some(now));
    }

    #[test]
    fn test_check_window_reset_below_threshold_clears_flags() {
        let now = Utc::now();
        let mut ns = WindowNotificationState {
            warned: true,
            critical: true,
            last_notified: Some(now),
        };
        let config = make_test_config();
        let cooldown = chrono::Duration::minutes(5);

        let (action, pending) = check_window_notification(&mut ns, 49.0, &config, now, cooldown);
        assert!(matches!(action, NotificationAction::Reset));
        assert!(pending.is_none()); // Reset has no pending change
                                    // State IS updated immediately for reset
        assert!(!ns.warned);
        assert!(!ns.critical);
        assert!(ns.last_notified.is_none());
    }

    #[test]
    fn test_check_window_cooldown_prevents_duplicate_critical() {
        let now = Utc::now();
        let mut ns = WindowNotificationState {
            warned: true,
            critical: true,
            last_notified: Some(now),
        };
        let config = make_test_config();
        let cooldown = chrono::Duration::minutes(5);

        // 1 minute later - still in cooldown, already sent critical
        let later = now + chrono::Duration::minutes(1);
        let (action, pending) = check_window_notification(&mut ns, 95.0, &config, later, cooldown);

        // Cannot send again because already sent critical (no duplicate)
        assert!(matches!(action, NotificationAction::None));
        assert!(pending.is_none());
        assert!(ns.critical); // Still critical
    }

    #[test]
    fn test_check_window_escalation_warning_to_critical_bypasses_cooldown() {
        let now = Utc::now();
        let mut ns = WindowNotificationState {
            warned: true,
            critical: false,
            last_notified: Some(now),
        };
        let config = make_test_config();
        let cooldown = chrono::Duration::minutes(5);

        // 1 minute later - still in cooldown, but usage escalated to critical
        let later = now + chrono::Duration::minutes(1);
        let (action, pending) = check_window_notification(&mut ns, 92.0, &config, later, cooldown);

        // Escalation bypasses cooldown - critical should be sent
        assert!(matches!(action, NotificationAction::SendCritical));
        // State NOT updated yet
        assert!(!ns.critical);
        // Apply pending change
        let pending = pending.expect("should have pending change");
        pending.apply(&mut ns);
        assert!(ns.critical);
        assert!(ns.warned);
        assert_eq!(ns.last_notified, Some(later));
    }

    #[test]
    fn test_check_window_warning_respects_cooldown_for_duplicate_warnings() {
        let now = Utc::now();
        let mut ns = WindowNotificationState {
            warned: true,
            critical: false,
            last_notified: Some(now),
        };
        let config = make_test_config();
        let cooldown = chrono::Duration::minutes(5);

        // 1 minute later - still in cooldown, usage still at warning level
        let later = now + chrono::Duration::minutes(1);
        let (action, pending) = check_window_notification(&mut ns, 78.0, &config, later, cooldown);

        // Cooldown prevents duplicate warning
        assert!(matches!(action, NotificationAction::None));
        assert!(pending.is_none());
        assert!(ns.warned);
        assert!(!ns.critical);
    }

    #[test]
    fn test_check_window_first_critical_sends_immediately() {
        let now = Utc::now();
        let mut ns = WindowNotificationState::default();
        let config = make_test_config();
        let cooldown = chrono::Duration::minutes(5);

        // Usage at critical on first check (no prior notifications)
        let (action, pending) = check_window_notification(&mut ns, 92.0, &config, now, cooldown);

        // Can send critical immediately (no previous critical sent)
        assert!(matches!(action, NotificationAction::SendCritical));
        // State NOT updated yet
        assert!(!ns.critical);
        assert!(!ns.warned);
        // Apply pending change
        let pending = pending.expect("should have pending change");
        pending.apply(&mut ns);
        assert!(ns.critical);
        assert!(ns.warned);
    }

    #[test]
    fn test_pending_state_change_not_applied_on_failure() {
        // This test verifies the race condition fix:
        // If notification send fails, state should NOT be updated
        let mut ns = WindowNotificationState::default();
        let config = make_test_config();
        let now = Utc::now();
        let cooldown = chrono::Duration::minutes(5);

        // First check - should want to send warning
        let (action, pending) = check_window_notification(&mut ns, 75.0, &config, now, cooldown);
        assert!(matches!(action, NotificationAction::SendWarning));
        let _pending = pending.expect("should have pending change");

        // Simulate notification failure - DON'T apply pending change
        // State should remain unchanged
        assert!(!ns.warned);
        assert!(!ns.critical);
        assert!(ns.last_notified.is_none());

        // On next poll, should still want to send warning (retry)
        let later = now + chrono::Duration::seconds(10);
        let (action2, pending2) =
            check_window_notification(&mut ns, 75.0, &config, later, cooldown);
        assert!(matches!(action2, NotificationAction::SendWarning));
        assert!(pending2.is_some());
    }

    // =========================================================================
    // Retry-After jitter tests
    // =========================================================================

    #[test]
    fn test_calculate_next_poll_with_retry_after_respects_server_requirement() {
        // Test with various retry_after values
        let test_cases = vec![1, 10, 60, 300, 1000];

        for &retry_after_seconds in &test_cases {
            // Run multiple iterations to account for randomness
            for _ in 0..100 {
                let now = Instant::now();
                let next_poll = calculate_next_poll_with_retry_after(Some(retry_after_seconds));

                // Calculate the actual delay
                let delay = next_poll.duration_since(now);

                // The delay must be at least the retry_after value
                // Allow small margin for test execution time (±10ms)
                assert!(
                    delay >= Duration::from_secs(retry_after_seconds) - Duration::from_millis(10),
                    "Delay {}ms must be >= {}s for retry_after={}s",
                    delay.as_millis(),
                    retry_after_seconds,
                    retry_after_seconds
                );
            }
        }
    }

    #[test]
    fn test_calculate_next_poll_with_retry_after_jitter_is_always_positive() {
        // Test that jitter only adds delay, never reduces it
        let retry_after_seconds = 100u64;

        // Run many iterations to test the randomness
        let mut delays = Vec::new();
        for _ in 0..50 {
            let now = Instant::now();
            let next_poll = calculate_next_poll_with_retry_after(Some(retry_after_seconds));
            let delay = next_poll.duration_since(now);
            delays.push(delay);
        }

        // All delays should be >= retry_after value
        let min_retry_after = Duration::from_secs(retry_after_seconds);
        let max_retry_after = Duration::from_secs(retry_after_seconds) + Duration::from_secs(10); // 10% jitter max
        let tolerance = Duration::from_millis(100); // 100ms tolerance for timing precision

        for delay in &delays {
            assert!(
                *delay >= min_retry_after - tolerance,
                "Delay {}ms must be >= {}s (retry_after minimum)",
                delay.as_millis(),
                retry_after_seconds
            );
            assert!(
                *delay <= max_retry_after + tolerance,
                "Delay {}ms must be <= {}s (retry_after + max jitter)",
                delay.as_millis(),
                retry_after_seconds + 10
            );
        }

        // Verify we actually got some variation (jitter is working)
        let min_delay = delays.iter().min().unwrap();
        let max_delay = delays.iter().max().unwrap();
        assert!(
            max_delay > min_delay,
            "Jitter should produce variation in delays: min={}ms, max={}ms",
            min_delay.as_millis(),
            max_delay.as_millis()
        );
    }
}
