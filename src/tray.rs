//! System tray icon and menu management for Claude Usage Tracker.
//!
//! This module handles all tray-related functionality including:
//! - Split icon system showing 5-hour (left) and 7-day (right) status
//! - Menu construction with usage data display
//! - Tray event handling (click refresh, quit)

// =============================================================================
// Imports
// =============================================================================

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt as _;

use crate::api::UsageResponse;
use crate::config::Config;
use crate::AppState;

// =============================================================================
// Constants
// =============================================================================

// Display and UI constants
const DISPLAY_PROGRESS_BAR_WIDTH: u8 = 10;
const DISPLAY_TOOLTIP_MAX_LENGTH: usize = 50;
const DISPLAY_TOOLTIP_TRUNCATED_LENGTH: usize = 47;
const DISPLAY_HOURS_PER_DAY: i64 = 24;
const DISPLAY_MINUTES_PER_HOUR: i64 = 60;

// Timing constants
const TIMING_TRAY_CLICK_REFRESH_COOLDOWN_SECS: i64 = 30;
const TIMING_GRACEFUL_SHUTDOWN_DELAY_MS: u64 = 100;

// Tray identifier
const TRAY_ID: &str = "main-tray";

// Icon bytes (embedded at compile time)
const ICON_LOADING: &[u8] = include_bytes!("../icons/icon-loading.png");
const ICON_ERROR: &[u8] = include_bytes!("../icons/icon-error.png");

// Split icons: left half = 5-hour status, right half = 7-day status
// n = normal, w = warning, c = critical
const ICON_NN: &[u8] = include_bytes!("../icons/icon-nn.png");
const ICON_NW: &[u8] = include_bytes!("../icons/icon-nw.png");
const ICON_NC: &[u8] = include_bytes!("../icons/icon-nc.png");
const ICON_WN: &[u8] = include_bytes!("../icons/icon-wn.png");
const ICON_WW: &[u8] = include_bytes!("../icons/icon-ww.png");
const ICON_WC: &[u8] = include_bytes!("../icons/icon-wc.png");
const ICON_CN: &[u8] = include_bytes!("../icons/icon-cn.png");
const ICON_CW: &[u8] = include_bytes!("../icons/icon-cw.png");
const ICON_CC: &[u8] = include_bytes!("../icons/icon-cc.png");

// Cache for decoded icons using OnceLock + HashMap for efficient access
static ICON_CACHE: OnceLock<HashMap<IconName, Image<'static>>> = OnceLock::new();

// =============================================================================
// Submodules
// =============================================================================

/// Window label constants for consistent usage across the tray
mod window_labels {
    pub const FIVE_HOUR: &str = "5h";
    pub const SEVEN_DAY: &str = "7d";
    pub const OPUS: &str = "Opus";
    pub const SONNET: &str = "Sonnet";
}

// =============================================================================
// Public Types
// =============================================================================

/// Usage level for icon state mapping
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    Normal,
    Warning,
    Critical,
}

/// Menu item identifiers with compile-time safety
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuId {
    Header,
    Status,
    Usage5h,
    Usage7d,
    UsageOpus,
    UsageSonnet,
    LastChecked,
    StartOnLogin,
    Refresh,
    Quit,
}

// =============================================================================
// Private Types
// =============================================================================

/// Icon variants for type-safe icon lookup.
///
/// Each variant corresponds to a tray icon file. The split icons encode
/// 5-hour (left) and 7-day (right) status: N=normal, W=warning, C=critical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconName {
    /// Normal 5h, Normal 7d (green/green)
    Nn,
    /// Normal 5h, Warning 7d (green/yellow)
    Nw,
    /// Normal 5h, Critical 7d (green/red)
    Nc,
    /// Warning 5h, Normal 7d (yellow/green)
    Wn,
    /// Warning 5h, Warning 7d (yellow/yellow)
    Ww,
    /// Warning 5h, Critical 7d (yellow/red)
    Wc,
    /// Critical 5h, Normal 7d (red/green)
    Cn,
    /// Critical 5h, Warning 7d (red/yellow)
    Cw,
    /// Critical 5h, Critical 7d (red/red)
    Cc,
    /// Loading state icon
    Loading,
    /// Error state icon
    Error,
}

// =============================================================================
// Implementations
// =============================================================================

impl std::str::FromStr for MenuId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "header" => Ok(Self::Header),
            "status" => Ok(Self::Status),
            "usage-5h" => Ok(Self::Usage5h),
            "usage-7d" => Ok(Self::Usage7d),
            "usage-opus" => Ok(Self::UsageOpus),
            "usage-sonnet" => Ok(Self::UsageSonnet),
            "last-checked" => Ok(Self::LastChecked),
            "start-on-login" => Ok(Self::StartOnLogin),
            "refresh" => Ok(Self::Refresh),
            "quit" => Ok(Self::Quit),
            _ => Err(()),
        }
    }
}

impl UsageLevel {
    /// Determine usage level based on utilization and config thresholds
    pub fn from_utilization(utilization: f64, config: &Config) -> Self {
        if config.is_above_critical(utilization) {
            UsageLevel::Critical
        } else if config.is_above_warning(utilization) {
            UsageLevel::Warning
        } else {
            UsageLevel::Normal
        }
    }

    /// Convert to single character for icon naming (n=normal, w=warning, c=critical)
    ///
    /// Used for constructing icon filenames in debug logs (e.g., "icon-nw.png" for Normal+Warning).
    pub fn to_char(self) -> char {
        match self {
            Self::Normal => 'n',
            Self::Warning => 'w',
            Self::Critical => 'c',
        }
    }
}

impl MenuId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Status => "status",
            Self::Usage5h => "usage-5h",
            Self::Usage7d => "usage-7d",
            Self::UsageOpus => "usage-opus",
            Self::UsageSonnet => "usage-sonnet",
            Self::LastChecked => "last-checked",
            Self::StartOnLogin => "start-on-login",
            Self::Refresh => "refresh",
            Self::Quit => "quit",
        }
    }
}

impl IconName {
    /// Get the static bytes for this icon variant
    fn bytes(&self) -> &'static [u8] {
        match self {
            Self::Nn => ICON_NN,
            Self::Nw => ICON_NW,
            Self::Nc => ICON_NC,
            Self::Wn => ICON_WN,
            Self::Ww => ICON_WW,
            Self::Wc => ICON_WC,
            Self::Cn => ICON_CN,
            Self::Cw => ICON_CW,
            Self::Cc => ICON_CC,
            Self::Loading => ICON_LOADING,
            Self::Error => ICON_ERROR,
        }
    }
}

// =============================================================================
// Public Functions
// =============================================================================

/// Create the system tray with menu
///
/// # Panics
///
/// Panics if the `cancel_token` mutex is poisoned.
pub fn create_tray(app: &AppHandle) -> Result<TrayIcon, tauri::Error> {
    // Use hardcoded constants from main.rs
    let click_cooldown_secs = TIMING_TRAY_CLICK_REFRESH_COOLDOWN_SECS;
    let graceful_delay_ms = TIMING_GRACEFUL_SHUTDOWN_DELAY_MS;

    let quit_item = MenuItem::with_id(app, MenuId::Quit.as_str(), "Quit", true, None::<&str>)?;
    let loading_item = MenuItem::with_id(
        app,
        MenuId::Status.as_str(),
        "Loading...",
        false,
        None::<&str>,
    )?;

    let menu = Menu::with_items(app, &[&loading_item, &quit_item])?;

    // SAFETY: The "loading" icon is a bundled resource that must exist for the app to function.
    // If it's missing, this is a build/packaging error and we should fail fast.
    let icon = get_icon(IconName::Loading).expect("loading icon must decode");

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip("Claude Usage Tracker - Loading...")
        .on_tray_icon_event(move |tray, event| {
            if let tauri::tray::TrayIconEvent::Click { .. } = event {
                let state: tauri::State<Arc<AppState>> = tray.app_handle().state();
                // Check cooldown before triggering refresh (uses std::sync::Mutex to avoid deadlock)
                // SAFETY: Mutex poisoning indicates a thread panicked while holding the lock.
                // In a tray callback, crashing is safer than continuing with corrupted state.
                let last_checked = state
                    .last_checked
                    .lock()
                    .expect("last_checked lock should not be poisoned");
                let should_refresh = match *last_checked {
                    None => true,
                    Some(ts) => {
                        let now = chrono::Utc::now();
                        let duration = now.signed_duration_since(ts);
                        duration.num_seconds() > click_cooldown_secs
                    }
                };
                drop(last_checked);
                if should_refresh {
                    log::debug!("Tray icon clicked - triggering refresh");
                    state.refresh_notify.notify_one();
                } else {
                    log::debug!("Tray icon clicked - skipping refresh (cooldown)");
                }
            }
        })
        .on_menu_event(move |app, event| {
            match event.id.as_ref().parse::<MenuId>().ok() {
                Some(MenuId::Refresh) => {
                    log::info!("Refresh requested from tray menu");
                    let state: tauri::State<Arc<AppState>> = app.state();
                    state.refresh_notify.notify_one();
                }
                Some(MenuId::Quit) => {
                    log::info!("Quit requested from tray menu");
                    // Cancel the polling loop for graceful shutdown
                    let state: tauri::State<Arc<AppState>> = app.state();
                    state.cancel_token.cancel();
                    // Exit after a brief delay to allow polling loop to clean up
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(graceful_delay_ms))
                            .await;
                        app_handle.exit(0);
                    });
                }
                Some(MenuId::StartOnLogin) => {
                    log::info!("Start on login toggle requested from tray menu");
                    let manager = app.autolaunch();
                    match manager.is_enabled() {
                        Ok(true) => {
                            log::info!("Disabling start on login");
                            if let Err(e) = manager.disable() {
                                log::error!("Failed to disable start on login: {e}");
                            }
                        }
                        Ok(false) => {
                            log::info!("Enabling start on login");
                            if let Err(e) = manager.enable() {
                                log::error!("Failed to enable start on login: {e}");
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to check start on login state: {e}");
                        }
                    }
                    // Update menu to reflect new state
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        update_tray_menu(&app_clone).await;
                    });
                }
                // Non-interactive menu items (labels, separators) - no action needed
                Some(
                    MenuId::Header
                    | MenuId::Status
                    | MenuId::Usage5h
                    | MenuId::Usage7d
                    | MenuId::UsageOpus
                    | MenuId::UsageSonnet
                    | MenuId::LastChecked,
                ) => {}
                // Unknown menu ID - log for debugging
                None => {
                    log::debug!("Unknown menu event: {}", event.id.as_ref());
                }
            }
        })
        .build(app)?;

    log::info!("System tray created successfully");
    Ok(tray)
}

/// Update tray icon based on usage level or error state
pub fn update_tray_icon(app: &AppHandle, usage: &UsageResponse, error: Option<&str>) {
    // Config is read-only, no lock needed
    let state: tauri::State<'_, Arc<AppState>> = app.state();
    let config = &state.config;

    // Get icon name from usage/error state
    let icon_name = if error.is_some() {
        IconName::Error
    } else {
        get_icon_name_for_usage(usage, config)
    };

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        // Try to get the requested icon, fall back to Loading icon on failure
        let icon = get_icon(icon_name).unwrap_or_else(|| {
            log::error!(
                "Failed to load icon '{icon_name:?}' from cache, falling back to default icon"
            );
            get_icon(IconName::Loading).expect("Loading icon must be available")
        });

        if let Err(e) = tray.set_icon(Some(icon)) {
            log::error!("Failed to update tray icon: {e}");
        }

        // Update tooltip with current usage or error
        let tooltip = if let Some(e) = error {
            format!("Error: {e}")
        } else {
            let five_hour_util = get_utilization(usage.five_hour.as_ref());
            let seven_day_util = get_utilization(usage.seven_day.as_ref());
            format!("5h: {five_hour_util:.1}% | 7d: {seven_day_util:.1}%")
        };
        if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
            log::debug!("Failed to set tooltip: {e}");
        }
    }
}

/// Update tray icon to error state without usage data (e.g., first launch failure)
pub fn update_tray_icon_error(app: &AppHandle, error: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        // Try to get the error icon, fall back to Loading icon on failure
        let icon = get_icon(IconName::Error).unwrap_or_else(|| {
            log::error!("Failed to load error icon from cache, falling back to default icon");
            get_icon(IconName::Loading).expect("Loading icon must be available")
        });

        if let Err(e) = tray.set_icon(Some(icon)) {
            log::error!("Failed to update tray icon: {e}");
        }

        let tooltip = format!("Error: {error}");
        if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
            log::debug!("Failed to set tooltip: {e}");
        }
    }
}

/// Update tray menu with current usage
pub async fn update_tray_menu(app: &AppHandle) {
    let state: tauri::State<Arc<AppState>> = app.state();

    // Get async state (separate lock acquisitions)
    let usage = {
        let latest_usage = state.latest_usage.read().await;
        latest_usage.as_ref().map(Arc::clone)
    };
    let error = {
        let last_error = state.last_error.read().await;
        last_error.clone()
    };

    // Get last_checked (sync lock, brief hold)
    // SAFETY: Mutex poisoning indicates a thread panicked while holding the lock.
    // In UI update code, continuing with corrupted state could display incorrect information.
    let last_checked = *state
        .last_checked
        .lock()
        .expect("last_checked lock should not be poisoned");

    // Config is read-only, no lock needed
    let config = &state.config;

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    // Create menu based on current state - prioritize errors even when cached usage exists
    let menu = match (&usage, &error) {
        (_, Some(e)) => {
            let truncated = if e.chars().count() > DISPLAY_TOOLTIP_MAX_LENGTH {
                format!(
                    "{}...",
                    e.chars()
                        .take(DISPLAY_TOOLTIP_TRUNCATED_LENGTH)
                        .collect::<String>()
                )
            } else {
                e.clone()
            };
            let text = format!("Error: {truncated}");
            let _ = tray.set_tooltip(Some(&text));
            build_status_menu(app, &text)
        }
        (Some(u), None) => {
            let five_h = u.five_hour.as_ref().map_or(0.0, |w| w.utilization.as_f64());
            let seven_d = u.seven_day.as_ref().map_or(0.0, |w| w.utilization.as_f64());
            let tooltip = format!("Claude: 5h {five_h:.0}% | 7d {seven_d:.0}%");
            let _ = tray.set_tooltip(Some(&tooltip));
            build_usage_menu(app, u, last_checked, config)
        }
        (None, None) => {
            let _ = tray.set_tooltip(Some("Loading..."));
            build_status_menu(app, "Loading...")
        }
    };

    if let Some(m) = menu {
        if let Err(e) = tray.set_menu(Some(m)) {
            log::error!("Failed to update tray menu: {e}");
        }
    }
}

pub fn get_icon_name_for_usage(usage: &UsageResponse, config: &Config) -> IconName {
    let five_hour_util = get_utilization(usage.five_hour.as_ref());
    let seven_day_util = get_utilization(usage.seven_day.as_ref());

    let five_hour_level = UsageLevel::from_utilization(five_hour_util, config);
    let seven_day_level = UsageLevel::from_utilization(seven_day_util, config);

    log::debug!(
        "Icon state: icon-{}{}.png (5h: {:.1}%, 7d: {:.1}%)",
        five_hour_level.to_char(),
        seven_day_level.to_char(),
        five_hour_util,
        seven_day_util
    );

    // Convert usage levels to icon name using string construction
    // Left char = 5-hour status, right char = 7-day status
    // n=normal, w=warning, c=critical
    match (five_hour_level, seven_day_level) {
        (UsageLevel::Normal, UsageLevel::Normal) => IconName::Nn,
        (UsageLevel::Normal, UsageLevel::Warning) => IconName::Nw,
        (UsageLevel::Normal, UsageLevel::Critical) => IconName::Nc,
        (UsageLevel::Warning, UsageLevel::Normal) => IconName::Wn,
        (UsageLevel::Warning, UsageLevel::Warning) => IconName::Ww,
        (UsageLevel::Warning, UsageLevel::Critical) => IconName::Wc,
        (UsageLevel::Critical, UsageLevel::Normal) => IconName::Cn,
        (UsageLevel::Critical, UsageLevel::Warning) => IconName::Cw,
        (UsageLevel::Critical, UsageLevel::Critical) => IconName::Cc,
    }
}

// =============================================================================
// Private Functions
// =============================================================================

/// Check if start on login is enabled
fn is_autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Create the autostart menu item with checkmark indicator
fn create_autostart_menu_item(app: &AppHandle) -> Option<MenuItem<tauri::Wry>> {
    let autostart_enabled = is_autostart_enabled(app);
    let autostart_text = if autostart_enabled {
        "  ✓ Start on login"
    } else {
        "    Start on login"
    };
    MenuItem::with_id(
        app,
        MenuId::StartOnLogin.as_str(),
        autostart_text,
        true,
        None::<&str>,
    )
    .ok()
}

/// Get icon from cache by name, decoding all icons on first access
fn get_icon(name: IconName) -> Option<Image<'static>> {
    // Initialize cache first - ensures fallback path always works
    let cache = ICON_CACHE.get_or_init(|| {
        let mut map = HashMap::new();

        // Precompute all icons at startup
        let icon_variants = [
            IconName::Nn,
            IconName::Nw,
            IconName::Nc,
            IconName::Wn,
            IconName::Ww,
            IconName::Wc,
            IconName::Cn,
            IconName::Cw,
            IconName::Cc,
            IconName::Loading,
            IconName::Error,
        ];

        for name in icon_variants {
            match Image::from_bytes(name.bytes()) {
                Ok(image) => {
                    map.insert(name, image);
                }
                Err(e) => {
                    log::error!("Failed to decode icon '{name:?}': {e}");
                }
            }
        }

        map
    });

    cache.get(&name).cloned()
}

/// Helper to extract utilization from an optional `UsageWindow`
fn get_utilization(window: Option<&crate::api::UsageWindow>) -> f64 {
    window.map_or(0.0, |w| w.utilization.as_f64())
}

/// Build a simple status menu (error/loading states)
fn build_status_menu(app: &AppHandle, status_text: &str) -> Option<Menu<tauri::Wry>> {
    use tauri::menu::PredefinedMenuItem;

    let header = MenuItem::with_id(
        app,
        MenuId::Header.as_str(),
        " ━━ Claude Usage ━━ ",
        false,
        None::<&str>,
    )
    .ok();
    let sep1 = PredefinedMenuItem::separator(app).ok();
    let status = MenuItem::with_id(
        app,
        MenuId::Status.as_str(),
        status_text,
        false,
        None::<&str>,
    )
    .ok();
    let sep2 = PredefinedMenuItem::separator(app).ok();
    let autostart = create_autostart_menu_item(app);

    let refresh = MenuItem::with_id(
        app,
        MenuId::Refresh.as_str(),
        "  ↻ Refresh",
        true,
        None::<&str>,
    )
    .ok();
    let quit = MenuItem::with_id(app, MenuId::Quit.as_str(), "  Quit", true, None::<&str>).ok();

    match (header, sep1, status, sep2, autostart, refresh, quit) {
        (Some(h), Some(s1), Some(st), Some(s2), Some(a), Some(r), Some(q)) => {
            Menu::with_items(app, &[&h, &s1, &st, &s2, &a, &r, &q]).ok()
        }
        _ => None,
    }
}

/// Build a usage menu with current usage data
fn build_usage_menu(
    app: &AppHandle,
    usage: &UsageResponse,
    last_checked: Option<chrono::DateTime<chrono::Utc>>,
    config: &Config,
) -> Option<Menu<tauri::Wry>> {
    use chrono::Local;
    use tauri::menu::PredefinedMenuItem;

    // Centered header (approximately centered for typical menu width ~30 chars)
    let header = MenuItem::with_id(
        app,
        MenuId::Header.as_str(),
        " ━━ Claude Usage ━━ ",
        false,
        None::<&str>,
    )
    .ok()?;
    let sep1 = PredefinedMenuItem::separator(app).ok()?;

    // Build formatted usage items with visual indicators
    let item_5h = format_usage_item(
        app,
        usage.five_hour.as_ref(),
        window_labels::FIVE_HOUR,
        config,
    );
    let item_7d = format_usage_item(
        app,
        usage.seven_day.as_ref(),
        window_labels::SEVEN_DAY,
        config,
    );
    let item_opus = format_usage_item(
        app,
        usage.seven_day_opus.as_ref(),
        window_labels::OPUS,
        config,
    );
    let item_sonnet = format_usage_item(
        app,
        usage.seven_day_sonnet.as_ref(),
        window_labels::SONNET,
        config,
    );

    // Last checked time
    let last_checked_text = last_checked.map_or_else(
        || "  Last check: never".to_string(),
        |ts| format!("  Last check: {}", ts.with_timezone(&Local).format("%H:%M")),
    );
    let item_last_checked = MenuItem::with_id(
        app,
        MenuId::LastChecked.as_str(),
        &last_checked_text,
        false,
        None::<&str>,
    )
    .ok()?;

    let sep2 = PredefinedMenuItem::separator(app).ok()?;

    let autostart = create_autostart_menu_item(app)?;

    let refresh = MenuItem::with_id(
        app,
        MenuId::Refresh.as_str(),
        "  ↻ Refresh",
        true,
        None::<&str>,
    )
    .ok()?;
    let quit = MenuItem::with_id(app, MenuId::Quit.as_str(), "  Quit", true, None::<&str>).ok()?;

    // Build menu items list - collect non-optional items
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&header, &sep1];

    // Add each usage item if it exists
    for i in [&item_5h, &item_7d, &item_opus, &item_sonnet]
        .into_iter()
        .flatten()
    {
        items.push(i);
    }

    items.push(&item_last_checked);
    items.push(&sep2);
    items.push(&autostart);
    items.push(&refresh);
    items.push(&quit);

    Menu::with_items(app, &items).ok()
}

/// Format reset time as relative duration
fn format_reset(reset_at: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    use chrono::Utc;

    let Some(reset_time) = reset_at else {
        return String::new();
    };

    let now = Utc::now();
    let duration = reset_time.signed_duration_since(now);

    if duration.num_seconds() <= 0 {
        return " (resetting)".to_string();
    }

    let hours = duration.num_hours();
    let minutes = duration.num_minutes() % DISPLAY_MINUTES_PER_HOUR;

    if hours >= DISPLAY_HOURS_PER_DAY {
        let days = hours / DISPLAY_HOURS_PER_DAY;
        let rem_hours = hours % DISPLAY_HOURS_PER_DAY;
        format!(" ({days}d {rem_hours}h)")
    } else if hours > 0 {
        format!(" ({hours}h {minutes}m)")
    } else {
        format!(" ({minutes}m)")
    }
}

/// Get visual indicator emoji based on usage percentage
fn get_usage_indicator(utilization: f64, config: &Config) -> &'static str {
    if config.is_above_critical(utilization) {
        "🔴"
    } else if config.is_above_warning(utilization) {
        "🟡"
    } else {
        "🟢"
    }
}

/// Create a visual progress bar using Unicode block characters
fn make_progress_bar(utilization: f64, width: u8) -> String {
    // Calculate filled portion using lossless f64::from(u8) conversion
    let width_f64 = f64::from(width);
    let filled_f64 = (utilization / 100.0 * width_f64)
        .round()
        .clamp(0.0, width_f64);
    // Safe: clamped to 0..=width, which fits in u8
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = filled_f64 as usize;
    let empty = usize::from(width) - filled;

    // Use block characters: █ for filled, ░ for empty
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Format a single usage item with visual styling
fn format_usage_item(
    app: &AppHandle,
    window: Option<&crate::api::UsageWindow>,
    label: &str,
    config: &Config,
) -> Option<tauri::menu::MenuItem<tauri::Wry>> {
    let window = window?;
    let util = window.utilization.as_f64();

    // Fixed column positions for alignment
    // Format: "  [LABEL    ] E 00.0% BARTEN...... (reset)"
    //         ^  ^--------^ ^ ^----^ ^----------^
    //         |  label(9)  sp perc  bar(10)

    // Select appropriate indicator based on window type
    // Opus/Sonnet use neutral indicator (no notifications, no icon influence per design decision)
    let indicator = match label {
        window_labels::OPUS | window_labels::SONNET => "⚪",
        _ => get_usage_indicator(util, config),
    };

    let bar = make_progress_bar(util, DISPLAY_PROGRESS_BAR_WIDTH);
    let reset = format_reset(window.resets_at.as_ref());

    // Format label in fixed-width field
    let text = format!("  [{label}] {indicator} {util:>5.1}% {bar}{reset}");

    let id = match label {
        window_labels::FIVE_HOUR => MenuId::Usage5h.as_str(),
        window_labels::SEVEN_DAY => MenuId::Usage7d.as_str(),
        window_labels::OPUS => MenuId::UsageOpus.as_str(),
        window_labels::SONNET => MenuId::UsageSonnet.as_str(),
        _ => MenuId::Status.as_str(),
    };

    MenuItem::with_id(app, id, &text, false, None::<&str>).ok()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(dead_code)]
    use super::*;

    /// Get the appropriate icon based on usage levels (split icon for 5h/7d)
    pub fn get_icon_for_usage(usage: &UsageResponse, config: &Config) -> bool {
        get_icon(get_icon_name_for_usage(usage, config)).is_some()
    }

    #[test]
    fn test_get_icon_name_for_usage_all_combinations() {
        use crate::api::{UsageResponse, UsageWindow, Utilization};
        use UsageLevel::*;

        let config = Config::default();

        // All 9 combinations should return valid icon names
        let combinations = [
            (Normal, Normal, IconName::Nn),
            (Normal, Warning, IconName::Nw),
            (Normal, Critical, IconName::Nc),
            (Warning, Normal, IconName::Wn),
            (Warning, Warning, IconName::Ww),
            (Warning, Critical, IconName::Wc),
            (Critical, Normal, IconName::Cn),
            (Critical, Warning, IconName::Cw),
            (Critical, Critical, IconName::Cc),
        ];

        for (five_h, seven_d, expected) in combinations {
            let usage = UsageResponse {
                five_hour: Some(UsageWindow {
                    utilization: Utilization::new(0.0),
                    resets_at: None,
                }),
                seven_day: Some(UsageWindow {
                    utilization: Utilization::new(0.0),
                    resets_at: None,
                }),
                seven_day_opus: None,
                seven_day_sonnet: None,
            };
            // Modify the usage levels by setting appropriate utilization values
            let usage = modify_usage_for_level(usage, five_h, seven_d);
            let icon_name = get_icon_name_for_usage(&usage, &config);
            assert_eq!(
                icon_name, expected,
                "Icon for ({five_h:?}, {seven_d:?}) should be {expected:?}"
            );
        }
    }

    // Helper to create UsageResponse with specific usage levels
    fn modify_usage_for_level(
        mut usage: UsageResponse,
        five_h: UsageLevel,
        seven_d: UsageLevel,
    ) -> UsageResponse {
        use crate::api::Utilization;
        let five_val = match five_h {
            UsageLevel::Normal => 50.0,
            UsageLevel::Warning => 80.0,
            UsageLevel::Critical => 95.0,
        };
        let seven_val = match seven_d {
            UsageLevel::Normal => 50.0,
            UsageLevel::Warning => 80.0,
            UsageLevel::Critical => 95.0,
        };
        if let Some(w) = usage.five_hour.as_mut() {
            w.utilization = Utilization::new(five_val);
        }
        if let Some(w) = usage.seven_day.as_mut() {
            w.utilization = Utilization::new(seven_val);
        }
        usage
    }

    #[test]
    fn test_get_usage_indicator_normal() {
        let config = Config::default();
        // Below warning threshold (default 75%)
        assert_eq!(get_usage_indicator(50.0, &config), "🟢");
        assert_eq!(get_usage_indicator(0.0, &config), "🟢");
        assert_eq!(get_usage_indicator(74.9, &config), "🟢");
    }

    #[test]
    fn test_get_usage_indicator_warning() {
        let config = Config::default();
        // At or above warning (75%) but below critical (90%)
        assert_eq!(get_usage_indicator(75.0, &config), "🟡");
        assert_eq!(get_usage_indicator(80.0, &config), "🟡");
        assert_eq!(get_usage_indicator(89.9, &config), "🟡");
    }

    #[test]
    fn test_get_usage_indicator_critical() {
        let config = Config::default();
        // At or above critical threshold (default 90%)
        assert_eq!(get_usage_indicator(90.0, &config), "🔴");
        assert_eq!(get_usage_indicator(95.0, &config), "🔴");
        assert_eq!(get_usage_indicator(100.0, &config), "🔴");
    }

    #[test]
    fn test_get_usage_indicator_custom_thresholds() {
        use crate::config::Percentage;
        let config = Config {
            warning_threshold: Percentage::new(60.0).unwrap(),
            critical_threshold: Percentage::new(85.0).unwrap(),
            ..Default::default()
        };

        // Below warning
        assert_eq!(get_usage_indicator(50.0, &config), "🟢");

        // At warning but below critical
        assert_eq!(get_usage_indicator(60.0, &config), "🟡");
        assert_eq!(get_usage_indicator(80.0, &config), "🟡");

        // At critical
        assert_eq!(get_usage_indicator(85.0, &config), "🔴");
        assert_eq!(get_usage_indicator(95.0, &config), "🔴");
    }

    #[test]
    fn test_get_usage_indicator_edge_values() {
        let config = Config::default();
        // Negative values should be treated as normal (below all thresholds)
        assert_eq!(get_usage_indicator(-1.0, &config), "🟢");
        assert_eq!(get_usage_indicator(-100.0, &config), "🟢");
        // Values above 100% should still be critical
        assert_eq!(get_usage_indicator(150.0, &config), "🔴");
    }
}
