//! Post-pull portabilization pass.
//!
//! After all per-kind pull drivers have run, this pass walks every lockfile
//! entry, reads the corresponding on-disk JSON file, rewrites any portable-kind
//! URLs to `rdc://<kind>/<slug>` form, and re-records the lockfile
//! `content_hash` so the next `sync` sees the object as `Clean` (no phantom
//! drift).
//!
//! The pass is idempotent: objects that already use `rdc://` refs (or carry no
//! portable refs at all) are skipped without touching the file or the lockfile.

use anyhow::{Context, Result};

use crate::paths::Paths;
use crate::snapshot::refs::portabilize_value;
use crate::state::Lockfile;

/// Walk every lockfile entry, rewrite portable-kind URLs in the corresponding
/// on-disk JSON file to `rdc://` form, and update the lockfile `content_hash`
/// so the next `sync` classification sees the object as `Clean`.
///
/// Objects with no portable refs are skipped (no file write, no re-hash).
pub fn portabilize_refs(paths: &Paths, lockfile: &mut Lockfile) -> Result<()> {
    // Snapshot the (kind, slug) list to avoid borrow conflicts while we
    // mutate the lockfile entries below.
    let entries: Vec<(String, String)> = lockfile
        .objects
        .iter()
        .flat_map(|(kind, slugs)| slugs.keys().map(move |slug| (kind.clone(), slug.clone())))
        .collect();

    for (kind, slug) in entries {
        // Locate the on-disk JSON path.
        let path = match locate_json_path(paths, &kind, &slug) {
            Some(p) => p,
            None => continue,
        };
        if !path.exists() {
            continue;
        }
        // A conflicted object has a shadow file (`<file>.<env>`) alongside it —
        // the local was kept verbatim for the user to resolve against the
        // remote shadow. Leave it byte-untouched: don't migrate its refs while
        // the user is mid-resolution. It converges on the next sync once the
        // conflict is resolved.
        if crate::paths::shadow_path_for(&path, paths.env()).exists() {
            continue;
        }

        // Read and parse. A read error on a file we just confirmed exists is a
        // real I/O problem (permissions, races) and should surface, not be
        // silently skipped — consistent with how the sidecar reads below fail.
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue, // defensive: skip unparseable files
        };

        // Portabilize, then canonicalize a hook's `run_after` order. Both run
        // before the change check so a sort-only delta (refs already portable
        // but in the source env's arbitrary id order) still rewrites + rehashes
        // — keeping run_after slug-sorted and stable across re-pulls and envs.
        let before = value.clone();
        portabilize_value(&mut value, lockfile);
        if kind == "hooks" {
            crate::snapshot::hook::sort_run_after(&mut value);
            // `queues` was sorted at serialize time, before portabilization —
            // i.e. in URL/id order. Re-sort the now-portable slugs so the
            // on-disk order is env-stable and `doctor` never re-canonicalizes
            // freshly pulled hooks. Same rationale as `run_after` above.
            crate::snapshot::hook::sort_queues(&mut value);
        }
        if value == before {
            continue;
        }

        // Serialize: pretty-printed with a trailing newline, matching the
        // convention used by all other pull writers.
        let mut new_bytes = serde_json::to_vec_pretty(&value)
            .with_context(|| format!("serializing portabilized {kind}/{slug}"))?;
        new_bytes.push(b'\n');

        // Atomic write.
        crate::snapshot::writer::write_atomic(&path, &new_bytes)
            .with_context(|| format!("writing portabilized {}/{}", kind, slug))?;

        // Re-hash: mirror the EXACT per-kind algorithm used by the pull driver
        // and by `sync::classify` so the lockfile baseline stays consistent.
        let new_hash = compute_hash_for_kind(&kind, &slug, &new_bytes, paths)?;

        // Update the lockfile entry.
        if let Some(entry) = lockfile
            .objects
            .get_mut(&kind)
            .and_then(|m| m.get_mut(&slug))
        {
            entry.content_hash = Some(new_hash);
        }
    }

    Ok(())
}

/// Locate the primary JSON file for `(kind, slug)`.
///
/// - `queues`/`schemas`/`inboxes`: the lockfile key is a BARE queue slug; we
///   glob under `workspaces/*/queues/<slug>/` via `locate_queue_dir`, then
///   join the kind-specific filename.
/// - All other kinds: `codec(kind).path(paths, slug)`.
fn locate_json_path(paths: &Paths, kind: &str, slug: &str) -> Option<std::path::PathBuf> {
    match kind {
        "queues" => {
            let queue_dir = crate::cli::deploy::create::locate_queue_dir(paths, slug)?;
            Some(queue_dir.join("queue.json"))
        }
        "schemas" => {
            let queue_dir = crate::cli::deploy::create::locate_queue_dir(paths, slug)?;
            Some(queue_dir.join("schema.json"))
        }
        "inboxes" => {
            let queue_dir = crate::cli::deploy::create::locate_queue_dir(paths, slug)?;
            Some(queue_dir.join("inbox.json"))
        }
        _ => {
            let codec = crate::snapshot::codec::codec(kind)?;
            Some(codec.path(paths, slug))
        }
    }
}

/// Compute the canonical lockfile hash for the on-disk bytes of `(kind, slug)`,
/// mirroring the per-kind algorithm used by the pull drivers.
///
/// - **schemas**: `schema_combined_hash(json_bytes, formulas)` — reads the
///   formula sidecars from disk, exactly as `pull::queues` does.
/// - **hooks**: `hook_combined_hash(json_bytes, code)` — reads the `.py`/`.js`
///   sidecar from disk, using runtime-derived extension detection.
/// - **rules**: `combined_hash(json_bytes, [("trigger_condition", code)])` —
///   reads the `.py` sidecar from disk.
/// - **everything else**: `content_hash(json_bytes)` (= `combined_hash` with
///   no sidecars).
fn compute_hash_for_kind(
    kind: &str,
    slug: &str,
    json_bytes: &[u8],
    paths: &Paths,
) -> Result<String> {
    match kind {
        "schemas" => {
            // Need the formula sidecars. The queue dir is the parent of schema.json,
            // which we already know lives under `workspaces/*/queues/<slug>/`. Use
            // locate_queue_dir again to find it.
            let queue_dir = crate::cli::deploy::create::locate_queue_dir(paths, slug)
                .with_context(|| format!("locating queue dir for schema re-hash ({slug})"))?;
            let formulas = crate::snapshot::schema::read_local_formulas(&queue_dir)
                .with_context(|| format!("reading formulas for schema re-hash ({slug})"))?;
            Ok(crate::state::schema_combined_hash(
                json_bytes,
                &formulas,
                &crate::state::Lockfile::default(),
            ))
        }
        "hooks" => {
            // Derive the code sidecar extension from the JSON we just wrote.
            let value: serde_json::Value = serde_json::from_slice(json_bytes)
                .with_context(|| format!("re-parsing just-written hook JSON for {slug}"))?;
            let ext = crate::snapshot::hook::hook_code_extension_from_value(&value);
            let code_path = paths.hooks_dir().join(format!("{slug}.{ext}"));
            // Also check the other extension (stale sidecar, runtime changed).
            let stale_ext = if ext == "py" { "js" } else { "py" };
            let stale_code_path = paths.hooks_dir().join(format!("{slug}.{stale_ext}"));
            let code: Option<String> = if code_path.exists() {
                Some(
                    std::fs::read_to_string(&code_path)
                        .with_context(|| format!("reading hook code {}", code_path.display()))?,
                )
            } else if stale_code_path.exists() {
                Some(
                    std::fs::read_to_string(&stale_code_path).with_context(|| {
                        format!("reading hook code {}", stale_code_path.display())
                    })?,
                )
            } else {
                None
            };
            Ok(crate::state::hook_combined_hash(
                json_bytes,
                &code,
                &crate::state::Lockfile::default(),
            ))
        }
        "rules" => {
            let py_path = paths.rules_dir().join(format!("{slug}.py"));
            let code: Option<String> = if py_path.exists() {
                Some(
                    std::fs::read_to_string(&py_path)
                        .with_context(|| format!("reading rule code {}", py_path.display()))?,
                )
            } else {
                None
            };
            Ok(crate::state::rule_combined_hash(
                json_bytes,
                &code,
                &crate::state::Lockfile::default(),
            ))
        }
        _ => {
            // All remaining kinds (queues, inboxes, workspaces, labels,
            // engines, engine_fields, email_templates, etc.) are single-file
            // and use content_hash.
            Ok(crate::state::content_hash(
                json_bytes,
                &crate::state::Lockfile::default(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::state::{Lockfile, ObjectEntry, content_hash};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    /// Seed the lockfile with a single object entry.
    fn seed_entry(lockfile: &mut Lockfile, kind: &str, slug: &str, id: u64) {
        lockfile.upsert(
            kind,
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                content_hash: Some("placeholder-hash".to_string()),
                secrets_hash: None,
            },
        );
    }

    /// Write `bytes` to `path`, creating parent directories as needed.
    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// Build and return the on-disk JSON for a label that references a
    /// workspace URL. The workspace URL is the `organization` field (a
    /// generic cross-reference), chosen because `labels` is a flat kind
    /// that uses `codec.path` (no locate_queue_dir needed).
    fn label_json_with_workspace_url(ws_url: &str) -> serde_json::Value {
        json!({
            "id": 42,
            "url": "https://example.rossum.app/api/v1/labels/42",
            "name": "AP",
            "organization": "https://example.rossum.app/api/v1/organizations/1",
            // A generic cross-ref field pointing at the workspace.
            "workspace": ws_url
        })
    }

    #[test]
    fn portabilize_refs_rewrites_url_and_updates_hash() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        // IDs and URLs for our test objects.
        const WS_ID: u64 = 1001;
        const WS_SLUG: &str = "acme-ws";
        const WS_URL: &str = "https://example.rossum.app/api/v1/workspaces/1001";
        const LABEL_ID: u64 = 42;
        const LABEL_SLUG: &str = "ap-label";

        // Build the initial lockfile with both objects registered.
        let mut lockfile = Lockfile::default();
        seed_entry(&mut lockfile, "workspaces", WS_SLUG, WS_ID);
        seed_entry(&mut lockfile, "labels", LABEL_SLUG, LABEL_ID);

        // Write a label JSON that contains a raw workspace URL.
        let label_value = label_json_with_workspace_url(WS_URL);
        let mut label_bytes = serde_json::to_vec_pretty(&label_value).unwrap();
        label_bytes.push(b'\n');

        let label_path = paths.labels_dir().join(format!("{LABEL_SLUG}.json"));
        write_file(&label_path, &label_bytes);

        // Also write a minimal workspace JSON (so the workspace entry on disk
        // won't trigger a portabilize, as it has no portal refs to rewrite).
        let ws_value = json!({
            "id": WS_ID,
            "url": WS_URL,
            "name": "Acme Workspace"
        });
        let mut ws_bytes = serde_json::to_vec_pretty(&ws_value).unwrap();
        ws_bytes.push(b'\n');
        let ws_path = paths.workspace_dir(WS_SLUG).join("workspace.json");
        write_file(&ws_path, &ws_bytes);

        // Record the pre-portabilize hash for the label.
        let pre_hash = content_hash(&label_bytes, &Lockfile::default());
        lockfile
            .objects
            .get_mut("labels")
            .unwrap()
            .get_mut(LABEL_SLUG)
            .unwrap()
            .content_hash = Some(pre_hash.clone());

        // Run the portabilize pass.
        portabilize_refs(&paths, &mut lockfile).expect("portabilize_refs must succeed");

        // (a) The label file now contains `rdc://workspaces/<slug>` and
        //     no raw workspace URL.
        let on_disk = fs::read_to_string(&label_path).unwrap();
        assert!(
            on_disk.contains(&format!("rdc://workspaces/{WS_SLUG}")),
            "expected rdc://workspaces/{WS_SLUG} in:\n{on_disk}"
        );
        assert!(
            !on_disk.contains(WS_URL),
            "raw workspace URL must be gone:\n{on_disk}"
        );

        // (b) The lockfile content_hash was updated to match the new file bytes.
        let new_hash = lockfile
            .objects
            .get("labels")
            .unwrap()
            .get(LABEL_SLUG)
            .unwrap()
            .content_hash
            .as_deref()
            .unwrap();
        let expected_hash = content_hash(
            fs::read(&label_path).unwrap().as_slice(),
            &Lockfile::default(),
        );
        assert_eq!(
            new_hash, expected_hash,
            "lockfile hash must match content_hash of the new file bytes"
        );
        // And it must differ from the pre-portabilize hash (the file changed).
        assert_ne!(new_hash, pre_hash, "hash must change after portabilization");

        // (c) The workspace entry was also updated (its own `url` field points
        //     to itself, so portabilize_value rewrites it to rdc:// too). The
        //     lockfile hash must match what's now on disk.
        let ws_hash = lockfile
            .objects
            .get("workspaces")
            .unwrap()
            .get(WS_SLUG)
            .unwrap()
            .content_hash
            .as_deref()
            .unwrap();
        let ws_on_disk_bytes = fs::read(&ws_path).unwrap();
        let expected_ws_hash = content_hash(ws_on_disk_bytes.as_slice(), &Lockfile::default());
        assert_eq!(
            ws_hash, expected_ws_hash,
            "workspace lockfile hash must match the portabilized on-disk bytes"
        );
    }

    #[test]
    fn portabilize_refs_skips_already_portabilized_objects() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        const WS_SLUG: &str = "demo-ws";
        const LABEL_SLUG: &str = "demo-label";

        let mut lockfile = Lockfile::default();
        seed_entry(&mut lockfile, "workspaces", WS_SLUG, 2002);
        seed_entry(&mut lockfile, "labels", LABEL_SLUG, 99);

        // Write a label that already uses rdc:// for the workspace cross-ref
        // AND uses rdc:// for its own url — should not be changed.
        let already_portabilized = json!({
            "id": 99,
            "url": format!("rdc://labels/{LABEL_SLUG}"),
            "name": "Demo",
            "workspace": format!("rdc://workspaces/{WS_SLUG}")
        });
        let mut bytes = serde_json::to_vec_pretty(&already_portabilized).unwrap();
        bytes.push(b'\n');
        let label_path = paths.labels_dir().join(format!("{LABEL_SLUG}.json"));
        write_file(&label_path, &bytes);

        // Seed a stable content_hash for this label.
        let stable_hash = content_hash(&bytes, &Lockfile::default());
        lockfile
            .objects
            .get_mut("labels")
            .unwrap()
            .get_mut(LABEL_SLUG)
            .unwrap()
            .content_hash = Some(stable_hash.clone());

        portabilize_refs(&paths, &mut lockfile).expect("portabilize_refs must succeed");

        // The lockfile hash must be unchanged (skip-if-unchanged).
        let post_hash = lockfile
            .objects
            .get("labels")
            .unwrap()
            .get(LABEL_SLUG)
            .unwrap()
            .content_hash
            .as_deref()
            .unwrap();
        assert_eq!(
            post_hash, stable_hash,
            "hash must be unchanged for already-portabilized label"
        );

        // The file content must be identical (no rdc:// double-conversion).
        let on_disk = fs::read(&label_path).unwrap();
        assert_eq!(on_disk, bytes, "file bytes must be unchanged");
    }

    #[test]
    fn portabilize_refs_sorts_hook_queues_into_slug_order() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        const SLUG: &str = "exporter";
        let mut lockfile = Lockfile::default();
        seed_entry(&mut lockfile, "hooks", SLUG, 500);

        // Pull serializes hooks with `queues` sorted BEFORE portabilization —
        // i.e. in URL/id order. After the rdc:// rewrite the slugs land in
        // that id order, not slug order, so `doctor` would "canonicalize"
        // every freshly pulled hook. The post-pass must leave the array in
        // slug-sorted order, exactly like `run_after`.
        let hook = json!({
            "id": 500,
            "url": format!("rdc://hooks/{SLUG}"),
            "name": "Exporter",
            "type": "webhook",
            "queues": ["rdc://queues/zeta-line", "rdc://queues/alpha-line"],
            "run_after": [],
        });
        let mut bytes = serde_json::to_vec_pretty(&hook).unwrap();
        bytes.push(b'\n');
        let path = paths.hooks_dir().join(format!("{SLUG}.json"));
        write_file(&path, &bytes);

        let code: Option<String> = None;
        let pre = crate::state::hook_combined_hash(&bytes, &code, &Lockfile::default());
        lockfile
            .objects
            .get_mut("hooks")
            .unwrap()
            .get_mut(SLUG)
            .unwrap()
            .content_hash = Some(pre.clone());

        portabilize_refs(&paths, &mut lockfile).expect("portabilize_refs must succeed");

        let after_bytes = fs::read(&path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&after_bytes).unwrap();
        assert_eq!(
            v["queues"],
            json!(["rdc://queues/alpha-line", "rdc://queues/zeta-line"]),
            "post-pass must sort hook.queues into slug order so doctor finds no drift"
        );
        // Hash is order-invariant for rdc:// ref arrays by design.
        let new = lockfile
            .objects
            .get("hooks")
            .unwrap()
            .get(SLUG)
            .unwrap()
            .content_hash
            .clone()
            .unwrap();
        assert_eq!(new, pre, "queues order must not affect the combined hash");
    }

    #[test]
    fn portabilize_refs_sorts_run_after_even_when_already_portable() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        const SLUG: &str = "exporter";
        let mut lockfile = Lockfile::default();
        seed_entry(&mut lockfile, "hooks", SLUG, 500);

        // A hook whose refs are ALREADY portable rdc:// — so `portabilize_value`
        // is a pure no-op — but whose `run_after` is in the source env's
        // arbitrary (non-sorted) id order. The post-pass must still canonicalize
        // it, otherwise a re-pull would keep churning and migrate would reorder.
        let hook = json!({
            "id": 500,
            "url": format!("rdc://hooks/{SLUG}"),
            "name": "Exporter",
            "type": "webhook",
            "run_after": ["rdc://hooks/valve-template", "rdc://hooks/fitting-template"],
        });
        let mut bytes = serde_json::to_vec_pretty(&hook).unwrap();
        bytes.push(b'\n');
        let path = paths.hooks_dir().join(format!("{SLUG}.json"));
        write_file(&path, &bytes);

        let code: Option<String> = None;
        let pre = crate::state::hook_combined_hash(&bytes, &code, &Lockfile::default());
        lockfile
            .objects
            .get_mut("hooks")
            .unwrap()
            .get_mut(SLUG)
            .unwrap()
            .content_hash = Some(pre.clone());

        portabilize_refs(&paths, &mut lockfile).expect("portabilize_refs must succeed");

        // The on-disk BYTES must change: the file is rewritten with run_after
        // in canonical sorted order (this is the symptom the user hit — a
        // spurious reorder in `git diff`/`migrate`).
        let after_bytes = fs::read(&path).unwrap();
        assert_ne!(after_bytes, bytes, "the file must be rewritten with sorted run_after");
        let v: serde_json::Value = serde_json::from_slice(&after_bytes).unwrap();
        assert_eq!(
            v["run_after"],
            json!([
                "rdc://hooks/fitting-template",
                "rdc://hooks/valve-template"
            ]),
            "post-pass must sort run_after into a canonical, env-stable order"
        );

        // The lockfile hash stays consistent with the on-disk bytes. (It does
        // not change: `canonicalize_for_hash` already sorts `rdc://` ref arrays,
        // so run_after order never affected the hash — the instability was
        // purely in the written bytes, which this fix canonicalizes.)
        let new = lockfile
            .objects
            .get("hooks")
            .unwrap()
            .get(SLUG)
            .unwrap()
            .content_hash
            .clone()
            .unwrap();
        let expected = crate::state::hook_combined_hash(&after_bytes, &code, &Lockfile::default());
        assert_eq!(new, expected, "lockfile hash must match the on-disk bytes");
        assert_eq!(new, pre, "hash is order-invariant for rdc:// ref arrays by design");
    }

    #[test]
    fn portabilize_refs_skips_missing_files() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        let mut lockfile = Lockfile::default();
        // Register a label that has NO corresponding file on disk.
        seed_entry(&mut lockfile, "labels", "ghost-label", 777);

        // Must not panic or error.
        portabilize_refs(&paths, &mut lockfile)
            .expect("portabilize_refs must succeed even with missing files");
    }
}
