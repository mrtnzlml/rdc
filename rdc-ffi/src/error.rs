//! The single error type crossing the FFI boundary. rdc returns rich
//! `anyhow::Error` chains; we flatten them to a message string the app
//! shows verbatim.

#[allow(dead_code)] // wired up in Task 5
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    Operation { message: String },
}

/// Build an `Operation` error from a plain message.
#[allow(dead_code)] // wired up in Task 5
pub fn op(message: String) -> FfiError {
    FfiError::Operation { message }
}

/// Flatten an `anyhow::Error` (with its full `{:#}` chain) into an `FfiError`.
#[allow(dead_code)] // wired up in Task 5
pub fn map_err(e: anyhow::Error) -> FfiError {
    FfiError::Operation { message: format!("{e:#}") }
}
