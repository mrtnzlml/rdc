//! Live coverage for the commands and flags the suite never invoked.
//!
//! Three things here genuinely need a real server:
//!
//! * **`rdc auth`** validates a token with `GET /organizations/{id}` *before*
//!   writing it. Whether a given string is accepted is the server's opinion, so
//!   a mock proves nothing — and the property that matters most on a bad token
//!   is negative: a rejected token must not overwrite a working one.
//! * **`rdc doctor`** is offline, but its slug realign rewrites cross-refs,
//!   moves base-cache sidecars and refreshes lockfile hashes. Whether it got
//!   all three right is only visible when the next `sync` against the real env
//!   still converges.
//! * **`--no-push` / `--no-pull`** are one-directional guarantees about a real
//!   remote: that the env was not written, and that local was not overwritten.

use crate::support::assert_local::{load_lockfile, lockfile_keys, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined, TreeSnapshot};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
///
/// The fake genuinely refuses a bad token rather than ignoring it, which is
/// what keeps the negative half of this scenario from asserting nothing:
/// `fake::authorized` compares the `authorization` header against
/// `token <fake::TOKEN>` and `fake::route` answers
/// `state::ApiError::unauthorized` (401, `{"detail": "Invalid token."}`)
/// before it looks at the path at all. `cli::auth::validate_token` validates
/// with `GET /organizations/<id>`, which is inside `/api/v1/` and therefore
/// behind exactly that check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_auth_validates_before_writing() {
    let fake = crate::support::fake::FakeOrg::start().await;
    auth_validates_before_writing(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_auth_validates_before_writing() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    auth_validates_before_writing(&cfg).await;
}

/// `rdc auth <env> --token` accepts a real token and REFUSES a bad one without
/// destroying the good credentials already on disk.
async fn auth_validates_before_writing(cfg: &LiveConfig) {
    // No remote objects are created, so no teardown guard is needed.
    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let secrets_rel = "secrets/test.secrets.json";

    // A valid token is accepted and persisted.
    let ok = project.run_rdc(&["auth", "test", "--token", &cfg.token]);
    assert!(
        ok.status.success(),
        "rdc auth with a valid token must succeed: {}",
        combined(&ok)
    );
    let saved = project
        .read_to_string(secrets_rel)
        .expect("secrets file must exist after auth");
    assert!(
        saved.contains(&cfg.token),
        "the validated token must be written to {secrets_rel}"
    );

    // A bad token is rejected by the SERVER, and the good one survives.
    let before = saved.clone();
    let bad = project.run_rdc(&["auth", "test", "--token", "definitely-not-a-real-token"]);
    assert!(
        !bad.status.success(),
        "rdc auth must fail on a token the server rejects: {}",
        combined(&bad)
    );
    let after = project
        .read_to_string(secrets_rel)
        .expect("secrets file must still exist after a rejected auth");
    assert_eq!(
        before, after,
        "a rejected token must NOT overwrite the working credentials already on disk"
    );
}

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_doctor_realign_after_a_remote_rename() {
    let fake = crate::support::fake::FakeOrg::start().await;
    doctor_realign_after_a_remote_rename(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_doctor_realign_after_a_remote_rename() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    doctor_realign_after_a_remote_rename(&cfg).await;
}

/// `rdc doctor` is a no-op on a freshly pulled snapshot, realigns a slug after
/// the object is renamed remotely, and leaves the env still converging.
async fn doctor_realign_after_a_remote_rename(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let prefix = run_id.list_prefix();
    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull failed: {}", combined(&pull));
    assert_converged(&project, "test", &prefix, "after the initial pull");

    // (1) Nothing to fix straight after a pull. `doctor` MUTATES by default,
    // so "no-op" is asserted on the bytes, not on the wording of its output.
    let before = TreeSnapshot::capture(project.path(), "test", &prefix);
    let clean = project.run_rdc(&["doctor", "test"]);
    assert!(clean.status.success(), "doctor failed: {}", combined(&clean));
    if let Some(d) = before.diff(&TreeSnapshot::capture(project.path(), "test", &prefix)) {
        panic!("doctor changed a freshly pulled snapshot — it should have found nothing:\n{d}");
    }

    // (2) Rename the queue remotely and re-pull. Slugs are id-pinned, so the
    // local slug deliberately does NOT follow the new name; realigning it is
    // exactly doctor's job.
    let qid = index.id("queue-invoices-main").expect("queue id");
    client
        .patch_name("queue", qid, &run_id.prefix("Invoices Renamed"))
        .await
        .expect("rename the queue remotely");
    let repull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(repull.status.success(), "re-pull failed: {}", combined(&repull));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();
    let old_slug = lf
        .slug_for_id("queues", qid)
        .unwrap_or_else(|| panic!("the renamed queue must still be tracked under its old slug"))
        .to_string();

    // (3) `--dry-run` previews without touching disk.
    let pre_dry = TreeSnapshot::capture(project.path(), "test", &prefix);
    let dry = project.run_rdc(&["doctor", "test", "--dry-run"]);
    assert!(dry.status.success(), "doctor --dry-run failed: {}", combined(&dry));
    if let Some(d) = pre_dry.diff(&TreeSnapshot::capture(project.path(), "test", &prefix)) {
        panic!("doctor --dry-run WROTE to disk:\n{d}");
    }

    // (4) The real run realigns the slug to the new name.
    let fix = project.run_rdc(&["doctor", "test"]);
    assert!(fix.status.success(), "doctor failed: {}", combined(&fix));

    let lf2 = load_lockfile(project.path(), "test").expect("lockfile after doctor");
    let new_slug = lf2
        .slug_for_id("queues", qid)
        .unwrap_or_else(|| panic!("the queue must still be tracked after realign"))
        .to_string();
    assert_ne!(new_slug, old_slug, "doctor must have realigned the slug to the new name");
    assert!(
        new_slug.starts_with(&prefix) && new_slug.contains("renamed"),
        "the realigned slug must derive from the new remote name; got '{new_slug}'"
    );
    assert!(
        queue_file_path(project.path(), "test", &new_slug, "queue.json").is_some(),
        "the queue tree must have moved to the realigned slug '{new_slug}'"
    );
    assert!(
        queue_file_path(project.path(), "test", &old_slug, "queue.json").is_none(),
        "the old queue directory '{old_slug}' must be gone after the realign"
    );

    // Cross-refs into the renamed queue must have been rewritten, not left
    // pointing at a slug that no longer exists.
    let qpath = queue_file_path(project.path(), "test", &new_slug, "queue.json").unwrap();
    let qv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&qpath).unwrap()).unwrap();
    let schema_ref = qv["schema"].as_str().expect("queue.schema");
    assert!(
        schema_ref.starts_with("rdc://schemas/"),
        "queue.schema must still be a portable ref after realign, got {schema_ref:?}"
    );
    assert!(
        !schema_ref.contains(&old_slug),
        "queue.schema still names the pre-realign slug '{old_slug}': {schema_ref}"
    );

    // (5) The realign must leave the env settled.
    //
    // The known cause of the lag here is FIXED: `refresh_lockfile_hashes`
    // recomputes hashes from base-cache bytes, and it framed a schema's
    // formula sidecars by bare `field_id` where every other path frames them
    // `formulas/<field_id>.py`. The recorded hash was therefore one no other
    // path could reproduce, so every realign silently dirtied each schema
    // carrying a formula and the next sync re-pulled it. Pinned now by
    // `realign::tests::base_sidecars_hash_matches_the_codec_*`.
    //
    // The realign moved the base cache and refreshed the lockfile hashes
    // correctly — otherwise the next cycle re-pulls or re-pushes forever.
    assert_converged(&project, "test", &prefix, "after doctor realigned the renamed queue");

    drop(teardown);
}

/// The label's `color` as the env currently has it.
async fn remote_color(client: &LiveClient, id: u64) -> String {
    client
        .find_listed_value("label", id)
        .await
        .expect("list labels")
        .expect("the label must exist remotely")
        .get("color")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_sync_direction_flags() {
    let fake = crate::support::fake::FakeOrg::start().await;
    sync_direction_flags(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_sync_direction_flags() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    sync_direction_flags(&cfg).await;
}

/// `--no-push` never writes to the env; `--no-pull` never overwrites local.
///
/// The `--no-pull` half needs one assertion the `--no-push` half does not, and
/// it was missing until this comment was written. "Local still reads
/// `#0a0a0a`" is true of a server that never applied the out-of-band drift
/// patch at all, so against such a backend the half passes while testing
/// nothing — verified during the fake port by making the fake drop exactly
/// that PATCH: green. The limitation was the scenario's, not the fake's; the
/// live twin had it too, and the only record of it was a commit message. The
/// fix is the `remote_color` check between the drift patch and the
/// `--no-pull` cycle below: once the env is known to hold `#0b0b0b`, local
/// still holding `#0a0a0a` afterwards is a real demonstration that the pull
/// was suppressed, because a normal cycle would have taken that remote
/// change (base and local agree, so it is not even a conflict).
async fn sync_direction_flags(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let prefix = run_id.list_prefix();
    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull failed: {}", combined(&pull));
    assert_converged(&project, "test", &prefix, "after the initial pull");

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let lslug = lockfile_keys(&lf, "labels")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("a label slug for this run");
    let lrel = format!("envs/test/labels/{lslug}.json");
    let lid = index.id("label-priority").expect("label id");

    let set_local_color = |color: &str| {
        let mut v: serde_json::Value =
            serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
        v["color"] = serde_json::Value::String(color.into());
        std::fs::write(
            project.path().join(&lrel),
            serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap();
    };
    let local_color = || -> String {
        let v: serde_json::Value =
            serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
        v["color"].as_str().unwrap_or_default().to_string()
    };

    // --- --no-push: a local edit must not reach the env ---
    let before_remote = remote_color(&client, lid).await;
    set_local_color("#0a0a0a");
    let audit = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(audit.status.success(), "sync --no-push failed: {}", combined(&audit));
    assert_eq!(
        remote_color(&client, lid).await,
        before_remote,
        "--no-push must not write the local edit to the env"
    );
    assert_eq!(
        local_color(),
        "#0a0a0a",
        "--no-push must also not revert the local edit; it is an audit, not a reset"
    );

    // Settle: a normal cycle pushes it, and converges.
    let settle = project.run_rdc(&["sync", "test"]);
    assert!(settle.status.success(), "settling sync failed: {}", combined(&settle));
    assert_eq!(remote_color(&client, lid).await, "#0a0a0a");
    assert_converged(&project, "test", &prefix, "after settling the --no-push edit");

    // --- --no-pull: a remote change must not overwrite local ---
    client
        .patch_fields("label", lid, serde_json::json!({ "color": "#0b0b0b" }))
        .await
        .expect("patch remote label");
    // The drift must actually exist before `--no-pull` can be shown to ignore
    // it — see this function's doc comment for what this half asserted (and
    // did not) without this line.
    assert_eq!(
        remote_color(&client, lid).await,
        "#0b0b0b",
        "the out-of-band drift patch did not land, so `--no-pull` would have nothing to \
         ignore and the assertion below would pass against an env that never changed"
    );
    let deploy = project.run_rdc(&["sync", "test", "--no-pull"]);
    assert!(deploy.status.success(), "sync --no-pull failed: {}", combined(&deploy));
    assert_eq!(
        local_color(),
        "#0a0a0a",
        "--no-pull must not overwrite the local file with the env's copy"
    );

    // Settle the other way: a normal cycle takes the remote change.
    let settle2 = project.run_rdc(&["sync", "test", "--conflict", "use-remote"]);
    assert!(settle2.status.success(), "settling sync failed: {}", combined(&settle2));
    assert_converged(&project, "test", &prefix, "after settling the --no-pull divergence");

    drop(teardown);
}
