use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Current lockfile schema version.
///
/// v3 dropped the redundant per-object `url` field: the URL is now DERIVED
/// from `id` + the env's `api_base` (see [`Lockfile::url_for_slug`]). The
/// slug remains the portable identity; `id` is the single source of truth
/// for the live URL.
pub const LOCKFILE_VERSION: u32 = 3;

/// rdc lockfile contents. One file per environment, stored at
/// `.rdc/state/<env>.lock.json`. Records the slug↔ID mapping plus
/// metadata used by future three-way-merge logic.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Lockfile {
    pub version: u32,
    /// The env's API base (e.g. `https://api.elis.rossum.ai/v1`). Used to
    /// DERIVE each object's live URL from its `id` + kind. Defaults to the
    /// empty string when absent (v1/v2 lockfiles); every production loader
    /// sets it from the env's [`crate::config::EnvConfig::api_base`] right
    /// after `load`. An empty `api_base` makes [`Lockfile::url_for_slug`]
    /// return `None` (fail-loud) rather than emit a malformed URL.
    #[serde(default)]
    pub api_base: String,
    /// Per object-type, a map of slug -> entry.
    pub objects: BTreeMap<String, BTreeMap<String, ObjectEntry>>,
}

/// One row in the lockfile.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ObjectEntry {
    /// Numeric Rossum ID. Also the single source of truth for the object's
    /// live URL, which is derived as `{api_base}/{endpoint(kind)}/{id}` —
    /// see [`Lockfile::url_for_slug`]. An `id` of `0` means "no URL"
    /// (currently only `mdh_indexes`, recorded as `id: 0`).
    pub id: u64,
    /// ISO 8601 server timestamp from `modified_at`, if present.
    #[serde(default)]
    pub modified_at: Option<String>,
    /// Rossum user URL from `modified_by`, if present — who last touched the
    /// object server-side. Recorded here so the snapshot JSON doesn't have to
    /// carry it (see [`crate::snapshot::key_order::HIDDEN_FIELDS`]), the same
    /// trade `modified_at` makes. Informational: no rdc decision reads it.
    ///
    /// `skip_serializing_if` keeps it out of entries that have no value, and
    /// `default` keeps lockfiles written before the field existed loadable at
    /// the SAME [`LOCKFILE_VERSION`] — bumping the version would make every
    /// older rdc refuse the file outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_by: Option<String>,
    /// Hex-encoded SHA-256 of the snapshot bytes that produced this entry.
    /// The merge base for the three-way comparison on subsequent pulls
    /// and pushes.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Hex-encoded SHA-256 of the local hook-secrets map that was last
    /// pushed to the remote for this slug. Used so a sync detects an
    /// edit to `secrets/<env>.hook-secrets.json` alone and force-PATCHes
    /// the affected hook, even when the hook JSON/code didn't change.
    /// Only meaningful for `hooks/<slug>` entries today; `None` and
    /// absent in serialized form on every other kind. The default is
    /// `None`, so lockfiles written before this field existed still
    /// load — and the first sync after that records a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_hash: Option<String>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            api_base: String::new(),
            objects: BTreeMap::new(),
        }
    }
}

/// Map a lockfile `kind` to the API endpoint segment that appears in its
/// URLs. Identity for every kind except `organization`, whose URLs use the
/// plural `organizations`. Verified empirically against the live TEST org
/// for all 297 url-bearing lockfile entries across 11 kinds.
fn endpoint_for(kind: &str) -> &str {
    if kind == "organization" {
        "organizations"
    } else {
        kind
    }
}

/// Inverse of [`endpoint_for`]: map a URL endpoint segment back to its
/// lockfile `kind`.
///
/// `organizations` → `organization` mirrors [`endpoint_for`]. `webhooks` →
/// `hooks` is a many-to-one alias with no [`endpoint_for`] counterpart: a
/// queue's `webhooks` back-reference array carries hook URLs under the legacy
/// `/webhooks/<id>` endpoint, but those hooks are tracked under the `hooks`
/// kind. Without this mapping the ref stays raw `https://` on disk while the
/// parallel `queue.hooks` entry (same hook, `/hooks/<id>` form) portabilizes,
/// producing mixed https/rdc noise. rdc never *emits* a `/webhooks/` URL
/// (`endpoint_for("hooks")` is `hooks`), so the alias is inbound-only.
fn kind_for_endpoint(ep: &str) -> &str {
    match ep {
        "organizations" => "organization",
        "webhooks" => "hooks",
        _ => ep,
    }
}

/// Parse the trailing `/<endpoint>/<id>` of a Rossum API URL into
/// `(endpoint, id)`. Returns `None` for any URL that doesn't end in
/// `/<segment>/<digits>` (e.g. `rdc://` refs, malformed URLs).
fn split_endpoint_id(url: &str) -> Option<(&str, u64)> {
    let trimmed = url.trim_end_matches('/');
    let (rest, id_str) = trimmed.rsplit_once('/')?;
    let id: u64 = id_str.parse().ok()?;
    let endpoint = rest.rsplit('/').next()?;
    if endpoint.is_empty() {
        return None;
    }
    Some((endpoint, id))
}

impl Lockfile {
    /// Load a lockfile from disk, returning the default value if the file
    /// does not exist. v1 and v2 lockfiles are silently migrated to v3:
    /// the legacy per-object `url` field is dropped (serde ignores it —
    /// there is no `#[serde(deny_unknown_fields)]`), the URL is now derived
    /// from `id` + `api_base`, and `api_base` defaults to `""` until a
    /// production caller sets it from the env config.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut lf: Lockfile =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

        match lf.version {
            1 | 2 => {
                // v1/v2 → v3: same top-level shape. v2's per-object `url`
                // field is silently dropped (no `deny_unknown_fields`), the
                // URL is now derived, and `api_base` defaults to `""` until
                // the caller sets it. Just bump the version field.
                lf.version = LOCKFILE_VERSION;
            }
            v if v == LOCKFILE_VERSION => {}
            v if v > LOCKFILE_VERSION => {
                anyhow::bail!(
                    "lockfile {} was written by a newer rdc (lockfile version {}, this rdc supports up to version {}). \
                    Run `rdc upgrade` to install a matching binary.",
                    path.display(),
                    v,
                    LOCKFILE_VERSION
                );
            }
            v => {
                anyhow::bail!(
                    "lockfile {} has unknown version {} (this rdc supports version {}). \
                    Delete it and run `rdc sync <env>` to reconstruct it.",
                    path.display(),
                    v,
                    LOCKFILE_VERSION
                );
            }
        }
        Ok(lf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self).context("serializing lockfile")?;
        crate::snapshot::writer::write_atomic(path, format!("{s}\n").as_bytes())?;
        Ok(())
    }

    pub fn upsert(&mut self, kind: &str, slug: &str, entry: ObjectEntry) {
        self.objects
            .entry(kind.to_string())
            .or_default()
            .insert(slug.to_string(), entry);
    }

    /// Find the slug of an object by its URL within a kind.
    /// Returns None if no entry matches.
    pub fn slug_for_url(&self, kind: &str, url: &str) -> Option<&str> {
        let by_kind = self.objects.get(kind)?;
        // A portable `rdc://<kind>/<slug>` reference carries its slug directly.
        if let Some((ref_kind, slug)) = crate::snapshot::refs::parse_rdc_ref(url) {
            if ref_kind != kind {
                return None;
            }
            return by_kind.get_key_value(slug).map(|(sl, _)| sl.as_str());
        }
        // A live API URL: parse its `/<endpoint>/<id>` and match by id
        // within this kind (the URL is no longer stored — `id` is canonical).
        // The endpoint must map to the requested `kind`, so a URL for a
        // different kind never matches here even if ids happen to collide.
        let (endpoint, id) = split_endpoint_id(url)?;
        if kind_for_endpoint(endpoint) != kind {
            return None;
        }
        self.slug_for_id(kind, id)
    }

    /// Find the slug of an object by its numeric ID within a kind.
    /// Returns None if no entry matches.
    /// Used by pull drivers to keep slugs stable across remote renames:
    /// once an object has a slug, that slug stays even if the remote
    /// `name` changes.
    pub fn slug_for_id(&self, kind: &str, id: u64) -> Option<&str> {
        let by_kind = self.objects.get(kind)?;
        for (slug, entry) in by_kind.iter() {
            if entry.id == id {
                return Some(slug.as_str());
            }
        }
        None
    }

    /// Multi-kind reverse lookup: given a URL, find which `(kind, slug)`
    /// owns it. Drives [`crate::snapshot::refs::url_to_rdc`], i.e. the pull
    /// side's portabilization of a live cross-reference into
    /// `rdc://<kind>/<slug>`.
    pub fn lookup_url(&self, url: &str) -> Option<(&str, &str)> {
        // A portable `rdc://<kind>/<slug>` reference resolves directly to its
        // (kind, slug) coordinate when that object is tracked here.
        if let Some((kind, slug)) = crate::snapshot::refs::parse_rdc_ref(url) {
            let (k, entries) = self.objects.get_key_value(kind)?;
            let (sl, _) = entries.get_key_value(slug)?;
            return Some((k.as_str(), sl.as_str()));
        }
        // A live API URL: parse `/<endpoint>/<id>`, map the endpoint back to
        // its kind, and find the slug by id. Borrow the real key str from
        // `self.objects` so the returned `&str` outlives this call.
        let (endpoint, id) = split_endpoint_id(url)?;
        let kind = kind_for_endpoint(endpoint);
        let (k, _entries) = self.objects.get_key_value(kind)?;
        let slug = self.slug_for_id(kind, id)?;
        Some((k.as_str(), slug))
    }

    /// Derive the live URL for a given `(kind, slug)` from its `id` and the
    /// env's `api_base`: `{api_base}/{endpoint(kind)}/{id}`. Returns `None`
    /// (fail-loud — never a malformed URL) when the kind/slug isn't tracked,
    /// when `api_base` is unset (a v1/v2 load whose caller forgot to set it),
    /// or when the entry's `id` is `0` (the `mdh_indexes` sentinel: index
    /// sets are never URL-resolved).
    pub fn url_for_slug(&self, kind: &str, slug: &str) -> Option<String> {
        let entry = self.objects.get(kind)?.get(slug)?;
        if entry.id == 0 || self.api_base.is_empty() {
            return None;
        }
        Some(format!(
            "{}/{}/{}",
            self.api_base.trim_end_matches('/'),
            endpoint_for(kind),
            entry.id
        ))
    }
}

/// Compute a stable SHA-256 over canonical JSON bytes (with noise fields
/// stripped). Falls back to raw-byte SHA-256 for inputs that aren't valid
/// JSON. Hex-encoded output.
pub fn content_hash(bytes: &[u8], lockfile: &Lockfile) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(bytes, lockfile);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    to_hex(&hasher.finalize())
}

/// SHA-256 over `bytes` VERBATIM — no JSON canonicalization, no noise-field
/// stripping. For content whose bytes ARE the artifact: MDH row data
/// (`data.jsonl`), where a line is customer data rather than a Rossum object.
///
/// [`content_hash`] must not be used there. It canonicalizes whenever the whole
/// stream parses as one JSON value, which a single-line JSONL file does, and its
/// noise-strip then deletes `modified_at` / `modifier` / `training_enabled` at
/// any depth — so a one-row dataset's edit to such a column would hash
/// identically and the change would be silently dropped. Verified empirically.
pub fn raw_content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// SHA-256 over a single hook's local secrets map, encoded as the
/// canonical JSON object `{ "key1": "val1", "key2": "val2", ... }` with
/// `BTreeMap`-sorted keys (`serde_json::to_vec` over a `BTreeMap` is
/// stable). Empty/absent maps hash to the canonical empty-object hash
/// so "user removed all secrets" and "user never set any" don't share
/// the same `None` lockfile state — the recorded hash distinguishes
/// "synced and empty" from "never seen". Hex-encoded.
pub fn hook_secrets_hash(map: &std::collections::BTreeMap<String, String>) -> String {
    let bytes = serde_json::to_vec(map).expect("BTreeMap<String,String> always serializes");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    to_hex(&hasher.finalize())
}

/// Hex-encode a digest as a 64-char lowercase string. Used by every
/// `*_hash` function to format its SHA-256 output.
fn to_hex(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}

/// Compute a stable SHA-256 over a schema's combined content: the
/// post-extraction `schema.json` bytes plus each formula file (path + body).
/// Formulas must be passed sorted by `field_id` for determinism.
///
/// The inputs are the split `write_schema_bytes` in `src/snapshot/schema.rs`
/// writes to disk. The hash is:
///
/// ```text
/// SHA-256(
///     json_bytes
///     || 0x00 || "formulas/<id>.py" || 0x00 || formula_bytes
///     || ...   (continued for every formula, in field_id order)
/// )
/// ```
/// EOF newlines and CRLF line endings in a code sidecar are not drift — see
/// [`crate::snapshot::codec::sidecar_bytes_for_hash`], the single definition
/// shared with `KindCodec::base_hash` so both hashing paths agree.
use crate::snapshot::codec::sidecar_bytes_for_hash as code_bytes_for_hash;

pub fn schema_combined_hash(
    json_bytes: &[u8],
    formulas: &[(String, Vec<u8>)],
    lockfile: &Lockfile,
) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes, lockfile);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    for (field_id, bytes) in formulas {
        hasher.update([0u8]);
        let path = format!("formulas/{field_id}.py");
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(&*code_bytes_for_hash(bytes));
    }
    to_hex(&hasher.finalize())
}

/// Compute the combined hash for a hook: the post-extraction `<slug>.json`
/// bytes plus the extracted code (when present).
///
/// ```text
/// SHA-256(
///     canonicalize_for_hash(json_bytes)
///     [|| 0x00 || "code" || 0x00 || code_bytes]
/// )
/// ```
///
/// `status` is NOT stripped here. Upstream, `serialize_hook` (and thus
/// `KindCodec::disk_bytes` for hooks) redacts `status` to the constant
/// sentinel `REDACTED_VALUE_SENTINEL` before the JSON bytes are ever
/// passed here. A constant value is hash-stable regardless of server
/// churn, so an extra strip is redundant. Removing it makes
/// `hook_combined_hash(json, code)` byte-identical to
/// `crate::snapshot::codec::combined_hash(json, &[("code", code_bytes)])`,
/// which is what `KindCodec::base_hash` computes — aligning pull, push,
/// sync, deploy, and doctor on a single hash definition.
pub fn hook_combined_hash(json_bytes: &[u8], code: &Option<String>, lockfile: &Lockfile) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes, lockfile);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"code");
        hasher.update([0u8]);
        hasher.update(&*code_bytes_for_hash(code.as_bytes()));
    }
    to_hex(&hasher.finalize())
}

/// Compute the combined hash for a rule: the post-extraction
/// `<slug>.json` bytes plus the extracted `trigger_condition` (when
/// present).
///
/// ```text
/// SHA-256(
///     json_bytes
///     [|| 0x00 || "trigger_condition" || 0x00 || code_bytes]
/// )
/// ```
///
/// The separator includes the field name (not just `"code"`) so a
/// future rule field that also carries Python wouldn't silently
/// collide.
pub fn rule_combined_hash(json_bytes: &[u8], code: &Option<String>, lockfile: &Lockfile) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes, lockfile);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"trigger_condition");
        hasher.update([0u8]);
        hasher.update(&*code_bytes_for_hash(code.as_bytes()));
    }
    to_hex(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_empty() {
        let lf = Lockfile::load(Path::new("/nope.json")).unwrap();
        assert_eq!(lf, Lockfile::default());
        assert_eq!(lf.version, LOCKFILE_VERSION);
    }

    #[test]
    fn round_trip_v3() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile {
            api_base: "https://x.rossum.app/api/v1".to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry {
                id: 1,
                modified_at: Some("2026-04-01T10:00:00Z".to_string()),
                modified_by: None,
                content_hash: Some("a".repeat(64)),
                secrets_hash: None,
            },
        );
        lf.save(&path).unwrap();
        let loaded = Lockfile::load(&path).unwrap();
        assert_eq!(loaded, lf);
    }

    #[test]
    fn future_version_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(&path, r#"{"version":999,"objects":{}}"#).unwrap();
        let err = Lockfile::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("version"));
    }

    /// `modified_by` rides alongside `modified_at`: recorded on pull, kept out
    /// of the snapshot JSON. Both survive a save/load round-trip.
    #[test]
    fn object_entry_round_trips_modified_by() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            "validator",
            ObjectEntry {
                id: 7,
                modified_at: Some("2026-03-01T09:00:00Z".to_string()),
                modified_by: Some("https://x/api/v1/users/4".to_string()),
                content_hash: Some("abc".to_string()),
                secrets_hash: None,
            },
        );
        lf.save(&path).unwrap();

        let loaded = Lockfile::load(&path).unwrap();
        let entry = &loaded.objects["hooks"]["validator"];
        assert_eq!(entry.modified_by.as_deref(), Some("https://x/api/v1/users/4"));
        assert_eq!(entry.modified_at.as_deref(), Some("2026-03-01T09:00:00Z"));
    }

    /// A lockfile written before the field existed must still load — and must
    /// NOT need a `LOCKFILE_VERSION` bump, which would make every older rdc
    /// refuse the file. The field is additive: absent means `None`.
    #[test]
    fn lockfile_without_modified_by_loads_as_none_at_current_version() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(
            &path,
            r#"{
  "version": 3,
  "api_base": "https://api.elis.rossum.ai/v1",
  "objects": {
    "queues": {
      "invoices": { "id": 123, "modified_at": "2026-03-01T09:00:00Z", "content_hash": "abc" }
    }
  }
}
"#,
        )
        .unwrap();

        let lf = Lockfile::load(&path).unwrap();
        assert_eq!(lf.version, LOCKFILE_VERSION);
        assert!(lf.objects["queues"]["invoices"].modified_by.is_none());
    }

    /// An entry with no `modified_by` must not serialize the key at all, so
    /// existing lockfiles don't grow a wall of `"modified_by": null` lines.
    #[test]
    fn absent_modified_by_is_not_serialized() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "labels",
            "urgent",
            ObjectEntry {
                id: 1,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let json = serde_json::to_string_pretty(&lf).unwrap();
        assert!(
            !json.contains("modified_by"),
            "absent modified_by must be omitted: {json}"
        );
    }

    #[test]
    fn v1_lockfile_migrates_to_v3_in_memory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        // Hand-write a v1 lockfile (no url, no content_hash).
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "objects": {
    "hooks": {
      "old-hook": {
        "id": 7,
        "modified_at": "2026-03-01T09:00:00Z"
      }
    }
  }
}
"#,
        )
        .unwrap();

        let lf = Lockfile::load(&path).unwrap();
        assert_eq!(lf.version, LOCKFILE_VERSION);
        // api_base defaults to empty until a caller sets it.
        assert_eq!(lf.api_base, "");
        let entry = &lf.objects["hooks"]["old-hook"];
        assert_eq!(entry.id, 7);
        assert_eq!(entry.modified_at.as_deref(), Some("2026-03-01T09:00:00Z"));
        assert!(entry.content_hash.is_none());
    }

    /// A v2 lockfile carries a per-object `url` field that v3 dropped.
    /// `load` must migrate it to v3 (version bumped, `api_base` defaulting to
    /// empty) and silently ignore the legacy `url` — there is no
    /// `#[serde(deny_unknown_fields)]`, so the entry still deserializes.
    #[test]
    fn v2_lockfile_migrates_to_v3_and_drops_legacy_url() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(
            &path,
            r#"{
  "version": 2,
  "objects": {
    "queues": {
      "invoices": {
        "id": 123,
        "url": "https://api.elis.rossum.ai/v1/queues/123",
        "modified_at": "2026-03-01T09:00:00Z",
        "content_hash": "abc"
      }
    },
    "organization": {
      "self": {
        "id": 5,
        "url": "https://api.elis.rossum.ai/v1/organizations/5"
      }
    }
  }
}
"#,
        )
        .unwrap();

        let mut lf = Lockfile::load(&path).unwrap();
        assert_eq!(lf.version, LOCKFILE_VERSION);
        assert_eq!(lf.api_base, "", "api_base defaults to empty on migration");
        let q = &lf.objects["queues"]["invoices"];
        assert_eq!(q.id, 123);
        assert_eq!(q.modified_at.as_deref(), Some("2026-03-01T09:00:00Z"));
        assert_eq!(q.content_hash.as_deref(), Some("abc"));
        assert_eq!(lf.objects["organization"]["self"].id, 5);

        // Once a caller supplies api_base, the URL is derived (not read from
        // the dropped legacy field).
        lf.api_base = "https://api.elis.rossum.ai/v1".to_string();
        assert_eq!(
            lf.url_for_slug("queues", "invoices").as_deref(),
            Some("https://api.elis.rossum.ai/v1/queues/123")
        );
        assert_eq!(
            lf.url_for_slug("organization", "self").as_deref(),
            Some("https://api.elis.rossum.ai/v1/organizations/5")
        );
    }

    /// Empirical-gate: the derivation rule verified against the live TEST org
    /// for all 297 url-bearing entries across 11 kinds —
    /// `{api_base}/{endpoint(kind)}/{id}`, where `endpoint == kind` for every
    /// kind except `organization` → `organizations`, and `id == 0`
    /// (the `mdh_indexes` / `mdh_data` sentinel kinds) yields no URL. An empty
    /// `api_base` fails loud.
    #[test]
    fn url_for_slug_derivation_matches_empirical_rule() {
        let mut lf = Lockfile {
            api_base: "https://api.elis.rossum.ai/v1".to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            "queues",
            "q",
            ObjectEntry {
                id: 123,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "organization",
            "org",
            ObjectEntry {
                id: 5,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "mdh_indexes",
            "ds",
            ObjectEntry {
                id: 0,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "mdh_data",
            "ds",
            ObjectEntry {
                id: 0,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        // queues: endpoint == kind.
        assert_eq!(
            lf.url_for_slug("queues", "q").as_deref(),
            Some("https://api.elis.rossum.ai/v1/queues/123")
        );
        // organization: the one kind whose endpoint is pluralized.
        assert_eq!(
            lf.url_for_slug("organization", "org").as_deref(),
            Some("https://api.elis.rossum.ai/v1/organizations/5")
        );
        // mdh_indexes: id == 0 → no URL.
        assert_eq!(lf.url_for_slug("mdh_indexes", "ds"), None);
        // mdh_data: the second id-0 sentinel kind (row data has no API URL).
        assert_eq!(lf.url_for_slug("mdh_data", "ds"), None);
        // Unknown kind/slug → None.
        assert_eq!(lf.url_for_slug("queues", "nope"), None);

        // Empty api_base fails loud (never a malformed URL).
        lf.api_base = String::new();
        assert_eq!(lf.url_for_slug("queues", "q"), None);
    }

    /// Round-trip: a derived URL fed back through `lookup_url` resolves to the
    /// original `(kind, slug)`, including the pluralized `organization`
    /// endpoint.
    #[test]
    fn lookup_url_round_trips_derived_urls() {
        let mut lf = Lockfile {
            api_base: "https://api.elis.rossum.ai/v1".to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            "queues",
            "q",
            ObjectEntry {
                id: 123,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "organization",
            "org",
            ObjectEntry {
                id: 5,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        assert_eq!(
            lf.lookup_url("https://api.elis.rossum.ai/v1/queues/123"),
            Some(("queues", "q"))
        );
        assert_eq!(
            lf.lookup_url("https://api.elis.rossum.ai/v1/organizations/5"),
            Some(("organization", "org"))
        );
        // A URL whose id isn't tracked → None.
        assert_eq!(
            lf.lookup_url("https://api.elis.rossum.ai/v1/queues/999"),
            None
        );
    }

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash(b"hello", &Lockfile::default());
        let h2 = content_hash(b"hello", &Lockfile::default());
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_distinguishes_inputs() {
        assert_ne!(
            content_hash(b"foo", &Lockfile::default()),
            content_hash(b"bar", &Lockfile::default())
        );
    }

    /// `content_hash` canonicalizes a byte stream that parses as ONE JSON value,
    /// which a single-line JSONL file does — and its noise-strip then removes
    /// `modified_at`. `raw_content_hash` must see that edit, or a one-row MDH
    /// dataset's change would be silently dropped by the sync gate.
    #[test]
    fn raw_content_hash_sees_edits_content_hash_canonicalizes_away() {
        let before = b"{\"code\":\"1000\",\"modified_at\":\"2026-01-01\"}\n";
        let after = b"{\"code\":\"1000\",\"modified_at\":\"2026-02-01\"}\n";
        assert_eq!(
            content_hash(before, &Lockfile::default()),
            content_hash(after, &Lockfile::default()),
            "precondition: content_hash is blind here (this is why raw_content_hash exists)"
        );
        assert_ne!(
            raw_content_hash(before),
            raw_content_hash(after),
            "raw_content_hash must see a real edit to a noise-named column"
        );
    }

    #[test]
    fn schema_combined_hash_no_formulas() {
        let h1 = schema_combined_hash(b"{}", &[], &Lockfile::default());
        let h2 = schema_combined_hash(b"{}", &[], &Lockfile::default());
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}", &Lockfile::default()));
    }

    #[test]
    fn schema_combined_hash_with_formulas_is_deterministic() {
        let formulas = vec![
            ("amount_total".to_string(), b"a + b".to_vec()),
            ("invoice_id".to_string(), b"x".to_vec()),
        ];
        let h1 = schema_combined_hash(b"{}", &formulas, &Lockfile::default());
        let h2 = schema_combined_hash(b"{}", &formulas, &Lockfile::default());
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn schema_combined_hash_changes_when_formula_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"a + b".to_vec())];
        let f2 = vec![("amount_total".to_string(), b"a + b + c".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1, &Lockfile::default()),
            schema_combined_hash(json, &f2, &Lockfile::default())
        );
    }

    #[test]
    fn schema_combined_hash_changes_when_field_id_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"x".to_vec())];
        let f2 = vec![("amount_due".to_string(), b"x".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1, &Lockfile::default()),
            schema_combined_hash(json, &f2, &Lockfile::default())
        );
    }

    #[test]
    fn hook_combined_hash_no_code() {
        let h1 = hook_combined_hash(b"{}", &None, &Lockfile::default());
        let h2 = hook_combined_hash(b"{}", &None, &Lockfile::default());
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}", &Lockfile::default()));
    }

    #[test]
    fn code_hash_ignores_an_editor_added_final_newline() {
        let bare = Some("def x():\n    return 1".to_string());
        let with_nl = Some("def x():\n    return 1\n".to_string());
        let with_crlf = Some("def x():\n    return 1\r\n".to_string());
        let lf = Lockfile::default();
        assert_eq!(
            hook_combined_hash(b"{}", &bare, &lf),
            hook_combined_hash(b"{}", &with_nl, &lf),
            "a trailing newline is an editor artifact — the API never stores one, so \
             hashing it makes an untouched save look like a local edit"
        );
        assert_eq!(
            hook_combined_hash(b"{}", &bare, &lf),
            hook_combined_hash(b"{}", &with_crlf, &lf),
            "same for CRLF line endings"
        );
        assert_eq!(
            rule_combined_hash(b"{}", &bare, &lf),
            rule_combined_hash(b"{}", &with_nl, &lf),
            "rules extract trigger_condition to a sidecar with the same convention"
        );
        let f_bare = vec![("total".to_string(), b"a + b".to_vec())];
        let f_nl = vec![("total".to_string(), b"a + b\n".to_vec())];
        assert_eq!(
            schema_combined_hash(b"{}", &f_bare, &lf),
            schema_combined_hash(b"{}", &f_nl, &lf),
            "formula sidecars follow the same convention"
        );
    }

    #[test]
    fn code_hash_still_sees_interior_and_non_newline_whitespace() {
        let lf = Lockfile::default();
        // A trailing SPACE inside the body is real: the API preserves it,
        // so it must stay visible as drift (a blanket trim once corrupted data).
        assert_ne!(
            hook_combined_hash(b"{}", &Some("x = 1\ny = 2".to_string()), &lf),
            hook_combined_hash(b"{}", &Some("x = 1 \ny = 2".to_string()), &lf),
        );
        // Trailing space at EOF (not a newline) is likewise not ignored.
        assert_ne!(
            hook_combined_hash(b"{}", &Some("x = 1".to_string()), &lf),
            hook_combined_hash(b"{}", &Some("x = 1 ".to_string()), &lf),
        );
        // A blank line ADDED before EOF text is a real change.
        assert_ne!(
            hook_combined_hash(b"{}", &Some("x = 1\ny = 2".to_string()), &lf),
            hook_combined_hash(b"{}", &Some("x = 1\n\ny = 2".to_string()), &lf),
        );
    }

    #[test]
    fn hook_combined_hash_with_code_differs_from_no_code() {
        let h_no = hook_combined_hash(b"{}", &None, &Lockfile::default());
        let h_with = hook_combined_hash(
            b"{}",
            &Some("def x(): pass".to_string()),
            &Lockfile::default(),
        );
        assert_ne!(h_no, h_with);
    }

    #[test]
    fn hook_combined_hash_changes_when_code_changes() {
        let json = b"{}";
        let h1 = hook_combined_hash(json, &Some("v1".to_string()), &Lockfile::default());
        let h2 = hook_combined_hash(json, &Some("v2".to_string()), &Lockfile::default());
        assert_ne!(h1, h2);
    }

    #[test]
    fn content_hash_equal_when_only_modified_at_differs() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"2026-01-01\"}";
        let b = b"{\"name\":\"x\",\"modified_at\":\"2026-12-31\"}";
        assert_eq!(
            content_hash(a, &Lockfile::default()),
            content_hash(b, &Lockfile::default())
        );
    }

    #[test]
    fn content_hash_differs_on_real_change() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"y\",\"modified_at\":\"t\"}";
        assert_ne!(
            content_hash(a, &Lockfile::default()),
            content_hash(b, &Lockfile::default())
        );
    }

    #[test]
    fn content_hash_falls_back_for_non_json_bytes() {
        let h1 = content_hash(b"hello world", &Lockfile::default());
        let h2 = content_hash(b"hello world", &Lockfile::default());
        assert_eq!(h1, h2);
        assert_ne!(
            content_hash(b"hello world", &Lockfile::default()),
            content_hash(b"goodbye", &Lockfile::default())
        );
    }

    #[test]
    fn hook_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"h\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"h\",\"modified_at\":\"t2\"}";
        let code = Some("def x(): pass".to_string());
        assert_eq!(
            hook_combined_hash(a, &code, &Lockfile::default()),
            hook_combined_hash(b, &code, &Lockfile::default())
        );
    }

    #[test]
    fn rule_combined_hash_no_code() {
        let h1 = rule_combined_hash(b"{}", &None, &Lockfile::default());
        let h2 = rule_combined_hash(b"{}", &None, &Lockfile::default());
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}", &Lockfile::default()));
    }

    #[test]
    fn rule_combined_hash_with_code_differs_from_no_code() {
        let h_no = rule_combined_hash(b"{}", &None, &Lockfile::default());
        let h_with = rule_combined_hash(b"{}", &Some("x > 0".to_string()), &Lockfile::default());
        assert_ne!(h_no, h_with);
    }

    #[test]
    fn rule_combined_hash_changes_when_code_changes() {
        let json = b"{}";
        let h1 = rule_combined_hash(json, &Some("x > 0".to_string()), &Lockfile::default());
        let h2 = rule_combined_hash(json, &Some("y < 10".to_string()), &Lockfile::default());
        assert_ne!(h1, h2);
    }

    #[test]
    fn rule_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"r\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"r\",\"modified_at\":\"t2\"}";
        let code = Some("x > 0".to_string());
        assert_eq!(
            rule_combined_hash(a, &code, &Lockfile::default()),
            rule_combined_hash(b, &code, &Lockfile::default())
        );
    }

    /// rule_combined_hash and hook_combined_hash must NOT collide when
    /// given the same json + code bytes, because the field-name
    /// separator differs (`code` vs `trigger_condition`).
    #[test]
    fn rule_and_hook_combined_hashes_do_not_collide() {
        let json = b"{}";
        let code = Some("x > 0".to_string());
        assert_ne!(
            rule_combined_hash(json, &code, &Lockfile::default()),
            hook_combined_hash(json, &code, &Lockfile::default())
        );
    }

    #[test]
    fn schema_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"s\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"s\",\"modified_at\":\"t2\"}";
        let formulas = vec![("42".to_string(), b"return 1\n".to_vec())];
        assert_eq!(
            schema_combined_hash(a, &formulas, &Lockfile::default()),
            schema_combined_hash(b, &formulas, &Lockfile::default())
        );
    }

    #[test]
    fn slug_for_id_finds_match() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry {
                id: 42,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        assert_eq!(lf.slug_for_id("hooks", 42), Some("validator-invoices"));
        assert_eq!(lf.slug_for_id("hooks", 99), None);
        assert_eq!(lf.slug_for_id("rules", 42), None);
    }

    #[test]
    fn slug_for_url_finds_match() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "workspaces",
            "invoices-ap",
            ObjectEntry {
                id: 1,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // The URL is matched by its trailing id, not by a stored string.
        assert_eq!(
            lf.slug_for_url("workspaces", "https://x/api/v1/workspaces/1"),
            Some("invoices-ap"),
        );
        assert_eq!(lf.slug_for_url("workspaces", "https://nope"), None);
        // A workspaces URL must not resolve within `hooks` even if ids
        // collide — the endpoint segment is checked against the kind.
        assert_eq!(
            lf.slug_for_url("hooks", "https://x/api/v1/workspaces/1"),
            None
        );
    }

    /// A queue's `webhooks` back-reference array carries hook URLs under the
    /// legacy `/webhooks/<id>` endpoint (not `/hooks/<id>`). The endpoint
    /// segment must still map back to the `hooks` kind so the ref resolves to
    /// its tracked slug — otherwise the URL stays raw `https://` on disk while
    /// the parallel `queue.hooks` entry (same hook, `/hooks/<id>` form) is
    /// portabilized, producing mixed https/rdc noise in the snapshot.
    #[test]
    fn lookup_url_maps_webhooks_endpoint_to_hooks_kind() {
        let mut lf = Lockfile {
            api_base: "https://x.rossum.app/api/v1".to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            "hooks",
            "validator",
            ObjectEntry {
                id: 55,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // The `/hooks/55` form already resolves.
        assert_eq!(
            lf.lookup_url("https://x.rossum.app/api/v1/hooks/55"),
            Some(("hooks", "validator"))
        );
        // The `/webhooks/55` form (from `queue.webhooks`) must resolve to the
        // SAME hook, not fall through to a nonexistent `webhooks` kind.
        assert_eq!(
            lf.lookup_url("https://x.rossum.app/api/v1/webhooks/55"),
            Some(("hooks", "validator"))
        );
        // And via the kind-scoped lookup too.
        assert_eq!(
            lf.slug_for_url("hooks", "https://x.rossum.app/api/v1/webhooks/55"),
            Some("validator")
        );
    }

    /// Guard: `hook_combined_hash` must produce the same digest as
    /// `crate::snapshot::codec::combined_hash` for the same input.
    ///
    /// This locks the alignment between the legacy state hash path
    /// (pull/push/sync/deploy/doctor callers) and the codec-based hash path,
    /// so a future refactor that accidentally re-introduces divergence
    /// (e.g. extra strips or a different sidecar label) is caught immediately.
    #[test]
    fn hook_combined_hash_equals_codec_combined_hash() {
        use crate::snapshot::codec::combined_hash as codec_combined_hash;

        // --- with code ---
        let json = b"{\"name\":\"validator\",\"status\":\"<refreshed live in Rossum; not synced by rdc>\"}";
        let code = Some("def process(payload, settings):\n    pass\n".to_string());

        let legacy = hook_combined_hash(json, &code, &Lockfile::default());
        let sidecars = vec![("code".to_string(), code.clone().unwrap().into_bytes())];
        let codec = codec_combined_hash(json, &sidecars, &Lockfile::default());
        assert_eq!(
            legacy, codec,
            "hook_combined_hash must equal codec::combined_hash (with code)"
        );

        // --- without code ---
        let json_no_code =
            b"{\"name\":\"webhook\",\"status\":\"<refreshed live in Rossum; not synced by rdc>\"}";
        let legacy_no_code = hook_combined_hash(json_no_code, &None, &Lockfile::default());
        let codec_no_code = codec_combined_hash(json_no_code, &[], &Lockfile::default());
        assert_eq!(
            legacy_no_code, codec_no_code,
            "hook_combined_hash must equal codec::combined_hash (no code)"
        );
    }
}
