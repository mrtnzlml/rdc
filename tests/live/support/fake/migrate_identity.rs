//! A migrate report names each file's object with the identity the sync plan
//! and `skipped` use for the same object, so a host can map a file it edits
//! to what a later sync reports. Checked against both: the source env's
//! lockfile (what `skipped` and the classifier key on) and the target's
//! dry-run plan.

use super::crash::yes;
use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use rdc::cli::sync::embed::{EmbedSyncOptions, MirrorMode};
use rdc::cli::sync::plan::PlanChange as C;
use serde_json::json;
use std::collections::BTreeSet;

fn assert_ok(out: &std::process::Output, ctx: &str) {
    assert!(out.status.success(), "{ctx}:\n{}", String::from_utf8_lossy(&out.stderr));
}

/// The sync plan / `skipped` identity of a lockfile kind.
fn sync_kind(kind: &str) -> &str {
    match kind {
        "mdh_indexes" | "mdh_data" => "mdh",
        k => k,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_file_names_its_object_as_sync_does() {
    let (source, target) = (FakeOrg::start_with_org(1).await, FakeOrg::start_with_org(2).await);
    for f in [&source, &target] {
        f.enable_data_storage();
    }
    let project = ProjectFixture::init_envs(&[("test", &source.creds()), ("prod", &target.creds())]).expect("init");
    let run_id = RunId::new();
    write_snapshot(&project, "test", &run_id, &source.state().org_url());
    project.write_json("envs/test/mdh/vendors/collection.json", &json!({ "name": "vendors", "data": "manual" }));
    project.write_json("envs/test/mdh/vendors/indexes.json", &json!({ "regular": [], "search": [] }));
    std::fs::write(project.path().join("envs/test/mdh/vendors/data.jsonl"), "{\"_id\": \"r1\", \"code\": \"a\"}\n").unwrap();
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "pushing the source");
    // A workflow and its step, pulled (workflows are pull-only).
    {
        // Read-only kinds: put straight into the fake's store.
        let mut st = source.state();
        let (wf_url, step_url) = (st.url("workflows", 900_001), st.url("workflow_steps", 900_002));
        let org = st.org_url();
        let wf = json!({ "id": 900_001, "url": wf_url, "name": "approval", "organization": org, "steps": [step_url],
            "modified_at": "2026-01-01T00:00:00Z" });
        let step = json!({ "id": 900_002, "url": step_url, "name": "first check", "workflow": wf_url,
            "modified_at": "2026-01-01T00:00:00Z" });
        st.objects.entry("workflows").or_default().insert(900_001, wf);
        st.objects.entry("workflow_steps").or_default().insert(900_002, step);
    }
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "pulling the source back");
    // The target holds one label of its own, which `--mirror` prunes.
    {
        let mut st = target.state();
        let org = st.org_url();
        st.create("labels", json!({ "name": "target-only", "organization": org, "color": "#000000" })).unwrap();
    }
    assert_ok(&project.run_rdc(&["sync", "prod", "--yes"]), "reading the target");

    let log = rdc::log::Log::new(rdc::cli::resolve::ColorMode::Plain);
    let report = rdc::cli::migrate::run_at_logged(
        project.path(),
        "test",
        "prod",
        MirrorMode::Mirror { allow_recreate: false },
        false,
        vec![],
        rdc::cli::migrate::Carry::NONE,
        log,
    )
    .expect("migrate");

    // Every file the run considered names an object, and that object is one
    // the source lockfile records under the same identity (no mapping, so
    // slugs carry over).
    let recorded: BTreeSet<(String, String)> = load_lockfile(project.path(), "test")
        .unwrap()
        .objects
        .iter()
        .flat_map(|(k, m)| m.keys().map(move |s| (sync_kind(k).to_string(), s.clone())))
        .collect();
    let mut named: BTreeSet<(String, String)> = BTreeSet::new();
    for f in &report.files {
        let (kind, slug) = f.object.clone().unwrap_or_else(|| panic!("{} names no object", f.target.display()));
        assert!(recorded.contains(&(kind.clone(), slug.clone())), "{} -> {kind}/{slug}, not in {recorded:?}", f.target.display());
        named.insert((kind, slug));
    }
    for want in [
        ("hooks", format!("{}validator", run_id.list_prefix())),
        ("mdh", "vendors".to_string()),
        ("workflows", "approval".to_string()),
        ("workflow_steps", "approval/first-check".to_string()),
    ] {
        assert!(named.contains(&(want.0.to_string(), want.1.clone())), "{want:?} missing from {named:?}");
    }
    let sidecar = report.files.iter().find(|f| f.target.extension().is_some_and(|e| e == "py")).expect("a hook sidecar");
    assert_eq!(sidecar.object.as_ref().unwrap().0, "hooks", "a sidecar belongs to its hook");
    let objects: BTreeSet<(String, String)> = report.objects.iter().map(|o| (o.kind.clone(), o.slug.clone())).collect();
    let pruned_ids: BTreeSet<(String, String)> = report.pruned.iter().filter_map(|p| p.object.clone()).collect();
    let both: BTreeSet<(String, String)> = named.union(&pruned_ids).cloned().collect();
    assert_eq!(objects, both, "the objects are the files' and the prunes' objects");

    // And the target's dry-run plan names the same objects.
    let prod = project.path();
    let out = rdc::state::store::scope(
        Arc::new(rdc::state::store::FsStateStore),
        rdc::cli::sync::embed::sync_logged(prod, "prod", "fake-token", EmbedSyncOptions { dry_run: true, ..yes(false) }, Box::new(std::io::sink())),
    )
    .await
    .unwrap();
    let plan_items = out.plan.unwrap().items;
    let planned: BTreeSet<(String, String)> = plan_items.iter().map(|i| (i.kind.clone(), i.slug.clone())).collect();
    for p in &planned {
        assert!(named.contains(p) || pruned_ids.contains(p), "the plan's {p:?} is no migrated file's object: {named:?}");
    }
    // Every pruned file names an object the target's lockfile records, and
    // the plan deletes it under that identity.
    let target_recorded: BTreeSet<(String, String)> = load_lockfile(project.path(), "prod")
        .unwrap()
        .objects
        .iter()
        .flat_map(|(k, m)| m.keys().map(move |s| (sync_kind(k).to_string(), s.clone())))
        .collect();
    assert!(!report.pruned.is_empty(), "the mirror pruned the target's own label");
    let plan_deletes: BTreeSet<(String, String)> =
        plan_items.iter().filter(|i| i.change == C::Delete).map(|i| (i.kind.clone(), i.slug.clone())).collect();
    for p in &report.pruned {
        let id = p.object.clone().unwrap_or_else(|| panic!("{} names no object", p.target.display()));
        assert!(target_recorded.contains(&id), "{id:?} not in the target's lockfile {target_recorded:?}");
        assert!(plan_deletes.contains(&id), "{id:?} is not a planned delete: {plan_deletes:?}");
    }
    assert!(report.pruned.iter().any(|p| p.object == Some(("labels".into(), "target-only".into()))));

    // Every object the migrate changed, but the pull-only workflows, is in it.
    for o in report.objects.iter().filter(|o| o.change != rdc::cli::migrate::ObjectChange::Unchanged) {
        if o.kind.starts_with("workflow") {
            continue;
        }
        assert!(planned.contains(&(o.kind.clone(), o.slug.clone())), "{}/{} is not in the target's plan {planned:?}", o.kind, o.slug);
    }
}
