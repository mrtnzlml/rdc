use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Rossum API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("response body could not be decoded as JSON: {0}")]
    Decode(#[from] serde_json::Error),
}
