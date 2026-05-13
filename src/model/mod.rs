pub mod collection;
pub mod email_template;
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod hook_template;
pub mod inbox;
pub mod index_set;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workflow;
pub mod workflow_step;
pub mod workspace;

pub use collection::Collection;
pub use email_template::EmailTemplate;
pub use engine::Engine;
pub use engine_field::EngineField;
pub use hook::Hook;
pub use hook_template::HookTemplate;
pub use inbox::Inbox;
pub use index_set::IndexSet;
pub use label::Label;
pub use organization::Organization;
pub use queue::Queue;
pub use rule::Rule;
pub use schema::Schema;
pub use workflow::Workflow;
pub use workflow_step::WorkflowStep;
pub use workspace::Workspace;

use serde_json::Value;
use std::collections::BTreeMap;

/// Read `modified_at` from a model's `extra` map. Every Rossum object
/// has the server-set `modified_at` timestamp in the forward-compat
/// flatten bucket; this helper isolates the lookup so each model can
/// expose a one-line accessor.
pub(crate) fn modified_at(extra: &BTreeMap<String, Value>) -> Option<&str> {
    extra.get("modified_at").and_then(|v| v.as_str())
}
