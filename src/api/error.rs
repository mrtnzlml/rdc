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

/// Walk an `anyhow::Error` chain looking for an `ApiError::Status` with the
/// given HTTP status code. Used by push/apply drivers to react to specific
/// failure modes (e.g. 405 Method Not Allowed for read-only-via-PATCH kinds
/// like workflows).
pub fn anyhow_has_status(err: &anyhow::Error, code: u16) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<ApiError>()
            .map(|api| matches!(api, ApiError::Status { status, .. } if *status == code))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn anyhow_has_status_finds_405() {
        let err: anyhow::Error = anyhow!(ApiError::Status {
            status: 405,
            body: "method_not_allowed".into(),
        });
        assert!(anyhow_has_status(&err, 405));
        assert!(!anyhow_has_status(&err, 403));
    }

    #[test]
    fn anyhow_has_status_walks_context_chain() {
        let inner: anyhow::Error = anyhow!(ApiError::Status {
            status: 403,
            body: "forbidden".into(),
        });
        let wrapped = inner.context("listing engines to verify no drift");
        assert!(anyhow_has_status(&wrapped, 403));
    }

    #[test]
    fn anyhow_has_status_returns_false_for_non_api_error() {
        let err = anyhow!("some other error");
        assert!(!anyhow_has_status(&err, 405));
    }
}
