//! The fake's rejections.
//!
//! Strict on purpose: a mid-run 400 that wedges a real env is one of the three
//! symptoms this whole exercise exists to make reproducible offline. A
//! permissive fake would model the state and miss the failure.

use serde_json::Value;

use super::state::{ApiError, OrgState};

/// Refuse a delete the real API refuses.
pub fn on_delete(st: &OrgState, kind: &'static str, id: u64) -> Result<(), ApiError> {
    if kind == "engines" {
        let engine_url = st.url("engines", id);
        let blocked = st
            .queues_awaiting_deletion()
            .iter()
            .any(|q| q.get("engine").and_then(Value::as_str) == Some(engine_url.as_str()));
        if blocked {
            // "after up to 24 hours" with no unbind escape hatch — see
            // `tests/live/support/teardown.rs:62`.
            return Err(ApiError::bad_request("engine_attached_to_queues_waiting_for_deletion"));
        }
    }
    Ok(())
}
