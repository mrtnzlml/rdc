use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Current lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 2;

/// rdc lockfile contents. One file per environment, stored at
/// `.rdc/state/<env>.lock.json`. Records the slug↔ID mapping plus
/// metadata used by future three-way-merge logic.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Lockfile {
    pub version: u32,
    /// Per object-type, a map of slug -> entry.
    pub objects: BTreeMap<String, BTreeMap<String, ObjectEntry>>,
}

/// One row in the lockfile.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ObjectEntry {
    /// Numeric Rossum ID.
    pub id: u64,
    /// Canonical Rossum URL for the object. Powers cross-reference
    /// resolution (e.g. queue.workspace → workspace slug).
    #[serde(default)]
    pub url: Option<String>,
    /// ISO 8601 server timestamp from `modified_at`, if present.
    #[serde(default)]
    pub modified_at: Option<String>,
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
        Self { version: LOCKFILE_VERSION, objects: BTreeMap::new() }
    }
}

impl Lockfile {
    /// Load a lockfile from disk, returning the default value if the file
    /// does not exist. v1 lockfiles are silently migrated to v2 (the new
    /// fields default to None and will be populated on the next pull).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut lf: Lockfile = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        match lf.version {
            1 => {
                // v1 → v2: same top-level shape, but ObjectEntry's new fields
                // default to None thanks to #[serde(default)]. Just bump the
                // version field; the next pull will populate url and content_hash.
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
                    Run `rdc repair --rebuild-lock <env>` to reconstruct it.",
                    path.display(),
                    v,
                    LOCKFILE_VERSION
                );
            }
        }
        Ok(lf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)
            .context("serializing lockfile")?;
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
        for (slug, entry) in by_kind.iter() {
            if entry.url.as_deref() == Some(url) {
                return Some(slug.as_str());
            }
        }
        None
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
    /// owns it. Used by `rdc deploy` to rewrite cross-references in a
    /// payload from src URLs to tgt URLs.
    pub fn lookup_url(&self, url: &str) -> Option<(&str, &str)> {
        for (kind, entries) in &self.objects {
            for (slug, entry) in entries {
                if entry.url.as_deref() == Some(url) {
                    return Some((kind.as_str(), slug.as_str()));
                }
            }
        }
        None
    }

    /// Recover the URL for a given `(kind, slug)`. Returns `None` if
    /// either the kind isn't tracked or the entry has no URL recorded.
    pub fn url_for_slug(&self, kind: &str, slug: &str) -> Option<&str> {
        self.objects.get(kind)?.get(slug)?.url.as_deref()
    }
}

/// Compute a stable SHA-256 over canonical JSON bytes (with noise fields
/// stripped). Falls back to raw-byte SHA-256 for inputs that aren't valid
/// JSON. Hex-encoded output.
pub fn content_hash(bytes: &[u8]) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
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
/// Algorithm matches the documentation on `write_schema` in
/// `src/snapshot/schema.rs`:
///
/// ```text
/// SHA-256(
///     json_bytes
///     || 0x00 || "formulas/<id>.py" || 0x00 || formula_bytes
///     || ...   (continued for every formula, in field_id order)
/// )
/// ```
pub fn schema_combined_hash(json_bytes: &[u8], formulas: &[(String, Vec<u8>)]) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    for (field_id, bytes) in formulas {
        hasher.update([0u8]);
        let path = format!("formulas/{field_id}.py");
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
    }
    to_hex(&hasher.finalize())
}

/// Compute the combined hash for a hook: the post-extraction `<slug>.json`
/// bytes plus the extracted code (when present).
///
/// ```text
/// SHA-256(
///     json_bytes
///     [|| 0x00 || "code" || 0x00 || code_bytes]
/// )
/// ```
pub fn hook_combined_hash(json_bytes: &[u8], code: &Option<String>) -> String {
    // `status` is server-managed for hooks (pending → ready transition
    // happens asynchronously after a POST). Strip it from the hash so
    // a hook created at T0 doesn't show drift at T0+a-few-seconds.
    let canonical = crate::snapshot::noise::canonicalize_with_extra_strips(
        json_bytes,
        &["status"],
    );
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"code");
        hasher.update([0u8]);
        hasher.update(code.as_bytes());
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
pub fn rule_combined_hash(json_bytes: &[u8], code: &Option<String>) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"trigger_condition");
        hasher.update([0u8]);
        hasher.update(code.as_bytes());
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
    fn round_trip_v2() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry {
                id: 1,
                url: Some("https://x.rossum.app/api/v1/hooks/1".to_string()),
                modified_at: Some("2026-04-01T10:00:00Z".to_string()),
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

    #[test]
    fn v1_lockfile_migrates_to_v2_in_memory() {
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
        let entry = &lf.objects["hooks"]["old-hook"];
        assert_eq!(entry.id, 7);
        assert_eq!(entry.modified_at.as_deref(), Some("2026-03-01T09:00:00Z"));
        assert!(entry.url.is_none());
        assert!(entry.content_hash.is_none());
    }

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_distinguishes_inputs() {
        assert_ne!(content_hash(b"foo"), content_hash(b"bar"));
    }

    #[test]
    fn schema_combined_hash_no_formulas() {
        let h1 = schema_combined_hash(b"{}", &[]);
        let h2 = schema_combined_hash(b"{}", &[]);
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}"));
    }

    #[test]
    fn schema_combined_hash_with_formulas_is_deterministic() {
        let formulas = vec![
            ("amount_total".to_string(), b"a + b".to_vec()),
            ("invoice_id".to_string(), b"x".to_vec()),
        ];
        let h1 = schema_combined_hash(b"{}", &formulas);
        let h2 = schema_combined_hash(b"{}", &formulas);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn schema_combined_hash_changes_when_formula_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"a + b".to_vec())];
        let f2 = vec![("amount_total".to_string(), b"a + b + c".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1),
            schema_combined_hash(json, &f2)
        );
    }

    #[test]
    fn schema_combined_hash_changes_when_field_id_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"x".to_vec())];
        let f2 = vec![("amount_due".to_string(), b"x".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1),
            schema_combined_hash(json, &f2)
        );
    }

    #[test]
    fn hook_combined_hash_no_code() {
        let h1 = hook_combined_hash(b"{}", &None);
        let h2 = hook_combined_hash(b"{}", &None);
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}"));
    }

    #[test]
    fn hook_combined_hash_with_code_differs_from_no_code() {
        let h_no = hook_combined_hash(b"{}", &None);
        let h_with = hook_combined_hash(b"{}", &Some("def x(): pass".to_string()));
        assert_ne!(h_no, h_with);
    }

    #[test]
    fn hook_combined_hash_changes_when_code_changes() {
        let json = b"{}";
        let h1 = hook_combined_hash(json, &Some("v1".to_string()));
        let h2 = hook_combined_hash(json, &Some("v2".to_string()));
        assert_ne!(h1, h2);
    }

    #[test]
    fn content_hash_equal_when_only_modified_at_differs() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"2026-01-01\"}";
        let b = b"{\"name\":\"x\",\"modified_at\":\"2026-12-31\"}";
        assert_eq!(content_hash(a), content_hash(b));
    }

    #[test]
    fn content_hash_differs_on_real_change() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"y\",\"modified_at\":\"t\"}";
        assert_ne!(content_hash(a), content_hash(b));
    }

    #[test]
    fn content_hash_falls_back_for_non_json_bytes() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2);
        assert_ne!(content_hash(b"hello world"), content_hash(b"goodbye"));
    }

    #[test]
    fn hook_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"h\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"h\",\"modified_at\":\"t2\"}";
        let code = Some("def x(): pass".to_string());
        assert_eq!(hook_combined_hash(a, &code), hook_combined_hash(b, &code));
    }

    #[test]
    fn rule_combined_hash_no_code() {
        let h1 = rule_combined_hash(b"{}", &None);
        let h2 = rule_combined_hash(b"{}", &None);
        assert_eq!(h1, h2);
        assert_eq!(h1, content_hash(b"{}"));
    }

    #[test]
    fn rule_combined_hash_with_code_differs_from_no_code() {
        let h_no = rule_combined_hash(b"{}", &None);
        let h_with = rule_combined_hash(b"{}", &Some("x > 0".to_string()));
        assert_ne!(h_no, h_with);
    }

    #[test]
    fn rule_combined_hash_changes_when_code_changes() {
        let json = b"{}";
        let h1 = rule_combined_hash(json, &Some("x > 0".to_string()));
        let h2 = rule_combined_hash(json, &Some("y < 10".to_string()));
        assert_ne!(h1, h2);
    }

    #[test]
    fn rule_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"r\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"r\",\"modified_at\":\"t2\"}";
        let code = Some("x > 0".to_string());
        assert_eq!(rule_combined_hash(a, &code), rule_combined_hash(b, &code));
    }

    /// rule_combined_hash and hook_combined_hash must NOT collide when
    /// given the same json + code bytes, because the field-name
    /// separator differs (`code` vs `trigger_condition`).
    #[test]
    fn rule_and_hook_combined_hashes_do_not_collide() {
        let json = b"{}";
        let code = Some("x > 0".to_string());
        // hook_combined_hash also strips `status`; the JSON has no
        // status so canonicalize_with_extra_strips reduces to the same
        // canonicalize_for_hash input on this minimal payload.
        assert_ne!(rule_combined_hash(json, &code), hook_combined_hash(json, &code));
    }

    #[test]
    fn schema_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"s\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"s\",\"modified_at\":\"t2\"}";
        let formulas = vec![("42".to_string(), b"return 1\n".to_vec())];
        assert_eq!(
            schema_combined_hash(a, &formulas),
            schema_combined_hash(b, &formulas)
        );
    }

    #[test]
    fn slug_for_id_finds_match() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry { id: 42, url: None, modified_at: None, content_hash: None, secrets_hash: None },
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
                url: Some("https://x/api/v1/workspaces/1".to_string()),
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        assert_eq!(
            lf.slug_for_url("workspaces", "https://x/api/v1/workspaces/1"),
            Some("invoices-ap"),
        );
        assert_eq!(lf.slug_for_url("workspaces", "https://nope"), None);
        assert_eq!(lf.slug_for_url("hooks", "https://x/api/v1/workspaces/1"), None);
    }
}
