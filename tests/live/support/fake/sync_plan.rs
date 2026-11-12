//! A sync's structured report: the plan a dry run returns, and the objects a
//! run did not write. One env with one label per case, changed on either or
//! both sides.

use super::crash::{sync_in_process, yes};
use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::project::ProjectFixture;
use rdc::cli::sync::embed::{EmbedSyncOptions, Prompt, PromptKind, PromptRoute};
use rdc::cli::sync::plan::{PlanChange as C, PlanDirection as D, PlanPrompt as P, SkipReason};
use serde_json::json;

const TOKEN: &str = "fake-token";

fn fs() -> Arc<dyn rdc::state::store::StateStore> {
    Arc::new(rdc::state::store::FsStateStore)
}

fn assert_ok(out: &std::process::Output, ctx: &str) {
    assert!(out.status.success(), "{ctx}:\n{}", String::from_utf8_lossy(&out.stderr));
}

/// Labels `edited`, `gone`, `remote`, `both`, `lost` on the env and pulled;
/// then: `edited` changed locally, `gone` deleted locally, `remote` changed
/// on the env, `both` changed on both sides, `lost` edited locally and
/// deleted on the env, `new` created locally and `theirs` on the env.
async fn scenario() -> (FakeOrg, ProjectFixture) {
    let fake = FakeOrg::start().await;
    let org = fake.state().org_url();
    for name in ["edited", "gone", "remote", "both", "lost"] {
        fake.state().create("labels", json!({ "name": name, "organization": org, "color": "#000000" })).unwrap();
    }
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "the first pull");
    let ids = load_lockfile(project.path(), "test").unwrap().objects["labels"].clone();
    let local = |name: &str, color: &str| {
        let rel = format!("envs/test/labels/{name}.json");
        let mut v = project.read_json(&rel);
        v["color"] = json!(color);
        project.write_json(&rel, &v);
    };
    local("edited", "#111111");
    local("both", "#222222");
    local("lost", "#333333");
    std::fs::remove_file(project.path().join("envs/test/labels/gone.json")).unwrap();
    project.write_json("envs/test/labels/new.json", &json!({ "name": "new", "organization": org, "color": "#444444" }));
    fake.state().patch("labels", ids["remote"].id, &json!({ "color": "#555555" })).unwrap();
    fake.state().patch("labels", ids["both"].id, &json!({ "color": "#666666" })).unwrap();
    fake.state().delete("labels", ids["lost"].id).unwrap();
    fake.state().create("labels", json!({ "name": "theirs", "organization": org, "color": "#777777" })).unwrap();
    (fake, project)
}

type Row = (String, D, C, Option<P>);

fn rows(plan: &rdc::cli::sync::plan::SyncPlan) -> Vec<Row> {
    let mut out: Vec<Row> = plan.items.iter().map(|i| (i.slug.clone(), i.direction, i.change, i.prompt)).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_returns_its_plan() {
    let (fake, project) = scenario().await;
    let before = std::fs::read(project.path().join(".rdc/state/test.lock.json")).unwrap();

    let out = sync_in_process(project.path(), TOKEN, EmbedSyncOptions { dry_run: true, ..yes(false) }, fs()).await.unwrap();

    let plan = out.plan.expect("a dry run returns its plan");
    let want: Vec<Row> = vec![
        ("both".into(), D::Both, C::Conflict, Some(P::Conflict)),
        ("edited".into(), D::Push, C::Update, None),
        ("gone".into(), D::Push, C::Delete, Some(P::DeleteGate)),
        ("lost".into(), D::Both, C::Conflict, Some(P::RemoteDelete)),
        ("new".into(), D::Push, C::Create, None),
        ("remote".into(), D::Pull, C::Update, None),
        ("theirs".into(), D::Pull, C::Create, None),
    ];
    assert_eq!(rows(&plan), want);
    assert!(plan.items.iter().all(|i| i.kind == "labels"), "{:?}", plan.items);
    assert!(plan.recovered.is_empty() && plan.problems.is_empty(), "{plan:?}");
    assert!(out.skipped.is_empty());
    assert_eq!(std::fs::read(project.path().join(".rdc/state/test.lock.json")).unwrap(), before, "a dry run writes nothing");
    assert_eq!(fake.state().ids("labels").len(), 5, "nor does it touch the env");

    // With the flags that waive the prompts, the plan says so.
    let waived = EmbedSyncOptions {
        dry_run: true,
        allow_deletes: true,
        conflict: Some(rdc::cli::resolve::ConflictStrategy::Skip),
        ..yes(false)
    };
    let plan = sync_in_process(project.path(), TOKEN, waived, fs()).await.unwrap().plan.unwrap();
    assert!(plan.items.iter().all(|i| i.prompt.is_none()), "{:?}", plan.items);

    // A run that is not a dry run returns no plan.
    let audit = EmbedSyncOptions { no_push: true, ..yes(false) };
    assert!(sync_in_process(project.path(), TOKEN, audit, fs()).await.unwrap().plan.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_runs_problems_are_its_block_lines() {
    let (_fake, project) = scenario().await;
    std::fs::write(project.path().join("envs/test/labels/new.json"), b"{ not json").unwrap();
    let out = sync_in_process(project.path(), TOKEN, EmbedSyncOptions { dry_run: true, ..yes(false) }, fs()).await.unwrap();
    let problems = out.plan.unwrap().problems;
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("- labels/new -- ") && problems[0].contains("new.json"), "{problems:?}");
}

/// Answers by prompt kind, and records the kinds it was asked.
struct ByKind {
    asked: Mutex<Vec<PromptKind>>,
}

impl PromptRoute for ByKind {
    fn ask<'a>(&'a self, p: &'a Prompt) -> rdc::rt::BoxFuture<'a, Option<String>> {
        self.asked.lock().unwrap().push(p.kind);
        let answer = match p.kind {
            PromptKind::Conflict | PromptKind::RemoteDelete => Some("s".to_string()),
            PromptKind::DeleteGate => Some("n".to_string()),
            _ => None,
        };
        Box::pin(async move { answer })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_reports_what_it_did_not_write() {
    let (fake, project) = scenario().await;
    let route = Arc::new(ByKind { asked: Mutex::default() });
    let interactive = EmbedSyncOptions { interactive: true, ..yes(false) };

    let out = rdc::cli::sync::embed::with_route(route.clone(), sync_in_process(project.path(), TOKEN, interactive, fs()))
        .await
        .unwrap();

    let mut skipped: Vec<(String, SkipReason)> = out.skipped.iter().map(|s| (s.slug.clone(), s.reason)).collect();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![
            ("both".to_string(), SkipReason::ConflictSkipped),
            ("gone".to_string(), SkipReason::DeleteGateDeclined),
            ("lost".to_string(), SkipReason::RemoteDeleteSkipped),
        ],
        "asked {:?}",
        route.asked.lock().unwrap()
    );
    assert!(out.skipped.iter().all(|s| s.kind == "labels"));
    assert!(out.plan.is_none());
    assert_eq!(fake.state().ids("labels").len(), 6, "`gone` is still on the env, `new` was created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflict_strategy_skip_is_reported_too() {
    let (_fake, project) = scenario().await;
    let opts = EmbedSyncOptions { allow_deletes: true, conflict: Some(rdc::cli::resolve::ConflictStrategy::Skip), ..yes(true) };
    let out = sync_in_process(project.path(), TOKEN, opts, fs()).await.unwrap();
    let mut skipped: Vec<(String, SkipReason)> = out.skipped.iter().map(|s| (s.slug.clone(), s.reason)).collect();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![("both".to_string(), SkipReason::ConflictSkipped), ("lost".to_string(), SkipReason::RemoteDeleteSkipped)]
    );
}

/// The MDH plan is part of it: a local-only dataset is a push create.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plan_carries_the_mdh_plan() {
    let fake = FakeOrg::start().await;
    fake.enable_data_storage();
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "the first pull");
    project.write_json("envs/test/mdh/vendors/collection.json", &json!({ "name": "vendors" }));
    project.write_json(
        "envs/test/mdh/vendors/indexes.json",
        &json!({ "regular": [{ "key": { "code": 1 }, "name": "code_idx" }], "search": [] }),
    );
    let out = sync_in_process(project.path(), TOKEN, EmbedSyncOptions { dry_run: true, ..yes(false) }, fs()).await.unwrap();
    let items = out.plan.unwrap().items;
    let mdh: Vec<(&str, &str, D, C)> =
        items.iter().filter(|i| i.kind == "mdh").map(|i| (i.kind.as_str(), i.slug.as_str(), i.direction, i.change)).collect();
    assert_eq!(mdh, vec![("mdh", "vendors", D::Push, C::Create)], "{items:?}");
}
