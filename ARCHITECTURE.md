# Architecture

## Overview

Claude Usage Tracker is a Tauri 2.x system tray application that monitors Claude Code API usage and displays status via a color-coded split icon with desktop notifications. The application polls the OAuth usage endpoint every 2 minutes (with jitter), displays real-time status in the system tray, and sends notifications when usage thresholds are exceeded.

**Key characteristics:**
- **Rust + HTML/JS popup** - Rust backend with a decorationless WebView popup window for usage display
- **Async-first** - Tokio-based polling with event-driven architecture
- **Type-safe** - Strong typing with newtypes (`Percentage`, `Utilization`) and comprehensive error handling
- **Event-driven** - Service layer emits events, UI responds asynchronously
- **Resource-efficient** - Minimal memory footprint with cached icons and shared state

## Component Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                           main.rs                               │
│  (Entry point, thin wiring layer)                               │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ AppState                                                │    │
│  │ ├─ latest_usage: RwLock<Option<Arc<UsageResponse>>>     │    │
│  │ ├─ last_error: RwLock<Option<String>>                   │    │
│  │ ├─ notification_state: RwLock<NotificationState>        │    │
│  │ ├─ last_checked: Mutex<Option<DateTime>>                │    │
│  │ ├─ refresh_notify: Notify                               │    │
│  │ ├─ credentials: Mutex<Option<Arc<Credentials>>>         │    │
│  │ ├─ cancel_token: CancellationToken                      │    │
│  │ ├─ config: Config (read-only)                           │    │
│  │ ├─ http_client: reqwest::Client                         │    │
│  │ ├─ keep_window_open: AtomicBool                         │    │
│  │ ├─ always_on_top: AtomicBool                            │    │
│  │ ├─ refresh_on_open: AtomicBool                          │    │
│  │ └─ window_position: Mutex<Option<(i32, i32)>>           │    │
│  └─────────────────────────────────────────────────────────┘    │
└──────────────┬──────────────────────┬───────────────────────────┘
               │                      │
               │ spawns               │ uses
               v                      v
┌──────────────┴──────────┐  ┌────────┴───────────┐
│    service.rs           │  │    api.rs          │
│ (polling_loop)          │  │ (HTTP client,      │
│                         │  │  retry logic)      │
│ • polling_loop          │  │ • build_http_client│
│ • credential refresh    │  │ • fetch_usage      │
│                         │  │ • Exponential      │
└──────────────┬──────────┘  │   backoff + jitter │
               │             └────────┬───────────┘
               │                      │ calls
               │ uses                 v
               │              ┌───────┴────────┐
               └──────────────┤  Claude API    │
                              │  (OAuth usage  │
                              │   endpoint)    │
                              └────────────────┘

               │ events (AppEvent from events.rs)
               │ via mpsc channel
               v
┌──────────────┴──────────┐  ┌────────┴──────────┐
│    tray.rs              │  │    config.rs      │
│ (icon, menu, popup pos) │  │ (validation,      │
│                         │  │  thresholds)      │
│ • create_tray           │  │                   │
│ • show_popup (toggle)   │  │ • Percentage      │
│ • update_tray_icon      │  │ • Config          │
│ • update_tray_menu      │  │ • ConfigError     │
│ • UsageLevel enum       │  │                   │
└──────────────┬──────────┘  └────────┬──────────┘
               │
               │ Tauri commands / events
               v
┌──────────────┴──────────┐
│    commands.rs          │
│ (popup ↔ Rust bridge)   │
│                         │
│ • get_usage_data        │
│ • trigger_refresh       │
│ • save_window_position  │
│ • hide_popup            │
│ • open_github           │
│ • UsageDto / WindowDto  │
└─────────────────────────┘
               │                      │
               │ uses                 │ reads
               v                      v
┌──────────────┴──────────┐  ┌────────┴──────────┐
│    auth.rs              │  │  events.rs        │
│ (credentials)           │  │ (event types)     │
│                         │  │                   │
│ • load_credentials      │  │ • AppEvent        │
│ • get_credentials_path  │  │ • Credential      │
│ • minutes_until_expiry  │  │   RefreshResult   │
│ • AuthContextError      │  │                   │
└──────────────┬──────────┘  └────────┬──────────┘
               │                      │
               v                      v
      ~/.claude/.credentials.json
```

## Data Flow

The application follows a **polling → processing → notification** pattern with decoupled event handling:

### 1. Initialization Flow

```
main()
 ├─ Load config (config::load)
 ├─ Load credentials (auth::load_credentials)
 ├─ Build HTTP client (api::build_http_client)
 ├─ Create AppState
 ├─ Create tray (tray::create_tray)
 ├─ Spawn polling_loop task
 └─ Spawn tray_update_loop task
```

### 2. Polling Loop (Async)

```
polling_loop (service.rs)
 ├─ Load credentials
 │   └─ Check ~/.claude/.credentials.json
 ├─ Main polling cycle
 │   ├─ Wait for interval (2 minutes ± jitter)
 │   └─ User refresh triggered ─────┐
 ├─ Fetch usage data                │
 │   ├─ Get credentials             │
 │   ├─ api::fetch_usage            │
 │   │   └─ Exponential backoff     │
 │   │       with retry (max 3)     │
 │   ├─ On success:                 │
 │   │   ├─ Update usage data       │
 │   │   ├─ Check notifications     │
 │   │   └─ Emit UsageUpdated       │
 │   ├─ On 401 Unauthorized:        │
 │   │   ├─ Attempt credential      │
 │   │   │   refresh                │
 │   │   └─ Emit CredentialsExpired │
 │   └─ On other errors:            │
 │       ├─ Log error               │
 │       └─ Emit ErrorOccurred      │
 └─ On shutdown
     └─ Cancel all tasks
```

**Note:** Retry and rate limit backoff are handled within `api::fetch_usage()` using exponential backoff.

### 3. Successful Fetch Path

```
handle_successful_fetch()
 ├─ Wrap usage in Arc
 ├─ Update latest_usage (direct RwLock)
 ├─ Update last_checked timestamp
 └─ Send AppEvent::UsageUpdated
     └─ event_handler_loop (lib.rs)
         ├─ check_notifications (lib.rs)
         │   ├─ For each window (5h, 7d)
         │   │   ├─ Check thresholds
         │   │   ├─ Update notification flags
         │   │   └─ Send notification if needed
         │   └─ Opus/Sonnet skipped (no notifications)
         ├─ update_tray_icon
         ├─ update_tray_menu
         └─ app.emit("usage-updated", UsageDto) → popup WebView
```

### 4. Error Handling Path

```
handle_fetch_error()
 ├─ Log at appropriate level
 ├─ Update last_error (direct RwLock)
 ├─ Send AppEvent::ErrorOccurred
 │   └─ event_handler_loop (main.rs)
 │       ├─ update_tray_icon_error
 │       └─ update_tray_menu
```

### 5. Manual Refresh Flow

```
Tray icon clicked or menu → refresh
 ├─ Check cooldown (30s)
 ├─ Notify polling loop (refresh_notify.notify_one)
 └─ Immediately transition: Ready → Polling
```

## State Management

### Lock Strategy: Individual RwLocks

The application uses **individual locks** for each field that needs synchronization:

```rust
pub struct AppState {
    // Async locks for async-only fields
    pub latest_usage: RwLock<Option<Arc<UsageResponse>>>,
    pub last_error: RwLock<Option<String>>,
    pub notification_state: RwLock<NotificationState>,

    // Sync locks for fields accessed in Tauri callbacks
    pub last_checked: Mutex<Option<DateTime<Utc>>>,
    pub credentials: Mutex<Option<Arc<Credentials>>>,
    pub window_position: Mutex<Option<(i32, i32)>>,
    pub cancel_token: CancellationToken,

    // Atomic flags (lock-free, accessed from both sync and async contexts)
    pub keep_window_open: AtomicBool,
    pub always_on_top: AtomicBool,
    pub refresh_on_open: AtomicBool,

    // No lock for read-only fields
    pub config: Config,
    pub http_client: reqwest::Client,
}
```

**Rationale:**
- Each async field has its own `RwLock`, allowing concurrent reads of different fields
- Sync locks (`std::sync`) for Tauri callbacks prevent async lock blocking
- Config and HTTP client are immutable after startup, no synchronization needed
- This design eliminates lock ordering concerns and maximizes concurrency

## Notification Logic

### Threshold System

Three-tier threshold system with hysteresis:

```
0% ─────┬────── 50% ─────┬────── 75% ─────┬────── 100%
        │                │                │
        │                │                │
        └──── RESET      │                │
                         │                │
                         ├─── WARNING ────┤
                         │                │
                         │                │
                         └─── CRITICAL ───┘
```

**Threshold values:**
- **Reset**: 50% - Clears notification state when usage drops below
- **Warning**: 75% - Triggers yellow indicator and warning notification
- **Critical**: 90% - Triggers red indicator and critical notification

### Per-Window State Tracking

Each window maintains independent state:

```rust
pub struct WindowNotificationState {
    pub warned: bool,              // Warning sent
    pub critical: bool,            // Critical sent
    pub last_notified: Option<DateTime<Utc>>, // Cooldown tracking
}
```

```rust
pub struct NotificationState {
    pub five_hour: WindowNotificationState,
    pub seven_day: WindowNotificationState,
}
```

**Design:** Independent cooldowns per window (5h and 7d) prevent notification spam.

### Notification Decision Matrix

| Utilization | Current State | Cooldown | Action | New State |
|-------------|---------------|----------|--------|-----------|
| < 50%       | Any           | N/A      | Reset  | Cleared (warned=false, critical=false, last_notified=None) |
| 50-74%      | Any           | N/A      | None   | No change |
| 75-89%      | Not warned    | Expired  | Send Warning | warned=true, last_notified=now |
| 75-89%      | Warned        | Any      | None   | No change (no duplicate warning) |
| 90-100%     | Warned, not critical | Any | Send Critical | critical=true, last_notified=now (escalation bypass) |
| 90-100%     | Not warned    | Expired  | Send Critical | critical=true, warned=true, last_notified=now |
| 90-100%     | Not warned    | Active   | None   | No change (cooldown active, no prior warning) |
| 90-100%     | Critical      | Any      | None   | No change |

**Cooldown:** 5 minutes (configurable) prevents duplicate alerts per window. Each window tracks notification state independently - one warning and one critical notification per window until usage drops below reset threshold.

**Escalation Bypass:** When usage escalates from warning to critical level, the cooldown is bypassed. This ensures users receive timely critical alerts even if they just received a warning notification. The rationale: critical situations warrant immediate notification regardless of recent warning.

### Notification Flow

```
check_notifications()
 ├─ For each window (5h, 7d)
 │   ├─ Extract utilization
 │   ├─ Get WindowNotificationState
 │   ├─ check_window_notification()
 │   │   ├─ Check cooldown expiry
 │   │   ├─ Check thresholds (critical first, then warning)
 │   │   ├─ Check reset threshold
 │   │   └─ Return NotificationAction
 │   └─ Process action
 │       ├─ Reset → Clear flags, log
 │       ├─ SendWarning → Show notification, set warned
 │       ├─ SendCritical → Show notification, set critical
 │       └─ None → No action
 └─ Opus/Sonnet windows skipped (no notifications per design)
```

## Error Handling

### Error Strategy

The codebase uses **`anyhow::Result`** for error propagation combined with **module-specific error types** for domain errors:

```rust
// Module-specific errors use thiserror for rich error messages
#[derive(thiserror::Error, Debug)]
pub enum ApiError { /* ... */ }

#[derive(thiserror::Error, Debug)]
pub enum AuthContextError { /* ... */ }

#[derive(thiserror::Error, Debug)]
pub enum ConfigError { /* ... */ }

// Functions return anyhow::Result for easy propagation
pub fn load() -> anyhow::Result<Config> {
    // Module errors automatically convert via thiserror's Display
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(config)
}
```

### Error Categories

#### Transient Errors (Auto-retry)
- `ApiError::Network` - Connection failures
- `ApiError::ServerError` - 5xx responses

**Handling:** Exponential backoff with jitter in API layer.

#### Rate Limiting
- `ApiError::RateLimited` - 429 with optional retry-after

**Handling:** API layer handles rate limiting with exponential backoff based on Retry-After header.

#### User Action Required (No retry)
- `ApiError::Unauthorized` - 401 token expired
- `AuthContextError::NotFound` - Missing credentials file
- `AuthContextError::MissingField` - Corrupted credentials

**Handling:** Show notification, wait for user to re-authenticate.

#### Configuration Errors (Fatal)
- `ConfigError` - Invalid thresholds
- `ConfigParse` - Malformed config file

**Handling:** Fail fast at startup with descriptive error message.

### Error Recovery Flow

```
┌─────────────┐
│  Polling    │
└──────┬──────┘
       │
       ├─ Unauthorized (401) ──┐
       │                       │
       v                       │
┌─────────────────────┐        │
│ Credential Refresh  │        │
│ (load from disk)    │        │
└──────┬──────────────┘        │
       │                       │
       ├─ Token changed ───────┼─> Retry fetch
       │                       │
       └─ Token unchanged/     │
         Failed to load ───────┤
                               │
┌──────────────────────────────┘
│
v
┌────────────────────────┐
│Emit CredentialsExpired │
└────────────────────────┘

┌─────────────┐
│  Polling    │
└──────┬──────┘
       │
       ├─ Network / Server Error
       │                         │
       └─────────────────────────┘
                │
                v
┌───────────────────────────────┐
│ API Layer Retry               │
│ (exponential backoff + jitter)│
│ Max 3 retries, then fail      │
└───────────┬───────────────────┘
            │
            ├─ Success ─────────> Continue polling
            │
            └─ Still failing ───> Emit ErrorOccurred
```

## Configuration

### Flat Structure Design

Configuration uses a **flat structure** (not nested) for simplicity:

```rust
pub struct Config {
    pub warning_threshold: Percentage,   // 75.0
    pub critical_threshold: Percentage,  // 90.0
    pub reset_threshold: Percentage,     // 50.0
    pub polling_interval_minutes: u8,    // 2
    pub notification_cooldown_minutes: u8, // 5
}
```

**Design decision:** All windows (5h, 7d) use same thresholds. This reduces complexity and cognitive load. Users have one set of thresholds to manage.

### File Format

**Location:** `~/.config/claude-usage-tracker/config.toml`

**Content:**
```toml
# Claude Usage Tracker Configuration
# All thresholds apply uniformly to all usage windows (5-hour, 7-day)

# Warning threshold - triggers yellow indicator (default: 75.0)
warning-threshold = 75.0

# Critical threshold - triggers red indicator (default: 90.0)
critical-threshold = 90.0

# Reset threshold - clears notification state when usage drops below (default: 50.0)
reset-threshold = 50.0

# Polling interval in minutes (default: 2)
polling-interval-minutes = 2

# Cooldown between notifications in minutes (default: 5)
notification-cooldown-minutes = 5
```

### Type Safety

**`Percentage` newtype:** Validates 0-100 range at construction (for config values):

```rust
pub struct Percentage(f64);

impl Percentage {
    pub fn new(value: f64) -> Option<Self> {
        if value.is_nan() || !(0.0..=100.0).contains(&value) {
            None
        } else {
            Some(Self(value))
        }
    }
}
```

**`Utilization` newtype:** Wraps f64 for API response values, clamped to 0-100 range:

```rust
pub struct Utilization(f64);

impl Utilization {
    pub fn new(value: f64) -> Self {
        Self(value.clamp(0.0, 100.0))
    }
}
```

**Benefits:**
- Invalid values rejected at parse time
- No NaN or out-of-range values in config
- Clear error messages for invalid TOML

### Validation Rules

Config is validated at load time:

1. **Range check:** All percentages in [0, 100]
2. **Ordering check:** `reset < warning < critical`
3. **Polling interval:** Must be ≥ 1 minute

**Errors are collected and reported together:**

```rust
config.validate() -> Result<(), ConfigError>
```

Example error output:
```
Configuration error: 'reset' (50.0) must be less than 'warning' (75.0); 'polling interval' must be at least 1 minute
```

## Decision Records

### DR-001: Sync vs Async Locks

**Context:** Tauri tray callbacks are synchronous (must return quickly), but polling loop is asynchronous (Tokio).

**Problem:** Using `tokio::sync::Mutex` in synchronous tray callbacks can deadlock if the async runtime is blocked waiting for the lock.

**Decision:** Use separate lock types based on access pattern:

- `std::sync::Mutex` for fields accessed in **sync callbacks** (tray icon clicks, menu events)
- `tokio::sync::RwLock` for **async-only fields** (usage data, errors, notification state)

**Consequences:**

✅ **Pros:**
- No deadlock risk - sync code never blocks on async locks
- Clear separation of concerns - lock choice indicates access pattern
- Individual RwLocks allow concurrent reads of different fields
- Simple lock ordering - no cross-type dependencies

❌ **Cons:**
- Two lock types to understand
- Multiple locks to manage

**Alternative considered:** All async with `block_on` in callbacks - rejected due to blocking the event loop.

---

### DR-002: Individual Field Locks

**Context:** Async state fields need concurrent access from polling loop and event handler.

**Problem:** A single consolidated lock would serialize all operations even when accessing different fields.

**Decision:** Use individual `RwLock` for each async field:

```rust
pub struct AppState {
    pub latest_usage: RwLock<Option<Arc<UsageResponse>>>,
    pub last_error: RwLock<Option<String>>,
    pub notification_state: RwLock<NotificationState>,
    // ...
}
```

**Consequences:**

✅ **Pros:**
- Concurrent reads of different fields (no blocking)
- Clear field-level access pattern
- Each lock can be held independently
- Simpler update pattern (direct field access)

❌ **Cons:**
- Multiple lock types to manage
- Need to be aware of lock ordering when accessing multiple fields

**Example update:**
```rust
// Update latest_usage
*state.latest_usage.write().await = Some(Arc::new(usage));

// Clear last_error
*state.last_error.write().await = None;
```

---

### DR-003: Event-Driven Updates

**Context:** Initial implementation updated tray directly from polling loop. This created tight coupling and potential blocking issues.

**Problem:** Polling loop (async) calling tray update functions (sync) created complexity. Also, if tray update was slow, it would delay the next polling cycle.

**Decision:** Introduce event system with async channel:

```rust
enum AppEvent {
    UsageUpdated(Arc<UsageResponse>),
    ErrorOccurred(String),
    CredentialsExpired,
    AuthRequired,
}
```

Service layer sends events via `mpsc::Sender<AppEvent>`, event handler loop receives and processes them in main.rs.

**Consequences:**

✅ **Pros:**
- Decoupled polling from UI updates
- Independent event processing
- Easy to add new event types
- Simple to test (can mock event stream)
- Polling loop not blocked by tray updates

❌ **Cons:**
- Additional complexity (event loop, channel management)
- Slight latency between event and update (one event tick)
- More code to maintain

**Alternative considered:** Direct calls with `spawn` - rejected because it doesn't guarantee ordering and makes error handling more complex.

---

### DR-004: Split Icon Design

**Context:** Icon needed to show both 5-hour and 7-day status simultaneously.

**Problem:** Single color/icon can only represent one metric. Need to show both windows at once.

**Decision:** Split icon design - left half shows 5-hour status, right half shows 7-day status:

```
┌──────────┬──────────┐
│   5h     │   7d     │
│  status  │  status  │
└──────────┴──────────┘
```

**Icon matrix (9 combinations):**

| 5h \\ 7d | Normal (n) | Warning (w) | Critical (c) |
|----------|------------|-------------|--------------|
| **Normal (n)** | nn (green/green) | nw (green/yellow) | nc (green/red) |
| **Warning (w)** | wn (yellow/green) | ww (yellow/yellow) | wc (yellow/red) |
| **Critical (c)** | cn (red/green) | cw (red/yellow) | cc (red/red) |

**Consequences:**

✅ **Pros:**
- Shows both windows simultaneously
- Intuitive - two halves = two windows
- 9 states clearly differentiated
- Icon cache prevents runtime loading

❌ **Cons:**
- Need to generate 9 icons (already done)
- Requires understanding of split design
- Slightly complex mapping logic

**Implementation:**
```rust
fn get_split_icon(five_hour_level: UsageLevel, seven_day_level: UsageLevel) -> &'static [u8] {
    match (five_hour_level, seven_day_level) {
        (Normal, Normal) => ICON_NN,
        (Normal, Warning) => ICON_NW,
        // ... all 9 combinations
    }
}
```

---

### DR-005: Anyhow-Based Error Handling

**Context:** Different modules (`api`, `auth`, `config`) define their own error types with `thiserror::Error`.

**Problem:** A unified `AppError` wrapper was initially considered but added boilerplate without significant benefit for this application size.

**Decision:** Use `anyhow::Result` for error propagation with module-specific `thiserror` enums for domain errors:

```rust
// Module errors remain specific and descriptive
pub enum ApiError { Network(reqwest::Error), Unauthorized, ... }
pub enum AuthContextError { NotFound(PathBuf), ... }
pub enum ConfigError { InvalidOrder { ... }, ... }

// anyhow::Result for propagation
pub fn load() -> anyhow::Result<Config> { ... }
pub async fn main() -> Result<()> { ... }
```

**Consequences:**

✅ **Pros:**
- Less boilerplate than wrapper enum
- Module errors remain type-safe for pattern matching where needed
- `anyhow` provides good error context with `.context()`
- Simpler for application of this size

❌ **Cons:**
- Less compile-time enforcement at module boundaries
- Errors lose their specific type after conversion to `anyhow`

**Pattern used:**
```rust
// Convert module errors with context
config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

// Or use .context() for additional info
fs::read_to_string(&path).context("Failed to read config file")?;
```

---

### DR-006: Exponential Backoff with Jitter

**Context:** API calls can fail transiently (network issues, server errors). Simple retry with fixed delay risks thundering herd problem.

**Problem:** Without jitter, multiple app instances would retry simultaneously, overwhelming the API.

**Decision:** Implement exponential backoff with random jitter:

```rust
fn calculate_retry_delay(attempt: u32) -> u64 {
    // Exponential backoff: 1s, 2s, 4s, 8s, 16s
    let exp_delay = BASE_DELAY_MS.saturating_mul(1u64 << attempt);
    let capped_delay = exp_delay.min(MAX_DELAY_MS);  // Cap at 60s

    // Add jitter (0-25% of delay)
    let jitter = rand::thread_rng().gen_range(0..=capped_delay * 25 / 100);
    capped_delay + jitter
}
```

**Consequences:**

✅ **Pros:**
- Reduces load on failing API
- Prevents thundering herd
- Faster recovery for transient failures
- Configurable (base delay, max delay, jitter %)
- Maximum 3 retries (fails fast after)

❌ **Cons:**
- More complex than fixed delay
- Random delays make testing harder
- Still eventual failure after retries

**Parameters:**
- Base delay: 1000ms
- Max delay: 60000ms (60 seconds)
- Jitter: 25%
- Max retries: 3

**Alternative considered:** Fixed delay (1s) - rejected due to thundering herd risk.

---

### DR-007: Hardcoded vs Configurable

**Context:** Many values could be configurable, but not all should be.

**Decision:** Hardcode these values (move from config to constants):

**Hardcoded:**
- HTTP timeout: 30s
- HTTP connect timeout: 10s
- Retry: 3 attempts
- Rate limit backoff: 5 minutes
- Token expiry buffer: 10 minutes
- Polling jitter: ±30 seconds

**Rationale:**
- Rarely changed by users
- Setting incorrectly breaks functionality
- Adding to config increases cognitive load
- Constants module provides flexibility if needed later

**Configurable:**
- Thresholds (warning/critical/reset)
- Polling interval
- Notification cooldown

**Rationale:**
- User preferences
- Safe to adjust
- Improves user experience

---

### DR-008: Notification State Persistence

**Context:** Notification state tracks whether warning/critical notifications have been sent.

**Problem:** If app restarts, should notifications re-trigger immediately? Should state persist across restarts?

**Decision:** **Do not persist** notification state across restarts. State is in-memory only.

**Rationale:**
- Simpler implementation (no state file)
- User gets fresh start on each launch
- Notifications serve as "heads-up" not "audit log"
- State mostly useful within single session
- Cross-session persistence adds complexity for minimal benefit

**Trade-off:** User might see same notification multiple times across restarts, but this is acceptable given notification purpose (heads-up, not critical alert).

---

## Testing Strategy

### Unit Tests
- **Config validation** - Threshold ordering, range checks
- **Notification logic** - All state transitions, cooldowns
- **API parsing** - JSON deserialization, error handling
- **Icon mapping** - All 9 combinations
- **Error categorization** - Transient vs user-action

### Property Tests
- Config validation with arbitrary values
- Notification state invariants
- Floating-point comparisons with epsilon

### Integration Tests
- Full polling cycle (mock API)
- Error recovery paths
- Tray menu updates
- Credential refresh

## Performance Characteristics

### Memory
- **AppState**: ~1KB (config, locks, client)
- **AsyncState**: ~100B (option + error string)
- **UsageResponse**: ~200B (4 windows × 50B each)
- **Icon cache**: ~150KB (11 PNG icons × ~14KB each)
- **Total**: < 2MB RSS

### CPU
- **Polling loop**: 99% idle time (sleeps 2min ± 30s jitter)
- **API fetch**: ~100-500ms (depending on network)
- **Tray updates**: ~10-50ms (icon swap + menu rebuild)
- **Average**: Near-zero CPU when idle

### Network
- **Polling**: 1 request per 2 minutes (18KB request, ~50KB response)
- **Per hour**: ~30 requests, ~2MB data
- **Per day**: ~720 requests, ~50MB data
- **Idle cost**: Negligible

## Security Considerations

### Token Storage
- **Source**: Read from `~/.claude/.credentials.json` (Claude CLI location)
- **Format**: OAuth access token (`sk-ant-...`)
- **Handling**: `secrecy::SecretString` prevents accidental logging
- **Redaction**: Logs show only first/last 4 characters
- **Lifetime**: Read at startup, cached in memory, refreshed on 401

### File Permissions
- Credentials file checked for world-readable permissions on Unix
- Warning logged if mode includes `0o004` bit
- App doesn't modify permissions (user responsibility)

### Network Security
- HTTPS only for API endpoint
- Custom user agent identifies app
- Timeouts prevent hanging connections
- Retry logic respects rate limits

## Platform Considerations

### Windows
- Tray implementation: Windows API
- Credentials: `%APPDATA%\claude\.credentials.json`
- Autostart: Registry-based

### macOS
- Tray implementation: NSStatusItem
- Credentials: `~/.claude/.credentials.json`
- Autostart: LaunchAgent

### Linux
- Tray implementation: libayatana-appindicator
- Credentials: `~/.claude/.credentials.json`
- Autostart: XDG autostart desktop entry
- **Note**: Suppresses deprecation warnings for libayatana-appindicator

## Future Enhancements

### Potential Improvements
1. **Persistence**: Save/restore notification state (DR-10)
2. **Multiple profiles**: Support for multiple Claude accounts
3. **Historical data**: Track usage over time, show graphs
4. **Custom themes**: Dark/light icon themes
5. **Web dashboard**: Optional web interface for detailed stats
6. **Slack/Discord**: Send notifications to external services
7. **API key support**: Direct API key authentication (not just OAuth)

### Extensibility Points
- **Event system**: Easy to add new event types
- **Service layer**: Can add new polling logic or states
- **Icons**: OnceLock cache allows runtime icon loading
- **Error handling**: Module-specific thiserror enums with anyhow propagation
- **Configuration**: Flat structure easy to extend with new fields

## Maintenance Guide

### Adding a New Configuration Option
1. Add field to `Config` struct in `src/config.rs`
2. Add to `Default` implementation
3. Add validation in `validate()` method
4. Update `DEFAULT_CONFIG_TOML` constant
5. Add tests for validation

### Adding a New Error Type
1. Create error enum in relevant module with `#[derive(thiserror::Error, Debug)]`
2. Use descriptive `#[error("...")]` messages for each variant
3. Return `anyhow::Result` from functions, converting with `.map_err()` or `.context()`
4. Add test coverage

### Modifying Icon Set
1. Replace icon files in `src/icons/` (must be same filenames)
2. Rebuild application (icons embedded at compile time)
3. Test all 9 combinations visually
4. Update documentation if icon meanings change

## Conclusion

Claude Usage Tracker implements a robust, type-safe, and maintainable architecture for monitoring API usage through a system tray interface. Key architectural strengths include:

- **Clear separation of concerns** via modular design
- **Type safety** throughout (newtypes, strong enums, validated configs)
- **Async-first design** with proper sync/async lock separation
- **Event-driven updates** decoupling polling from UI
- **Comprehensive error handling** with clear recovery paths
- **Service-oriented architecture** with clear polling logic
- **Resource efficiency** with minimal memory and CPU footprint

The architecture supports easy extension and maintenance while providing excellent user experience through real-time monitoring and non-intrusive notifications.
