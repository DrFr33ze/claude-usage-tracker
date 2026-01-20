//! Event types for the service layer.

use std::sync::Arc;

/// Events emitted by the service layer to be handled by the application layer.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Usage data was successfully fetched and updated.
    UsageUpdated(Arc<crate::api::UsageResponse>),
    /// An error occurred during polling or API communication.
    ErrorOccurred(String),
    /// Credentials have expired and re-authentication is required.
    CredentialsExpired,
    /// Authentication is required (no credentials available).
    AuthRequired,
}

/// Result of attempting to refresh credentials after a 401 Unauthorized response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRefreshResult {
    /// Credentials were successfully loaded and differ from previous.
    Changed,
    /// Credentials were loaded but are the same as before (still invalid).
    Unchanged,
    /// Failed to load credentials from disk.
    Failed,
}
