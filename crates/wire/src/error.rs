//! Structured API and protocol error definitions.

use serde::{Deserialize, Serialize};

/// Stable, machine-readable API error format returned by daemon responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// Canonical error code (e.g. "not_connected", "read_only_violation", "not_found").
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured details (JSON map or null).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// Construct a new structured error.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Attach extra structured details to the error.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn not_connected(message: impl Into<String>) -> Self {
        Self::new("not_connected", message)
    }

    pub fn read_only(message: impl Into<String>) -> Self {
        Self::new("read_only_violation", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new("timeout", message)
    }
}
