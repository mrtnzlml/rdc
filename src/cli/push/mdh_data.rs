//! Push driver for MDH row data (`data.jsonl`) — the hybrid keyed diff.
//!
//! The local file is authoritative: after a push the env holds exactly the rows
//! the file lists. How that is achieved matters, though, so this is NOT a
//! delete-everything-then-reinsert:
//!
//! - a row carrying an explicit (non-ObjectId) `_id` is identified BY that id,
//!   so editing it is one in-place `replace_one` and its identity survives;
//! - a row without one is identified by its canonical content, as a multiset,
//!   so only genuinely new rows are inserted and only genuinely surplus rows
//!   are deleted. Untouched rows are never rewritten — no empty window, no
//!   ObjectId churn.
//!
//! Deletes are applied BEFORE inserts so a unique index survives a row edit:
//! the outgoing row releases its key before the incoming row claims it.

use crate::api::DataStorageClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::snapshot::mdh_data::{ROW_HARD_LIMIT, ROW_WARN_THRESHOLD, canonicalize_row, from_jsonl};
use crate::state::{Lockfile, ObjectEntry, raw_content_hash};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The operations that make a collection match `data.jsonl`.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RowDiff {
    /// Documents to insert, canonical form (may carry an explicit `_id`).
    pub insert: Vec<Value>,
    /// `(id, replacement)` pairs for in-place replace. The replacement never
    /// contains `_id` — the filter carries identity and the field is immutable.
    pub replace: Vec<(Value, Value)>,
    /// RAW remote `_id` values to delete.
    pub delete: Vec<Value>,
}

/// The explicit, user-authored `_id` of a canonical row, if any. A
/// server-generated ObjectId is not one — `canonicalize_row` has already
/// dropped it, so this returns `None` for those.
fn explicit_id(canonical: &Value) -> Option<Value> {
    canonical.get("_id").cloned()
}

/// Canonical JSON text of a value — the map key for both identity flavors.
fn key_of(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// A canonical row with its `_id` removed: the shape `replace_one` wants.
fn without_id(canonical: &Value) -> Value {
    let mut out = canonical.clone();
    if let Value::Object(obj) = &mut out {
        obj.shift_remove("_id");
    }
    out
}

/// Diff local rows (from `data.jsonl`) against raw remote rows (from
/// `find_all`, `_id`s intact). Output order is derived from sorted maps, so it
/// is deterministic and independent of input order.
pub(crate) fn diff_rows(local: &[Value], remote: &[Value]) -> RowDiff {
    // key → canonical row
    let mut local_keyed: BTreeMap<String, Value> = BTreeMap::new();
    // canonical json → (row, wanted count)
    let mut local_unkeyed: BTreeMap<String, (Value, usize)> = BTreeMap::new();
    for row in local {
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                local_keyed.insert(key_of(&id), canonical);
            }
            None => {
                let k = key_of(&canonical);
                local_unkeyed.entry(k).or_insert((canonical, 0)).1 += 1;
            }
        }
    }

    // key → (raw id, canonical row)
    let mut remote_keyed: BTreeMap<String, (Value, Value)> = BTreeMap::new();
    // canonical json → raw ids, sorted so the surplus we drop is deterministic
    let mut remote_unkeyed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in remote {
        let raw_id = row.get("_id").cloned().unwrap_or(Value::Null);
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                remote_keyed.insert(key_of(&id), (raw_id, canonical));
            }
            None => remote_unkeyed
                .entry(key_of(&canonical))
                .or_default()
                .push(raw_id),
        }
    }
    for ids in remote_unkeyed.values_mut() {
        ids.sort_by_key(key_of);
    }

    let mut diff = RowDiff::default();

    for (key, local_row) in &local_keyed {
        match remote_keyed.get(key) {
            None => diff.insert.push(local_row.clone()),
            Some((_, remote_row)) if remote_row != local_row => {
                let id = explicit_id(local_row).expect("a keyed row always carries its _id");
                diff.replace.push((id, without_id(local_row)));
            }
            Some(_) => {}
        }
    }
    for (key, (raw_id, _)) in &remote_keyed {
        if !local_keyed.contains_key(key) {
            diff.delete.push(raw_id.clone());
        }
    }

    for (key, (row, wanted)) in &local_unkeyed {
        let have = remote_unkeyed.get(key).map_or(0, |ids| ids.len());
        for _ in have..*wanted {
            diff.insert.push(row.clone());
        }
    }
    for (key, ids) in &remote_unkeyed {
        let wanted = local_unkeyed.get(key).map_or(0, |(_, n)| *n);
        for raw_id in ids.iter().skip(wanted) {
            diff.delete.push(raw_id.clone());
        }
    }

    diff
}

/// Documents per `insert_many` / ids per `delete_many`. 1 000 documents ×
/// 20 fields (≈440 KB) was measured at ~1.1 s against the live API, so 500
/// leaves comfortable headroom.
const WRITE_CHUNK: usize = 500;

/// Make `collection_name` hold exactly the rows in `data.jsonl`.
///
/// An ABSENT `data.jsonl` is never authoritative: it means the dataset was
/// flagged manual but has not been pulled yet, NOT that the env should be
/// emptied. Such a dataset is skipped entirely (0 ops, no reads, no writes).
/// Only a present file — including a deliberately 0-byte one — expresses
/// "these are all the rows".
///
/// The lockfile hash and the base cache are advanced only when the push FULLY
/// reconciled the env (no gated skip, no error). A partial push leaves the
/// dataset looking locally-diverged, so the next sync re-reads and re-diffs —
/// which is the correct recovery given that a failed `insert_many` is
/// partially applied.
#[allow(clippy::too_many_arguments)]
pub async fn push_dataset_data(
    client: &DataStorageClient,
    lockfile: &mut Lockfile,
    collection_name: &str,
    slug: &str,
    paths: &Paths,
    allow_deletes: bool,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<usize> {
    let data_path = paths.dataset_data(slug);
    let Ok(local_raw) = std::fs::read(&data_path) else {
        return Ok(0); // not pulled yet — never authoritative
    };

    let local_rows = from_jsonl(&local_raw, &data_path.display().to_string())?;
    if local_rows.len() > ROW_HARD_LIMIT {
        anyhow::bail!(
            "mdh/{slug}: {} rows in {} exceeds rdc's {ROW_HARD_LIMIT}-row ceiling for \
             versioned MDH data. Remove the \"data\" key from {} to stop versioning this \
             dataset's rows (its indexes and name stay managed).",
            local_rows.len(),
            data_path.display(),
            crate::cli::pull::mdh::COLLECTION_MANIFEST,
        );
    }
    if local_rows.len() > ROW_WARN_THRESHOLD {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {} rows is large for a git-versioned dataset (warns above \
                 {ROW_WARN_THRESHOLD})",
                local_rows.len()
            ),
        );
    }

    // `from_jsonl` already hard-errors on two rows sharing an explicit `_id`
    // (MongoDB enforces `_id` uniqueness), so any duplicate that survives to
    // here is a byte-identical UNKEYED row — a legitimate multiset entry, not
    // a data-integrity problem. Still worth flagging: in a lookup table like
    // this, a repeated row is usually a copy-paste mistake rather than intent.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let duplicates = local_rows.iter().filter(|r| !seen.insert(key_of(r))).count();
    if duplicates > 0 {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {duplicates} duplicate row(s) in {} — kept as-is (rows are a \
                 multiset), but duplicates in a lookup table are usually unintended",
                data_path.display()
            ),
        );
    }

    let remote_rows = client
        .find_all(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("reading rows of '{collection_name}'"))?;

    let diff = diff_rows(&local_rows, &remote_rows);

    // Deletions ride the same gate as index drops.
    let pending = diff.delete.len();
    let mut deletes: &[Value] = &diff.delete;
    let mut skipped = false;
    match crate::cli::push::mdh::classify_delete_gate(pending, allow_deletes, interactive) {
        crate::cli::push::mdh::DeleteGate::Proceed => {}
        crate::cli::push::mdh::DeleteGate::Bail => {
            anyhow::bail!(
                "mdh/{slug}: {pending} row(s) present on the env are absent from {} and \
                 would be DELETED, but --allow-deletes was not passed. Re-run with \
                 --allow-deletes to authorise it, or restore the rows in that file to cancel.",
                data_path.display()
            );
        }
        crate::cli::push::mdh::DeleteGate::Prompt => {
            if !prompt_confirm_row_deletes(progress, collection_name, pending)? {
                deletes = &[];
                skipped = true;
                progress.event(
                    Action::Skip,
                    &format!(
                        "mdh/{slug} {pending} row deletion(s) skipped — the remaining \
                         replaces/inserts still run and may now conflict with a row that \
                         would otherwise have been deleted first"
                    ),
                );
            }
        }
    }

    let mut ops = 0usize;

    // Deletes FIRST: a row edit on a uniquely-indexed field needs the outgoing
    // row gone before the incoming row claims the key.
    for chunk in deletes.chunks(WRITE_CHUNK) {
        client
            .delete_many_by_ids(collection_name, chunk, Some(progress.clone()))
            .await
            .with_context(|| format!("deleting rows from '{collection_name}'"))?;
        progress.event(
            Action::Delete,
            &format!("mdh/{slug} {} row(s)", chunk.len()),
        );
        ops += 1;
    }

    for (id, replacement) in &diff.replace {
        let matched = client
            .replace_one(collection_name, id, replacement, Some(progress.clone()))
            .await
            .with_context(|| format!("replacing row {id} in '{collection_name}'"))?;
        if matched == 0 {
            progress.event(
                Action::Warn,
                &format!(
                    "mdh/{slug} row {id} vanished between read and write; will retry next sync"
                ),
            );
            skipped = true;
        } else {
            progress.event(Action::Patch, &format!("mdh/{slug} row {id}"));
            ops += 1;
        }
    }

    for chunk in diff.insert.chunks(WRITE_CHUNK) {
        client
            .insert_many(collection_name, chunk, Some(progress.clone()))
            .await
            .with_context(|| format!("inserting rows into '{collection_name}'"))?;
        progress.event(Action::Post, &format!("mdh/{slug} {} row(s)", chunk.len()));
        ops += 1;
    }

    if !skipped {
        lockfile.upsert(
            "mdh_data",
            slug,
            ObjectEntry {
                id: 0,
                modified_at: None,
                modified_by: None,
                content_hash: Some(raw_content_hash(&local_raw)),
                secrets_hash: None,
            },
        );
        crate::state::base_cache::write(paths, &data_path, &local_raw)
            .with_context(|| format!("writing base cache for mdh/{slug} rows"))?;
    }

    Ok(ops)
}

/// Interactive [y/N] confirmation for deleting remote rows absent from
/// `data.jsonl`. Byte-for-byte the shape of `prompt_confirm_index_drops`
/// (`push/mdh.rs:668`): `with_prompt` clears any active status line, and
/// `read_line_coordinated()` (no arguments, `io::Result<Option<String>>`) is the
/// single chokepoint that also rings the watch attention bell.
fn prompt_confirm_row_deletes(
    progress: &Arc<Log>,
    collection_name: &str,
    pending: usize,
) -> Result<bool> {
    progress.with_prompt(|| -> Result<bool> {
        use std::io::Write;
        eprintln!();
        eprintln!(
            "{pending} row(s) on '{collection_name}' are absent from data.jsonl and would \
             be DELETED."
        );
        eprint!("Proceed with the deletion(s)? [y/N] ");
        std::io::stderr().flush().ok();
        let ans = crate::cli::stdin_coord::read_line_coordinated()?
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(ans == "y" || ans == "yes")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn oid(hex: &str) -> Value {
        json!({ "$oid": hex })
    }

    use crate::api::DataStorageClient;
    use crate::state::Lockfile;
    use std::sync::Arc;

    fn log() -> Arc<crate::log::Log> {
        crate::log::Log::new(crate::cli::resolve::ColorMode::Plain)
    }

    /// Seed a manual dataset on disk: just the row data. `push_dataset_data`
    /// never reads `collection.json` itself — the manual-flag gate is the
    /// caller's job — so the fixture doesn't write one; doing so would imply
    /// a coupling that doesn't exist.
    fn seed(paths: &crate::paths::Paths, slug: &str, jsonl: &[u8]) {
        std::fs::create_dir_all(paths.dataset_dir(slug)).unwrap();
        std::fs::write(paths.dataset_data(slug), jsonl).unwrap();
    }

    /// An ABSENT data.jsonl means "not pulled yet", never "the env should have
    /// zero rows". Push must make no calls at all — the safety rule that keeps
    /// a hand-added flag from wiping a collection.
    #[tokio::test]
    async fn absent_data_file_is_never_authoritative() {
        let server = wiremock::MockServer::start().await;
        // No mocks mounted: any request would 404 and fail the call.
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.dataset_dir("gl-codes")).unwrap();
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 0, "an absent file must produce no writes");
        assert!(!lf.objects.contains_key("mdh_data"), "nothing to record");
    }

    /// Local rows the env lacks are inserted, and a fully-applied push advances
    /// both the lockfile hash and the base cache — the invariant that makes the
    /// next sync see the dataset as clean.
    #[tokio::test]
    async fn inserts_missing_rows_then_records_lockfile_and_base() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "inserted_ids": ["a"] } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let jsonl = b"{\"code\":\"1000\"}\n".to_vec();
        seed(&paths, "gl-codes", &jsonl);
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 1);
        assert_eq!(
            lf.objects["mdh_data"]["gl-codes"].content_hash.as_deref(),
            Some(raw_content_hash(&jsonl).as_str())
        );
        assert_eq!(
            crate::state::base_cache::read(&paths, &paths.dataset_data("gl-codes")).unwrap(),
            Some(jsonl),
            "base cache must move in lockstep with the lockfile"
        );
    }

    /// Deleting rows is destructive, so it rides the same gate index drops do:
    /// non-interactive without --allow-deletes must BAIL, having written nothing.
    #[tokio::test]
    async fn pending_deletes_bail_without_allow_deletes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "9999" }]
            })))
            .mount(&server)
            .await;
        // No delete mock: if a delete were attempted the test would fail.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let err = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--allow-deletes"), "must name the flag: {msg}");
        assert!(msg.contains("gl-codes"), "must name the dataset: {msg}");
        assert!(!lf.objects.contains_key("mdh_data"), "must not record a bailed push");
    }

    #[tokio::test]
    async fn allow_deletes_applies_deletes_before_inserts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "old" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/delete_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "deleted_count": 1 } }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "inserted_ids": ["b"] } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"code\":\"new\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, true, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 2, "one delete + one insert");
    }

    /// A keyed row edit is one in-place replace — no delete, no insert.
    #[tokio::test]
    async fn keyed_edit_issues_a_single_replace() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": "gl-1000", "label": "old" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/replace_one"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "matched_count": 1 } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"_id\":\"gl-1000\",\"label\":\"new\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 1);
    }

    /// A replace that matched nothing means the row vanished between our read
    /// and our write. That must NOT count as an applied op and must NOT advance
    /// the lockfile or base cache, so the next sync re-reads and re-diffs.
    #[tokio::test]
    async fn replace_with_zero_matched_count_is_not_recorded() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": "gl-1000", "label": "old" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/replace_one"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "matched_count": 0 } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"_id\":\"gl-1000\",\"label\":\"new\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 0, "a vanished-row replace must not count as an applied op");
        assert!(
            !lf.objects.contains_key("mdh_data"),
            "must not record a partially-applied push"
        );
        assert_eq!(
            crate::state::base_cache::read(&paths, &paths.dataset_data("gl-codes")).unwrap(),
            None,
            "base cache must not advance either"
        );
    }

    /// Nothing to do → no writes, but the lockfile/base still get recorded so a
    /// first sync of an already-matching dataset converges instead of retrying.
    #[tokio::test]
    async fn no_op_push_records_state_without_writing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "1000" }]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"code\":\"1000\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 0);
        assert!(lf.objects["mdh_data"].contains_key("gl-codes"));
    }

    /// The hard ceiling protects git from an import-fed table: refuse before any
    /// write, naming the count and the way out.
    #[tokio::test]
    async fn local_rows_over_the_hard_limit_are_refused() {
        let server = wiremock::MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let mut jsonl = Vec::new();
        for i in 0..(crate::snapshot::mdh_data::ROW_HARD_LIMIT + 1) {
            jsonl.extend_from_slice(format!("{{\"code\":\"{i:06}\"}}\n").as_bytes());
        }
        seed(&paths, "gl-codes", &jsonl);
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let err = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        let over = (crate::snapshot::mdh_data::ROW_HARD_LIMIT + 1).to_string();
        assert!(msg.contains(&over), "must state the count: {msg}");
        assert!(
            msg.contains("collection.json"),
            "must point at the opt-out: {msg}"
        );
    }

    #[test]
    fn identical_sides_produce_no_operations() {
        let local = vec![json!({ "code": "1000" }), json!({ "_id": "gl-2", "code": "2000" })];
        let remote = vec![
            json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" }),
            json!({ "_id": "gl-2", "code": "2000" }),
        ];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    /// Key order and nesting must not register as a change — both sides are
    /// reduced to the same canonical form first.
    #[test]
    fn key_order_difference_is_not_a_change() {
        let local = vec![json!({ "a": 1, "b": { "d": 4, "c": 3 } })];
        let remote = vec![json!({ "_id": oid("aa"), "b": { "c": 3, "d": 4 }, "a": 1 })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn keyed_row_edit_becomes_an_in_place_replace_without_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert!(d.insert.is_empty() && d.delete.is_empty());
        assert_eq!(
            d.replace,
            vec![(json!("gl-1000"), json!({ "code": "1000", "label": "new" }))],
            "identity is carried by the filter; the replacement must omit _id"
        );
    }

    #[test]
    fn keyed_row_missing_remotely_is_inserted_with_its_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&local, &[]);
        assert_eq!(d.insert, vec![json!({ "_id": "gl-1000", "code": "1000" })]);
        assert!(d.replace.is_empty() && d.delete.is_empty());
    }

    #[test]
    fn keyed_row_missing_locally_is_deleted_by_its_raw_id() {
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&[], &remote);
        assert_eq!(d.delete, vec![json!("gl-1000")]);
        assert!(d.insert.is_empty() && d.replace.is_empty());
    }

    #[test]
    fn unkeyed_row_edit_becomes_a_delete_plus_an_insert() {
        // Without a business key there is nothing to pair old and new by, so an
        // edit is expressed as removing the old row and adding the new one.
        let local = vec![json!({ "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert_eq!(d.insert, vec![json!({ "code": "1000", "label": "new" })]);
        assert_eq!(d.delete, vec![oid("6a8403a6070b60eaa348d173")]);
        assert!(d.replace.is_empty());
    }

    /// Unkeyed rows are a MULTISET: two identical rows are two rows. The diff
    /// moves only the surplus, in either direction.
    #[test]
    fn unkeyed_duplicates_are_diffed_by_count() {
        let row = json!({ "code": "1000" });
        // local wants 3, remote has 1 → insert 2.
        let d = diff_rows(
            &[row.clone(), row.clone(), row.clone()],
            &[json!({ "_id": oid("a1"), "code": "1000" })],
        );
        assert_eq!(d.insert, vec![row.clone(), row.clone()]);
        assert!(d.delete.is_empty());

        // local wants 1, remote has 3 → delete 2 (the surplus, deterministically).
        let d = diff_rows(
            std::slice::from_ref(&row),
            &[
                json!({ "_id": oid("a1"), "code": "1000" }),
                json!({ "_id": oid("a2"), "code": "1000" }),
                json!({ "_id": oid("a3"), "code": "1000" }),
            ],
        );
        assert!(d.insert.is_empty());
        assert_eq!(d.delete, vec![oid("a2"), oid("a3")]);
    }

    /// A hand-written `$oid` `_id` on the local side is NOT a business key: it
    /// is dropped by canonicalization, so the row diffs as unkeyed content and
    /// does not churn against the identical remote row.
    #[test]
    fn hand_written_server_id_locally_is_not_treated_as_a_key() {
        let local = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let a = json!({ "code": "1000" });
        let b = json!({ "code": "2000" });
        let one = diff_rows(&[a.clone(), b.clone()], &[]);
        let two = diff_rows(&[b.clone(), a.clone()], &[]);
        assert_eq!(one, two, "op order must not depend on input order");
    }
}
