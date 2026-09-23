pub mod collection;
pub mod email_template;
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod index_set;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod saved_view;
pub mod schema;
pub mod workflow;
pub mod workflow_step;
pub mod workspace;

pub use collection::Collection;
pub use email_template::EmailTemplate;
pub use engine::Engine;
pub use engine_field::EngineField;
pub use hook::Hook;
pub use inbox::Inbox;
pub use index_set::IndexSet;
pub use label::Label;
pub use organization::Organization;
pub use queue::Queue;
pub use rule::Rule;
pub use saved_view::SavedView;
pub use schema::Schema;
pub use workflow::Workflow;
pub use workflow_step::WorkflowStep;
pub use workspace::Workspace;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

/// Deserialize helper shared by every model: treat an explicit JSON `null`
/// exactly like an absent key by falling back to `T::default()`.
///
/// serde's `#[serde(default)]` alone only covers a *missing* key — an explicit
/// `null` is still handed to the field's deserializer, which fails for
/// non-`Option` types (`id: u64`, `url: String`) with e.g. "invalid type:
/// null, expected u64". A new-object file scaffolded by blanking a pulled
/// object's server-managed fields carries exactly those nulls, and the create
/// push path strips `id`/`url` before POST anyway, so deserialization must
/// tolerate them. Always pair with `#[serde(default)]` so a missing key keeps
/// working too:
///
/// ```ignore
/// #[serde(default, deserialize_with = "crate::model::null_as_default")]
/// pub id: u64,
/// ```
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Read `modified_at` from a model's `extra` map. Every Rossum object
/// has the server-set `modified_at` timestamp in the forward-compat
/// flatten bucket; this helper isolates the lookup so each model can
/// expose a one-line accessor.
pub(crate) fn modified_at(extra: &IndexMap<String, Value>) -> Option<&str> {
    extra.get("modified_at").and_then(|v| v.as_str())
}

/// Read the server's `modified_by` (a Rossum user URL) out of a model's
/// flattened extras. Companion to [`modified_at`]: both are stripped from the
/// on-disk JSON and recorded in the lockfile instead.
pub(crate) fn modified_by(extra: &IndexMap<String, Value>) -> Option<&str> {
    extra.get("modified_by").and_then(|v| v.as_str())
}
