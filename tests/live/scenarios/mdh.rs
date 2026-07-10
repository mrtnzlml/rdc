use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::mdh::{mdh_collection_name, MdhRaw};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::teardown::Teardown;
use serde_json::json;

/// Read the local indexes.json for the run's dataset; returns (slug, value).
fn read_indexes(project: &ProjectFixture, run_id: &RunId) -> (String, serde_json::Value) {
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let slug = lockfile_keys(&lf, "mdh_indexes")
        .into_iter()
        .find(|s| s.contains(run_id.as_str()))
        .expect("an mdh_indexes slug for this run");
    let rel = format!("envs/test/mdh/{slug}/indexes.json");
    let v: serde_json::Value =
        serde_json::from_str(&project.read_to_string(&rel).expect("indexes.json on disk")).unwrap();
    (slug, v)
}

fn write_indexes(project: &ProjectFixture, slug: &str, v: &serde_json::Value) {
    let rel = format!("envs/test/mdh/{slug}/indexes.json");
    std::fs::write(project.path().join(&rel), serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn regular_names(v: &serde_json::Value) -> Vec<String> {
    v.get("regular")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn remote_regular_names(raw: &MdhRaw, coll: &str) -> Vec<String> {
    raw.ds_client()
        .list_indexes(coll, None)
        .await
        .expect("list remote indexes")
        .iter()
        .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// Full MDH index lifecycle on a per-run throwaway collection: pull round-trip,
/// push create, push modify, gated safe-delete, admin-added survives,
/// idempotent re-sync. Never touches a real collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_mdh_index_lifecycle() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let coll = mdh_collection_name(&run_id);
    let raw = MdhRaw::connect(&cfg).expect("connect mdh");

    // Teardown FIRST (drops the collection on any panic).
    let teardown = Teardown::with_mdh(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
        cfg.clone(),
    );

    // --- seed remote out-of-band: collection + a doc + one regular index ---
    raw.create_collection(&coll).await.expect("create collection");
    raw.insert_one(&coll, json!({ "k": "v" })).await.expect("seed doc");
    raw.ds_client()
        .create_index(&coll, "ix_a", &json!({ "a": 1 }), &json!({}), None)
        .await
        .expect("seed ix_a");
    raw.wait_for_regular_index(&coll, "ix_a", true).await.expect("ix_a present");
    // Optionally seed an Atlas Search index (gracefully skipped if the cluster
    // has no Search support — `has_search` gates every search assertion below).
    let has_search = raw.try_create_search_index(&coll, "sx_a").await.expect("try search");
    if has_search {
        raw.wait_for_search_index(&coll, "sx_a", true).await.expect("sx_a present");
    }

    // --- Phase 1: pull round-trip ---
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));
    let (slug, idx) = read_indexes(&project, &run_id);
    let names = regular_names(&idx);
    assert!(names.contains(&"ix_a".to_string()), "pulled regular names: {names:?}");
    assert!(!names.contains(&"_id_".to_string()), "_id_ must be stripped: {names:?}");
    if has_search {
        let search = idx.get("search").and_then(|s| s.as_array()).cloned().unwrap_or_default();
        let sx = search
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("sx_a"))
            .expect("pulled search index sx_a");
        assert!(sx.get("mappings").is_some(), "search index must carry mappings: {sx:?}");
        // Server-managed fields must be stripped at pull time (normalized shape).
        for junk in ["id", "status", "queryable", "latestDefinition"] {
            assert!(sx.get(junk).is_none(), "server field '{junk}' must be stripped: {sx:?}");
        }
    }

    // --- Phase 2: push create (add ix_b locally) ---
    let mut idx2 = idx.clone();
    idx2["regular"].as_array_mut().unwrap().push(json!({ "name": "ix_b", "key": { "b": -1 } }));
    write_indexes(&project, &slug, &idx2);
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "push-create failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_b", true).await.expect("ix_b created");
    // Idempotent: a second sync makes no further changes.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "re-sync after create failed: {}", String::from_utf8_lossy(&out.stderr));
    let (_slug, idx_after) = read_indexes(&project, &run_id);
    assert!(regular_names(&idx_after).contains(&"ix_b".to_string()), "ix_b must persist after idempotent re-sync");

    // --- Phase 3: push modify (change ix_b's key) ---
    let (slug, mut idx3) = read_indexes(&project, &run_id);
    for ix in idx3["regular"].as_array_mut().unwrap() {
        if ix.get("name").and_then(|n| n.as_str()) == Some("ix_b") {
            ix["key"] = json!({ "b": 1 }); // -1 -> 1
        }
    }
    write_indexes(&project, &slug, &idx3);
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "push-modify failed: {}", String::from_utf8_lossy(&out.stderr));
    // After drop+recreate, ix_b exists with the new key.
    raw.wait_for_regular_index(&coll, "ix_b", true).await.expect("ix_b present after modify");
    let remote = raw.ds_client().list_indexes(&coll, None).await.expect("list");
    let ix_b = remote.iter().find(|ix| ix.get("name").and_then(|n| n.as_str()) == Some("ix_b")).expect("ix_b");
    assert_eq!(ix_b.get("key"), Some(&json!({ "b": 1 })), "ix_b key not updated: {ix_b:?}");
    assert!(remote_regular_names(&raw, &coll).await.contains(&"ix_a".to_string()), "ix_a must survive modify of ix_b");

    // --- Phase 4: gated safe-delete (remove ix_b locally) ---
    let (slug, mut idx4) = read_indexes(&project, &run_id);
    idx4["regular"]
        .as_array_mut()
        .unwrap()
        .retain(|ix| ix.get("name").and_then(|n| n.as_str()) != Some("ix_b"));
    write_indexes(&project, &slug, &idx4);
    // Without --allow-deletes (non-interactive): must REFUSE and leave ix_b.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(!out.status.success(), "sync without --allow-deletes should fail on a removal");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("allow-deletes"), "expected allow-deletes refusal: {combined}");
    assert!(
        remote_regular_names(&raw, &coll).await.contains(&"ix_b".to_string()),
        "ix_b must survive the refused delete"
    );
    // With --allow-deletes: ix_b is dropped.
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "push-delete failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_b", false).await.expect("ix_b dropped");

    // --- Phase 5: admin-added survives ---
    // Out-of-band create ix_admin (not in base, not in local). Make an
    // unrelated local change so the dataset is dirty, then sync --allow-deletes.
    raw.ds_client()
        .create_index(&coll, "ix_admin", &json!({ "z": 1 }), &json!({}), None)
        .await
        .expect("create ix_admin");
    raw.wait_for_regular_index(&coll, "ix_admin", true).await.expect("ix_admin present");
    let (slug, mut idx5) = read_indexes(&project, &run_id);
    idx5["regular"].as_array_mut().unwrap().push(json!({ "name": "ix_c", "key": { "c": 1 } }));
    write_indexes(&project, &slug, &idx5);
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "admin-added sync failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_c", true).await.expect("ix_c created");
    let after = remote_regular_names(&raw, &coll).await;
    assert!(after.contains(&"ix_admin".to_string()), "admin-added index must survive: {after:?}");
    assert!(after.contains(&"ix_a".to_string()), "ix_a must survive all syncs: {after:?}");

    // --- Phase 6: idempotency (final re-sync = no error, stable) ---
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "final re-sync failed: {}", String::from_utf8_lossy(&out.stderr));

    // Search index (if seeded) must have survived every sync — a broken
    // normalize/equivalence check would have drop+recreated it each cycle.
    if has_search {
        let search_names: Vec<String> = raw
            .ds_client()
            .list_search_indexes(&coll, None)
            .await
            .expect("list search")
            .iter()
            .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        assert!(search_names.contains(&"sx_a".to_string()), "sx_a must survive: {search_names:?}");
    }

    drop(teardown);
}

/// Regression: a local dataset whose collection does NOT exist on the env yet
/// must have its collection + indexes CREATED on sync. Before the fix the
/// deploy iterated only server-listed collections, so a brand-new local
/// dataset was silently skipped and its indexes never created ("MDH indexes
/// are not being created when there are no relevant collections existing
/// yet"). The dataset is authored by hand — there is nothing remote to pull —
/// then synced; the collection + index must appear on the server. Strictly
/// additive: it only creates a per-run throwaway collection, never a real one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_mdh_creates_collection_when_absent() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let coll = mdh_collection_name(&run_id);
    // The on-disk dataset slug is what rdc derives from the collection name.
    let slug = rdc::slug::slugify(&coll);
    let raw = MdhRaw::connect(&cfg).expect("connect mdh");

    // MDH must be provisioned on the target org; skip cleanly otherwise (a
    // non-MDH cluster 404s the collection listing).
    if raw.list_collection_names().await.is_err() {
        eprintln!("SKIP live_mdh_creates_collection_when_absent: MDH not available on this org");
        return;
    }

    // Teardown FIRST (drops the collection on any panic).
    let teardown = Teardown::with_mdh(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
        cfg.clone(),
    );

    // Precondition: the collection does not exist yet — this is the whole point.
    let before = raw.list_collection_names().await.expect("list collections");
    assert!(!before.contains(&coll), "precondition: {coll} must not exist yet");

    // Author a local-only dataset: a name manifest (nothing remote to pull, so
    // the name — which the lossy slug cannot recover — must be persisted) plus
    // one regular index.
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let dataset_dir = project.path().join(format!("envs/test/mdh/{slug}"));
    std::fs::create_dir_all(&dataset_dir).expect("create dataset dir");
    std::fs::write(
        dataset_dir.join("collection.json"),
        format!("{{\n  \"name\": \"{coll}\"\n}}\n"),
    )
    .expect("write collection.json");
    std::fs::write(
        dataset_dir.join("indexes.json"),
        serde_json::to_vec_pretty(&json!({
            "regular": [{ "name": "ix_new", "key": { "a": 1 } }],
            "search": []
        }))
        .unwrap(),
    )
    .expect("write indexes.json");

    // Sync: stage 2 must create the collection + its index.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "sync failed: {}", String::from_utf8_lossy(&out.stderr));

    // The collection now exists on the server and carries ix_new.
    let after = raw.list_collection_names().await.expect("list collections");
    assert!(after.contains(&coll), "collection {coll} must be created: {after:?}");
    raw.wait_for_regular_index(&coll, "ix_new", true).await.expect("ix_new created");

    // Idempotent: a second sync makes no further changes and still succeeds.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(
        out.status.success(),
        "idempotent re-sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        remote_regular_names(&raw, &coll).await.contains(&"ix_new".to_string()),
        "ix_new must persist after idempotent re-sync"
    );

    drop(teardown);
}
