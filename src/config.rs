//! Simplified configuration for usage thresholds and polling settings.
//!
//! ## Type Safety Strategy
//!
//! This module uses [`Percentage`] for configuration thresholds:
//!
//! - **[`Percentage`]**: Rejects invalid values (NaN, negative, >100), returning `None`.
//!   Used in configuration to catch errors early during deserialization.
//!
//! In contrast, [`crate::api::Utilization`] is used for API responses:
//!
//! - **`Utilization`**: Clamps invalid values to [0, 100] range. Used for API responses
//!   where we must handle potentially malformed data gracefully without failing.
//!
//! This distinction ensures:
//! - Config errors are caught at startup (fail fast)
//! - API quirks don't crash the application (defensive handling)

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

// ============================================================================
// Constants
// ============================================================================

/// Config file name
const CONFIG_FILENAME: &str = "config.toml";

/// Epsilon for floating point comparisons
const FLOAT_EPSILON: f64 = 1e-9;

// ============================================================================
// Structs
// ============================================================================

/// A validated percentage value in the range [0.0, 100.0].
///
/// Unlike `Utilization` which clamps values, `Percentage` rejects invalid values
/// to catch configuration errors early.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Percentage(f64);

/// Simplified configuration with flat structure.
///
/// All threshold values apply uniformly to all usage windows (5-hour, 7-day).
///
/// # Validation Invariants
///
/// A valid `Config` MUST satisfy:
///
/// - **Threshold ordering**: `reset_threshold < warning_threshold < critical_threshold`
///   - This ensures notification states can reset correctly
///   - Validated by `Config::validate()` which returns `ConfigError` if violated
///   - Checked at startup after loading config file
///
/// - **Range constraints**: All thresholds are in `[0.0, 100.0]`
///   - Enforced by the `Percentage` type at deserialization time
///   - Invalid values cause `toml::from_str()` to fail before `validate()` runs
///
/// - **Polling interval**: `polling_interval_minutes >= 1`
///   - Zero would cause infinite polling loops
///   - Validated by `Config::validate()`
///
/// # Immutability
///
/// - Once loaded at startup, `Config` is never modified
/// - Stored in `AppState` without a lock (safe for concurrent reads)
/// - Changes require application restart
///
/// # Default Values
///
/// - `warning_threshold`: 75.0
/// - `critical_threshold`: 90.0
/// - `reset_threshold`: 50.0
/// - `polling_interval_minutes`: 2
/// - `notification_cooldown_minutes`: 5
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// Warning threshold percentage - triggers yellow indicator (default: 75)
    pub warning_threshold: Percentage,
    /// Critical threshold percentage - triggers red indicator (default: 90)
    pub critical_threshold: Percentage,
    /// Reset threshold percentage - clears notification state when usage drops below (default: 50)
    pub reset_threshold: Percentage,
    /// Polling interval in minutes (default: 2)
    pub polling_interval_minutes: u8,
    /// Cooldown between notifications in minutes (default: 5).
    ///
    /// This value controls how often the same notification level can be sent for a window.
    /// Each window (5-hour, 7-day) has independent cooldown tracking stored in
    /// `WindowNotificationState::last_notified`.
    ///
    /// ## Cooldown Semantics
    ///
    /// - **Purpose**: Prevents notification spam when usage fluctuates near thresholds
    /// - **Scope**: Per-window (5h and 7d windows track cooldowns independently)
    /// - **Escalation bypass**: When transitioning from warning to critical, cooldown is
    ///   BYPASSED - critical alerts fire immediately even if a warning was recently sent.
    ///   This ensures critical situations get immediate attention regardless of recent alerts.
    /// - **Reset behavior**: When usage drops below `reset_threshold`, the notification
    ///   state is cleared entirely (both `warned`/`critical` flags and `last_notified`)
    ///
    /// ## Special Values
    ///
    /// - **0**: Disables cooldown entirely - notifications fire on every poll cycle
    ///   that meets threshold criteria. Useful for testing but may cause notification
    ///   spam in production.
    pub notification_cooldown_minutes: u8,
}

// ============================================================================
// Enums
// ============================================================================

/// Identifies a usage monitoring window for notification purposes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKind {
    FiveHour,
    SevenDay,
}

/// Configuration validation errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("'{low_name}' ({low_value:.1}) must be less than '{high_name}' ({high_value:.1})")]
    InvalidOrder {
        low_name: &'static str,
        low_value: f64,
        high_name: &'static str,
        high_value: f64,
    },

    #[error("polling interval must be at least 1 minute")]
    InvalidPollingInterval,

    #[error("multiple validation errors: {}", .0.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join("; "))]
    Multiple(Vec<ConfigError>),
}

// ============================================================================
// Trait implementations
// ============================================================================

impl std::fmt::Display for Percentage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}%", self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Percentage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::custom(format!("percentage must be in range [0, 100], got {value}"))
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        // SAFETY: These are hardcoded values within the valid range [0.0, 100.0] for Percentage.
        // The unwrap() calls will never panic because the values are constants known to be valid.
        Self {
            warning_threshold: Percentage::new(75.0).unwrap(),
            critical_threshold: Percentage::new(90.0).unwrap(),
            reset_threshold: Percentage::new(50.0).unwrap(),
            polling_interval_minutes: 2,
            notification_cooldown_minutes: 5,
        }
    }
}

// ============================================================================
// Inherent implementations
// ============================================================================

impl Percentage {
    /// Creates a new Percentage if value is in valid range [0, 100].
    ///
    /// Returns `None` for NaN, negative, or values > 100.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        if value.is_nan() || !(0.0..=100.0).contains(&value) {
            None
        } else {
            Some(Self(value))
        }
    }

    /// Returns the inner f64 value.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl WindowKind {
    /// Human-readable label for the window
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5-Hour",
            Self::SevenDay => "7-Day",
        }
    }
}

impl Config {
    /// Validate threshold configuration.
    ///
    /// Ensures:
    /// - `reset < warning < critical`
    /// - Range validation is handled by [`Percentage`] type
    ///
    /// Returns a single error that may contain multiple validation issues.
    #[must_use = "validation errors must be handled"]
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        // Ordering checks (only if values are valid numbers)
        let warning = self.warning_threshold.as_f64();
        let critical = self.critical_threshold.as_f64();
        let reset = self.reset_threshold.as_f64();

        if warning >= critical {
            errors.push(ConfigError::InvalidOrder {
                low_name: "warning",
                low_value: warning,
                high_name: "critical",
                high_value: critical,
            });
        }
        if reset >= warning {
            errors.push(ConfigError::InvalidOrder {
                low_name: "reset",
                low_value: reset,
                high_name: "warning",
                high_value: warning,
            });
        }

        // Polling interval check
        if self.polling_interval_minutes == 0 {
            errors.push(ConfigError::InvalidPollingInterval);
        }

        match errors.len() {
            0 => Ok(()),
            1 => Err(errors.remove(0)),
            _ => Err(ConfigError::Multiple(errors)),
        }
    }

    /// Check if usage is at or above warning threshold (with epsilon tolerance)
    pub fn is_above_warning(&self, usage: f64) -> bool {
        usage >= self.warning_threshold.as_f64() - FLOAT_EPSILON
    }

    /// Check if usage is at or above critical threshold (with epsilon tolerance)
    pub fn is_above_critical(&self, usage: f64) -> bool {
        usage >= self.critical_threshold.as_f64() - FLOAT_EPSILON
    }

    /// Check if usage has dropped below reset threshold (with epsilon tolerance)
    pub fn is_below_reset(&self, usage: f64) -> bool {
        usage <= self.reset_threshold.as_f64() + FLOAT_EPSILON
    }
}

// ============================================================================
// Functions
// ============================================================================

/// Load configuration from file, creating defaults if needed
#[must_use = "this returns config that should be used or error handled"]
pub fn load() -> Result<Config> {
    let config_path = get_config_path()?;

    // Ensure config directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    // Attempt atomic file creation (avoids TOCTOU race condition)
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
    {
        Ok(mut file) => {
            // File was created - write default config for user discovery
            file.write_all(generate_default_config_toml().as_bytes())
                .with_context(|| {
                    format!(
                        "Failed to write default config to {}",
                        config_path.display()
                    )
                })?;

            log::info!("Created default config file at {}", config_path.display());
            return Ok(Config::default());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // File exists - fall through to load it
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to create config file at {}", config_path.display())
            });
        }
    }

    // Load existing config
    let toml_content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config from {}", config_path.display()))?;

    let config: Config = toml::from_str(&toml_content)
        .with_context(|| format!("Failed to parse config from {}", config_path.display()))?;

    // Validate configuration
    config.validate().context("Config validation failed")?;

    log::info!("Loaded config from {}", config_path.display());
    Ok(config)
}

/// Get the config directory path
fn get_config_dir() -> Result<PathBuf> {
    let base_config_dir =
        dirs::config_dir().ok_or_else(|| anyhow::anyhow!("Failed to get config directory"))?;
    Ok(base_config_dir.join("claude-usage-tracker"))
}

/// Get the config file path
fn get_config_path() -> Result<PathBuf> {
    let config_dir = get_config_dir()?;
    Ok(config_dir.join(CONFIG_FILENAME))
}

fn generate_default_config_toml() -> String {
    let config = Config::default();
    format!(
        r#"# Claude Usage Tracker Configuration
# All thresholds apply uniformly to all usage windows (5-hour, 7-day)

# Warning threshold - triggers yellow indicator (default: {warning})
warning-threshold = {warning}

# Critical threshold - triggers red indicator (default: {critical})
critical-threshold = {critical}

# Reset threshold - clears notification state when usage drops below (default: {reset})
reset-threshold = {reset}

# Polling interval in minutes (default: {polling})
polling-interval-minutes = {polling}

# Cooldown between notifications in minutes (default: {cooldown})
notification-cooldown-minutes = {cooldown}
"#,
        warning = config.warning_threshold.as_f64(),
        critical = config.critical_threshold.as_f64(),
        reset = config.reset_threshold.as_f64(),
        polling = config.polling_interval_minutes,
        cooldown = config.notification_cooldown_minutes,
    )
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < f64::EPSILON
    }

    // ============================================================
    // Validation tests
    // ============================================================

    #[test]
    fn test_validate_default_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_warning_greater_than_critical() {
        let config = Config {
            warning_threshold: Percentage::new(95.0).unwrap(),
            critical_threshold: Percentage::new(90.0).unwrap(),
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("warning") && err.contains("less than"));
    }

    #[test]
    fn test_validate_polling_interval_zero() {
        let config = Config {
            polling_interval_minutes: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("polling interval"));
    }

    // ============================================================
    // TOML parsing tests
    // ============================================================

    #[test]
    fn test_toml_parse_partial_uses_defaults() {
        let toml_str = r"
            warning-threshold = 65.0
        ";

        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(approx_eq(config.warning_threshold.as_f64(), 65.0));
        assert!(approx_eq(config.critical_threshold.as_f64(), 90.0)); // default
        assert!(approx_eq(config.reset_threshold.as_f64(), 50.0)); // default
        assert_eq!(config.polling_interval_minutes, 2); // default
        assert_eq!(config.notification_cooldown_minutes, 5); // default
    }

    #[test]
    fn test_validate_reset_greater_than_warning() {
        let config = Config {
            reset_threshold: Percentage::new(60.0).unwrap(),
            warning_threshold: Percentage::new(50.0).unwrap(),
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("reset") && err.contains("less than") && err.contains("warning"));
    }

    #[test]
    fn test_validate_equal_thresholds() {
        // Warning == Critical should fail
        let config = Config {
            warning_threshold: Percentage::new(75.0).unwrap(),
            critical_threshold: Percentage::new(75.0).unwrap(),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        // Reset == Warning should fail
        let config2 = Config {
            reset_threshold: Percentage::new(50.0).unwrap(),
            warning_threshold: Percentage::new(50.0).unwrap(),
            ..Default::default()
        };
        assert!(config2.validate().is_err());
    }

    #[test]
    fn test_validate_multiple_errors() {
        // Deliberately create a completely invalid config to test error aggregation.
        // All threshold relationships are inverted: reset > warning > critical.
        let config = Config {
            reset_threshold: Percentage::new(95.0).unwrap(),
            warning_threshold: Percentage::new(85.0).unwrap(),
            critical_threshold: Percentage::new(80.0).unwrap(),
            polling_interval_minutes: 0,
            notification_cooldown_minutes: 5,
        };
        // Expected errors:
        // 1. reset (95) >= warning (85)
        // 2. warning (85) >= critical (80)
        // 3. polling_interval_minutes == 0
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Multiple(errors) => {
                assert_eq!(errors.len(), 3);
            }
            _ => panic!("Expected Multiple errors"),
        }
    }
}
