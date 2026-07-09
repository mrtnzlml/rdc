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
    #[error("{}", render_status(*status, body, env.as_deref()))]
    Status {
        status: u16,
        body: String,
        env: Option<String>,
    },

    #[error("response body could not be decoded as JSON: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Render an [`ApiError::Status`] into a human-facing diagnostic.
///
/// Rossum reports validation failures as a field-keyed error map
/// (`{"token_owner": ["Invalid hyperlink - Object does not exist."], …}`),
/// which is opaque when dumped raw mid-push. When the body has that shape it
/// is unpacked into a readable per-field list, each line annotated with an
/// actionable hint for recognised failure modes. Any other body (plain text,
/// `{"detail": …}` with a non-string value, non-JSON) falls back to the
/// original verbatim form, so programmatic callers and unknown shapes are
/// unaffected.
fn render_status(status: u16, body: &str, env: Option<&str>) -> String {
    let env_note = env.map(|e| format!(" (env '{e}')")).unwrap_or_default();
    let header = format!("Rossum API{env_note} returned status {status}");
    match parse_field_errors(body) {
        Some(fields) if !fields.is_empty() => {
            let mut out = format!("{header}:");
            for (field, msg) in &fields {
                out.push_str(&format!("\n  {field}: {msg}"));
                if let Some(hint) = hint_for(field, msg) {
                    for line in hint.lines() {
                        out.push_str(&format!("\n      {line}"));
                    }
                }
            }
            out
        }
        _ => format!("{header}: {body}"),
    }
}

/// Parse a Rossum field-keyed validation body into `(field, message)` pairs.
/// Accepts `{"field": ["msg", …]}` and `{"field": "msg"}`. Returns `None` for
/// anything that isn't a flat JSON object of strings / string-arrays (so the
/// caller falls back to the raw body rather than mangling an unknown shape).
fn parse_field_errors(body: &str) -> Option<Vec<(String, String)>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = value.as_object()?;
    let mut out = Vec::new();
    for (field, v) in obj {
        match v {
            serde_json::Value::String(s) => out.push((field.clone(), s.clone())),
            serde_json::Value::Array(items) => {
                for item in items {
                    let serde_json::Value::String(s) = item else {
                        return None;
                    };
                    out.push((field.clone(), s.clone()));
                }
            }
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Actionable hint for a recognised `(field, message)` failure, else `None`.
fn hint_for(field: &str, msg: &str) -> Option<String> {
    if msg.contains("Invalid hyperlink") {
        return Some(format!(
            "'{field}' references an object that does not exist in this environment.\n\
             This is usually a stale cross-environment reference — a URL carried over\n\
             from another org (a user such as token_owner, a hook_template, or a\n\
             queue). Set a value valid for THIS environment; per-env fields like\n\
             token_owner belong in the env's overlay.toml. Then re-run."
        ));
    }
    None
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

    #[test]
    fn status_display_unpacks_field_errors_with_hint() {
        let err = ApiError::Status {
            status: 400,
            body: r#"{"token_owner":["Invalid hyperlink - Object does not exist."]}"#.into(),
            env: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("returned status 400"), "{msg}");
        assert!(
            msg.contains("token_owner: Invalid hyperlink - Object does not exist."),
            "field + message not unpacked: {msg}"
        );
        assert!(msg.contains("token_owner' references an object"), "missing hint: {msg}");
        assert!(msg.contains("overlay.toml"), "hint should point at overlay.toml: {msg}");
    }

    #[test]
    fn status_display_handles_multiple_fields() {
        let err = ApiError::Status {
            status: 400,
            body: r#"{"name":["This field may not be blank."],"queues":["Invalid hyperlink - No URL match."]}"#.into(),
            env: Some("prod".into()),
        };
        let msg = err.to_string();
        assert!(msg.contains("(env 'prod')"), "{msg}");
        assert!(msg.contains("name: This field may not be blank."), "{msg}");
        assert!(msg.contains("queues: Invalid hyperlink - No URL match."), "{msg}");
    }

    #[test]
    fn status_display_falls_back_to_raw_body_for_non_field_shapes() {
        // Plain text (non-JSON) and JSON that isn't a flat string map both
        // keep the original verbatim form.
        let plain = ApiError::Status {
            status: 500,
            body: "Internal Server Error".into(),
            env: Some("prod".into()),
        };
        assert_eq!(
            plain.to_string(),
            "Rossum API (env 'prod') returned status 500: Internal Server Error"
        );
        let nested = ApiError::Status {
            status: 400,
            body: r#"{"config":{"url":["bad"]}}"#.into(),
            env: None,
        };
        assert_eq!(
            nested.to_string(),
            r#"Rossum API returned status 400: {"config":{"url":["bad"]}}"#
        );
    }
}
