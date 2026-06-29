//! The single error type crossing the FFI boundary. rdc returns rich
//! `anyhow::Error` chains; we flatten them to a message string the app
//! shows verbatim.

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    Operation { message: String },
}

/// Build an `Operation` error from a plain message.
pub fn op(message: String) -> FfiError {
    FfiError::Operation { message }
}

/// Flatten an `anyhow::Error` (with its full `{:#}` chain) into an `FfiError`.
pub fn map_err(e: anyhow::Error) -> FfiError {
    FfiError::Operation { message: format!("{e:#}") }
}
