//! Crash recovery, and a host-kept state over Data Storage, against a real org.
//!
//! Four scenarios, each with its fake twin:
//!
//! - `data_storage_facts`: what a host store over Data Storage relies on —
//!   the test-only one in `support::ds_state`, which these scenarios use, and
//!   the fake's model of it (`fake::quirks`) — about `data/replace_one`,
//!   `data/insert_many` and `data/find`, probed in a collection under a
//!   run-scoped key. rdc itself no longer talks to Data Storage for the
//!   state (2026-11-12): the host does.
//! - `create_identity`: every create of a whole hand-written graph can be
//!   found again from its journal entry alone, by the matcher the retry
//!   guard and recovery share (`RossumClient::find_create_match`). A field
//!   the server rewrites on create would make that matcher miss, and a lost
//!   create would then be sent twice.
//! - `crash_recovery`: a chain of syncs, each killed right after one create
//!   was applied, then one that completes. The env converges with one object
//!   per name.
//! - `data_storage_state_crash`: the same kill with the state kept by a
//!   test-only host store over Data Storage (`support::ds_state`) and a fresh
//!   in-memory tree per run, as a reloaded browser tab.
//!
//! # How a run is killed
//!
//! The run is `rdc`'s own sync in this process, so dropping its future stops
//! it exactly where a killed process stops. The drop happens at the
//! checkpoint that would make the create's `finish` (its server id) durable,
//! and that checkpoint is first undone ([`CrashStore`]): what the run leaves
//! is the create applied on the server and, in the state, the journal op with
//! no id. That is the "after apply, before response" row of
//! `support::fake::crash`'s table, reached from the client side. The real
//! server cannot be made to hold a response, and a proxy that swallows one
//! would also have to rewrite every URL in both directions; the checkpoint
//! needs neither and no production hook, and it never races the server.

use crate::support::assert_local::load_lockfile;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::assert_converged;
use crate::support::crash::{finished, sync_in_process, until_crash, yes, CrashStore};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use crate::support::teardown::Teardown;
use crate::support::ds_state::{DsBackend, COLLECTION};
use rdc::api::RossumClient;
use rdc::state::snapshot_store::SnapshotStateStore;
use rdc::state::store::StateStore;
use rdc::vfs::{MemStore, Store};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ------------------------------------------------------------------ facts

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_data_storage_facts() {
    let fake = crate::support::fake::FakeOrg::start().await;
    fake.enable_data_storage();
    data_storage_facts(&fake.config()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_data_storage_facts() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    data_storage_facts(&cfg).await;
}

/// The Data Storage facts a host store relies on. Every document is under
/// this run's key in the test collection, and is deleted again.
async fn data_storage_facts(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let key = format!("{}facts", run_id.list_prefix());
    let ds = DsBackend::new(&cfg.api_base, &cfg.token, COLLECTION, &key);
    let (prod, dev, size) = (format!("{key}/prod"), format!("{key}/dev"), format!("{key}/size"));
    let ok = |r: std::result::Result<serde_json::Value, (u16, String)>, what: &str| r.unwrap_or_else(|e| panic!("{what}: {e:?}"));
    let replace = |filter: serde_json::Value, doc: serde_json::Value, upsert: bool| {
        json!({ "filter": filter, "replacement": doc, "options": { "upsert": upsert }, "waitForFullWrite": true })
    };
    let find = |query: serde_json::Value| json!({ "query": query });

    // Field names may hold dots and slashes.
    let v1 = json!({ ".rdc/state/prod.lock.json": "a1", ".rdc/state/prod.base/labels/x.json": "b1" });
    let v2 = json!({ ".rdc/state/prod.lock.json": "a2" });

    // The token writes Data Storage, and a write creates the collection if it
    // is not there yet. An upsert says what it inserted.
    let up = ok(ds.post("data/replace_one", replace(json!({ "_id": prod }), json!({ "rev": 1, "files": v1 }), true)).await.unwrap(), "upsert");
    assert_eq!((up["matched_count"].as_u64(), up["upserted_id"].clone()), (Some(0), json!(prod)));

    // A compare-and-swap on `{_id, rev}` reports a match, then a miss.
    let hit = ok(ds.post("data/replace_one", replace(json!({ "_id": prod, "rev": 1 }), json!({ "rev": 2, "files": v2 }), false)).await.unwrap(), "cas");
    assert_eq!((hit["matched_count"].as_u64(), hit["modified_count"].as_u64()), (Some(1), Some(1)));
    let miss = ok(ds.post("data/replace_one", replace(json!({ "_id": prod, "rev": 1 }), json!({ "rev": 3 }), false)).await.unwrap(), "stale cas");
    assert_eq!(miss["matched_count"].as_u64(), Some(0));

    // Dotted field names come back intact from `replace_one`, and from
    // `insert_many`.
    let got = ok(ds.post("data/find", find(json!({ "_id": prod }))).await.unwrap(), "find");
    assert_eq!(got, json!([{ "_id": prod, "rev": 2, "files": v2 }]));
    ok(ds.post("data/insert_many", json!({ "documents": [{ "_id": dev, "rev": 1, "files": v1 }], "ordered": false })).await.unwrap(), "insert");
    let got = ok(ds.post("data/find", find(json!({ "_id": dev }))).await.unwrap(), "find");
    assert_eq!(got, json!([{ "_id": dev, "rev": 1, "files": v1 }]));

    // A duplicate `_id` on `insert_many` is HTTP 400 "batch op errors
    // occurred", and the existing document stays as it was.
    let err = ds.post("data/insert_many", json!({ "documents": [{ "_id": prod, "rev": 9 }], "ordered": false })).await.unwrap().unwrap_err();
    assert!(err.0 == 400 && err.1.contains("batch op errors occurred"), "{err:?}");
    let got = ok(ds.post("data/find", find(json!({ "_id": prod }))).await.unwrap(), "find");
    assert_eq!(got[0]["rev"], 2, "the duplicate insert changed nothing");

    // A `$regex` prefix on `_id` with an `_id`-only projection finds exactly
    // this key's documents, as the clean-up reads them.
    assert_eq!(ds.stored_ids().await.unwrap(), vec![dev.clone(), prod.clone()]);

    // The collection is listed like any other (its `__` prefix hides it from
    // the sync's MDH mirror, not from the API).
    let base = rdc::config::EnvConfig { api_base: cfg.api_base.clone(), org_id: cfg.org_id }.data_storage_base();
    let client = rdc::api::DataStorageClient::new(base, cfg.token.clone()).expect("a Data Storage client");
    let names: Vec<String> = client.list_collections(None).await.expect("list collections").into_iter().map(|c| c.name).collect();
    assert!(names.iter().any(|n| n == COLLECTION), "{COLLECTION} is listed: {names:?}");

    // Document size: 2 MiB fits; 16 MiB of text does not.
    let fits = "a".repeat(2 * 1024 * 1024);
    ok(ds.post("data/replace_one", replace(json!({ "_id": size }), json!({ "text": fits }), true)).await.unwrap(), "2 MiB");
    let too_big = "a".repeat(16 * 1024 * 1024);
    let err = ds.post("data/replace_one", replace(json!({ "_id": size }), json!({ "text": too_big }), true)).await.unwrap().unwrap_err();
    assert!(err.1.contains("larger than 16777216"), "{err:?}");

    assert_eq!(ds.delete_all().await.unwrap(), 3, "the three documents are deleted");
    assert!(ds.stored_ids().await.unwrap().is_empty());
}

// -------------------------------------------------------- shared fixture

/// The hand-written graph (`testdata/live/snapshot`), with the queue NOT
/// bound to the engine so teardown can delete the engine in the same run,
/// and a secret for the `validator` hook.
fn graph_project(cfg: &LiveConfig, run_id: &RunId, org_url: &str) -> ProjectFixture {
    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    write_snapshot(&project, "test", run_id, org_url);
    let prefix = run_id.list_prefix();
    let queue = format!("envs/test/workspaces/{prefix}ws/queues/{prefix}invoices/queue.json");
    let mut body = project.read_json(&queue);
    body.as_object_mut().expect("a queue object").remove("engine");
    project.write_json(&queue, &body);
    std::fs::write(
        project.path().join("secrets/test.hook-secrets.json"),
        format!("{{\"hooks\": {{\"{prefix}validator\": {{\"api_key\": \"s3cret\"}}}}}}\n"),
    )
    .expect("writing the hook secret");
    project
}

/// Lockfile kind → `LiveClient` kind, for every kind the graph creates.
const KINDS: &[(&str, &str)] = &[
    ("workspaces", "workspace"),
    ("schemas", "schema"),
    ("queues", "queue"),
    ("inboxes", "inbox"),
    ("hooks", "hook"),
    ("labels", "label"),
    ("rules", "rule"),
    ("email_templates", "email_template"),
    ("engines", "engine"),
    ("engine_fields", "engine_field"),
    ("saved_views", "saved_view"),
];

/// Every object of this run on the env carries a name no other object of its
/// kind has, and is the one the lockfile records under some slug.
async fn assert_one_object_per_name(client: &LiveClient, project: &Path, prefix: &str, ctx: &str) {
    let lf = load_lockfile(project, "test").expect("lockfile");
    assert_eq!(lf.pending, None, "{ctx}: the journal is settled");
    for (kind, singular) in KINDS {
        let found = client.list_ids_by_name_prefix(singular, prefix).await.expect("list by prefix");
        let mut by_name: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for (id, name) in &found {
            by_name.entry(name).or_default().push(*id);
        }
        let dups: Vec<_> = by_name.iter().filter(|(_, ids)| ids.len() > 1).collect();
        assert!(dups.is_empty(), "{ctx}: {kind} created twice: {dups:?}");
        let recorded: HashSet<u64> = lf.objects.get(*kind).map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default();
        for (id, name) in &found {
            assert!(recorded.contains(id), "{ctx}: {kind} {id} ({name}) is on the env but recorded under no slug");
        }
    }
}

// -------------------------------------------------------- create identity

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_create_identity() {
    let fake = crate::support::fake::FakeOrg::start().await;
    create_identity(&fake.config()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_create_identity() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    create_identity(&cfg).await;
}

/// One sync creates the whole graph; then each create's journal entry, as
/// persisted before its POST, is matched against the env by
/// `find_create_match` and must find exactly the object that create made.
/// Before asserting, every top-level field the server returns differently
/// from the create body is printed: such a field is ignored only when the
/// listing omits it, and makes a create unrecoverable otherwise.
async fn create_identity(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).expect("connect (teardown)"), run_id.clone());
    let prefix = run_id.list_prefix();
    let project = graph_project(cfg, &run_id, &client.org_url);

    let observer = CrashStore::new(|_| false, false);
    sync_in_process(project.path(), &cfg.token, yes(false), observer.clone()).await.expect("the graph sync");

    // Each op as last persisted: its fields and, after its finish, its id.
    let mut ops: BTreeMap<String, rdc::state::journal::CreateIntent> = BTreeMap::new();
    for bytes in observer.seen() {
        let lf: rdc::state::Lockfile = serde_json::from_slice(&bytes).expect("a checkpointed lockfile");
        for op in lf.pending.into_iter().flat_map(|p| p.ops) {
            if op.id.is_some() {
                ops.insert(format!("{}/{}", op.kind, op.slug), op);
            }
        }
    }
    let kinds: HashSet<&str> = ops.values().map(|op| op.kind.as_str()).collect();
    for kind in ["workspaces", "schemas", "queues", "inboxes", "hooks", "labels", "rules", "email_templates", "engines", "engine_fields", "saved_views"] {
        assert!(kinds.contains(kind), "the graph sync journaled no {kind} create: {:?}", ops.keys());
    }

    let rossum = RossumClient::new(cfg.api_base.clone(), cfg.token.clone()).expect("a client");
    let mut misses = Vec::new();
    // Per kind, the top-level fields the server did not store as sent.
    let mut rewritten_by_kind: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (what, op) in &ops {
        let id = op.id.expect("finished");
        let remote = rossum.get_created(&op.kind, id, None).await.expect("GET the created object").expect("it exists");
        let theirs = rdc::state::journal::create_fields(&op.kind, &remote);
        // A field journaled whole is rewritten when its hash differs; a
        // server-filled one (journaled per sub-key) when the server added a
        // sub-key or changed one the body sent.
        let mut rewritten: std::collections::BTreeSet<String> = op
            .fields
            .iter()
            .filter(|(k, h)| theirs.get(*k).is_some_and(|t| t != *h) || (k.contains('.') && !theirs.contains_key(*k)))
            .map(|(k, _)| k.split('.').next().unwrap_or(k).to_string())
            .collect();
        for field in rdc::state::journal::server_filled_fields(&op.kind) {
            let sub = format!("{field}.");
            if theirs.keys().any(|k| k.starts_with(&sub) && !op.fields.contains_key(k)) {
                rewritten.insert(field.to_string());
            }
        }
        let sent_changed: Vec<&str> = op
            .fields
            .iter()
            .filter(|(k, h)| k.contains('.') && theirs.get(*k) != Some(*h))
            .map(|(k, _)| k.as_str())
            .collect();
        eprintln!("create_identity: {what}: rewritten {rewritten:?}, sent sub-keys not echoed {sent_changed:?}");
        for k in &rewritten {
            if k != "content" {
                eprintln!("create_identity:   {k} as returned: {}", remote[k.as_str()]);
            }
        }
        // A schema's `content` comes back with per-datapoint defaults on a
        // real org (observed, not modelled by the fake); the listing the
        // matcher reads omits `content`, so it never takes part.
        if op.kind != "schemas" {
            rewritten_by_kind.entry(op.kind.clone()).or_default().extend(rewritten.iter().cloned());
        }
        let found = rossum.find_create_match(&op.kind, &op.fields, &HashSet::new(), None).await.expect("match");
        let ids: Vec<u64> = found.iter().filter_map(|v| v["id"].as_u64()).collect();
        if ids != [id] {
            misses.push(format!("{what} (id {id}) matched {ids:?}; rewritten fields {rewritten:?}"));
        }
    }
    rewritten_by_kind.retain(|_, fields| !fields.is_empty());
    let expected: BTreeMap<String, std::collections::BTreeSet<String>> = [("engines", "settings"), ("hooks", "config")]
        .into_iter()
        .map(|(k, f)| (k.to_string(), std::collections::BTreeSet::from([f.to_string()])))
        .collect();
    assert_eq!(rewritten_by_kind, expected, "the fields a create's answer does not echo as sent, per kind");
    assert!(misses.is_empty(), "creates the matcher cannot find again:\n{}", misses.join("\n"));

    assert_converged(&project, "test", &prefix, "after creating the graph");
    drop(teardown);
}

// -------------------------------------------------------- crash recovery

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_crash_recovery() {
    let fake = crate::support::fake::FakeOrg::start().await;
    crash_recovery(&fake.config()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_crash_recovery() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    crash_recovery(&cfg).await;
}

/// Pause for a dropped run's last requests, as a killed process's last
/// bytes may still land.
async fn after_crash() {
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

/// A chain of syncs on one tree, in push order. Each recovers the create the
/// one before it lost, then is itself killed right after the next create
/// was applied: a schema, its queue, the queue's inbox, a hook with a
/// secret, a label and a rule that uses it. A last sync completes. The env
/// then converges and holds one object per name.
async fn crash_recovery(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).expect("connect (teardown)"), run_id.clone());
    let prefix = run_id.list_prefix();
    let project = graph_project(cfg, &run_id, &client.org_url);

    let chain = [
        ("schemas", "invoices"),
        ("queues", "invoices"),
        ("inboxes", "invoices"),
        ("hooks", "validator"),
        ("labels", "priority"),
        ("rules", "totals"),
    ];
    for (kind, name) in chain {
        let name = format!("{prefix}{name}");
        let store = CrashStore::new(finished(kind, &name), true);
        until_crash(sync_in_process(project.path(), &cfg.token, yes(false), store.clone()), &store.fired).await;
        let lf = load_lockfile(project.path(), "test").expect("lockfile");
        let open: Vec<_> = lf.pending.iter().flat_map(|p| p.ops.iter()).filter(|op| op.id.is_none()).map(|op| op.kind.as_str()).collect();
        assert!(open.contains(&kind), "the {kind} create is journaled without its id: {open:?}");
        after_crash().await;
    }

    let fs: Arc<dyn StateStore> = Arc::new(rdc::state::store::FsStateStore);
    sync_in_process(project.path(), &cfg.token, yes(false), fs).await.expect("the completing sync");

    assert_converged(&project, "test", &prefix, "after the interrupted chain");
    assert_one_object_per_name(&client, project.path(), &prefix, "after the interrupted chain").await;
    let secrets = project.read_json("secrets/test.hook-secrets.json");
    assert_eq!(secrets["hooks"][format!("{prefix}validator")]["api_key"], "s3cret", "the secret stays: {secrets}");
    drop(teardown);
}

// ------------------------------------------ crash with state in Data Storage

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_data_storage_state_crash() {
    let fake = crate::support::fake::FakeOrg::start().await;
    fake.enable_data_storage();
    data_storage_state_crash(&fake.config()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_data_storage_state_crash() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    data_storage_state_crash(&cfg).await;
}

const STATE_PREFIXES: &[&str] = &[".rdc/state/test.lock.json", ".rdc/state/test.base/", ".rdc/conflicts/test/"];

/// `project`'s files in memory, rooted where nothing exists on disk, with no
/// env state: a freshly loaded tab.
fn tab_of(files: Vec<(String, Vec<u8>)>, root: &Path) -> Arc<MemStore> {
    let files = files.into_iter().filter(|(k, _)| !STATE_PREFIXES.iter().any(|p| k.starts_with(p)));
    Arc::new(MemStore::from_files(root.to_path_buf(), files).expect("an in-memory tree"))
}

fn disk_files(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("file type").is_dir() {
            disk_files(root, &entry.path(), out);
        } else {
            let rel = entry.path().strip_prefix(root).expect("under root").to_string_lossy().replace('\\', "/");
            out.push((rel, std::fs::read(entry.path()).expect("read")));
        }
    }
}

/// One sync of `tab` with `state`, in this process.
async fn sync_tab(
    tab: &Arc<MemStore>,
    root: &Path,
    token: &str,
    state: Arc<dyn StateStore>,
) -> anyhow::Result<rdc::cli::sync::CycleOutcome> {
    rdc::cli::sync::embed::sync_logged_in_with_state(tab.clone() as Arc<dyn Store>, state, root, "test", token, yes(false), Box::new(std::io::sink())).await
}

/// Two new labels, synced from an in-memory tree with the state in the
/// state collection under this run's key. Run 1 is killed right after the
/// second label's create was applied; runs 2 and 3 each start from a fresh
/// tree holding no state, which their hydrate brings back from Data
/// Storage. Run 2 recovers the lost create; run 3 has nothing to do. The
/// state documents are deleted afterwards.
async fn data_storage_state_crash(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).expect("connect (teardown)"), run_id.clone());
    let prefix = run_id.list_prefix();
    let key = format!("{prefix}state");
    let ds = DsBackend::new(&cfg.api_base, &cfg.token, COLLECTION, &key);
    let store = || -> Arc<dyn StateStore> {
        Arc::new(SnapshotStateStore::new(Arc::new(DsBackend::new(&cfg.api_base, &cfg.token, COLLECTION, &key))))
    };

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let names = [format!("{prefix}first"), format!("{prefix}second")];
    for name in &names {
        project.write_json(
            &format!("envs/test/labels/{name}.json"),
            &json!({ "id": 0, "url": "", "name": name, "organization": client.org_url, "color": "#ff8800" }),
        );
    }
    let root: PathBuf = project.path().join("in-memory-root");
    let mut files = Vec::new();
    disk_files(project.path(), project.path(), &mut files);

    let first = tab_of(files, &root);
    let crash = CrashStore::wrapping(store(), finished("labels", &names[1]), false);
    until_crash(sync_tab(&first, &root, &cfg.token, crash.clone()), &crash.fired).await;
    after_crash().await;
    assert!(!ds.stored_ids().await.unwrap().is_empty(), "run 1 published state");

    let second = tab_of(first.files().into_iter().collect(), &root);
    sync_tab(&second, &root, &cfg.token, store()).await.expect("run 2");
    let third = tab_of(second.files().into_iter().collect(), &root);
    let out = sync_tab(&third, &root, &cfg.token, store()).await.expect("run 3");
    assert_eq!((out.items_pushed, out.conflicts), (0, 0), "run 3 found work");
    let ours = |tab: &MemStore| -> BTreeMap<String, Vec<u8>> {
        tab.files().into_iter().filter(|(k, _)| k.contains(&prefix)).collect()
    };
    assert_eq!(ours(&second), ours(&third), "run 3 started from run 2's state and moved none of this run's files");
    assert!(!root.exists(), "a run wrote to the disk");

    let lf: rdc::state::Lockfile =
        serde_json::from_slice(&third.files()[".rdc/state/test.lock.json"]).expect("run 3's lockfile");
    assert_eq!(lf.pending, None, "the journal is settled");
    let found = client.list_ids_by_name_prefix("label", &prefix).await.expect("list labels");
    let recorded: HashSet<u64> = lf.objects.get("labels").map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default();
    for name in &names {
        let ids: Vec<u64> = found.iter().filter(|(_, n)| n == name).map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 1, "one label named {name}: {found:?}");
        assert!(recorded.contains(&ids[0]), "{name} is recorded");
    }

    assert!(ds.delete_all().await.unwrap() > 0, "the state documents are deleted");
    assert!(ds.stored_ids().await.unwrap().is_empty(), "no state document is left");
    drop(teardown);
}
