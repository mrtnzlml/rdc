//! Cross-org promotion: per-env **overlays** and `--mirror` **deletion
//! promotion**, against two real organizations.
//!
//! `rdc migrate` itself is pure-local and zero-network, and its flags are
//! covered thoroughly by `tests/cli_migrate.rs`. What no mock can prove is what
//! happens once a migrated snapshot meets the API:
//!
//! * **Overlay survival.** Every env-tuned reconcile in the push path once
//!   clobbered `overlay.toml`'s value with the source env's, so the override
//!   held for exactly one cycle and then reverted. That is a two-cycle,
//!   real-server failure: the first `sync` looks perfect.
//! * **Deletion promotion.** Removing an object in `test` must travel to
//!   `prod` as `migrate --mirror` (prune the file) + `sync --allow-deletes`
//!   (DELETE it remotely). The prune is path-based, so a mapping regression
//!   shows up as either a missed prune or — much worse — a prune of something
//!   unrelated.
//!
//! This scenario REQUIRES a second organization (`RDC_LIVE_TGT_*`) and skips
//! without one. That is not fussiness: with both envs on one org they are two
//! views of the same objects, so the source env's objects appear in the
//! target's whole-org pull and `--mirror` correctly reads them as target-only
//! extras — the following `--allow-deletes` would then delete the SOURCE env's
//! objects. Deletion promotion is only meaningful across orgs.

use crate::support::assert_local::{load_lockfile, lockfile_keys, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The queue's `locale` as it stands on disk.
///
/// `locale` is the probe field on purpose. It is a plain per-env string that
/// `rdc` really pushes, and the overlay docs name it as a canonical use. The
/// obvious-looking alternative, `training_enabled`, is a trap: it sits in
/// `snapshot::noise::NOISE_FIELDS`, so rdc canonicalizes it away and never
/// pushes it — Rossum resets it to `false` on queue creation, which is exactly
/// why it is ignored. A test written on it asserts against a deliberate design
/// decision and fails for the wrong reason.
fn locale(project: &ProjectFixture, env: &str, q_slug: &str) -> Option<String> {
    let path = queue_file_path(project.path(), env, q_slug, "queue.json")
        .unwrap_or_else(|| panic!("queue.json not found for {env}/{q_slug}"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v.get("locale").and_then(|l| l.as_str()).map(str::to_string)
}

/// The queue's `automation_level` as it stands on disk.
///
/// A valid probe where `training_enabled` is not: `automation_level` is absent
/// from `snapshot::noise::NOISE_FIELDS`, so rdc hashes it, pushes it, and a
/// difference is a real diff — which is precisely why migrate carrying it
/// across organizations was worth fixing.
fn automation_level(project: &ProjectFixture, env: &str, q_slug: &str) -> Option<String> {
    let path = queue_file_path(project.path(), env, q_slug, "queue.json")
        .unwrap_or_else(|| panic!("queue.json not found for {env}/{q_slug}"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v.get("automation_level").and_then(|l| l.as_str()).map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_migrate_overlay_and_mirror() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let Some(tgt) = cfg.target.clone() else {
        eprintln!("{}", LiveConfig::skip_reason_target());
        return;
    };
    let run_id = RunId::new();
    let src_client = LiveClient::connect(&cfg).expect("connect (source)");
    let tgt_client = LiveClient::connect_creds(&tgt).expect("connect (target)");

    // One guard PER ORG: the run creates objects in both, and each org's
    // teardown only sees its own.
    let teardown_src = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());
    let teardown_tgt = Teardown::new(
        LiveClient::connect_creds(&tgt).unwrap(),
        run_id.clone(),
    );

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&src_client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed source org");

    let project =
        ProjectFixture::init_envs(&[("test", &cfg.source()), ("prod", &tgt)]).expect("init");
    let prefix = run_id.list_prefix();
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull test failed: {}", combined(&pull));
    assert_converged(&project, "test", &prefix, "after the initial pull of test");

    // Scope every migrate to the objects THIS RUN owns. The source org is a
    // live one with unrelated content (and unrelated MDH datasets other people
    // are changing); promoting all of it would be slow, would copy strangers'
    // data into the target org, and would fail on drift that has nothing to do
    // with the thing under test. `*` matches any kind; the slug pattern anchors
    // on the run prefix.
    let only = format!("*/{prefix}*");

    // No mapping file: across orgs the slugs are free to be identical, which is
    // what a real promotion looks like, and migrate auto-matches them. (The
    // explicit rename mapping has its own coverage in `deploy_flow`.)
    let lf_test = load_lockfile(project.path(), "test").expect("test lockfile");
    let q_slug = lockfile_keys(&lf_test, "queues")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("a test queue slug for this run");

    // -------------------------------------------------------------------------
    // Overlay: promote, then override one field for prod only.
    // -------------------------------------------------------------------------
    // Drive the override off the SOURCE env's real, pulled value rather than a
    // hardcoded default: the point is that the overlay wins over whatever
    // migrate would otherwise produce, whichever way the server's default points.
    let source_value = locale(&project, "test", &q_slug)
        .expect("the pulled test queue must carry an explicit locale");
    let overridden = if source_value == "en_US" { "en_GB" } else { "en_US" }.to_string();

    std::fs::create_dir_all(project.path().join("envs/prod")).ok();
    std::fs::write(
        project.path().join("envs/prod/overlay.toml"),
        format!("version = 1\n\n[queues.\"{q_slug}\"]\nlocale = \"{overridden}\"\n"),
    )
    .expect("writing envs/prod/overlay.toml");

    let m1 = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(m1.status.success(), "migrate with overlay failed: {}", combined(&m1));
    assert_eq!(
        locale(&project, "prod", &q_slug).as_deref(),
        Some(overridden.as_str()),
        "the prod overlay must win over the value migrated from test"
    );
    assert_eq!(
        locale(&project, "test", &q_slug).as_deref(),
        Some(source_value.as_str()),
        "a prod overlay must never write back into the source env"
    );

    let s1 = project.run_rdc(&["sync", "prod"]);
    assert!(s1.status.success(), "sync prod failed: {}", combined(&s1));

    let prod_qid = load_lockfile(project.path(), "prod")
        .expect("prod lockfile")
        .objects
        .get("queues")
        .and_then(|m| m.get(&q_slug))
        .unwrap_or_else(|| panic!("prod lockfile missing queue '{q_slug}'"))
        .id;
    let remote_q = tgt_client
        .find_listed_value("queue", prod_qid)
        .await
        .expect("list queues in the target org")
        .expect("the prod queue must exist remotely");
    assert_eq!(
        remote_q.get("locale").and_then(|l| l.as_str()),
        Some(overridden.as_str()),
        "the overlay value did not reach the prod remote: {remote_q:?}"
    );

    // A single cycle must settle: the same-pass back-ref refresh now covers
    // objects this cycle created, so `workspace.queues` / `schema.queues` /
    // `queue.inbox` land in this pass rather than the next one.
    assert_converged(&project, "prod", &prefix, "after pushing the overlaid queue");
    // Across orgs this IS assertable: promoting must not disturb the source.
    assert_converged(&project, "test", &prefix, "after deploying test -> prod");

    // The clobber: a second promotion cycle must NOT quietly restore the
    // source env's value over the overlay's.
    let m2 = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(m2.status.success(), "second migrate failed: {}", combined(&m2));
    assert_eq!(
        locale(&project, "prod", &q_slug).as_deref(),
        Some(overridden.as_str()),
        "a second migrate reverted the overlay to the source env's value — the \
         override survives exactly one cycle"
    );
    let s2 = project.run_rdc(&["sync", "prod"]);
    assert!(s2.status.success(), "second sync prod failed: {}", combined(&s2));
    assert_converged(&project, "prod", &prefix, "after a second overlaid promotion cycle");

    // -------------------------------------------------------------------------
    // Deletion promotion: delete in test, mirror to prod, delete in prod.
    // -------------------------------------------------------------------------
    // `post-validator` is chosen because it is a run_after LEAF — nothing
    // references it, so removing it leaves no dangling cross-ref.
    let hook_slug = lockfile_keys(&lf_test, "hooks")
        .into_iter()
        .find(|s| s.starts_with(&prefix) && s.contains("post"))
        .expect("the post-validator hook slug");

    let prod_hook_id = load_lockfile(project.path(), "prod")
        .expect("prod lockfile")
        .objects
        .get("hooks")
        .and_then(|m| m.get(&hook_slug))
        .unwrap_or_else(|| panic!("prod lockfile missing hook '{hook_slug}'"))
        .id;

    // Delete it in test — JSON and code sidecar together, or the orphaned
    // sidecar itself becomes a pre-flight failure.
    std::fs::remove_file(project.path().join(format!("envs/test/hooks/{hook_slug}.json")))
        .expect("test hook json must exist");
    std::fs::remove_file(project.path().join(format!("envs/test/hooks/{hook_slug}.py"))).ok();

    let dt = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(dt.status.success(), "deleting the test hook failed: {}", combined(&dt));
    assert_converged(&project, "test", &prefix, "after deleting the hook in test");

    // Without --mirror the promotion is additive: prod keeps its copy.
    let add = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(add.status.success(), "additive migrate failed: {}", combined(&add));
    assert!(
        project.exists(&format!("envs/prod/hooks/{hook_slug}.json")),
        "a migrate WITHOUT --mirror must leave target-only objects intact"
    );

    // With --mirror the prune reaches exactly that file.
    let mirror = project.run_rdc(&["migrate", "test", "prod", "--mirror", "--only", &only]);
    assert!(mirror.status.success(), "mirror migrate failed: {}", combined(&mirror));
    assert!(
        !project.exists(&format!("envs/prod/hooks/{hook_slug}.json")),
        "--mirror must prune the prod hook whose source no longer exists"
    );
    assert!(
        !project.exists(&format!("envs/prod/hooks/{hook_slug}.py")),
        "--mirror must prune the pruned hook's code sidecar too; an orphaned \
         sidecar wedges the next sync's pre-flight"
    );
    // The prune is path-based, so a mapping regression would take unrelated
    // objects with it. The queue must still be there.
    assert!(
        queue_file_path(project.path(), "prod", &q_slug, "queue.json").is_some(),
        "--mirror pruned an object it should not have"
    );
    // migrate is LOCAL: nothing may have been deleted remotely yet.
    assert!(
        tgt_client
            .find_listed_value("hook", prod_hook_id)
            .await
            .expect("list hooks after the mirror prune")
            .is_some(),
        "migrate --mirror must not delete anything on the remote by itself"
    );

    let dp = project.run_rdc(&["sync", "prod", "--allow-deletes"]);
    assert!(dp.status.success(), "deleting the prod hook failed: {}", combined(&dp));
    assert!(
        tgt_client
            .find_listed_value("hook", prod_hook_id)
            .await
            .expect("list hooks after the mirrored delete")
            .is_none(),
        "the mirrored deletion must reach the prod remote (hook id {prod_hook_id})"
    );
    assert_converged(&project, "prod", &prefix, "after promoting the deletion to prod");

    // -------------------------------------------------------------------------
    // Queue automation belongs to the target organization.
    // -------------------------------------------------------------------------
    // Turn automation on in the TARGET org only — the shape of a real prod env
    // that has earned automation its source env has not. Only a real pull can
    // produce the target snapshot the reconcile reads, which is why this is not
    // provable offline.
    tgt_client
        .patch_fields(
            "queue",
            prod_qid,
            serde_json::json!({ "automation_enabled": true, "automation_level": "confident" }),
        )
        .await
        .expect("enable automation on the target queue");
    let pull_prod = project.run_rdc(&["sync", "prod", "--no-push"]);
    assert!(pull_prod.status.success(), "pulling prod failed: {}", combined(&pull_prod));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        Some("confident"),
        "the pull must bring the target org's automation level onto disk"
    );

    let m5 = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(m5.status.success(), "automation migrate failed: {}", combined(&m5));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        Some("confident"),
        "migrate promoted the source org's automation level over the target's"
    );

    // Guard: the final assertion below only proves `--carry automation` did
    // something if the source's level is not already "confident" — that is
    // the value just patched into the target, so if the seed happened to
    // match it the comparison would pass whether or not `--carry` worked.
    let src_automation_level = automation_level(&project, "test", &q_slug);
    assert_ne!(
        src_automation_level.as_deref(),
        Some("confident"),
        "the seeded source queue must not already be \"confident\", or the \
         --carry automation assertion below would pass vacuously"
    );

    let m6 = project.run_rdc(&[
        "migrate", "test", "prod", "--only", &only, "--carry", "automation",
    ]);
    assert!(m6.status.success(), "--carry automation failed: {}", combined(&m6));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        automation_level(&project, "test", &q_slug).as_deref(),
        "--carry automation must promote the source org's value"
    );

    drop(teardown_tgt);
    drop(teardown_src);
}
