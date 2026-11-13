//! `SnapshotStateStore` over a host store kept in the fake's Data Storage
//! (`support::ds_state`): an env's sync state (lockfile, base cache, conflict
//! shadows) and its tree survive a page that is gone, with one `load` (one
//! `find`) per hydrate and one `save` (one write) per changed checkpoint; a
//! second writer is refused, and a crash before the write applies leaves the
//! previous state whole.

use super::crash::until_crash;
use super::*;
use crate::support::ds_state::{DsBackend, COLLECTION};
use rdc::paths::Paths;
use rdc::state::snapshot_store::SnapshotStateStore;
use rdc::state::store::StateStore;
use rdc::vfs::{MemStore, Store};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const KEY: &str = "acme/main";
const LOCKFILE: &str = ".rdc/state/test.lock.json";
const BASE: &str = ".rdc/state/test.base/labels/priority.json";
const SHADOW: &str = ".rdc/conflicts/test/labels/priority.json";

fn store(fake: &FakeOrg) -> SnapshotStateStore {
    SnapshotStateStore::new(Arc::new(DsBackend::new(&fake.api_base(), "fake-token", COLLECTION, KEY)))
}

fn paths() -> Paths {
    Paths::for_env(Path::new("/p"), "test")
}

/// A tab's in-memory tree holding `files` (relpath → text).
fn tab(files: &[(&str, &str)]) -> Arc<MemStore> {
    let files: Vec<(String, Vec<u8>)> = files.iter().map(|(p, t)| (p.to_string(), t.as_bytes().to_vec())).collect();
    Arc::new(MemStore::from_files("/p".into(), files).unwrap())
}

async fn hydrate(state: &SnapshotStateStore, tab: &Arc<MemStore>) -> anyhow::Result<()> {
    let p = paths();
    rdc::vfs::scope(tab.clone() as Arc<dyn Store>, state.hydrate(&p)).await
}

async fn checkpoint(state: &SnapshotStateStore, tab: &Arc<MemStore>) -> anyhow::Result<()> {
    let p = paths();
    rdc::vfs::scope(tab.clone() as Arc<dyn Store>, state.checkpoint(&p)).await
}

fn write(tab: &Arc<MemStore>, rel: &str, bytes: &[u8]) {
    let path = Path::new("/p").join(rel);
    rdc::vfs::scope_sync(tab.clone() as Arc<dyn Store>, || {
        rdc::vfs::create_dir_all(path.parent().unwrap()).unwrap();
        rdc::vfs::write(&path, bytes).unwrap()
    });
}

fn text_files(tab: &MemStore) -> BTreeMap<String, String> {
    tab.files().into_iter().map(|(k, v)| (k, String::from_utf8(v).unwrap())).collect()
}

fn manifest(fake: &FakeOrg) -> Option<Value> {
    fake.state().data_storage.as_ref().unwrap().doc(COLLECTION, &json!(format!("{KEY}/test")))
}

/// The files the stored document holds: its `snapshot` decoded.
fn stored_files(fake: &FakeOrg) -> BTreeMap<String, String> {
    super::data_storage::snapshot_files(&manifest(fake).expect("a stored document"))
}

/// `files` as a stored snapshot, encoded as the store does.
fn snapshot_of(files: &[(&str, &str)]) -> String {
    use std::io::Write;
    let contents: Vec<&str> = files.iter().map(|(_, t)| *t).collect();
    let index: serde_json::Map<String, Value> = files.iter().enumerate().map(|(i, (p, _))| (p.to_string(), json!(i))).collect();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&serde_json::to_vec(&json!({ "files": index, "contents": contents })).unwrap()).unwrap();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, gz.finish().unwrap())
}

/// Data Storage requests of `verb` to the state collection so far.
fn calls(fake: &FakeOrg, verb: &str) -> usize {
    fake.state().data_storage.as_ref().unwrap().calls.get(&(verb.to_string(), COLLECTION.to_string())).copied().unwrap_or(0)
}

async fn ds_fake() -> FakeOrg {
    let fake = FakeOrg::start().await;
    fake.enable_data_storage();
    fake
}

#[tokio::test]
async fn hydrate_then_checkpoint_round_trips() {
    let fake = ds_fake().await;
    let first = tab(&[
        (LOCKFILE, "{\"version\": 4}\n"),
        (BASE, "{\"name\": \"priority\"}\n"),
        (SHADOW, "{\"name\": \"théirs ✓\"}\n"),
        ("envs/test/labels/priority.json", "{}\n"),
        ("envs/test/overlay.toml", "version = 1\n# edited\n"),
        (".rdc/state/test.lock", ""),
        ("rdc.toml", "[envs.test]\n"),
    ]);
    let state = store(&fake);
    hydrate(&state, &first).await.unwrap();
    assert_eq!(manifest(&fake), None, "nothing is published by a hydrate");
    checkpoint(&state, &first).await.unwrap();

    // A reloaded tab: only what the host keeps, a scaffolded overlay, and
    // stale files the manifest lacks.
    let second = tab(&[
        ("rdc.toml", "[envs.test]\n"),
        ("envs/test/overlay.toml", "version = 1\n"),
        ("envs/test/labels/stale.json", "{}\n"),
        (".rdc/conflicts/test/labels/stale.json", "{}\n"),
        (".rdc/conflicts/dev/labels/other_env.json", "{}\n"),
    ]);
    hydrate(&store(&fake), &second).await.unwrap();

    let got = text_files(&second);
    let want: BTreeMap<String, String> = [
        (LOCKFILE, "{\"version\": 4}\n"),
        (BASE, "{\"name\": \"priority\"}\n"),
        (SHADOW, "{\"name\": \"théirs ✓\"}\n"),
        ("envs/test/labels/priority.json", "{}\n"),
        ("envs/test/overlay.toml", "version = 1\n# edited\n"),
        ("rdc.toml", "[envs.test]\n"),
        (".rdc/conflicts/dev/labels/other_env.json", "{}\n"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(got, want, "the state and the env tree are the manifest's; the env lock, rdc.toml and other envs are not kept");
    let m = manifest(&fake).unwrap();
    assert_eq!(m["rev"], 1);
    assert_eq!(stored_files(&fake).len(), 5);
    assert_eq!(fake.state().data_storage.as_ref().unwrap().docs(COLLECTION).len(), 1, "one document per env");
}

/// Later runs of the same store keep what the host changed in the env tree
/// since: it is a local edit, as on disk. The state stays the manifest's.
#[tokio::test]
async fn a_later_hydrate_keeps_the_hosts_edits() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n"), ("envs/test/labels/a.json", "a1\n"), ("envs/test/labels/b.json", "b1\n")]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();

    write(&page, "envs/test/labels/a.json", b"a2 by the host\n");
    write(&page, "envs/test/labels/c.json", b"new\n");
    rdc::vfs::scope_sync(page.clone() as Arc<dyn Store>, || rdc::vfs::remove_file(Path::new("/p/envs/test/labels/b.json")).unwrap());
    write(&page, LOCKFILE, b"tampered\n");
    let finds_before = calls(&fake, "data/find");
    hydrate(&state, &page).await.unwrap();

    let got = text_files(&page);
    assert_eq!(got["envs/test/labels/a.json"], "a2 by the host\n");
    assert_eq!(got["envs/test/labels/c.json"], "new\n");
    assert!(!got.contains_key("envs/test/labels/b.json"), "a local delete stays a local delete");
    assert_eq!(got[LOCKFILE], "v1\n", "the state is the manifest's");
    assert_eq!(calls(&fake, "data/find") - finds_before, 1, "one read");
}

/// Two tabs on one env: what one publishes reaches the other, merged with
/// the other's own edits; a file both changed refuses the run, writing nothing.
#[tokio::test]
async fn another_tabs_publish_is_merged_three_ways() {
    let fake = ds_fake().await;
    let seed = [(LOCKFILE, "v1\n"), ("envs/test/labels/a.json", "a1\n"), ("envs/test/labels/b.json", "b1\n"), ("envs/test/labels/c.json", "c1\n")];
    let (a, b) = (tab(&seed), tab(&[]));
    let (sa, sb) = (store(&fake), store(&fake));
    hydrate(&sa, &a).await.unwrap();
    checkpoint(&sa, &a).await.unwrap();
    hydrate(&sb, &b).await.unwrap();
    assert_eq!(b.files(), a.files(), "a new tab gets the published tree");

    // Tab a edits `a` and deletes `c`, and publishes; tab b edits `b`.
    write(&a, "envs/test/labels/a.json", b"a2\n");
    rdc::vfs::scope_sync(a.clone() as Arc<dyn Store>, || rdc::vfs::remove_file(Path::new("/p/envs/test/labels/c.json")).unwrap());
    write(&a, LOCKFILE, b"v2\n");
    checkpoint(&sa, &a).await.unwrap();
    write(&b, "envs/test/labels/b.json", b"b2\n");
    hydrate(&sb, &b).await.unwrap();

    let got = text_files(&b);
    assert_eq!(got["envs/test/labels/a.json"], "a2\n", "only the other tab changed it");
    assert_eq!(got["envs/test/labels/b.json"], "b2\n", "only this tab changed it");
    assert!(!got.contains_key("envs/test/labels/c.json"), "the other tab deleted it");
    assert_eq!(got[LOCKFILE], "v2\n");

    // Both change `a` now.
    write(&b, "envs/test/labels/a.json", b"a3 by b\n");
    checkpoint(&sb, &b).await.unwrap();
    write(&a, "envs/test/labels/a.json", b"a3 by a\n");
    let before = a.files();
    let err = format!("{:#}", hydrate(&sa, &a).await.unwrap_err());
    assert!(err.contains("envs/test/labels/a.json") && err.contains("changed here and by another run"), "{err}");
    assert_eq!(a.files(), before, "a refused hydrate writes nothing");
}


#[tokio::test]
async fn hydrate_without_a_manifest_keeps_local_state() {
    let fake = ds_fake().await;
    let local = tab(&[(LOCKFILE, "{}\n")]);
    hydrate(&store(&fake), &local).await.unwrap();
    assert_eq!(text_files(&local)[LOCKFILE], "{}\n");
}


#[tokio::test]
async fn second_writer_loses_the_cas() {
    let fake = ds_fake().await;
    let seed = tab(&[(LOCKFILE, "v1\n")]);
    let s0 = store(&fake);
    hydrate(&s0, &seed).await.unwrap();
    checkpoint(&s0, &seed).await.unwrap();

    let (a, b) = (tab(&[]), tab(&[]));
    let (sa, sb) = (store(&fake), store(&fake));
    hydrate(&sa, &a).await.unwrap();
    hydrate(&sb, &b).await.unwrap();
    write(&a, LOCKFILE, b"from a\n");
    write(&b, LOCKFILE, b"from b\n");
    checkpoint(&sa, &a).await.unwrap();
    let won = manifest(&fake).unwrap();
    assert_eq!(won["rev"], 2);

    let err = checkpoint(&sb, &b).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("the state of env 'test' was changed by another run"),
        "{err:#}"
    );
    assert_eq!(manifest(&fake).unwrap(), won, "the loser changed nothing");
    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "from a\n");
}

#[tokio::test]
async fn first_manifest_race_has_one_winner() {
    let fake = ds_fake().await;
    let (a, b) = (tab(&[(LOCKFILE, "from a\n")]), tab(&[(LOCKFILE, "from b\n")]));
    let (sa, sb) = (store(&fake), store(&fake));
    hydrate(&sa, &a).await.unwrap();
    hydrate(&sb, &b).await.unwrap();
    checkpoint(&sa, &a).await.unwrap();
    let err = checkpoint(&sb, &b).await.unwrap_err();
    assert!(format!("{err:#}").contains("changed by another run"), "{err:#}");

    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "from a\n");
    assert_eq!(manifest(&fake).unwrap()["rev"], 1);
}

/// The manifest's compare-and-swap, the request that publishes a checkpoint.
fn manifest_write(req: &Request) -> bool {
    req.url.path().ends_with("/data/replace_one") && body_of(req)["filter"].get("rev").is_some()
}



#[tokio::test]
async fn non_utf8_is_refused() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    write(&page, BASE, &[0x66, 0xff, 0x00]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    let err = checkpoint(&state, &page).await.unwrap_err();
    assert!(format!("{err:#}").contains(BASE), "the error names the file: {err:#}");
    assert_eq!(manifest(&fake), None, "nothing was published");
}

/// A manifest write that fails without applying is that failure, not a
/// lost race: the manifest is still the one this store saw.
#[tokio::test]
async fn a_failed_manifest_write_is_not_a_lost_race() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    fake.intercept(|_, req| {
        manifest_write(req).then(|| ResponseTemplate::new(400).set_body_json(json!({ "code": "error", "message": "refused by the test" })))
    });
    write(&page, LOCKFILE, b"v2\n");
    let err = format!("{:#}", checkpoint(&state, &page).await.unwrap_err());
    assert!(err.contains("refused by the test") && !err.contains("another run"), "{err}");
}

/// Write `doc` into the state collection, replacing the one with its `_id`.
fn put(fake: &FakeOrg, doc: Value) {
    let mut st = fake.state();
    let docs = &mut st.data_storage.as_mut().unwrap().collections.entry(COLLECTION.into()).or_default().docs;
    docs.retain(|d| d["_id"] != doc["_id"]);
    docs.push(doc);
}





#[tokio::test]
async fn hydrate_refuses_a_key_outside_the_state_prefixes() {
    for key in ["envs/dev/hooks/x.py", "rdc.toml", "../x", ".rdc/conflicts/test/../../envs/test/hooks/x.py", ".rdc/state/dev.lock.json"] {
        let fake = ds_fake().await;
        put(&fake, json!({ "_id": format!("{KEY}/test"), "rev": 1, "snapshot": snapshot_of(&[(key, "print('planted')\n")]) }));
        let page = tab(&[("envs/test/labels/a.json", "{}\n")]);
        let before = page.files();
        let err = format!("{:#}", hydrate(&store(&fake), &page).await.unwrap_err());
        assert!(err.contains(key), "{key}: {err}");
        assert_eq!(page.files(), before, "{key}: nothing was written");
    }
}

/// The manifest write applies and its answer is lost (here: an error after
/// applying): the re-read finds this run's manifest, and that is success.
#[tokio::test]
async fn a_lost_manifest_response_is_success() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    fake.intercept(|st, req| {
        if !manifest_write(req) {
            return None;
        }
        let _lost = super::data_storage::serve(st.data_storage.as_mut(), req);
        Some(ResponseTemplate::new(400).set_body_json(json!({ "code": "error", "message": "the answer was lost" })))
    });
    write(&page, LOCKFILE, b"v2\n");
    checkpoint(&state, &page).await.expect("the write applied: it is ours");
    assert_eq!(manifest(&fake).unwrap()["rev"], 2);
    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");
}

/// The run dies after the manifest write applied, before its answer: the
/// next hydrate sees the new state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_manifest_write_applied_keeps_the_new_state() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    write(&page, LOCKFILE, b"v2\n");
    let fired = fake.crash_on(Box::new(manifest_write), 1, CrashPoint::AfterApplyBeforeResponse);
    until_crash(checkpoint(&state, &page), &fired).await;
    fake.revive();

    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");
}



/// A load is one `find`, a save one `replace_one` (the first, one
/// `insert_many`), whatever the env's size.
#[tokio::test]
async fn a_load_is_one_find_and_a_save_one_write() {
    let fake = ds_fake().await;
    let many: Vec<(String, String)> =
        (0..500).map(|i| (format!("envs/test/labels/l{i}.json"), format!("{{\"name\": \"l{i}\"}}\n"))).collect();
    let mut files: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    files.push((LOCKFILE, "v1\n"));
    let page = tab(&files);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    assert_eq!((calls(&fake, "data/find"), calls(&fake, "data/insert_many"), calls(&fake, "data/replace_one")), (1, 1, 0));
    write(&page, LOCKFILE, b"v2\n");
    checkpoint(&state, &page).await.unwrap();
    assert_eq!((calls(&fake, "data/find"), calls(&fake, "data/insert_many"), calls(&fake, "data/replace_one")), (1, 1, 1));
    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    assert_eq!(calls(&fake, "data/find"), 2);
    assert_eq!(text_files(&fresh).len(), 501);
}



/// The run dies with its save on the wire, before it applied: the next
/// hydrate finds the previous state, whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_before_the_save_applied_keeps_the_old_state() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n"), (BASE, "base v1\n")]);
    let state = store(&fake);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    write(&page, LOCKFILE, b"v2\n");
    write(&page, BASE, b"base v2\n");
    let fired = fake.crash_on(Box::new(manifest_write), 1, CrashPoint::BeforeApply);
    until_crash(checkpoint(&state, &page), &fired).await;
    fake.revive();
    let fresh = tab(&[]);
    hydrate(&store(&fake), &fresh).await.unwrap();
    let got = text_files(&fresh);
    assert_eq!((got[LOCKFILE].as_str(), got[BASE].as_str()), ("v1\n", "base v1\n"));
}
