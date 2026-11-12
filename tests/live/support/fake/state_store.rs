//! `DataStorageStateStore` against the fake's Data Storage: an env's sync
//! state (lockfile, base cache, conflict shadows) survives a page that is
//! gone, a second writer is refused, and a crash between the blobs and the
//! manifest leaves the previous state whole.

use super::crash::until_crash;
use super::*;
use rdc::api::DataStorageClient;
use rdc::paths::Paths;
use rdc::state::data_storage_store::DataStorageStateStore;
use rdc::state::store::StateStore;
use rdc::vfs::{MemStore, Store};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const COLLECTION: &str = rdc::state::data_storage_store::STATE_COLLECTION;
const KEY: &str = "acme/main";
const LOCKFILE: &str = ".rdc/state/test.lock.json";
const BASE: &str = ".rdc/state/test.base/labels/priority.json";
const SHADOW: &str = ".rdc/conflicts/test/labels/priority.json";

fn client(fake: &FakeOrg) -> DataStorageClient {
    let base = rdc::config::EnvConfig { api_base: fake.api_base(), org_id: 1 }.data_storage_base();
    DataStorageClient::new(base, "fake-token".into()).unwrap()
}

fn store(fake: &FakeOrg, chunk_bytes: usize) -> DataStorageStateStore {
    DataStorageStateStore::new(client(fake), COLLECTION.into(), KEY.into(), chunk_bytes)
}

fn paths() -> Paths {
    Paths::for_env(Path::new("/p"), "test")
}

/// A tab's in-memory tree holding `files` (relpath → text).
fn tab(files: &[(&str, &str)]) -> Arc<MemStore> {
    let files: Vec<(String, Vec<u8>)> = files.iter().map(|(p, t)| (p.to_string(), t.as_bytes().to_vec())).collect();
    Arc::new(MemStore::from_files("/p".into(), files).unwrap())
}

async fn hydrate(state: &DataStorageStateStore, tab: &Arc<MemStore>) -> anyhow::Result<()> {
    let p = paths();
    rdc::vfs::scope(tab.clone() as Arc<dyn Store>, state.hydrate(&p)).await
}

async fn checkpoint(state: &DataStorageStateStore, tab: &Arc<MemStore>) -> anyhow::Result<()> {
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

fn blob_ids(fake: &FakeOrg) -> Vec<String> {
    let st = fake.state();
    let mut ids: Vec<String> = st
        .data_storage
        .as_ref()
        .unwrap()
        .docs(COLLECTION)
        .iter()
        .filter_map(|d| d["_id"].as_str().map(str::to_string))
        .filter(|id| id.starts_with(&format!("{KEY}/test/blob/")))
        .collect();
    ids.sort();
    ids
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
        (".rdc/state/test.lock", ""),
    ]);
    let state = store(&fake, 512 * 1024);
    hydrate(&state, &first).await.unwrap();
    assert_eq!(manifest(&fake), None, "nothing is published by a hydrate");
    checkpoint(&state, &first).await.unwrap();

    // A reloaded tab: the env tree is back, the state is not, and a stale
    // shadow the manifest lacks is there.
    let second = tab(&[
        ("envs/test/labels/priority.json", "{}\n"),
        (".rdc/conflicts/test/labels/stale.json", "{}\n"),
        (".rdc/conflicts/dev/labels/other_env.json", "{}\n"),
    ]);
    hydrate(&store(&fake, 512 * 1024), &second).await.unwrap();

    let got = text_files(&second);
    let want: BTreeMap<String, String> = [
        (LOCKFILE, "{\"version\": 4}\n"),
        (BASE, "{\"name\": \"priority\"}\n"),
        (SHADOW, "{\"name\": \"théirs ✓\"}\n"),
        ("envs/test/labels/priority.json", "{}\n"),
        (".rdc/conflicts/dev/labels/other_env.json", "{}\n"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(got, want, "the state is the manifest's; the env tree, the env lock and other envs are not state");
    let m = manifest(&fake).unwrap();
    assert_eq!(m["rev"], 1);
    assert_eq!(m["files"].as_object().unwrap().len(), 3);
}

#[tokio::test]
async fn hydrate_without_a_manifest_keeps_local_state() {
    let fake = ds_fake().await;
    let local = tab(&[(LOCKFILE, "{}\n")]);
    hydrate(&store(&fake, 1024), &local).await.unwrap();
    assert_eq!(text_files(&local)[LOCKFILE], "{}\n");
}

#[tokio::test]
async fn large_file_is_chunked() {
    let fake = ds_fake().await;
    // Multi-byte characters: a chunk boundary must never split one.
    let big: String = "ab✓é".repeat(40);
    let first = tab(&[(LOCKFILE, &big)]);
    let state = store(&fake, 16);
    hydrate(&state, &first).await.unwrap();
    checkpoint(&state, &first).await.unwrap();

    let ids = blob_ids(&fake);
    assert!(ids.len() > big.len() / 16, "several blob documents: {}", ids.len());
    for id in &ids {
        let doc = fake.state().data_storage.as_ref().unwrap().doc(COLLECTION, &json!(id)).unwrap();
        assert!(doc["text"].as_str().unwrap().len() <= 16, "{id} is over chunk_bytes");
    }
    let second = tab(&[]);
    hydrate(&store(&fake, 16), &second).await.unwrap();
    assert_eq!(text_files(&second)[LOCKFILE], big);
}

#[tokio::test]
async fn second_writer_loses_the_cas() {
    let fake = ds_fake().await;
    let seed = tab(&[(LOCKFILE, "v1\n")]);
    let s0 = store(&fake, 1024);
    hydrate(&s0, &seed).await.unwrap();
    checkpoint(&s0, &seed).await.unwrap();

    let (a, b) = (tab(&[]), tab(&[]));
    let (sa, sb) = (store(&fake, 1024), store(&fake, 1024));
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
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "from a\n");
}

#[tokio::test]
async fn first_manifest_race_has_one_winner() {
    let fake = ds_fake().await;
    let (a, b) = (tab(&[(LOCKFILE, "from a\n")]), tab(&[(LOCKFILE, "from b\n")]));
    let (sa, sb) = (store(&fake, 1024), store(&fake, 1024));
    hydrate(&sa, &a).await.unwrap();
    hydrate(&sb, &b).await.unwrap();
    checkpoint(&sa, &a).await.unwrap();
    let err = checkpoint(&sb, &b).await.unwrap_err();
    assert!(format!("{err:#}").contains("changed by another run"), "{err:#}");

    let fresh = tab(&[]);
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "from a\n");
    assert_eq!(manifest(&fake).unwrap()["rev"], 1);
}

/// The manifest's compare-and-swap, the request that publishes a checkpoint.
fn manifest_write(req: &Request) -> bool {
    req.url.path().ends_with("/data/replace_one") && body_of(req)["filter"].get("rev").is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_between_blobs_and_manifest_keeps_old_state() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n"), (BASE, "base v1\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    let before = blob_ids(&fake);

    write(&page, LOCKFILE, b"v2\n");
    write(&page, BASE, b"base v2\n");
    let fired = fake.crash_on(Box::new(manifest_write), 1, CrashPoint::BeforeApply);
    until_crash(checkpoint(&state, &page), &fired).await;
    fake.revive();
    assert!(blob_ids(&fake).len() > before.len(), "the new blobs were uploaded");

    let fresh = tab(&[]);
    let next = store(&fake, 1024);
    hydrate(&next, &fresh).await.unwrap();
    let got = text_files(&fresh);
    assert_eq!((got[LOCKFILE].as_str(), got[BASE].as_str()), ("v1\n", "base v1\n"), "the previous checkpoint, whole");

    // The next checkpoint publishes; the dead run's blobs are left behind,
    // unreferenced (only a replaced manifest's blobs are ever deleted).
    write(&fresh, LOCKFILE, b"v3\n");
    checkpoint(&next, &fresh).await.unwrap();
    let v3 = format!("{KEY}/test/blob/{}/0", rdc::state::raw_content_hash(b"v3\n"));
    let v2 = format!("{KEY}/test/blob/{}/0", rdc::state::raw_content_hash(b"v2\n"));
    let left = blob_ids(&fake);
    assert!(left.contains(&v3) && left.contains(&v2), "{left:?}");
    assert!(!left.contains(&format!("{KEY}/test/blob/{}/0", rdc::state::raw_content_hash(b"v1\n"))), "the replaced v1 lockfile blob is gone");
}

#[tokio::test]
async fn unreferenced_blobs_are_collected() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n"), (SHADOW, "shadow\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    assert_eq!(blob_ids(&fake).len(), 2);

    write(&page, LOCKFILE, b"v2\n");
    rdc::vfs::scope_sync(page.clone() as Arc<dyn Store>, || rdc::vfs::remove_file(Path::new("/p").join(SHADOW)).unwrap());
    checkpoint(&state, &page).await.unwrap();

    let sha = rdc::state::raw_content_hash(b"v2\n");
    assert_eq!(blob_ids(&fake), vec![format!("{KEY}/test/blob/{sha}/0")]);
    let m = manifest(&fake).unwrap();
    assert_eq!(m["rev"], 2);
    assert_eq!(m["files"], json!({ LOCKFILE: { "sha256": sha, "chunks": 1 } }));
}

#[tokio::test]
async fn non_utf8_is_refused() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    write(&page, BASE, &[0x66, 0xff, 0x00]);
    let state = store(&fake, 1024);
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
    let state = store(&fake, 1024);
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

/// `text` as a published one-chunk state file: its blob, and its manifest entry.
fn one_chunk(text: &str) -> (Value, Value) {
    let sha = rdc::state::raw_content_hash(text.as_bytes());
    (json!({ "_id": format!("{KEY}/test/blob/{sha}/0"), "text": text }), json!({ "sha256": sha, "chunks": 1 }))
}

/// Another run publishes a later manifest between this run's publish and
/// its clean-up: the clean-up deletes only what this run's replaced
/// manifest referenced, never the later manifest's blobs.
#[tokio::test]
async fn cleanup_spares_blobs_of_a_later_manifest() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();

    let (later_blob, later_ref) = one_chunk("later\n");
    let mut armed = true;
    fake.intercept(move |st, req| {
        if !(armed && manifest_write(req)) {
            return None;
        }
        armed = false;
        let out = super::data_storage::serve(st.data_storage.as_mut(), req);
        let docs = &mut st.data_storage.as_mut().unwrap().collections.get_mut(COLLECTION).unwrap().docs;
        docs.push(later_blob.clone());
        let manifest = docs.iter_mut().find(|d| d["_id"] == format!("{KEY}/test")).unwrap();
        *manifest = json!({ "_id": format!("{KEY}/test"), "rev": 3, "files": { LOCKFILE: later_ref.clone() } });
        out
    });
    write(&page, LOCKFILE, b"v2\n");
    checkpoint(&state, &page).await.unwrap();

    let fresh = tab(&[]);
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "later\n", "the later manifest is whole");
    let v1 = format!("{KEY}/test/blob/{}/0", rdc::state::raw_content_hash(b"v1\n"));
    assert!(!blob_ids(&fake).contains(&v1), "the replaced manifest's blob is gone");
}

#[tokio::test]
async fn cleanup_failure_only_warns() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    assert!(state.take_warnings().is_empty());
    fake.intercept(|_, req| {
        req.url.path().ends_with("/data/delete_many")
            .then(|| ResponseTemplate::new(400).set_body_json(json!({ "code": "error", "message": "refused by the test" })))
    });
    write(&page, LOCKFILE, b"v2\n");
    checkpoint(&state, &page).await.expect("a failed clean-up does not fail the checkpoint");

    let warnings = state.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("refused by the test"), "{warnings:?}");
    let fresh = tab(&[]);
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");
}

/// A run publishes (and cleans up) between a hydrate's manifest read and
/// its blob read: the hydrate reads the manifest again and gets the new state.
#[tokio::test]
async fn hydrate_retries_once_when_a_blob_vanishes() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    let at_v1 = fake.state().data_storage.as_ref().unwrap().docs(COLLECTION);
    write(&page, LOCKFILE, b"v2\n");
    checkpoint(&state, &page).await.unwrap();
    let at_v2 = fake.state().data_storage.as_ref().unwrap().docs(COLLECTION);
    fake.state().data_storage.as_mut().unwrap().collections.get_mut(COLLECTION).unwrap().docs = at_v1;

    let is_blob_read = |req: &Request| req.url.path().ends_with("/data/find") && body_of(req)["query"]["_id"].get("$in").is_some();
    let mut armed = true;
    fake.intercept(move |st, req| {
        if armed && is_blob_read(req) {
            armed = false;
            st.data_storage.as_mut().unwrap().collections.get_mut(COLLECTION).unwrap().docs = at_v2.clone();
        }
        None
    });
    let fresh = tab(&[]);
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");

    // A blob that stays missing is an error after the one retry.
    fake.state().data_storage.as_mut().unwrap().collections.get_mut(COLLECTION).unwrap().docs.retain(|d| d["_id"] == format!("{KEY}/test"));
    let err = format!("{:#}", hydrate(&store(&fake, 1024), &tab(&[])).await.unwrap_err());
    assert!(err.contains("is missing"), "{err}");
}

#[tokio::test]
async fn hydrate_refuses_a_key_outside_the_state_prefixes() {
    for key in ["envs/test/hooks/x.py", "../x", ".rdc/conflicts/test/../../envs/test/hooks/x.py", ".rdc/state/dev.lock.json"] {
        let fake = ds_fake().await;
        let (blob, file) = one_chunk("print('planted')\n");
        put(&fake, blob);
        put(&fake, json!({ "_id": format!("{KEY}/test"), "rev": 1, "files": { key: file } }));
        let page = tab(&[("envs/test/labels/a.json", "{}\n")]);
        let before = page.files();
        let err = format!("{:#}", hydrate(&store(&fake, 1024), &page).await.unwrap_err());
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
    let state = store(&fake, 1024);
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
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");
}

/// The run dies after the manifest write applied, before its answer: the
/// next hydrate sees the new state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_manifest_write_applied_keeps_the_new_state() {
    let fake = ds_fake().await;
    let page = tab(&[(LOCKFILE, "v1\n")]);
    let state = store(&fake, 1024);
    hydrate(&state, &page).await.unwrap();
    checkpoint(&state, &page).await.unwrap();
    write(&page, LOCKFILE, b"v2\n");
    let fired = fake.crash_on(Box::new(manifest_write), 1, CrashPoint::AfterApplyBeforeResponse);
    until_crash(checkpoint(&state, &page), &fired).await;
    fake.revive();

    let fresh = tab(&[]);
    hydrate(&store(&fake, 1024), &fresh).await.unwrap();
    assert_eq!(text_files(&fresh)[LOCKFILE], "v2\n");
}
