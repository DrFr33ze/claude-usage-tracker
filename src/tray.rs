//! System tray icon and menu management for Claude Usage Tracker.
//!
//! This module handles all tray-related functionality including:
//! - Split icon system showing 5-hour (left) and 7-day (right) status
//! - Menu construction with usage data display
//! - Tray event handling:
//!   - Left-click → show popup window
//!   - Right-click → context menu (settings)

// =============================================================================
// Imports
// =============================================================================

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};
use tauri_plugin_autostart::ManagerExt as _;

use crate::api::UsageResponse;
use crate::config::Config;
use crate::AppState;

// =============================================================================
// Constants
// =============================================================================

// Display and UI constants
const DISPLAY_TOOLTIP_MAX_LENGTH: usize = 50;
const DISPLAY_TOOLTIP_TRUNCATED_LENGTH: usize = 47;

// Timing constants
const TIMING_GRACEFUL_SHUTDOWN_DELAY_MS: u64 = 100;

// Popup window dimensions (must match the window built in lib.rs)
const POPUP_WIDTH: i32 = 290;
const POPUP_HEIGHT: i32 = 140;
/// Approximate taskbar height in logical pixels (used for default positioning)
const TASKBAR_APPROX_PX: i32 = 48;

// Tray identifier
const TRAY_ID: &str = "main-tray";

// Icon bytes (embedded at compile time)
const ICON_LOADING: &[u8] = include_bytes!("../icons/icon-loading.png");
const ICON_ERROR: &[u8] = include_bytes!("../icons/icon-error.png");

// Split icons: left half = 5-hour status, right half = 7-day status
const ICON_NN: &[u8] = include_bytes!("../icons/icon-nn.png");
const ICON_NW: &[u8] = include_bytes!("../icons/icon-nw.png");
const ICON_NC: &[u8] = include_bytes!("../icons/icon-nc.png");
const ICON_WN: &[u8] = include_bytes!("../icons/icon-wn.png");
const ICON_WW: &[u8] = include_bytes!("../icons/icon-ww.png");
const ICON_WC: &[u8] = include_bytes!("../icons/icon-wc.png");
const ICON_CN: &[u8] = include_bytes!("../icons/icon-cn.png");
const ICON_CW: &[u8] = include_bytes!("../icons/icon-cw.png");
const ICON_CC: &[u8] = include_bytes!("../icons/icon-cc.png");

static ICON_CACHE: OnceLock<HashMap<IconName, Image<'static>>> = OnceLock::new();

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
    KeepWindowOpen,
    AlwaysOnTop,
    StartOnLogin,
    RefreshOnOpen,
    Reauth,
    Quit,
}

// =============================================================================
// Private Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconName {
    Nn,
    Nw,
    Nc,
    Wn,
    Ww,
    Wc,
    Cn,
    Cw,
    Cc,
    Loading,
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
            "keep-window-open" => Ok(Self::KeepWindowOpen),
            "always-on-top" => Ok(Self::AlwaysOnTop),
            "start-on-login" => Ok(Self::StartOnLogin),
            "refresh-on-open" => Ok(Self::RefreshOnOpen),
            "reauth" => Ok(Self::Reauth),
            "quit" => Ok(Self::Quit),
            _ => Err(()),
        }
    }
}

impl UsageLevel {
    pub fn from_utilization(utilization: f64, config: &Config) -> Self {
        if config.is_above_critical(utilization) {
            UsageLevel::Critical
        } else if config.is_above_warning(utilization) {
            UsageLevel::Warning
        } else {
            UsageLevel::Normal
        }
    }

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
            Self::KeepWindowOpen => "keep-window-open",
            Self::AlwaysOnTop => "always-on-top",
            Self::StartOnLogin => "start-on-login",
            Self::RefreshOnOpen => "refresh-on-open",
            Self::Reauth => "reauth",
            Self::Quit => "quit",
        }
    }
}

impl IconName {
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

/// Create the system tray with menu.
///
/// Left-click shows the popup window; right-click shows the context menu.
pub fn create_tray(app: &AppHandle) -> Result<TrayIcon, tauri::Error> {
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

    let icon = get_icon(IconName::Loading).expect("loading icon must decode");

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip("Claude Usage Tracker - Loading...")
        // Prevent the menu from auto-opening on left-click so we can show popup instead
        .show_menu_on_left_click(false)
        .on_tray_icon_event(move |tray, event| {
            // Left-click (button-up) → show popup window
            if let tauri::tray::TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button == tauri::tray::MouseButton::Left
                    && button_state == tauri::tray::MouseButtonState::Up
                {
                    show_popup(tray.app_handle());
                }
            }
        })
        .on_menu_event(move |app, event| {
            match event.id.as_ref().parse::<MenuId>().ok() {
                Some(MenuId::Quit) => {
                    log::info!("Quit requested from tray menu");
                    let state: tauri::State<Arc<AppState>> = app.state();
                    state.cancel_token.cancel();
                    let app_handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(graceful_delay_ms))
                            .await;
                        app_handle.exit(0);
                    });
                }
                Some(MenuId::KeepWindowOpen) => {
                    log::info!("Keep window open toggle requested");
                    let state: tauri::State<Arc<AppState>> = app.state();
                    let current = state.keep_window_open.load(Ordering::Relaxed);
                    state.keep_window_open.store(!current, Ordering::Relaxed);
                    log::info!("Keep window open set to {}", !current);
                    // Emit updated DTO to popup so Close button visibility updates
                    let state_inner = state.inner().clone();
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let dto = crate::commands::build_usage_dto(&state_inner).await;
                        app_clone.emit("usage-updated", &dto).ok();
                        update_tray_menu(&app_clone).await;
                    });
                }
                Some(MenuId::AlwaysOnTop) => {
                    log::info!("Always on top toggle requested");
                    let state: tauri::State<Arc<AppState>> = app.state();
                    let current = state.always_on_top.load(Ordering::Relaxed);
                    state.always_on_top.store(!current, Ordering::Relaxed);
                    log::info!("Always on top set to {}", !current);
                    if let Some(popup) = app.get_webview_window("popup") {
                        if let Err(e) = popup.set_always_on_top(!current) {
                            log::error!("Failed to set always on top: {e}");
                        }
                    }
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        update_tray_menu(&app_clone).await;
                    });
                }
                Some(MenuId::RefreshOnOpen) => {
                    log::info!("Refresh on open toggle requested");
                    let state: tauri::State<Arc<AppState>> = app.state();
                    let current = state.refresh_on_open.load(Ordering::Relaxed);
                    state.refresh_on_open.store(!current, Ordering::Relaxed);
                    log::info!("Refresh on open set to {}", !current);
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        update_tray_menu(&app_clone).await;
                    });
                }
                Some(MenuId::Reauth) => {
                    log::info!("Re-authentication requested from tray menu");
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        if let Err(e) = std::process::Command::new("cmd")
                            .args(["/c", "claude", "auth", "login"])
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn()
                        {
                            log::error!("Failed to launch claude auth login: {e}");
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        if let Err(e) = std::process::Command::new("claude")
                            .args(["auth", "login"])
                            .spawn()
                        {
                            log::error!("Failed to launch claude auth login: {e}");
                        }
                    }
                }
                Some(MenuId::StartOnLogin) => {
                    log::info!("Start on login toggle requested");
                    let manager = app.autolaunch();
                    match manager.is_enabled() {
                        Ok(true) => {
                            if let Err(e) = manager.disable() {
                                log::error!("Failed to disable start on login: {e}");
                            }
                        }
                        Ok(false) => {
                            if let Err(e) = manager.enable() {
                                log::error!("Failed to enable start on login: {e}");
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to check start on login state: {e}");
                        }
                    }
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        update_tray_menu(&app_clone).await;
                    });
                }
                // Header → open GitHub repo in browser
                Some(MenuId::Header) => {
                    crate::commands::open_github();
                }
                // Non-interactive label items
                Some(MenuId::Status) => {}
                None => {
                    log::debug!("Unknown menu event: {}", event.id.as_ref());
                }
            }
        })
        .build(app)?;

    log::info!("System tray created successfully");
    Ok(tray)
}

/// Toggle the popup window: hide if currently visible, show at saved position otherwise.
pub fn show_popup(app: &AppHandle) {
    let Some(popup) = app.get_webview_window("popup") else {
        log::error!("Popup window not found");
        return;
    };

    // Toggle: if already visible, save position and hide
    if popup.is_visible().unwrap_or(false) {
        let state: tauri::State<Arc<AppState>> = app.state();
        if let Ok(pos) = popup.outer_position() {
            *state
                .window_position
                .lock()
                .expect("window_position lock not poisoned") = Some((pos.x, pos.y));
            crate::save_window_position_to_file(pos.x, pos.y);
        }
        let _ = popup.hide();
        return;
    }

    let state: tauri::State<Arc<AppState>> = app.state();

    // Determine where to show the popup
    let saved = {
        let pos = state
            .window_position
            .lock()
            .expect("window_position lock not poisoned");
        *pos
    };

    let position = saved
        .and_then(|(x, y)| validate_popup_position(app, x, y))
        .or_else(|| default_popup_position(app));

    if let Some((x, y)) = position {
        if let Err(e) = popup.set_position(PhysicalPosition::new(x, y)) {
            log::warn!("Failed to set popup position: {e}");
        }
    }

    if let Err(e) = popup.show() {
        log::error!("Failed to show popup: {e}");
        return;
    }
    if let Err(e) = popup.set_focus() {
        log::warn!("Failed to focus popup: {e}");
    }

    // Trigger a data refresh when popup opens (if enabled)
    if state.refresh_on_open.load(Ordering::Relaxed) {
        state.refresh_notify.notify_one();
    }

    log::debug!("Popup shown at position {:?}", position);
}

/// Update tray icon based on usage level or error state
pub fn update_tray_icon(app: &AppHandle, usage: &UsageResponse, error: Option<&str>) {
    let state: tauri::State<'_, Arc<AppState>> = app.state();
    let config = &state.config;

    let icon_name = if error.is_some() {
        IconName::Error
    } else {
        get_icon_name_for_usage(usage, config)
    };

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let icon = get_icon(icon_name).unwrap_or_else(|| {
            log::error!("Failed to load icon '{icon_name:?}', falling back to loading icon");
            get_icon(IconName::Loading).expect("Loading icon must be available")
        });

        if let Err(e) = tray.set_icon(Some(icon)) {
            log::error!("Failed to update tray icon: {e}");
        }

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

/// Update tray icon to error state without usage data
pub fn update_tray_icon_error(app: &AppHandle, error: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let icon = get_icon(IconName::Error).unwrap_or_else(|| {
            log::error!("Failed to load error icon, falling back to loading icon");
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

/// Update tray menu with current settings state (usage shown in popup only).
pub async fn update_tray_menu(app: &AppHandle) {
    let state: tauri::State<Arc<AppState>> = app.state();

    let error = {
        let last_error = state.last_error.read().await;
        last_error.clone()
    };

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    let error_display = error.as_ref().map(|e| {
        if e.chars().count() > DISPLAY_TOOLTIP_MAX_LENGTH {
            format!(
                "{}...",
                e.chars()
                    .take(DISPLAY_TOOLTIP_TRUNCATED_LENGTH)
                    .collect::<String>()
            )
        } else {
            e.clone()
        }
    });

    if let Some(ref e) = error_display {
        let _ = tray.set_tooltip(Some(&format!("Error: {e}")));
    }

    if let Some(m) = build_status_menu(app, error_display.as_deref()) {
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

/// Validate that the given position is visible on at least one monitor.
///
/// Returns `Some((x, y))` if valid, `None` to trigger fallback to default position.
fn validate_popup_position(app: &AppHandle, x: i32, y: i32) -> Option<(i32, i32)> {
    let popup = app.get_webview_window("popup")?;
    let monitors = popup.available_monitors().ok()?;

    // Check that at least the top-left corner of the popup is on a monitor
    let is_visible = monitors.iter().any(|m| {
        let pos = m.position();
        let size = m.size();
        x >= pos.x
            && x < pos.x + size.width as i32
            && y >= pos.y
            && y < pos.y + size.height as i32
    });

    if is_visible {
        Some((x, y))
    } else {
        log::debug!("Saved popup position ({x}, {y}) is off-screen, using default");
        None
    }
}

/// Calculate the default popup position: bottom-right corner above the taskbar.
///
/// `inner_size` is specified in **logical** pixels; we must multiply by the monitor's
/// scale factor to get the correct **physical** pixel offset for `set_position`.
fn default_popup_position(app: &AppHandle) -> Option<(i32, i32)> {
    let popup = app.get_webview_window("popup")?;
    let monitor = popup.primary_monitor().ok()??;

    let pos = monitor.position();
    let size = monitor.size();
    let scale = monitor.scale_factor();

    // Convert logical dimensions → physical pixels
    let popup_w = (f64::from(POPUP_WIDTH) * scale).round() as i32;
    let popup_h = (f64::from(POPUP_HEIGHT) * scale).round() as i32;
    let taskbar = (f64::from(TASKBAR_APPROX_PX) * scale).round() as i32;

    const MARGIN: i32 = 4;

    let x = pos.x + size.width as i32 - popup_w - MARGIN;
    let y = pos.y + size.height as i32 - popup_h - taskbar - MARGIN;

    Some((x.max(pos.x), y.max(pos.y)))
}

fn create_keep_window_open_menu_item(app: &AppHandle) -> Option<MenuItem<tauri::Wry>> {
    let state: tauri::State<Arc<AppState>> = app.state();
    let enabled = state.keep_window_open.load(Ordering::Relaxed);
    let text = if enabled {
        "  ✓ Keep window open"
    } else {
        "    Keep window open"
    };
    MenuItem::with_id(
        app,
        MenuId::KeepWindowOpen.as_str(),
        text,
        true,
        None::<&str>,
    )
    .ok()
}

fn create_always_on_top_menu_item(app: &AppHandle) -> Option<MenuItem<tauri::Wry>> {
    let state: tauri::State<Arc<AppState>> = app.state();
    let enabled = state.always_on_top.load(Ordering::Relaxed);
    let text = if enabled {
        "  ✓ Always on top"
    } else {
        "    Always on top"
    };
    MenuItem::with_id(
        app,
        MenuId::AlwaysOnTop.as_str(),
        text,
        true,
        None::<&str>,
    )
    .ok()
}

fn create_refresh_on_open_menu_item(app: &AppHandle) -> Option<MenuItem<tauri::Wry>> {
    let state: tauri::State<Arc<AppState>> = app.state();
    let enabled = state.refresh_on_open.load(Ordering::Relaxed);
    let text = if enabled {
        "  ✓ Refresh on open"
    } else {
        "    Refresh on open"
    };
    MenuItem::with_id(
        app,
        MenuId::RefreshOnOpen.as_str(),
        text,
        true,
        None::<&str>,
    )
    .ok()
}

fn is_autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

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

fn get_icon(name: IconName) -> Option<Image<'static>> {
    let cache = ICON_CACHE.get_or_init(|| {
        let mut map = HashMap::new();
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

fn get_utilization(window: Option<&crate::api::UsageWindow>) -> f64 {
    window.map_or(0.0, |w| w.utilization.as_f64())
}

/// Build the right-click context menu.
///
/// The header item is a clickable link to the GitHub repo.
/// If `error` is `Some`, an error line and Re-authenticate item are inserted
/// between the header and the settings items.
fn build_status_menu(app: &AppHandle, error: Option<&str>) -> Option<Menu<tauri::Wry>> {
    use tauri::menu::PredefinedMenuItem;

    // Clickable title → opens GitHub repo
    let header = MenuItem::with_id(
        app,
        MenuId::Header.as_str(),
        "  Claude Usage Tracker ↗",
        true,
        None::<&str>,
    )
    .ok()?;
    let sep1 = PredefinedMenuItem::separator(app).ok()?;
    let keep_window_open = create_keep_window_open_menu_item(app)?;
    let always_on_top = create_always_on_top_menu_item(app)?;
    let autostart = create_autostart_menu_item(app)?;
    let refresh_on_open = create_refresh_on_open_menu_item(app)?;
    let quit =
        MenuItem::with_id(app, MenuId::Quit.as_str(), "  Quit", true, None::<&str>).ok()?;

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        vec![&header, &sep1];

    // Error state: show message + re-auth + extra separator before settings
    let error_item = error.and_then(|e| {
        MenuItem::with_id(
            app,
            MenuId::Status.as_str(),
            &format!("  ⚠ {e}"),
            false,
            None::<&str>,
        )
        .ok()
    });
    let reauth = if error.is_some() {
        MenuItem::with_id(
            app,
            MenuId::Reauth.as_str(),
            "  ⟳ Re-authenticate",
            true,
            None::<&str>,
        )
        .ok()
    } else {
        None
    };
    let sep2 = if error.is_some() {
        PredefinedMenuItem::separator(app).ok()
    } else {
        None
    };

    if let Some(ref e) = error_item { items.push(e); }
    if let Some(ref r) = reauth    { items.push(r); }
    if let Some(ref s) = sep2      { items.push(s); }

    items.push(&keep_window_open);
    items.push(&always_on_top);
    items.push(&autostart);
    items.push(&refresh_on_open);
    items.push(&quit);

    Menu::with_items(app, &items).ok()
}

#[cfg(test)]
fn get_usage_indicator(utilization: f64, config: &Config) -> &'static str {
    if config.is_above_critical(utilization) {
        "🔴"
    } else if config.is_above_warning(utilization) {
        "🟡"
    } else {
        "🟢"
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(dead_code)]
    use super::*;

    pub fn get_icon_for_usage(usage: &UsageResponse, config: &Config) -> bool {
        get_icon(get_icon_name_for_usage(usage, config)).is_some()
    }

    #[test]
    fn test_get_icon_name_for_usage_all_combinations() {
        use crate::api::{UsageResponse, UsageWindow, Utilization};
        use UsageLevel::*;

        let config = Config::default();

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
            let usage = modify_usage_for_level(usage, five_h, seven_d);
            let icon_name = get_icon_name_for_usage(&usage, &config);
            assert_eq!(
                icon_name, expected,
                "Icon for ({five_h:?}, {seven_d:?}) should be {expected:?}"
            );
        }
    }

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
        assert_eq!(get_usage_indicator(50.0, &config), "🟢");
        assert_eq!(get_usage_indicator(0.0, &config), "🟢");
        assert_eq!(get_usage_indicator(74.9, &config), "🟢");
    }

    #[test]
    fn test_get_usage_indicator_warning() {
        let config = Config::default();
        assert_eq!(get_usage_indicator(75.0, &config), "🟡");
        assert_eq!(get_usage_indicator(80.0, &config), "🟡");
        assert_eq!(get_usage_indicator(89.9, &config), "🟡");
    }

    #[test]
    fn test_get_usage_indicator_critical() {
        let config = Config::default();
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

        assert_eq!(get_usage_indicator(50.0, &config), "🟢");
        assert_eq!(get_usage_indicator(60.0, &config), "🟡");
        assert_eq!(get_usage_indicator(80.0, &config), "🟡");
        assert_eq!(get_usage_indicator(85.0, &config), "🔴");
        assert_eq!(get_usage_indicator(95.0, &config), "🔴");
    }

    #[test]
    fn test_get_usage_indicator_edge_values() {
        let config = Config::default();
        assert_eq!(get_usage_indicator(-1.0, &config), "🟢");
        assert_eq!(get_usage_indicator(-100.0, &config), "🟢");
        assert_eq!(get_usage_indicator(150.0, &config), "🔴");
    }

    #[test]
    fn test_menu_id_roundtrip() {
        use std::str::FromStr;
        let ids = [
            MenuId::KeepWindowOpen,
            MenuId::AlwaysOnTop,
            MenuId::StartOnLogin,
            MenuId::RefreshOnOpen,
            MenuId::Quit,
        ];
        for id in ids {
            let s = id.as_str();
            let parsed = MenuId::from_str(s).expect("should parse");
            assert_eq!(parsed, id);
        }
    }
}
