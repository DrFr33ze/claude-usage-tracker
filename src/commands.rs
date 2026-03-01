//! Tauri commands for JS↔Rust communication with the popup window.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::Local;
use tauri::State;

use crate::api::UsageWindow;
use crate::config::Config;
use crate::AppState;

// =============================================================================
// DTOs (serializable structs for JS)
// =============================================================================

/// Serializable usage data for a single time window
#[derive(serde::Serialize, Clone)]
pub struct WindowDto {
    pub utilization: f64,
    /// "normal" | "warning" | "critical"
    pub level: String,
    /// Human-readable remaining time until reset, e.g. "1h 23m", "45m", "resetting"
    pub resets_in: Option<String>,
}

/// Full usage payload sent to the popup window
#[derive(serde::Serialize, Clone)]
pub struct UsageDto {
    pub five_hour: Option<WindowDto>,
    pub seven_day: Option<WindowDto>,
    pub opus: Option<WindowDto>,
    pub sonnet: Option<WindowDto>,
    /// Last successful check time formatted as "HH:MM" in local time
    pub last_checked: Option<String>,
    /// Current error message, if any
    pub error: Option<String>,
    pub keep_window_open: bool,
    pub always_on_top: bool,
    pub warning_threshold: f64,
    pub critical_threshold: f64,
}

// =============================================================================
// Helpers
// =============================================================================

fn level_str(util: f64, config: &Config) -> String {
    if config.is_above_critical(util) {
        "critical".to_string()
    } else if config.is_above_warning(util) {
        "warning".to_string()
    } else {
        "normal".to_string()
    }
}

/// Format remaining time until a reset timestamp as a human-readable string.
fn format_remaining(resets_at: chrono::DateTime<chrono::Utc>) -> String {
    let secs = resets_at
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();
    if secs <= 0 {
        return "resetting".to_string();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h >= 24 {
        format!("{}d {}h", h / 24, h % 24)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m", m.max(1))
    }
}

fn window_to_dto(w: &UsageWindow, config: &Config) -> WindowDto {
    let util = w.utilization.as_f64();
    WindowDto {
        utilization: util,
        level: level_str(util, config),
        resets_in: w.resets_at.map(format_remaining),
    }
}

/// Build a [`UsageDto`] from current app state.
///
/// Called both from the `get_usage_data` command and from the event handler loop
/// to emit live updates to the popup window.
pub async fn build_usage_dto(state: &Arc<AppState>) -> UsageDto {
    let usage = state.latest_usage.read().await;
    let error = state.last_error.read().await;
    let last_checked = *state
        .last_checked
        .lock()
        .expect("last_checked lock not poisoned");
    let config = &state.config;

    let (five_hour, seven_day, opus, sonnet) = if let Some(u) = usage.as_ref() {
        (
            u.five_hour.as_ref().map(|w| window_to_dto(w, config)),
            u.seven_day.as_ref().map(|w| window_to_dto(w, config)),
            u.seven_day_opus.as_ref().map(|w| window_to_dto(w, config)),
            u.seven_day_sonnet
                .as_ref()
                .map(|w| window_to_dto(w, config)),
        )
    } else {
        (None, None, None, None)
    };

    UsageDto {
        five_hour,
        seven_day,
        opus,
        sonnet,
        last_checked: last_checked
            .map(|t| t.with_timezone(&Local).format("%H:%M").to_string()),
        error: error.clone(),
        keep_window_open: state.keep_window_open.load(Ordering::Relaxed),
        always_on_top: state.always_on_top.load(Ordering::Relaxed),
        warning_threshold: config.warning_threshold.as_f64(),
        critical_threshold: config.critical_threshold.as_f64(),
    }
}

// =============================================================================
// Tauri Commands
// =============================================================================

/// Return current usage data to the popup window on demand.
#[tauri::command]
pub async fn get_usage_data(state: State<'_, Arc<AppState>>) -> Result<UsageDto, String> {
    Ok(build_usage_dto(&state).await)
}

/// Trigger an immediate data refresh (same as the old tray click).
#[tauri::command]
pub fn trigger_refresh(state: State<'_, Arc<AppState>>) {
    state.refresh_notify.notify_one();
}

/// Persist the popup window position and store it in AppState.
///
/// Called from JS after a drag operation ends.
#[tauri::command]
pub fn save_window_position(x: i32, y: i32, state: State<'_, Arc<AppState>>) {
    *state
        .window_position
        .lock()
        .expect("window_position lock not poisoned") = Some((x, y));
    crate::save_window_position_to_file(x, y);
}

/// Hide the popup window (used by the Close button when keep_window_open is true).
#[tauri::command]
pub fn hide_popup(window: tauri::WebviewWindow) {
    let _ = window.hide();
}

/// Open the project GitHub page in the system default browser.
#[tauri::command]
pub fn open_github() {
    const URL: &str = "https://github.com/DrFr33ze/claude-usage-tracker";
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", URL])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(URL).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(URL).spawn();
}
