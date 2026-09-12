use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined, TreeSnapshot};
use crate::support::mapping::{rename_mapping, write_mapping};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. Two independent `FakeOrg`s, paired via
/// `paired_config` — a real promotion needs `test` and `prod` in SEPARATE
/// orgs (see `deploy_flow`'s own doc comment below), and pointing both envs
/// at one org would quietly defeat every promotion assertion it makes.
///
/// This port found a fake bug the single-org ports structurally could not:
/// two orgs mean the same object shape is created twice from two different
/// bodies, and the fake used to let the create body decide the field order
/// it served back. `migrate` writes the source env's order into the target's
/// files, so the second `migrate` rewrote them with `locale` moved and the
/// chain-stability assertion below charged `rdc` for it. Fixed at the fake's
/// response seam — quirk
/// `field_order_is_a_property_of_the_kind_not_of_the_request`
/// (`quirks::impose_field_order`), pinned by
/// `fake::tests::one_list_response_orders_every_row_the_same_way`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_deploy_flow() {
    let src = crate::support::fake::FakeOrg::start_with_org(1).await;
    let tgt = crate::support::fake::FakeOrg::start_with_org(2).await;
    deploy_flow(&src.paired_config(&tgt)).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_deploy_flow() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    deploy_flow(&cfg).await;
}

/// Deploy flow: pull `test`, `rdc migrate test prod` (renames every object via
/// an explicit mapping so prod objects don't collide with test in the shared
/// org), `rdc sync prod` to push, then assert the prod lockfile recorded
/// `-prod` slugs and that the pushed queue's schema ref resolved to a real URL
/// on the remote.
///
/// `test` and `prod` live in SEPARATE organizations (`RDC_LIVE_TGT_*`), which
/// is what a promotion actually is; the scenario skips without a second org.
/// The renames are therefore not strictly necessary across orgs — they are
/// kept deliberately, because rewriting every slug and every `rdc://` ref
/// through an explicit mapping is the part of `migrate` most worth exercising.
///
/// The mapping is written in the CURRENT `.rdc/mapping.toml` N-way format —
/// the one every project uses today. (The legacy per-pair
/// `.rdc/map/<a>-to-<b>.toml` conversion has its own hermetic coverage in
/// `tests/cli_migrate.rs`; feeding it here would mean the live suite never
/// exercised the format real users actually commit.)
///
/// Two convergence properties are pinned on top of the one-shot flow, because
/// both have regressed in production before and neither is visible to the mock
/// suite:
///
/// * **Post-push convergence** — after `sync prod` lands the creates, a
///   second cycle must be a byte-for-byte no-op. Push write-back writing raw
///   server URLs to disk and the base cache not mirroring code sidecars both
///   showed up exactly here.
/// * **Chain stability** — running the whole `migrate && sync` chain a second
///   time must not move a single byte. This is the mirror-chain oscillation
///   class (stale-map prune, unique-typed template skip, MDH KeepLocal base
///   preservation), which by construction needs two full chains to detect.
async fn deploy_flow(cfg: &LiveConfig) {
    let Some(tgt) = cfg.target.clone() else {
        eprintln!("{}", LiveConfig::skip_reason_target());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect (source)");
    let tgt_client = LiveClient::connect_creds(&tgt).expect("connect (target)");
    // One teardown guard PER ORG — the run creates objects in both, and each
    // org's sweep only sees its own.
    let teardown_src = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());
    let teardown_tgt =
        Teardown::new(LiveClient::connect_creds(&tgt).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    // Init project with each env pointing at its OWN org — a real promotion.
    let project =
        ProjectFixture::init_envs(&[("test", &cfg.source()), ("prod", &tgt)]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "sync test --no-push failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    let prefix = run_id.list_prefix();

    // A pure pull must already have converged: nothing to push back, nothing
    // to re-pull, no phantom drift. A pull that doesn't settle here is the
    // first-run-spurious-conflict class (74af7eb).
    assert_converged(&project, "test", &prefix, "after the initial pull of test");

    // Build the test->prod rename mapping from the pulled test lockfile, in
    // the generic N-way format: one `[[<kind>]]` row per object, naming this
    // object's slug in each env it exists in.
    //
    // Keys are flat leaf slugs (verified: flat workspace slug, flat queue-leaf
    // slug for queues/schemas/inboxes, flat slug for hooks/rules/labels).
    let lf_test = load_lockfile(project.path(), "test").expect("test lockfile");
    let prefix = run_id.list_prefix();
    write_mapping(
        project.path(),
        &rename_mapping(&lf_test, &prefix, "test", "prod", "-prod"),
    );

    // Scope every migrate to the objects THIS RUN owns.
    //
    // The sandbox is a live org: other people's MDH datasets change while the
    // suite runs, and a whole-snapshot migrate copies their `indexes.json` into
    // the target too. When the remote has since gained an index, the next
    // `sync` sees a deletion it was never asked to make and refuses without
    // `--allow-deletes` — a failure with nothing to do with the thing under
    // test, and one that must NEVER be "fixed" by passing that flag here.
    // `*` matches any kind and the slug pattern anchors on the run prefix.
    let only = format!("*/{prefix}*");

    // migrate (pure local rename) then sync prod (push to remote).
    let mg = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(
        mg.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&mg.stderr)
    );

    let sp = project.run_rdc(&["sync", "prod"]);
    assert!(
        sp.status.success(),
        "sync prod failed: {}",
        String::from_utf8_lossy(&sp.stderr)
    );

    // Verify via prod lockfile + remote ref resolution. migrate renames SLUGS,
    // not display names, so remote objects still carry the original
    // `rdc-it-<id>-` names. Assertions:
    //   1. Prod lockfile has at least one queue slug containing "-prod".
    //   2. That queue's schema cross-ref resolved to a real HTTP URL remotely.
    let lf_prod = load_lockfile(project.path(), "prod").expect("prod lockfile");
    let prod_queue_slugs = lockfile_keys(&lf_prod, "queues");

    let prod_slug = prod_queue_slugs
        .iter()
        .find(|s| s.contains("-prod"))
        .unwrap_or_else(|| {
            panic!(
                "prod lockfile must contain at least one queue slug with '-prod'; got: {:?}",
                prod_queue_slugs
            )
        });

    let prod_queue_id = lf_prod
        .objects
        .get("queues")
        .and_then(|m| m.get(prod_slug))
        .unwrap_or_else(|| panic!("prod lockfile missing entry for queue slug '{prod_slug}'"))
        .id;

    assert_remote_ref_resolved(&tgt_client, "queue", prod_queue_id, "schema")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "prod queue {prod_queue_id} (slug '{prod_slug}') schema ref not resolved: {e:#}"
            )
        });

    // --- convergence: a fresh-env deploy settles in ONE cycle ---
    //
    // It did not always. Creating a child makes the server fill in the other
    // side of the link, and the create-push writes back the POST response,
    // which predates the child — so the first `sync prod` used to leave
    // `workspace.queues` / `schema.queues` empty and `queue.inbox` absent, and
    // the deploy needed a second cycle to settle. Both halves are fixed now:
    // the same-pass back-ref refresh covers objects this cycle CREATED (it had
    // been restricted to `Clean` ones, which excluded exactly the queue that
    // needed it) and reaches workspaces and schemas, and the create/adopt
    // write-backs mirror to the base cache like the PATCH paths already did.
    // This assertion is what keeps it that way.
    assert_converged(&project, "prod", &prefix, "after a single sync prod (creates)");
    assert_converged(&project, "test", &prefix, "after deploying test -> prod");

    // --- chain stability: migrate && sync, a second time, moves no bytes ---
    let prod_before = TreeSnapshot::capture(project.path(), "prod", &prefix);
    let mg2 = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(
        mg2.status.success(),
        "second migrate failed: {}",
        combined(&mg2)
    );
    let prod_after_migrate = TreeSnapshot::capture(project.path(), "prod", &prefix);
    if let Some(d) = prod_before.diff(&prod_after_migrate) {
        panic!(
            "re-running `migrate test prod` on an already-migrated tree changed files — \
             migrate is not idempotent:\n{d}"
        );
    }

    let sp2 = project.run_rdc(&["sync", "prod"]);
    assert!(sp2.status.success(), "second sync prod failed: {}", combined(&sp2));
    let prod_after_chain = TreeSnapshot::capture(project.path(), "prod", &prefix);
    if let Some(d) = prod_before.diff(&prod_after_chain) {
        panic!(
            "a second `migrate && sync` chain rewrote the prod snapshot — the chain \
             oscillates rather than converging:\n{d}"
        );
    }
    assert_converged(&project, "prod", &prefix, "after a second migrate && sync chain");

    drop(teardown_tgt);
    drop(teardown_src);
}
