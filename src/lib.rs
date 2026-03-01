//! Claude Usage Tracker - Library
//!
//! This library exposes the core functionality for testing purposes.

pub mod api;
pub mod auth;
pub mod commands;
pub mod config;
pub mod events;
pub mod service;
pub mod tray;

// Re-export main types for convenience
pub use auth::Credentials;
pub use config::Config;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tauri::{Emitter, Manager};
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
/// - **`std::sync::Mutex`** (sync-capable): `credentials`, `last_checked`, `window_position`
///   - Accessed in Tauri's synchronous tray callbacks and commands
///   - Must NOT use `.await` when acquiring (would deadlock)
///   - Uses `.expect()`/`.unwrap()` - panics on poison (see module-level panic strategy)
///
/// - **Direct storage** (sync-capable): `cancel_token`
///   - `CancellationToken` is already thread-safe, no lock needed
///
/// - **No lock**: `config`, `http_client`
///   - Immutable after startup, no synchronization needed
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
    /// Whether clicking the tray icon triggers a data refresh
    pub refresh_on_open: AtomicBool,
    /// HTTP client with connection pooling (immutable after startup)
    pub http_client: reqwest::Client,

    // --- Popup window state ---
    /// When true: popup is not auto-closed on focus loss, Close button is shown
    pub keep_window_open: AtomicBool,
    /// When true: popup window floats above all other windows
    pub always_on_top: AtomicBool,
    /// Last saved popup position in physical pixels (persisted across restarts)
    pub window_position: Mutex<Option<(i32, i32)>>,
}

impl AppState {
    /// Create `AppState` with pre-loaded config and HTTP client.
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
            credentials: Mutex::default(),
            cancel_token,
            config,
            http_client,
            refresh_on_open: AtomicBool::new(true),
            keep_window_open: AtomicBool::new(false),
            always_on_top: AtomicBool::new(false),
            window_position: Mutex::new(None),
        }
    }
}

// =============================================================================
// Window position persistence
// =============================================================================

/// Load saved popup window position from `{config_dir}/claude-usage-tracker/window-state.json`.
pub fn load_window_position() -> Option<(i32, i32)> {
    let dir = dirs::config_dir()?.join("claude-usage-tracker");
    let path = dir.join("window-state.json");
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let x = v["x"].as_i64()? as i32;
    let y = v["y"].as_i64()? as i32;
    Some((x, y))
}

/// Persist popup window position to `{config_dir}/claude-usage-tracker/window-state.json`.
pub fn save_window_position_to_file(x: i32, y: i32) {
    if let Some(base) = dirs::config_dir() {
        let dir = base.join("claude-usage-tracker");
        if std::fs::create_dir_all(&dir).is_ok() {
            let path = dir.join("window-state.json");
            let content = format!(r#"{{"x":{},"y":{}}}"#, x, y);
            if let Err(e) = std::fs::write(&path, content) {
                log::error!("Failed to save window position: {e}");
            }
        }
    }
}

// =============================================================================
// Application entry point
// =============================================================================

/// Main entry point for the application.
pub fn run() -> Result<()> {
    suppress_glib_warnings();

    let config = config::load()?;
    log::info!("Configuration loaded successfully");

    let http_client = build_http_client();

    let cancel_token = CancellationToken::new();
    let app_state = Arc::new(AppState::new(config, http_client, cancel_token.clone()));

    // Pre-load saved window position into AppState
    if let Some(pos) = load_window_position() {
        *app_state
            .window_position
            .lock()
            .expect("window_position lock not poisoned") = Some(pos);
        log::debug!("Loaded saved window position: {:?}", pos);
    }

    let context = tauri::generate_context!();

    tauri::Builder::default()
        .manage(app_state.clone())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_log::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_usage_data,
            commands::trigger_refresh,
            commands::save_window_position,
            commands::hide_popup,
            commands::open_github,
        ])
        .setup(move |app| {
            log::info!("Starting Claude Usage Tracker");

            // Create event channel
            let (event_tx, event_rx) = tokio::sync::mpsc::channel::<AppEvent>(32);

            // Spawn event handler loop
            let handle_for_events = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                event_handler_loop(&handle_for_events, event_rx).await;
            });

            // Spawn async tray creation
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = tray::create_tray(&handle2) {
                    log::error!("Failed to create tray: {e}");
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
                    tokio::time::sleep(std::time::Duration::from_millis(
                        NOTIFICATION_DISPLAY_DELAY_MS,
                    ))
                    .await;
                    handle2.exit(EXIT_CODE_ERROR);
                }
            });

            // Spawn polling task
            let state_clone = app_state.clone();
            let cancel = cancel_token.clone();
            tauri::async_runtime::spawn(async move {
                polling_loop(event_tx, state_clone, cancel).await;
            });

            // Spawn Ctrl+C handler
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

            // ------------------------------------------------------------------
            // Create the popup window (hidden, decorationless, no taskbar entry)
            // ------------------------------------------------------------------
            let popup = tauri::WebviewWindowBuilder::new(
                app,
                "popup",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Claude Usage")
            .inner_size(290.0, 140.0)
            .decorations(false)
            .skip_taskbar(true)
            .visible(false)
            .resizable(false)
            .build()?;

            // Auto-hide on focus loss (unless keep_window_open is enabled).
            // A short delay + is_focused() re-check prevents startDragging()
            // from closing the popup: dragging briefly steals focus, but the
            // window regains it once the drag ends.
            let popup_clone = popup.clone();
            let app_handle_focus = app.handle().clone();
            popup.on_window_event(move |event| {
                if let tauri::WindowEvent::Focused(false) = event {
                    let state = app_handle_focus.state::<Arc<AppState>>();
                    if !state.keep_window_open.load(std::sync::atomic::Ordering::Relaxed) {
                        let popup_delayed = popup_clone.clone();
                        let state_arc = state.inner().clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            if !popup_delayed.is_focused().unwrap_or(false) {
                                // Save position before hiding so reopening restores it
                                if let Ok(pos) = popup_delayed.outer_position() {
                                    *state_arc
                                        .window_position
                                        .lock()
                                        .expect("window_position lock not poisoned") =
                                        Some((pos.x, pos.y));
                                    save_window_position_to_file(pos.x, pos.y);
                                }
                                let _ = popup_delayed.hide();
                            }
                        });
                    }
                }
            });

            log::info!("Popup window created (hidden)");
            Ok(())
        })
        .run(context)?;

    Ok(())
}

// =============================================================================
// Platform helpers
// =============================================================================

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

// =============================================================================
// Notification logic
// =============================================================================

async fn check_notifications(
    app: &tauri::AppHandle,
    state: &AppState,
    usage: &UsageResponse,
    config: &Config,
) {
    let now = chrono::Utc::now();
    let cooldown = chrono::Duration::minutes(i64::from(config.notification_cooldown_minutes));

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

    let mut successful_sends: Vec<(WindowKind, PendingStateChange)> = Vec::new();

    for (kind, action, utilization, pending) in actions {
        let label = kind.label();
        match action {
            NotificationAction::Reset => {
                log::debug!("{label}: flags reset (usage at {utilization:.1}%)");
            }
            NotificationAction::SendCritical | NotificationAction::SendWarning => {
                let alert_type = match action {
                    NotificationAction::SendCritical => "CRITICAL",
                    _ => "WARNING",
                };
                let title = format!("Claude {label} {alert_type} Alert");
                let body = format!("{label} usage at {utilization:.1}%. Consider slowing down.");

                let send_result = app
                    .notification()
                    .builder()
                    .title(&title)
                    .body(&body)
                    .show();

                match send_result {
                    Ok(()) => {
                        if let Some(pending_change) = pending {
                            successful_sends.push((kind, pending_change));
                        }
                        log::warn!("{label} {alert_type} Alert: {body}");
                    }
                    Err(e) => {
                        log::error!("Failed to show notification: {e}");
                    }
                }
            }
            NotificationAction::None => {}
        }
    }

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
}

async fn update_tray_for_error(
    app: &tauri::AppHandle,
    error_msg: &str,
    set_last_error: bool,
    show_notification: bool,
) {
    let cached_usage = {
        let state = app.state::<Arc<AppState>>();
        if set_last_error {
            *state.last_error.write().await = Some(error_msg.to_string());
        }
        let latest_usage = state.latest_usage.read().await;
        latest_usage.clone()
    };

    if let Some(ref usage) = cached_usage {
        tray::update_tray_icon(app, usage, Some(error_msg));
    } else {
        tray::update_tray_icon_error(app, error_msg);
    }

    tray::update_tray_menu(app).await;

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

// =============================================================================
// Event handler loop
// =============================================================================

async fn event_handler_loop(
    app: &tauri::AppHandle,
    mut event_rx: tokio::sync::mpsc::Receiver<AppEvent>,
) {
    log::debug!("Event handler loop started");

    while let Some(event) = event_rx.recv().await {
        match event {
            AppEvent::UsageUpdated(usage) => {
                log::trace!("Processing UsageUpdated event");

                let state = app.state::<Arc<AppState>>();
                check_notifications(app, &state, &usage, &state.config).await;
                tray::update_tray_icon(app, &usage, None);
                tray::update_tray_menu(app).await;

                // Emit live update to popup window
                let dto = commands::build_usage_dto(state.inner()).await;
                app.emit("usage-updated", &dto).ok();

                log::debug!("Tray and popup updated with new usage data");
            }
            AppEvent::ErrorOccurred(message) => {
                log::trace!("Processing ErrorOccurred event: {message}");

                // set_last_error=false: service layer already set it
                update_tray_for_error(app, &message, false, false).await;

                let state = app.state::<Arc<AppState>>();
                let dto = commands::build_usage_dto(state.inner()).await;
                app.emit("usage-updated", &dto).ok();
            }
            AppEvent::CredentialsExpired => {
                log::trace!("Processing CredentialsExpired event");

                let auth_error =
                    "Your token has expired. Please run 'claude auth login' to re-authenticate.";
                update_tray_for_error(app, auth_error, true, true).await;

                let state = app.state::<Arc<AppState>>();
                let dto = commands::build_usage_dto(state.inner()).await;
                app.emit("usage-updated", &dto).ok();
            }
            AppEvent::AuthRequired => {
                log::trace!("Processing AuthRequired event");

                let auth_error =
                    "No credentials found. Please run 'claude auth login' to authenticate.";
                update_tray_for_error(app, auth_error, true, true).await;

                let state = app.state::<Arc<AppState>>();
                let dto = commands::build_usage_dto(state.inner()).await;
                app.emit("usage-updated", &dto).ok();
            }
        }
    }

    log::debug!("Event handler loop ended");
}
