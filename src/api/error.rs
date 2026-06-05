use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// HTTP failure. `env` is the rdc env name (e.g. `"dev-eu"`) the
    /// failing call was made against, when the client knows it; it's
    /// `None` for code paths that don't carry an env label. Surfacing the
    /// env lets multi-env commands (notably `rdc deploy`, holding both
    /// src and tgt clients) attribute a 401 back to the right env on
    /// retry.
    #[error("Rossum API{} returned status {status}: {body}", env.as_deref().map(|e| format!(" (env '{e}')")).unwrap_or_default())]
    Status {
        status: u16,
        body: String,
        env: Option<String>,
    },

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

/// If the chain carries an `ApiError::Status` matching `code` AND that
/// status was tagged with an `env`, return the env name. Used by deploy's
/// two-env retry wrapper to attribute a 401 to either src or tgt.
pub fn anyhow_status_env(err: &anyhow::Error, code: u16) -> Option<String> {
    err.chain().find_map(|c| {
        c.downcast_ref::<ApiError>().and_then(|api| match api {
            ApiError::Status { status, env, .. } if *status == code => env.clone(),
            _ => None,
        })
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
            env: None,
        });
        assert!(anyhow_has_status(&err, 405));
        assert!(!anyhow_has_status(&err, 403));
    }

    #[test]
    fn anyhow_has_status_walks_context_chain() {
        let inner: anyhow::Error = anyhow!(ApiError::Status {
            status: 403,
            body: "forbidden".into(),
            env: None,
        });
        let wrapped = inner.context("listing engines to verify no drift");
        assert!(anyhow_has_status(&wrapped, 403));
    }

    #[test]
    fn anyhow_has_status_returns_false_for_non_api_error() {
        let err = anyhow!("some other error");
        assert!(!anyhow_has_status(&err, 405));
    }

    #[test]
    fn anyhow_status_env_extracts_tag_for_matching_status() {
        let err: anyhow::Error = anyhow!(ApiError::Status {
            status: 401,
            body: "Invalid token.".into(),
            env: Some("test-eu".into()),
        });
        let wrapped = err.context("listing tgt hook templates for plan output");
        assert_eq!(anyhow_status_env(&wrapped, 401).as_deref(), Some("test-eu"));
        assert_eq!(anyhow_status_env(&wrapped, 403), None);
    }

    #[test]
    fn anyhow_status_env_returns_none_when_status_has_no_env() {
        let err: anyhow::Error = anyhow!(ApiError::Status {
            status: 401,
            body: "Invalid token.".into(),
            env: None,
        });
        assert_eq!(anyhow_status_env(&err, 401), None);
    }
}
