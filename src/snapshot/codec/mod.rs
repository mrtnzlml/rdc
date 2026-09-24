//! Per-kind codec registry.
//!
//! Each Rossum object kind that rdc manages gets exactly one [`KindCodec`]
//! implementation. The codec is the single source of truth for:
//!
//! * how the remote API body is transformed into on-disk bytes (`disk_bytes`),
//! * how the on-disk bytes are hashed for the lockfile (`base_hash`),
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
pub(crate) use mdh::normalize_search_index;
mod organization;
mod queues;
mod rules;
mod saved_views;
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
    /// Transform a remote API body into the canonical on-disk representation.
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact>;

    /// Compute the lockfile hash from a remote API body.
    ///
    /// The default implementation delegates to [`combined_hash`] over the
    /// artifact produced by [`Self::disk_bytes`], so the hash and the on-disk
    /// bytes always agree.
    fn base_hash(
        &self,
        value: &Value,
        lockfile: &crate::state::Lockfile,
    ) -> anyhow::Result<String> {
        let art = self.disk_bytes(value)?;
        Ok(combined_hash(&art.json, &art.sidecars, lockfile))
    }

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

/// A code sidecar's bytes as they participate in a content hash, with any
/// end-of-file newline ignored.
///
/// Every sidecar-bearing kind (hook `code`, rule `trigger_condition`, schema
/// `formulas/*`) stores program text, and rdc writes it without a
/// terminating newline — that is the form the API hands back. Editors
/// disagree: "insert final newline" is on by default in most of them. Left
/// in the hash, saving an otherwise untouched sidecar registers as a local
/// edit, so `sync` PATCHes semantically identical code and the write-back
/// then rewrites the file without the newline — a phantom remote write plus
/// a silent edit of a file the user never changed, recurring on every save.
/// Ignoring EOF newlines on both sides keeps the object Clean, the remote
/// untouched, and the file exactly as the user saved it.
///
/// CRLF counts as LF for the same reason: Git rewrites line endings in the
/// working tree (`core.autocrlf`, on by default in Git for Windows), so the
/// same code checks out as CRLF on one machine and LF on another. Hashed
/// verbatim, each machine's sync would push its line endings over the
/// other's. A lone `\r` is kept.
///
/// Nothing else is ignored. Interior trailing whitespace — and a trailing
/// space at EOF — are hashed verbatim: the API demonstrably preserves
/// whitespace inside code bodies, and an earlier blanket trailing-whitespace
/// trim silently corrupted real data (see
/// `snapshot::noise::trim_trailing_whitespace`).
pub(crate) fn sidecar_bytes_for_hash(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    let body = &bytes[..end];
    if !body.windows(2).any(|w| w == b"\r\n") {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut lf = Vec::with_capacity(body.len());
    for (i, &b) in body.iter().enumerate() {
        if !(b == b'\r' && body.get(i + 1) == Some(&b'\n')) {
            lf.push(b);
        }
    }
    std::borrow::Cow::Owned(lf)
}

/// Compute a combined SHA-256 over canonical JSON bytes and any sidecars.
///
/// Algorithm:
/// 1. Hash `canonicalize_for_hash(json)`.
/// 2. For each sidecar in order: feed
///    `0x00 || path_bytes || 0x00 || sidecar_bytes_for_hash(content)`.
/// 3. Hex-encode the digest.
///
/// This is the canonical hash function for all `KindCodec` implementations.
pub fn combined_hash(
    json: &[u8],
    sidecars: &[(String, Vec<u8>)],
    lockfile: &crate::state::Lockfile,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonicalize_for_hash(json, lockfile));
    for (path, bytes) in sidecars {
        hasher.update([0x00u8]);
        hasher.update(path.as_bytes());
        hasher.update([0x00u8]);
        hasher.update(&*sidecar_bytes_for_hash(bytes));
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
        "saved_views" => Some(&saved_views::SavedViews),
        "schemas" => Some(&schemas::Schemas),
        "workflow_steps" => Some(&workflow_steps::WorkflowSteps),
        "workflows" => Some(&workflows::Workflows),
        "workspaces" => Some(&workspaces::Workspaces),
        _ => None,
    }
}
