//! Claude Usage Tracker - Library
//!
//! This library exposes the core functionality for testing purposes.

pub mod api;
pub mod auth;
pub mod config;
pub mod events;
pub mod service;
pub mod tray;

// Re-export main types for convenience
// AppState is the main application state, useful for integration testing
// (already pub, but explicit re-export improves discoverability)
pub use auth::Credentials;
pub use config::Config;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tauri::Manager;
use tauri_plugin_notification::NotificationExt;
use tokio::signal;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use api::{build_http_client, UsageResponse};
use config::WindowKind;
use events::AppEvent;
use service::polling_loop;
use service::{
    check_window_notification, NotificationAction, NotificationState, PendingStateChange,
};

// Timing and exit code constants
const NOTIFICATION_DISPLAY_DELAY_MS: u64 = 500;
const EXIT_CODE_ERROR: i32 = 1;

/// Application state shared across components.
///
/// # Locking Invariants
///
/// Fields are categorized by their access pattern and use appropriate lock types:
///
/// - **`tokio::sync::RwLock`** (async-only): `latest_usage`, `last_error`, `notification_state`
///   - Accessed in async contexts (service layer, event handler)
///   - Must always use `.await` when acquiring
///   - Never hold while performing sync operations
///
/// - **`std::sync::Mutex`** (sync-capable): `credentials`, `last_checked`
///   - Accessed in Tauri's synchronous tray callbacks
///   - Must NOT use `.await` when acquiring (would deadlock)
///   - Uses `.expect()`/`.unwrap()` - panics on poison (see module-level panic strategy)
///
/// - **Direct storage** (sync-capable): `cancel_token`
///   - `CancellationToken` is already thread-safe, no lock needed
///   - Accessed in async contexts and sync callbacks alike
///
/// - **No lock**: `config`, `http_client`
///   - Immutable after startup, no synchronization needed
///   - Safe to access from any thread without locking
///
/// # Initialization Guarantees
///
/// - Created via `AppState::new()` in `main()` before Tauri setup
/// - `credentials` starts as `None`, populated by the polling loop on startup
/// - `cancel_token` is set immediately after construction via `Mutex::lock()`
/// - All other fields are fully initialized at construction
///
/// # Lifetime Invariants
///
/// - Lives for the entire application duration (`'static` via `Arc`)
/// - Cloned via `Arc` for sharing between async tasks and Tauri
/// - `cancel_token.cancelled()` is the canonical shutdown signal
/// - Drop order is non-deterministic but acceptable since all locks are released on drop
///
/// # Usage Pattern
///
/// The polling loop (producer) updates `latest_usage`, `last_error`, `last_checked`,
/// and `credentials`. The event handler and tray code (consumer) read these fields.
/// This separation ensures no single task holds both async and sync locks simultaneously.
pub struct AppState {
    /// Cached usage data from last successful API fetch
    pub latest_usage: RwLock<Option<Arc<UsageResponse>>>,
    /// Last error message for display in tray menu
    pub last_error: RwLock<Option<String>>,
    /// Per-window notification tracking (5-hour and 7-day independent cooldowns)
    pub notification_state: RwLock<NotificationState>,
    /// Last check timestamp - uses `std::sync::Mutex` for sync tray callbacks
    pub last_checked: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    /// Manual refresh signal
    pub refresh_notify: Notify,
    /// Auth credentials - `std::sync::Mutex` for brief sync access
    pub credentials: Mutex<Option<Arc<auth::Credentials>>>,
    /// Shutdown coordination - stored directly, never changes after init
    pub cancel_token: CancellationToken,
    /// Read-only configuration (immutable after startup)
    pub config: Config,
    /// HTTP client with connection pooling (immutable after startup)
    pub http_client: reqwest::Client,
}

impl AppState {
    /// Create `AppState` with pre-loaded config and HTTP client.
    ///
    /// Credentials are initialized to `None` and loaded by the polling
    /// state machine on startup (via `PollingState::Startup`).
    #[must_use]
    pub fn new(
        config: Config,
        http_client: reqwest::Client,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            latest_usage: RwLock::default(),
            last_error: RwLock::default(),
            notification_state: RwLock::default(),
            last_checked: Mutex::new(None),
            refresh_notify: Notify::new(),
            credentials: Mutex::default(), // Initialized to None, loaded by polling_loop
            cancel_token,
            config,
            http_client,
        }
    }
}

/// Main entry point for the application.
pub fn run() -> Result<()> {
    suppress_glib_warnings();

    // Load configuration (includes validation)
    let config = config::load()?;
    log::info!("Configuration loaded successfully");

    // Initialize HTTP client with connection pooling
    let http_client = build_http_client();

    // Initialize app state with config and HTTP client
    // Note: credentials are loaded by polling_loop state machine
    let cancel_token = CancellationToken::new();
    let app_state = Arc::new(AppState::new(config, http_client, cancel_token.clone()));

    // Build Tauri app
    let context = tauri::generate_context!();

    tauri::Builder::default()
        .manage(app_state.clone())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_log::Builder::new().build())
        .setup(move |app| {
            log::info!("Starting Claude Usage Tracker");

            // Create event channel for service layer to app communication
            // Channel size of 32 allows for bursty events (usage updates, errors) without blocking
            // the polling loop. The service only sends one event per poll cycle, so 32 provides
            // ample headroom for rapid state changes and manual refresh signals.
            let (event_tx, event_rx) = tokio::sync::mpsc::channel::<AppEvent>(32);

            // Spawn event handler loop that processes events from service layer
            let handle_for_events = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                event_handler_loop(&handle_for_events, event_rx).await;
            });

            // Spawn async tray creation
            let handle = app.handle().clone();
            let handle2 = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = tray::create_tray(&handle2) {
                    log::error!("Failed to create tray: {e}");
                    // Show notification and exit - tray is essential for this app
                    if let Err(notify_err) = handle2
                        .notification()
                        .builder()
                        .title("Claude Usage - Startup Error")
                        .body(format!(
                            "Failed to create system tray: {e}. The app will exit."
                        ))
                        .show()
                    {
                        log::error!("Failed to show startup error notification: {notify_err}");
                    }
                    // Give notification time to display before exiting
                    tokio::time::sleep(std::time::Duration::from_millis(
                        NOTIFICATION_DISPLAY_DELAY_MS,
                    ))
                    .await;
                    handle2.exit(EXIT_CODE_ERROR);
                }
            });

            // Spawn polling task with cancellation support
            let state_clone = app_state.clone();
            let cancel = cancel_token.clone();
            tauri::async_runtime::spawn(async move {
                polling_loop(event_tx, state_clone, cancel).await;
            });

            // Spawn Ctrl+C handler for graceful shutdown
            let cancel_for_signal = cancel_token.clone();
            let handle_for_signal = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = signal::ctrl_c().await {
                    log::error!("Failed to listen for Ctrl+C signal: {e}");
                    return;
                }
                log::info!("Received Ctrl+C, initiating graceful shutdown");
                cancel_for_signal.cancel();
                handle_for_signal.exit(0);
            });

            Ok(())
        })
        .run(context)?;

    Ok(())
}

/// Suppress libayatana-appindicator deprecation warnings on Linux.
///
/// The warning "libayatana-appindicator is deprecated. Please use libayatana-appindicator-glib"
/// comes from the C library used by Tauri's tray-icon. This is an upstream issue:
/// - https://github.com/emirror-de/libayatana-appindicator-rs/issues/3
/// - https://github.com/tauri-apps/tray-icon
#[cfg(target_os = "linux")]
fn suppress_glib_warnings() {
    use glib::log_set_writer_func;
    log_set_writer_func(|level, fields| {
        for field in fields {
            if let Some(value) = field.value_str() {
                if value.contains("libayatana-appindicator is deprecated") {
                    return glib::LogWriterOutput::Handled;
                }
            }
        }
        glib::log_writer_default(level, fields)
    });
}

#[cfg(not(target_os = "linux"))]
fn suppress_glib_warnings() {}

/// Check usage thresholds and show desktop notifications.
///
/// Uses per-window cooldown timestamps and boolean flags.
/// Notifications trigger at warning/critical thresholds and reset when usage drops.
///
/// This function is called by the consumer (event handler) when usage is updated,
/// separating notification logic from the service layer (producer).
///
/// State is only updated AFTER successful notification send to prevent race conditions
/// where a failed send would incorrectly mark the notification as sent.
async fn check_notifications(
    app: &tauri::AppHandle,
    state: &AppState,
    usage: &UsageResponse,
    config: &Config,
) {
    let now = chrono::Utc::now();
    let cooldown = chrono::Duration::minutes(i64::from(config.notification_cooldown_minutes));

    // Collect windows to process
    let windows = [
        usage
            .five_hour
            .as_ref()
            .map(|w| (w.utilization.as_f64(), WindowKind::FiveHour)),
        usage
            .seven_day
            .as_ref()
            .map(|w| (w.utilization.as_f64(), WindowKind::SevenDay)),
    ];

    // Collect actions while holding lock briefly
    // Note: For Reset actions, state is updated immediately inside check_window_notification
    // For Send* actions, state is NOT updated - we get a PendingStateChange to apply after successful send
    let actions: Vec<_> = {
        let mut notification_state = state.notification_state.write().await;
        windows
            .into_iter()
            .flatten()
            .map(|(utilization, kind)| {
                let ns = match kind {
                    WindowKind::FiveHour => &mut notification_state.five_hour,
                    WindowKind::SevenDay => &mut notification_state.seven_day,
                };
                let (action, pending) =
                    check_window_notification(ns, utilization, config, now, cooldown);
                (kind, action, utilization, pending)
            })
            .collect()
    };
    // Lock dropped here

    // Process actions outside lock - no deadlock risk
    // Collect successful sends to apply state changes afterward
    let mut successful_sends: Vec<(WindowKind, PendingStateChange)> = Vec::new();

    for (kind, action, utilization, pending) in actions {
        let label = kind.label();
        match action {
            NotificationAction::Reset => {
                // State already updated inside check_window_notification for Reset
                log::debug!("{label}: flags reset (usage at {utilization:.1}%)");
            }
            NotificationAction::SendCritical | NotificationAction::SendWarning => {
                let alert_type = match action {
                    NotificationAction::SendCritical => "CRITICAL",
                    _ => "WARNING",
                };
                let title = format!("Claude {label} {alert_type} Alert");
                let body = format!("{label} usage at {utilization:.1}%. Consider slowing down.");

                // Attempt to send notification
                let send_result = app
                    .notification()
                    .builder()
                    .title(&title)
                    .body(&body)
                    .show();

                match send_result {
                    Ok(()) => {
                        // Notification sent successfully - mark for state update
                        if let Some(pending_change) = pending {
                            successful_sends.push((kind, pending_change));
                        }
                        log::warn!("{label} {alert_type} Alert: {body}");
                        log::debug!("{label}: sent {alert_type} notification at {utilization:.1}%");
                    }
                    Err(e) => {
                        // Notification failed - do NOT update state (will retry next poll)
                        log::error!("Failed to show notification: {e}");
                        log::debug!(
                            "{label}: notification send failed, state not updated (will retry)"
                        );
                    }
                }
            }
            NotificationAction::None => {}
        }
    }

    // Apply state changes for successful notification sends
    // This is done in a separate lock acquisition to minimize lock hold time
    if !successful_sends.is_empty() {
        let mut notification_state = state.notification_state.write().await;
        for (kind, pending_change) in successful_sends {
            let ns = match kind {
                WindowKind::FiveHour => &mut notification_state.five_hour,
                WindowKind::SevenDay => &mut notification_state.seven_day,
            };
            pending_change.apply(ns);
        }
    }

    // Note: Opus/Sonnet windows are intentionally not checked for notifications
    // as per design decision (they use neutral indicators only)
}

/// Update the tray icon and menu to reflect an error state.
///
/// This is a helper function that consolidates the common pattern used across
/// multiple error event handlers in `event_handler_loop`.
///
/// # Arguments
/// * `set_last_error` - Whether to update `AppState.last_error`. Set to `false` for
///   `ErrorOccurred` events since the service layer already sets it in `handle_fetch_error`.
///   Set to `true` for auth events (`CredentialsExpired`/`AuthRequired`) which bypass
///   `handle_fetch_error`.
async fn update_tray_for_error(
    app: &tauri::AppHandle,
    error_msg: &str,
    set_last_error: bool,
    show_notification: bool,
) {
    // Get cached usage for icon display (if available)
    let cached_usage = {
        let state = app.state::<Arc<AppState>>();
        if set_last_error {
            *state.last_error.write().await = Some(error_msg.to_string());
        }
        let latest_usage = state.latest_usage.read().await;
        latest_usage.clone()
    };

    // Update tray icon to error state (with or without cached data)
    if let Some(ref usage) = cached_usage {
        tray::update_tray_icon(app, usage, Some(error_msg));
    } else {
        tray::update_tray_icon_error(app, error_msg);
    }

    // Update menu to show error state
    tray::update_tray_menu(app).await;

    log::debug!("Tray updated with error state");

    // Show notification if requested
    if show_notification {
        if let Err(e) = app
            .notification()
            .builder()
            .title("Claude Usage - Authentication Required")
            .body(error_msg)
            .show()
        {
            log::error!("Failed to show notification: {e}");
        }
    }
}

/// Event handler loop that processes events from the service layer.
///
/// This decouples the service layer (which doesn't know about Tauri) from
/// the application layer (which handles UI updates like tray and notifications).
async fn event_handler_loop(
    app: &tauri::AppHandle,
    mut event_rx: tokio::sync::mpsc::Receiver<AppEvent>,
) {
    log::debug!("Event handler loop started");

    while let Some(event) = event_rx.recv().await {
        match event {
            AppEvent::UsageUpdated(usage) => {
                log::trace!("Processing UsageUpdated event");

                // Check for notifications (consumer side decision)
                let state = app.state::<Arc<AppState>>();
                check_notifications(app, &state, &usage, &state.config).await;

                // Update tray icon with new usage data
                tray::update_tray_icon(app, &usage, None);

                // Update tray menu to reflect new usage
                tray::update_tray_menu(app).await;

                log::debug!("Tray updated with new usage data");
            }
            AppEvent::ErrorOccurred(message) => {
                log::trace!("Processing ErrorOccurred event: {message}");

                // set_last_error=false: service layer already set it in handle_fetch_error
                update_tray_for_error(app, &message, false, false).await;
            }
            AppEvent::CredentialsExpired => {
                log::trace!("Processing CredentialsExpired event");

                let auth_error =
                    "Your token has expired. Please run 'claude auth login' to re-authenticate.";

                // set_last_error=true: auth events bypass handle_fetch_error
                update_tray_for_error(app, auth_error, true, true).await;

                log::debug!("Tray updated with credentials expired state");
            }
            AppEvent::AuthRequired => {
                log::trace!("Processing AuthRequired event");

                let auth_error =
                    "No credentials found. Please run 'claude auth login' to authenticate.";

                // set_last_error=true: auth events bypass handle_fetch_error
                update_tray_for_error(app, auth_error, true, true).await;

                log::debug!("Tray updated with auth required state");
            }
        }
    }

    log::debug!("Event handler loop ended");
}
