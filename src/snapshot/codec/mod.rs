//! Per-kind codec registry.
//!
//! Each Rossum object kind that rdc manages gets exactly one [`KindCodec`]
//! implementation. The codec is the single source of truth for:
//!
//! * how the remote API body is transformed into on-disk bytes (`disk_bytes`),
//! * how the on-disk bytes are hashed for the lockfile (`base_hash`),
//! * what to strip before a create POST (`create_body`),
//! * what to strip before a cross-env PATCH (`cross_env_body`),
//! * which overlay section applies (`overlay`),
//! * where on disk the primary JSON file lives (`path`).
//!
//! Call [`codec`] to look up the registry by kind string.

mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
mod inboxes;
mod labels;
mod mdh;
mod organization;
mod queues;
mod rules;
mod schemas;
mod workflow_steps;
mod workflows;
mod workspaces;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::noise::canonicalize_for_hash;

/// The primary JSON bytes and any sidecar files produced by [`KindCodec::disk_bytes`].
pub struct DiskArtifact {
    /// The canonical on-disk JSON representation (pretty-printed, trailing newline).
    pub json: Vec<u8>,
    /// Side-car files: `(relative_path, bytes)` pairs. For most kinds this is
    /// empty; hooks carry `.py` / `.js` formula files here.
    pub sidecars: Vec<(String, Vec<u8>)>,
}

/// Per-kind serialization and transformation logic.
///
/// Implementors are zero-sized structs (e.g. `pub struct Engines;`). The
/// trait is object-safe so `codec()` can return `&'static dyn KindCodec`.
pub trait KindCodec: Sync {
    /// The Rossum kind string (e.g. `"engines"`).
    fn kind(&self) -> &'static str;

    /// Transform a remote API body into the canonical on-disk representation.
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact>;

    /// Compute the lockfile hash from a remote API body.
    ///
    /// The default implementation delegates to [`combined_hash`] over the
    /// artifact produced by [`Self::disk_bytes`], so the hash and the on-disk
    /// bytes always agree.
    fn base_hash(&self, value: &Value) -> anyhow::Result<String> {
        let art = self.disk_bytes(value)?;
        Ok(combined_hash(&art.json, &art.sidecars))
    }

    /// Strip server-managed fields from `body` before a create POST.
    fn create_body(&self, body: &mut Value);

    /// Strip server-managed fields from `body` before a cross-env PATCH.
    fn cross_env_body(&self, body: &mut Value);

    /// Return the overlay overrides for this object (if any).
    ///
    /// `slug` is the object's canonical slug. The default returns `None`
    /// (kinds with no overlay section).
    fn overlay<'a>(
        &self,
        _overlay: &'a Overlay,
        _slug: &str,
    ) -> Option<&'a BTreeMap<String, Value>> {
        None
    }

    /// Return the path of the primary JSON file for this object.
    ///
    /// `slug` is the object's canonical slug.
    fn path(&self, paths: &Paths, slug: &str) -> PathBuf;
}

/// Compute a combined SHA-256 over canonical JSON bytes and any sidecars.
///
/// Algorithm:
/// 1. Hash `canonicalize_for_hash(json)`.
/// 2. For each sidecar in order: feed `0x00 || path_bytes || 0x00 || content`.
/// 3. Hex-encode the digest.
///
/// This is the canonical hash function for all `KindCodec` implementations.
pub fn combined_hash(json: &[u8], sidecars: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonicalize_for_hash(json));
    for (path, bytes) in sidecars {
        hasher.update([0x00u8]);
        hasher.update(path.as_bytes());
        hasher.update([0x00u8]);
        hasher.update(bytes);
    }
    to_hex(&hasher.finalize())
}

fn to_hex(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}

/// Look up the codec for `kind`. Returns `None` for unregistered kinds.
pub fn codec(kind: &str) -> Option<&'static dyn KindCodec> {
    match kind {
        "email_templates" => Some(&email_templates::EmailTemplates),
        "engine_fields" => Some(&engine_fields::EngineFields),
        "engines" => Some(&engines::Engines),
        "hooks" => Some(&hooks::Hooks),
        "inboxes" => Some(&inboxes::Inboxes),
        "labels" => Some(&labels::Labels),
        "mdh" => Some(&mdh::Mdh),
        "organization" => Some(&organization::Organization),
        "queues" => Some(&queues::Queues),
        "rules" => Some(&rules::Rules),
        "schemas" => Some(&schemas::Schemas),
        "workflow_steps" => Some(&workflow_steps::WorkflowSteps),
        "workflows" => Some(&workflows::Workflows),
        "workspaces" => Some(&workspaces::Workspaces),
        _ => None,
    }
}
