//! The browser extension's flow against two fake orgs, with each env's state
//! and tree in its own org's Data Storage, and tabs that close at any moment:
//! read `source` and `target` (`sync`, no push), `migrate source target`, then
//! `sync target` to push.
//!
//! A tab is a `MemStore` and one `DataStorageStateStore` per env, kept for the
//! tab's life as the wasm `Project` keeps them. Closing it drops all of that;
//! a reopened tab starts from `rdc.toml` alone. Nothing here touches the disk.

use super::crash::until_crash;
use super::*;
use rdc::cli::sync::embed::{
    migrate_logged_in, sync_logged_in_with_state, EmbedMigrateOptions, EmbedSyncOptions, MigrateReport, MirrorMode,
    ObjectChange,
};
use rdc::state::data_storage_store::DataStorageStateStore;
use rdc::state::store::StateStore;
use rdc::vfs::{MemStore, Store};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TOKEN: &str = "fake-token";
/// The extension's collection: no `__` prefix, so only the store's own
/// configuration keeps it out of MDH sync.
const COLLECTION: &str = "_SA_EXTENSION__rdc_state";
const KEY: &str = "acme/main";
const LABELS: [&str; 3] = ["alpha", "beta", "gamma"];

struct Orgs {
    source: FakeOrg,
    target: FakeOrg,
}

impl Orgs {
    /// `source` holds three labels; `target` holds one label of its own.
    async fn start() -> Orgs {
        let (source, target) = (FakeOrg::start_with_org(1).await, FakeOrg::start_with_org(2).await);
        for fake in [&source, &target] {
            fake.enable_data_storage();
        }
        let org = source.state().org_url();
        for name in LABELS {
            source.state().create("labels", json!({ "name": name, "organization": org, "color": "#112233" })).unwrap();
        }
        let org = target.state().org_url();
        target.state().create("labels", json!({ "name": "target-only", "organization": org, "color": "#445566" })).unwrap();
        Orgs { source, target }
    }

    fn fake(&self, env: &str) -> &FakeOrg {
        if env == "source" { &self.source } else { &self.target }
    }

    fn rdc_toml(&self) -> String {
        format!(
            "[envs.source]\napi_base = \"{}\"\norg_id = 1\n\n[envs.target]\napi_base = \"{}\"\norg_id = 2\n",
            self.source.api_base(),
            self.target.api_base()
        )
    }

    /// Target labels by name, each name once (a duplicate panics).
    fn target_labels(&self) -> BTreeMap<String, u64> {
        let st = self.target.state();
        let mut out = BTreeMap::new();
        for id in st.ids("labels") {
            let name = st.get("labels", id).unwrap()["name"].as_str().unwrap().to_string();
            assert!(out.insert(name.clone(), id).is_none(), "label {name} exists twice on the target");
        }
        out
    }
}

struct Tab {
    root: PathBuf,
    mem: Arc<MemStore>,
    stores: BTreeMap<&'static str, Arc<DataStorageStateStore>>,
}

impl Tab {
    /// A new tab: `rdc.toml` and nothing else, as the host keeps it.
    fn open(orgs: &Orgs) -> Tab {
        let root = PathBuf::from("/browser-project");
        let mem = Arc::new(MemStore::from_files(root.clone(), [("rdc.toml".to_string(), orgs.rdc_toml().into_bytes())]).unwrap());
        let stores = ["source", "target"]
            .into_iter()
            .map(|env| {
                let fake = orgs.fake(env);
                let base = rdc::config::EnvConfig { api_base: fake.api_base(), org_id: 1 }.data_storage_base();
                let client = rdc::api::DataStorageClient::new(base, TOKEN.into()).unwrap();
                (env, Arc::new(DataStorageStateStore::new(client, COLLECTION.into(), KEY.into(), 4096)))
            })
            .collect();
        Tab { root, mem, stores }
    }

    fn state(&self, env: &str) -> Arc<dyn StateStore> {
        self.stores[env].clone()
    }

    fn sync<'a>(
        &'a self,
        env: &'a str,
        opts: EmbedSyncOptions,
        state: Arc<dyn StateStore>,
    ) -> impl std::future::Future<Output = anyhow::Result<rdc::cli::sync::CycleOutcome>> + 'a {
        sync_logged_in_with_state(self.mem.clone() as Arc<dyn Store>, state, &self.root, env, TOKEN, opts, Box::new(std::io::sink()))
    }

    async fn read(&self, env: &str) {
        let audit = EmbedSyncOptions { interactive: false, no_push: true, ..Default::default() };
        self.sync(env, audit, self.state(env)).await.unwrap_or_else(|e| panic!("reading {env}: {e:#}"));
    }

    async fn push(&self, env: &str) -> rdc::cli::sync::CycleOutcome {
        self.sync(env, yes(), self.state(env)).await.unwrap_or_else(|e| panic!("pushing {env}: {e:#}"))
    }

    fn migrate<'a>(
        &'a self,
        opts: EmbedMigrateOptions,
        tgt_state: Arc<dyn StateStore>,
    ) -> impl std::future::Future<Output = anyhow::Result<MigrateReport>> + 'a {
        migrate_logged_in(
            self.mem.clone() as Arc<dyn Store>,
            self.state("source"),
            tgt_state,
            &self.root,
            "source",
            "target",
            opts,
            Box::new(std::io::sink()),
        )
    }

    /// `envs/<env>/**` and the env's state files, key → bytes.
    fn env_files(&self, env: &str) -> BTreeMap<String, Vec<u8>> {
        let prefixes = [format!("envs/{env}/"), format!(".rdc/state/{env}."), format!(".rdc/conflicts/{env}/")];
        self.mem.files().into_iter().filter(|(k, _)| prefixes.iter().any(|p| k.starts_with(p.as_str()))).collect()
    }

    fn assert_no_dataset_mirror(&self) {
        let mirrored: Vec<String> = self.mem.files().into_keys().filter(|k| k.contains("/mdh/")).collect();
        assert!(mirrored.is_empty(), "the state's collection was synced as a dataset: {mirrored:?}");
    }
}

fn yes() -> EmbedSyncOptions {
    EmbedSyncOptions { interactive: false, ..Default::default() }
}

fn mirror() -> EmbedMigrateOptions {
    EmbedMigrateOptions { mirror: MirrorMode::Mirror { allow_recreate: false }, ..Default::default() }
}

/// Counts every request to the target org that would change it: creates,
/// edits and deletes outside Data Storage.
fn count_writes(fake: &FakeOrg) -> Arc<Mutex<Vec<String>>> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    fake.intercept(move |_, req| {
        if req.method.as_str() != "GET" && !req.url.path().starts_with("/svc/") {
            sink.lock().unwrap().push(format!("{} {}", req.method, req.url.path()));
        }
        None
    });
    seen
}

/// A target store whose first checkpoint never returns: the tab closed
/// while that save was on its way, before it reached Data Storage.
struct ClosesAtCheckpoint {
    inner: Arc<DataStorageStateStore>,
    fired: Arc<tokio::sync::Notify>,
}

impl StateStore for ClosesAtCheckpoint {
    fn hydrate<'a>(&'a self, paths: &'a rdc::paths::Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        self.inner.hydrate(paths)
    }
    fn checkpoint<'a>(&'a self, _: &'a rdc::paths::Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        self.fired.notify_one();
        Box::pin(std::future::pending())
    }
    fn collection(&self) -> Option<String> {
        self.inner.collection()
    }
}

/// The settled end state: each source label once on the target, the
/// target's own label pruned by the mirror and deleted by the push, a third
/// run with nothing to do, and the journal empty.
async fn assert_settled(orgs: &Orgs, tab: &Tab, ctx: &str) {
    let mut names: Vec<String> = orgs.target_labels().into_keys().collect();
    names.sort();
    assert_eq!(names, LABELS.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{ctx}: the target's labels");
    let again = Tab::open(orgs);
    again.read("target").await;
    let out = again.sync("target", yes(), again.state("target")).await.unwrap();
    assert_eq!((out.items_pushed, out.items_pulled, out.conflicts), (0, 0, 0), "{ctx}: a later run found work");
    let lock: rdc::state::Lockfile =
        serde_json::from_slice(&again.mem.files()[".rdc/state/target.lock.json"]).unwrap();
    assert_eq!(lock.pending, None, "{ctx}: the journal is settled");
    let tree = |t: &Tab| t.env_files("target").into_iter().filter(|(k, _)| k.starts_with("envs/")).collect::<BTreeMap<_, _>>();
    assert_eq!(tree(&again), tree(tab), "{ctx}: a reopened tab has the same target tree");
    again.assert_no_dataset_mirror();
}

/// A migrate is local until its one checkpoint, so a tab that closes at any
/// point during its writes leaves Data Storage at the pre-migrate state. The
/// reopened tab gets that state back, migrates again, and pushes once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tab_closed_during_migrates_writes_restarts_from_the_pre_migrate_target() {
    let orgs = Orgs::start().await;
    let first = Tab::open(&orgs);
    first.read("source").await;
    first.read("target").await;
    let before = first.env_files("target");
    assert!(before.keys().any(|k| k.starts_with("envs/target/labels/")), "the target was read");

    let closing = Arc::new(ClosesAtCheckpoint { inner: first.stores["target"].clone(), fired: Arc::default() });
    until_crash(first.migrate(mirror(), closing.clone()), &closing.fired).await;
    assert_ne!(first.env_files("target"), before, "the migrate wrote the tab's tree");
    drop(first);

    let second = Tab::open(&orgs);
    let writes = count_writes(&orgs.target);
    second.read("target").await;
    assert_eq!(second.env_files("target").into_iter().filter(|(k, _)| !k.ends_with("_index.md")).collect::<Vec<_>>(),
        before.into_iter().filter(|(k, _)| !k.ends_with("_index.md")).collect::<Vec<_>>(),
        "the reopened tab has the pre-migrate target");
    let report = second.migrate(mirror(), second.state("target")).await.expect("the migrate again");
    let creates: Vec<&str> =
        report.objects.iter().filter(|o| o.change == ObjectChange::Create).map(|o| o.slug.as_str()).collect();
    assert_eq!(creates, LABELS.to_vec(), "{report:?}");
    assert!(report.objects.iter().any(|o| o.slug == "target-only" && o.change == ObjectChange::Delete));
    assert!(writes.lock().unwrap().is_empty(), "nothing reached the target org before the push");

    let out = second.sync("target", EmbedSyncOptions { allow_deletes: true, ..yes() }, second.state("target")).await.unwrap();
    assert_eq!(out.items_pushed, 4, "three creates and the mirror's delete");
    assert_settled(&orgs, &second, "closed during migrate").await;
}

/// The push after a migrate dies after one label's create was applied and
/// before its answer: the reopened tab gets the migrated tree back (no local
/// delete is offered), adopts the lost create, and creates the rest once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tab_closed_during_the_push_after_migrate_recovers_without_a_duplicate() {
    let orgs = Orgs::start().await;
    let first = Tab::open(&orgs);
    first.read("source").await;
    first.read("target").await;
    first.migrate(EmbedMigrateOptions::default(), first.state("target")).await.expect("migrate");
    let migrated = first.env_files("target");

    let second_create = Box::new(|req: &Request| {
        req.method.as_str() == "POST" && req.url.path() == "/api/v1/labels" && body_of(req)["name"] == "beta"
    });
    let fired = orgs.target.crash_on(second_create, 1, CrashPoint::AfterApplyBeforeResponse);
    until_crash(first.sync("target", yes(), first.state("target")), &fired).await;
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    orgs.target.revive();
    assert!(orgs.target_labels().contains_key("beta"), "the lost create was applied");

    let second = Tab::open(&orgs);
    let writes = count_writes(&orgs.target);
    // A dry run first: what the reopened tab would do, before it does it.
    let dry = second.sync("target", EmbedSyncOptions { dry_run: true, ..yes() }, second.state("target")).await;
    dry.expect("the dry run");
    let files = second.mem.files();
    for key in migrated.keys().filter(|k| k.starts_with("envs/target/labels/")) {
        assert!(files.contains_key(key), "{key} is back, so it does not read as a local delete");
    }
    second.push("target").await;
    let writes = writes.lock().unwrap().clone();
    assert!(writes.iter().all(|w| !w.starts_with("DELETE")), "nothing was deleted: {writes:?}");
    let posts = writes.iter().filter(|w| *w == "POST /api/v1/labels").count();
    assert_eq!(posts, 1, "only the label never created is posted: {writes:?}");
    let mut names: Vec<String> = orgs.target_labels().into_keys().collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta", "gamma", "target-only"], "each label once, the target's own kept");
    let out = second.sync("target", yes(), second.state("target")).await.unwrap();
    assert_eq!((out.items_pushed, out.items_pulled, out.conflicts), (0, 0, 0), "converged");
    second.assert_no_dataset_mirror();
}

/// A full flow, then a plain reopen: the tree and state come back byte for
/// byte, and a sync changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reopened_tab_gets_the_tree_back_unchanged() {
    let orgs = Orgs::start().await;
    let first = Tab::open(&orgs);
    first.read("source").await;
    first.read("target").await;
    first.migrate(mirror(), first.state("target")).await.expect("migrate");
    let out = first.sync("target", EmbedSyncOptions { allow_deletes: true, ..yes() }, first.state("target")).await.unwrap();
    assert_eq!(out.items_pushed, 4);
    let (source, target) = (first.env_files("source"), first.env_files("target"));
    drop(first);

    let second = Tab::open(&orgs);
    second.read("source").await;
    second.read("target").await;
    assert_eq!(second.env_files("source"), source, "the source env, state and tree");
    assert_eq!(second.env_files("target"), target, "the target env, state and tree");
    assert_settled(&orgs, &second, "reopened").await;
}

/// Edits made in a tab after it loaded an env are kept by its next run (they
/// are local changes), and reach Data Storage when that run starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_edit_survives_its_own_tab_and_a_closed_one() {
    let orgs = Orgs::start().await;
    let first = Tab::open(&orgs);
    first.read("target").await;
    let path = first.root.join("envs/target/labels/target-only.json");
    let edited = rdc::vfs::scope_sync(first.mem.clone() as Arc<dyn Store>, || {
        let mut v: Value = serde_json::from_slice(&rdc::vfs::read(&path).unwrap()).unwrap();
        v["color"] = json!("#abcdef");
        let bytes = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
        rdc::vfs::write(&path, &bytes).unwrap();
        bytes
    });
    // An audit run: pushes nothing, and keeps the edit.
    first.read("target").await;
    assert_eq!(first.mem.files()["envs/target/labels/target-only.json"], edited.as_bytes());
    drop(first);

    let second = Tab::open(&orgs);
    second.read("target").await;
    assert_eq!(second.mem.files()["envs/target/labels/target-only.json"], edited.as_bytes(), "the edit came back");
    second.push("target").await;
    let id = orgs.target_labels()["target-only"];
    assert_eq!(orgs.target.state().get("labels", id).unwrap()["color"], "#abcdef", "and was pushed");
}

/// Another tab publishes the target between this tab's hydrate and its
/// migrate's checkpoint: the compare-and-swap refuses the save, the run
/// fails loudly, and Data Storage keeps the other tab's state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migrate_whose_target_another_tab_changed_is_refused() {
    let orgs = Orgs::start().await;
    let tab = Tab::open(&orgs);
    tab.read("source").await;
    tab.read("target").await;
    let manifest_id = json!(format!("{KEY}/target"));
    let mut armed = true;
    let id = manifest_id.clone();
    orgs.target.intercept(move |st, req| {
        let is_cas = req.url.path().ends_with("/data/replace_one") && body_of(req)["filter"].get("rev").is_some();
        if armed && is_cas {
            armed = false;
            let docs = &mut st.data_storage.as_mut().unwrap().collections.get_mut(COLLECTION).unwrap().docs;
            let m = docs.iter_mut().find(|d| d["_id"] == id).unwrap();
            m["rev"] = json!(m["rev"].as_u64().unwrap() + 1);
        }
        None
    });
    let err = tab.migrate(EmbedMigrateOptions::default(), tab.state("target")).await.unwrap_err();
    assert!(format!("{err:#}").contains("the state of env 'target' was changed by another run"), "{err:#}");
    let st = orgs.target.state();
    let m = st.data_storage.as_ref().unwrap().doc(COLLECTION, &manifest_id).unwrap();
    assert!(
        !m["files"].as_object().unwrap().keys().any(|k| k == "envs/target/labels/alpha.json"),
        "the refused migrate's files were not published"
    );
}

/// The checks `migrate` makes before it reads anything come before the
/// stores are read: a typo reads no Data Storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_env_name_reads_no_state() {
    let orgs = Orgs::start().await;
    let tab = Tab::open(&orgs);
    let requests: Arc<Mutex<usize>> = Arc::default();
    let seen = requests.clone();
    orgs.source.intercept(move |_, _| {
        *seen.lock().unwrap() += 1;
        None
    });
    let err = migrate_logged_in(
        tab.mem.clone() as Arc<dyn Store>,
        tab.state("source"),
        tab.state("target"),
        Path::new(&tab.root),
        "source",
        "nope",
        EmbedMigrateOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("nope"), "{err:#}");
    assert_eq!(*requests.lock().unwrap(), 0, "no request was made");
}
