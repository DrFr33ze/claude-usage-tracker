<!-- OPENSPEC:START -->
# OpenSpec Instructions

These instructions are for AI assistants working in this project.

Always open `@/openspec/AGENTS.md` when the request:
- Mentions planning or proposals (words like proposal, spec, change, plan)
- Introduces new capabilities, breaking changes, architecture shifts, or big performance/security work
- Sounds ambiguous and you need the authoritative spec before coding

Use `@/openspec/AGENTS.md` to learn:
- How to create and apply change proposals
- Spec format and conventions
- Project structure and guidelines

Keep this managed block so 'openspec update' can refresh the instructions.

<!-- OPENSPEC:END -->

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
# Development (hot reload)
cargo tauri dev

# Production build (binary only)
cargo tauri build

# Production build with Linux bundles (.deb, .AppImage)
cargo tauri build --bundles deb,appimage

# Check/lint (fast type check without full build)
cargo check

# Run tests
cargo test

# Run a single test
cargo test test_name

# Run tests with output
cargo test -- --nocapture

# List all tests
cargo test -- --list
```

**Bundle output location:** Built bundles are written to `target/release/bundle/`:
- `.deb` packages: `target/release/bundle/deb/`
- `.AppImage` packages: `target/release/bundle/appimage/`

### Test Modules

Tests are colocated with their modules (inline `#[cfg(test)]` blocks). Key test coverage:

| Module | Description |
|--------|-------------|
| `api.rs` | API response parsing, reset timestamp handling, utilization clamping |
| `config.rs` | Config validation (thresholds ordering, polling interval bounds) |
| `service.rs` | Notification state machine, cooldown logic, escalation behavior |
| `tray.rs` | Usage indicator formatting, split icon name generation |

**Integration tests** in `tests/integration.rs` test actual HTTP behavior with a mock server.

**Test patterns:**
- Unit tests use `#[cfg(test)] mod tests {}` inline with modules
- Helper functions like `make_config()` create test fixtures
- Tests cover edge cases (boundary values, error conditions)

### Linux Dependencies

```bash
# Ubuntu/Debian
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev

# Fedora
sudo dnf install webkit2gtk4.1-devel libappindicator-gtk3-devel

# Arch
sudo pacman -S webkit2gtk-4.1 libayatana-appindicator
```

## Architecture

This is a **Tauri 2.x** system tray application that monitors Claude Code API usage. It has a decorationless **popup window** (HTML/JS frontend) that opens on left-click and a settings context menu on right-click.

### Module Structure

- **main.rs** - Entry point and thin wiring layer. Sets up Tauri, spawns the polling service and event handler tasks.
- **lib.rs** - Module declarations, `AppState` struct, popup window creation, and `event_handler_loop`. Re-exports `Config` and `Credentials`.
- **commands.rs** - Tauri commands exposed to the popup frontend: `get_usage_data`, `trigger_refresh`, `save_window_position`, `hide_popup`, `open_github`. Also contains `UsageDto`/`WindowDto` and `build_usage_dto()`.
- **events.rs** - Event types for service-to-application communication: `AppEvent` enum and `CredentialRefreshResult`.
- **service.rs** - Core service layer: polling logic (`polling_loop`), notification state management (`NotificationState`, `check_window_notification`), credential refresh handling, and cooldown logic. Framework-agnostic, emits `AppEvent` messages.
- **auth.rs** - Reads credentials from `~/.claude/.credentials.json`. Provides `load_credentials()` and `Credentials` struct.
- **api.rs** - HTTP client for Claude usage API. Returns `UsageResponse` with 5-hour, 7-day, and 7-day Opus/Sonnet utilization windows. Includes retry logic with exponential backoff.
- **tray.rs** - System tray icon, menu management, popup positioning, and `MenuId` enum for menu event handling.
- **config.rs** - Simplified flat configuration with 5 fields: thresholds (warning/critical/reset) and intervals (polling/notification cooldown).

### Configuration System

The app loads configuration from `~/.config/claude-usage-tracker/config.toml` at startup. If no config file exists, a default one is created automatically. All settings have sensible defaults.

**Config struct (flat design):**
- `Config` - Single flat struct with 5 fields:
  - `warning_threshold` (75.0) - Usage % to trigger warning
  - `critical_threshold` (90.0) - Usage % to trigger critical alert
  - `reset_threshold` (50.0) - Usage % below which alerts reset
  - `polling_interval_minutes` (2) - How often to check API
  - `notification_cooldown_minutes` (5) - Cooldown period that prevents duplicate notifications per window. Gates escalation from warning to critical and prevents repeated alerts at the same threshold level. Each window (5h/7d) has independent cooldown tracking.

**Methods:**
- `is_above_warning(utilization)` - Check if usage exceeds warning threshold
- `is_above_critical(utilization)` - Check if usage exceeds critical threshold
- `is_below_reset(utilization)` - Check if usage dropped below reset threshold
- `validate()` - Ensure thresholds are valid (warning < critical, reset < warning)

**TOML structure (flat):**
```toml
warning-threshold = 75.0
critical-threshold = 90.0
reset-threshold = 50.0
polling-interval-minutes = 2
notification-cooldown-minutes = 5
```

### Data Flow

The app uses an event-driven architecture separating service logic from UI concerns:

1. `main()` creates an `mpsc::channel<AppEvent>` and spawns two tasks:
   - **Polling service** (`service::polling_loop`) - Fetches usage data, emits events
   - **Event handler** (`event_handler_loop`) - Processes events, updates UI

2. `polling_loop` runs the state machine:
   - Loads credentials via `load_credentials()`
   - Calls `api::fetch_usage()` to get usage data
   - Emits `AppEvent::UsageUpdated` on success
   - Emits `AppEvent::ErrorOccurred` or `AppEvent::CredentialsExpired` on failure

3. `event_handler_loop` handles events:
   - `UsageUpdated` - Calls `check_notifications()`, updates tray icon and menu
   - `ErrorOccurred` - Updates tray to show error state
   - `CredentialsExpired` / `AuthRequired` - Shows re-auth notification

This design keeps the service layer framework-agnostic (no Tauri dependencies) while the event handler bridges to UI concerns.

### Key Types

**State Management:**
- `AppState` - Main state container with direct field access (no nested `AsyncState`):
  - `latest_usage: RwLock<Option<Arc<UsageResponse>>>` - Cached usage data
  - `last_error: RwLock<Option<String>>` - Last error message for display
  - `notification_state: RwLock<NotificationState>` - Per-window notification tracking
  - `last_checked: Mutex<Option<DateTime>>` - Last successful check time (sync for tray)
  - `refresh_notify: Notify` - Manual refresh signal
  - `credentials: Mutex<Option<Arc<Credentials>>>` - Auth credentials
  - `cancel_token: CancellationToken` - Shutdown coordination
  - `config: Config` - Read-only after startup (no lock needed)
  - `http_client: reqwest::Client` - Owned directly (no OnceLock)
  - `keep_window_open: AtomicBool` - Prevents popup auto-close on focus loss
  - `always_on_top: AtomicBool` - Keeps popup above all other windows
  - `refresh_on_open: AtomicBool` - Triggers data refresh when popup opens
  - `window_position: Mutex<Option<(i32, i32)>>` - Persisted popup position (physical pixels)

**Service Layer Types:**
- `AppEvent` - Events from service to application layer:
  - `UsageUpdated(Arc<UsageResponse>)` - New data available
  - `ErrorOccurred(String)` - Fetch failed
  - `CredentialsExpired` - Token invalid, user must re-auth
  - `AuthRequired` - No credentials found
- `CredentialRefreshResult` - Result of credential refresh attempt: `Changed`, `Unchanged`, `Failed`

**Notification State:**
- `NotificationState` - Contains per-window state:
  - `five_hour: WindowNotificationState`
  - `seven_day: WindowNotificationState`
- `WindowNotificationState` - Per-window tracking:
  - `warned: bool` - Warning notification sent
  - `critical: bool` - Critical notification sent
  - `last_notified: Option<DateTime>` - Cooldown timestamp for preventing duplicate alerts
- `NotificationAction` - Action from notification check: `None`, `Reset`, `SendWarning`, `SendCritical`

**API Types:**
- `UsageResponse` - API response with `five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet` windows
- `UsageWindow` - Contains `utilization` (Utilization newtype) and `resets_at` (DateTime)
- `Utilization` - Newtype wrapping f64, clamped to 0-100 range
- `ApiError` - Error variants: `Network`, `ParseError`, `Http`, `Unauthorized`, `RateLimited`, `ServerError`
- `AuthContextError` - Error variants: `NotFound`, `ReadError`, `ParseError`, `MissingField`

### Tauri Configuration

- Uses Tauri 2.x with `tauri.conf.json` (not `tauri.conf.json5`)
- One popup window (`"popup"`, 290×140 logical px) created programmatically in `setup()` — hidden by default, shown on tray left-click
- `dist/index.html` — full popup UI (dark Catppuccin theme, HTML/JS with `withGlobalTauri: true`)
- `security.csp: null` — required for Tauri IPC script injection in the WebView
- Plugins: `tauri-plugin-notification`, `tauri-plugin-autostart`, `tauri-plugin-log`
- Capabilities defined in `capabilities/default.json` with `"windows": ["*"]` so permissions apply to the popup window too

### Split Icon System

The tray icon uses a split design where left half shows 5-hour status and right half shows 7-day status. Each half independently reflects that window's threshold state.

**Icon naming convention:** `icon-{5h}{7d}.png` where each character is:
- `n` = Normal (green, below warning threshold)
- `w` = Warning (yellow, >= warning and < critical)
- `c` = Critical (red, >= critical threshold)

**Icon matrix (9 icons total):**

| 5h \ 7d | Normal | Warning | Critical |
|---------|--------|---------|----------|
| Normal  | nn     | nw      | nc       |
| Warning | wn     | ww      | wc       |
| Critical| cn     | cw      | cc       |
