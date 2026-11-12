//! The rdc Chrome extension's Data Storage collections (`_SA_EXTENSION__*`)
//! are skipped like Rossum's own `__` ones, by the CLI too. These cases pin
//! what happens to a project that pulled one as a dataset before that rule:
//!
//! - when the env lists at least one other dataset, the next sync treats it
//!   as deleted on the env and removes it locally (or defers, when it was
//!   edited);
//! - when it lists none, the stage-4 orphan prune does not run at all (it is
//!   gated on a non-empty listing, so an MDH outage cannot mass-delete), and
//!   the dataset stays in the tree and the lockfile, inert;
//! - migrate never copies it;
//! - no run ever writes to the collection, let alone drops it.

use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::project::ProjectFixture;
use serde_json::json;

const NAME: &str = "_SA_EXTENSION__rdc_state";
/// The slug the dataset was pulled under (any slug; the name is what counts).
const SLUG: &str = "sa-probe";
const DIR: &str = "envs/test/mdh/sa-probe";

fn assert_ok(out: &std::process::Output, ctx: &str) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{ctx}: failed:\nstdout: {}\nstderr: {stderr}", String::from_utf8_lossy(&out.stdout));
    stderr
}

fn rows() -> Vec<Value> {
    (1..=3).map(|i| json!({ "_id": format!("r{i}"), "blob": format!("state {i}") })).collect()
}

/// Every Data Storage request naming `NAME` other than a read.
fn count_writes(fake: &FakeOrg) -> Arc<Mutex<Vec<String>>> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    fake.intercept(move |_, req| {
        let path = req.url.path();
        let read = path.ends_with("/data/find") || path.ends_with("/list") || path.ends_with("/data/aggregate");
        if path.starts_with("/svc/") && !read && body_of(req)["collectionName"] == NAME {
            sink.lock().unwrap().push(path.to_string());
        }
        None
    });
    seen
}

/// A project whose env `test` (and an empty `prod`) holds a manual dataset
/// pulled from the collection `NAME` before such collections were skipped.
///
/// Built honestly: the dataset is synced while the collection has an ordinary
/// name, then the collection and its local `collection.json` are renamed to
/// `NAME`. `collection.json` is not hashed in the lockfile, so the tree,
/// base cache and lockfile are exactly what a pull of `NAME` left.
/// With `other`, the env also holds an ordinary dataset, `vendors`.
async fn already_pulled(other: bool) -> (FakeOrg, ProjectFixture) {
    let fake = FakeOrg::start().await;
    fake.enable_data_storage();
    let project = ProjectFixture::init(&fake.config(), &["test", "prod"]).expect("init");
    if other {
        project.write_json("envs/test/mdh/vendors/collection.json", &json!({ "name": "vendors" }));
        project.write_json(
            "envs/test/mdh/vendors/indexes.json",
            &json!({ "regular": [{ "key": { "code": 1 }, "name": "code_idx" }], "search": [] }),
        );
    }
    project.write_json(&format!("{DIR}/collection.json"), &json!({ "name": SLUG, "data": "manual" }));
    project.write_json(&format!("{DIR}/indexes.json"), &json!({ "regular": [], "search": [] }));
    let jsonl: String = rows().iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(project.path().join(DIR).join("data.jsonl"), jsonl).unwrap();
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "creating the dataset");
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "pulling it back");
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert!(lf.objects["mdh_indexes"].contains_key(SLUG), "the dataset is recorded");

    {
        let mut st = fake.state();
        let ds = st.data_storage.as_mut().unwrap();
        let c = ds.collections.remove(SLUG).expect("the collection exists");
        ds.collections.insert(NAME.to_string(), c);
    }
    project.write_json(&format!("{DIR}/collection.json"), &json!({ "name": NAME, "data": "manual" }));
    (fake, project)
}

fn remote_rows(fake: &FakeOrg) -> Vec<Value> {
    fake.state().data_storage.as_ref().unwrap().docs(NAME)
}

async fn removed_locally(extra: &[&str]) {
    let (fake, project) = already_pulled(true).await;
    let writes = count_writes(&fake);
    let mut args = vec!["sync", "test", "--yes"];
    args.extend_from_slice(extra);

    let stderr = assert_ok(&project.run_rdc(&args), "the next sync");

    assert!(!project.exists(DIR), "the dataset is removed locally:\n{stderr}");
    assert!(!project.exists(&format!(".rdc/state/test.base/mdh/{SLUG}/indexes.json")), "and its base copy");
    let lf = load_lockfile(project.path(), "test").unwrap();
    for kind in ["mdh_indexes", "mdh_data"] {
        assert!(!lf.objects.get(kind).is_some_and(|m| m.contains_key(SLUG)), "{kind} entry dropped");
    }
    assert!(stderr.contains(&format!("mdh/{SLUG} (deleted on test)")), "reported as deleted on the env:\n{stderr}");
    assert_eq!(remote_rows(&fake), rows(), "the collection's rows are untouched");
    assert!(writes.lock().unwrap().is_empty(), "nothing was written to {NAME}: {:?}", writes.lock().unwrap());

    // And it stays gone: the next sync neither re-creates nor re-pulls it.
    assert_ok(&project.run_rdc(&args), "a later sync");
    assert!(!project.exists(DIR));
    assert!(fake.state().data_storage.as_ref().unwrap().collections.contains_key(NAME), "the collection still exists");
    assert!(writes.lock().unwrap().is_empty(), "{:?}", writes.lock().unwrap());
}

/// The env lists no other dataset: the orphan prune is skipped, so the
/// dataset stays, recorded, and nothing reaches the env.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alone_on_its_env_it_stays_inert() {
    let (fake, project) = already_pulled(false).await;
    let writes = count_writes(&fake);
    let before = load_lockfile(project.path(), "test").unwrap();
    for args in [&["sync", "test", "--yes"][..], &["sync", "test", "--yes", "--allow-deletes"][..]] {
        assert_ok(&project.run_rdc(args), "the next sync");
        assert!(project.exists(&format!("{DIR}/indexes.json")) && project.exists(&format!("{DIR}/data.jsonl")));
        let lf = load_lockfile(project.path(), "test").unwrap();
        assert_eq!(lf.objects.get("mdh_indexes"), before.objects.get("mdh_indexes"), "the entry is kept");
    }
    assert_eq!(remote_rows(&fake), rows());
    assert!(writes.lock().unwrap().is_empty(), "{:?}", writes.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_pulled_dataset_is_removed_locally_and_never_touched_remotely() {
    removed_locally(&[]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_deletes_does_not_delete_the_collection_either() {
    removed_locally(&["--allow-deletes"]).await
}

/// An edited dataset is not removed without asking: a non-interactive run
/// leaves it and a deferral marker, and still writes nothing to the env.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_already_pulled_dataset_is_deferred_not_pushed() {
    let (fake, project) = already_pulled(true).await;
    project.write_json(
        &format!("{DIR}/indexes.json"),
        &json!({ "regular": [{ "key": { "blob": 1 }, "name": "blob_idx" }], "search": [] }),
    );
    let writes = count_writes(&fake);

    for args in [&["sync", "test", "--yes"][..], &["sync", "test", "--yes", "--allow-deletes"][..]] {
        let stderr = assert_ok(&project.run_rdc(args), "the next sync");
        assert!(stderr.contains(&format!("mdh/{SLUG}: env deletion deferred")), "{stderr}");
        assert!(project.exists(&format!("{DIR}/indexes.json")), "the edited dataset is kept");
    }
    let markers: Vec<String> = walk(&project.path().join(".rdc/conflicts/test"))
        .into_iter()
        .filter(|p| p.ends_with("-deleted"))
        .collect();
    assert_eq!(markers.len(), 1, "with a deferral marker: {markers:?}");
    assert!(writes.lock().unwrap().is_empty(), "{:?}", writes.lock().unwrap());
    assert!(fake.state().data_storage.as_ref().unwrap().collections[NAME].indexes.is_empty(), "no index was pushed");
    assert_eq!(remote_rows(&fake), rows());
}

/// Migrate never copies it, and a `--mirror` prune of a target copy only
/// removes local files: the target's next sync deletes nothing on the env.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_skips_it_and_a_mirror_prune_stays_local() {
    let (fake, project) = already_pulled(true).await;
    let writes = count_writes(&fake);
    assert_ok(&project.run_rdc(&["sync", "prod", "--yes"]), "pulling prod");
    assert!(!project.exists(&format!("envs/prod/mdh/{SLUG}")), "prod's pull skips the collection");

    let stderr = assert_ok(&project.run_rdc(&["migrate", "test", "prod"]), "migrate test -> prod");
    assert!(!project.exists(&format!("envs/prod/mdh/{SLUG}")), "not copied:\n{stderr}");
    assert!(stderr.contains(&format!("skipping mdh/{SLUG}")), "{stderr}");

    // `test` is now the target: prod produces no dataset, so `--mirror`
    // prunes test's copy, and the push that follows deletes nothing remote.
    assert_ok(&project.run_rdc(&["migrate", "prod", "test", "--mirror"]), "migrate prod -> test --mirror");
    assert!(!project.exists(&format!("{DIR}/indexes.json")), "pruned locally");
    assert_ok(&project.run_rdc(&["sync", "test", "--yes", "--allow-deletes"]), "sync test");
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert!(!lf.objects.get("mdh_indexes").is_some_and(|m| m.contains_key(SLUG)), "the entry is dropped");
    assert_eq!(remote_rows(&fake), rows());
    assert!(writes.lock().unwrap().is_empty(), "{:?}", writes.lock().unwrap());
}

/// Every file under `dir`, as a path string.
fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().unwrap().is_dir() {
            out.extend(walk(&entry.path()));
        } else {
            out.push(entry.path().to_string_lossy().into_owned());
        }
    }
    out
}
