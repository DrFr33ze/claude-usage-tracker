# Project Context

## Purpose

Claude Usage Tracker is a lightweight cross-platform system tray application that monitors Claude Code API usage in real-time. It provides:

- Color-coded split icon showing 5-hour and 7-day usage status at a glance
- Desktop notifications when usage exceeds warning (75%) or critical (90%) thresholds
- Detailed tray menu with usage windows, reset times, and manual refresh
- Autostart capability for all supported platforms

The goal is to help Claude Code users stay aware of their API usage limits without constantly checking the web dashboard.

## Tech Stack

- **Language**: Rust 1.77.2+ (2021 edition)
- **Framework**: Tauri 2.x (tray-only application, no frontend JavaScript)
- **Async Runtime**: Tokio (multi-threaded, with time, sync, macros, signal features)
- **HTTP Client**: reqwest with rustls-tls (no OpenSSL dependency)
- **Serialization**: serde + serde_json for JSON, toml for configuration
- **Date/Time**: chrono with serde support
- **Error Handling**: anyhow for propagation, thiserror for domain errors
- **Logging**: tauri-plugin-log with log facade
- **Security**: secrecy for token handling
- **Testing**: wiremock for HTTP mocking, serial_test for test isolation

### Platform-Specific Dependencies
- **Linux**: libwebkit2gtk-4.1, libayatana-appindicator3 (tray support)
- **macOS**: Native NSStatusItem via Tauri
- **Windows**: Windows API via Tauri

## Project Conventions

### Code Style

**Formatting & Linting**:
- Standard rustfmt defaults
- Use `cargo check` for fast type checking
- Use `cargo clippy` for linting

**Naming Conventions**:
- Modules: snake_case (`service.rs`, `config.rs`)
- Types: PascalCase (`UsageResponse`, `NotificationState`)
- Functions: snake_case (`load_credentials`, `check_window_notification`)
- Constants: SCREAMING_SNAKE_CASE (`BASE_DELAY_MS`, `MAX_RETRIES`)
- Newtypes for validated values: `Percentage`, `Utilization`

**Error Handling**:
- Use `thiserror::Error` for module-specific error enums (`ApiError`, `ConfigError`, `AuthContextError`)
- Use `anyhow::Result` for error propagation in application code
- Provide descriptive error messages with context

**Import Organization**:
1. Standard library
2. External crates
3. Internal modules
4. Self imports

### Architecture Patterns

**Event-Driven Architecture**:
- Service layer emits `AppEvent` messages via mpsc channel
- Event handler loop in `lib.rs` processes events and updates UI
- Decouples polling logic from tray updates

**Async/Sync Lock Separation**:
- `tokio::sync::RwLock` for async-only fields (usage data, errors, notification state)
- `std::sync::Mutex` for fields accessed in Tauri sync callbacks (last_checked, credentials)
- Prevents deadlocks between sync tray callbacks and async runtime

**State Management**:
- `AppState` struct with individual locks per field
- No nested state structures - flat design for simplicity
- Read-only fields (config, http_client) have no locks

**Modular Design**:
- `main.rs` - Entry point and wiring only
- `lib.rs` - Module declarations, AppState, event handler
- `service.rs` - Polling loop, notification logic (framework-agnostic)
- `api.rs` - HTTP client with retry logic
- `auth.rs` - Credential loading
- `tray.rs` - System tray UI
- `config.rs` - Configuration parsing and validation
- `events.rs` - Event type definitions

**Retry Strategy**:
- Exponential backoff with jitter for transient failures
- Maximum 3 retries before giving up
- Rate limit handling with Retry-After header support

### Testing Strategy

**Unit Tests**:
- Colocated with modules using `#[cfg(test)] mod tests {}`
- Test fixtures via helper functions like `make_config()`
- Focus on edge cases and boundary values

**Test Coverage Areas**:
- `api.rs`: Response parsing, timestamp handling, utilization clamping
- `config.rs`: Validation (threshold ordering, interval bounds)
- `service.rs`: Notification state machine, cooldown logic, escalation
- `tray.rs`: Usage indicator formatting, split icon name generation

**Integration Tests**:
- Located in `tests/integration.rs`
- Use wiremock for HTTP mocking
- Test actual HTTP behavior with mock server
- Use `testing` feature flag for test-specific code paths

**Running Tests**:
```bash
cargo test                    # All tests
cargo test test_name          # Single test
cargo test -- --nocapture     # With output
cargo test -- --list          # List all tests
```

### Git Workflow

**Branch Strategy**:
- `main` branch is the primary development branch
- Feature branches for significant changes

**Commit Style**:
- Conventional commits format: `type: description`
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`
- Examples:
  - `feat: add cross-platform CI builds for macOS and Windows`
  - `fix: swap misnamed split tray icons`
  - `docs: add screenshots to README`

**CI/CD**:
- GitHub Actions for cross-platform builds (Linux, macOS, Windows)
- Build artifacts: .deb, .AppImage (Linux), .dmg (macOS), .msi (Windows)

## Domain Context

**Claude API Usage Windows**:
- **5-hour window**: Short-term usage limit
- **7-day window**: Long-term usage limit
- **Opus/Sonnet windows**: Model-specific usage (displayed but no notifications)
- Each window has `utilization` (0-100%) and `resets_at` timestamp

**Threshold System**:
- **Warning** (75%): Yellow indicator, warning notification
- **Critical** (90%): Red indicator, critical notification
- **Reset** (50%): Clears notification state when usage drops below
- Hysteresis prevents notification ping-pong near thresholds

**Notification Cooldown**:
- 5-minute cooldown between notifications per window
- Escalation from warning to critical bypasses cooldown
- Each window (5h, 7d) tracks state independently

**Split Icon System**:
- Left half = 5-hour status, Right half = 7-day status
- 9 icon combinations (3x3 matrix: normal/warning/critical)
- Icon naming: `icon-{5h}{7d}.png` (e.g., `icon-nw.png` = normal/warning)

## Important Constraints

**Technical Constraints**:
- No frontend JavaScript - pure Rust tray application
- Must work without window (tray-only)
- Tauri requires `dist/` directory with placeholder index.html
- Icons embedded at compile time via `include_bytes!`

**Platform Requirements**:
- Linux: Requires AppIndicator-compatible tray (GNOME needs extension)
- macOS: Universal binary for Intel + Apple Silicon
- Windows: No special requirements

**API Constraints**:
- OAuth token from `~/.claude/.credentials.json` (Claude CLI location)
- Token can expire and needs refresh handling
- API rate limiting with Retry-After header

**Design Principles**:
- Minimal memory footprint (<2MB RSS)
- Near-zero CPU when idle
- Non-intrusive notifications (alerts, not audit log)
- Notification state not persisted across restarts

## External Dependencies

**Claude Code API**:
- OAuth usage endpoint for utilization data
- Requires authentication via `claude auth login`
- Credentials stored at `~/.claude/.credentials.json`

**Configuration File**:
- Location: `~/.config/claude-usage-tracker/config.toml` (Linux)
- Auto-created with defaults if missing
- TOML format with flat structure

**System Services**:
- Desktop notifications via `tauri-plugin-notification`
- Autostart via `tauri-plugin-autostart`
- Platform-specific tray implementations

## Build Commands Reference

```bash
# Development
cargo tauri dev                           # Hot reload

# Production
cargo tauri build                         # Binary only
cargo tauri build --bundles deb,appimage  # With Linux bundles

# Validation
cargo check                               # Fast type check
cargo test                                # Run tests
```

**Output locations**:
- Binaries: `target/release/`
- Bundles: `target/release/bundle/{deb,appimage}/`
