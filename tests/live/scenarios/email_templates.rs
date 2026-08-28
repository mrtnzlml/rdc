//! Live coverage for the `email_templates` kind.
//!
//! This kind had full `LiveClient` support and *no* scenario, which mattered
//! more than the bare gap suggests: its worst production bug was silent data
//! loss. Two local templates sharing a name on one queue both adopted the
//! first matching remote id, orphaning the sibling (deleted outright under
//! `--allow-deletes`) and leaving the surviving binding permanently
//! mismatched. `pick_adoption_id`'s `claimed` set is the fix, and it can only
//! be proven where names really do collide on a real queue.
//!
//! Templates are created out-of-band rather than through the shared manifest
//! so the rest of the suite's seed graph — and its committed goldens — stay
//! untouched. Only `custom` and `rejection` templates can be created or
//! deleted through the API; the queue's other typed defaults are
//! system-managed, arrive on their own, and are exercised here as the
//! match-by-`type` half of adoption.

use crate::support::assert_local::{load_lockfile, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Every template file under the run's queue, as (path, parsed json), for the
/// templates this run created (i.e. whose name carries the run-id prefix).
/// System-managed defaults on the same queue are filtered out by the prefix.
fn our_templates(
    project: &ProjectFixture,
    q_slug: &str,
    prefix: &str,
) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    let dir = queue_file_path(project.path(), "test", q_slug, "email-templates")
        .unwrap_or_else(|| panic!("no email-templates/ dir under queue {q_slug}"));
    let mut out: Vec<(std::path::PathBuf, serde_json::Value)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            let ours = v
                .get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.starts_with(prefix));
            ours.then(|| (e.path(), v))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn ids_of(templates: &[(std::path::PathBuf, serde_json::Value)]) -> Vec<u64> {
    let mut v: Vec<u64> = templates
        .iter()
        .filter_map(|(_, t)| t.get("id").and_then(|i| i.as_u64()))
        .collect();
    v.sort_unstable();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_email_templates_round_trip() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST — the templates carry the run-id name prefix, so
    // `teardown_by_prefix` reaches them (it deletes email_template before
    // queue, which is the order the API needs).
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let queue_url = index
        .url("queue", "queue-invoices-main")
        .expect("seeded queue url")
        .to_string();

    // --- three custom templates on ONE queue, two deliberately same-named ---
    // `type` defaults to "custom"; it is set explicitly so the adoption path
    // under test is the match-by-NAME branch, not match-by-type.
    let solo_name = run_id.prefix("Dispute Notice");
    let shared_name = run_id.prefix("Shared Name");
    let mk = |name: &str, subject: &str| {
        serde_json::json!({
            "name": name,
            "type": "custom",
            "subject": subject,
            "message": "<p>Please review the attached document.</p>",
            "automate": false,
            "queue": queue_url,
        })
    };
    let (solo_id, _) = client
        .create("email_template", &mk(&solo_name, "Subject A"))
        .await
        .expect("create the solo template");
    let (dup_a_id, _) = client
        .create("email_template", &mk(&shared_name, "Subject B"))
        .await
        .expect("create the first same-named template");
    let (dup_b_id, _) = client
        .create("email_template", &mk(&shared_name, "Subject C"))
        .await
        .expect("create the second same-named template");
    let mut created_ids = vec![solo_id, dup_a_id, dup_b_id];
    created_ids.sort_unstable();
    assert_eq!(
        created_ids.len(),
        3,
        "the API must accept two templates with the same name on one queue; \
         if it does not, this scenario's premise is wrong"
    );

    // --- pull ---
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull failed: {}", combined(&pull));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();
    // Resolve the owning queue by ID, not by picking the first prefixed slug:
    // the seed creates TWO queues both named "Invoices", so their slugs are
    // globally deduped (`…-invoices`, `…-invoices-2`) and which physical queue
    // holds which slug depends on create order.
    let queue_id = index.id("queue-invoices-main").expect("seeded queue id");
    let q_slug = lf
        .slug_for_id("queues", queue_id)
        .expect("the seeded queue must be tracked")
        .to_string();

    // Both same-named templates must land as SEPARATE files with distinct
    // slugs; a slug collapse here is the pull-side half of the same bug.
    let pulled = our_templates(&project, &q_slug, &prefix);
    assert_eq!(
        pulled.len(),
        3,
        "all three templates must reach disk as separate files; got {:?}",
        pulled.iter().map(|(p, _)| p.file_name()).collect::<Vec<_>>()
    );
    assert_eq!(
        ids_of(&pulled),
        created_ids,
        "the three on-disk templates must map onto the three DISTINCT remote ids"
    );
    assert_converged(&project, "test", &prefix, "after pulling the email templates");

    // --- push an edit ---
    let (solo_path, _) = pulled
        .iter()
        .find(|(_, t)| t.get("name").and_then(|n| n.as_str()) == Some(solo_name.as_str()))
        .expect("the solo template on disk");
    let mut edited: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(solo_path).unwrap()).unwrap();
    edited["subject"] = serde_json::Value::String("Subject A edited".into());
    std::fs::write(solo_path, serde_json::to_vec_pretty(&edited).unwrap()).unwrap();

    let push = project.run_rdc(&["sync", "test"]);
    assert!(push.status.success(), "push failed: {}", combined(&push));

    let remote_solo = client
        .find_listed_value("email_template", solo_id)
        .await
        .expect("list email templates")
        .expect("the solo template must still exist remotely");
    assert_eq!(
        remote_solo.get("subject"),
        Some(&serde_json::Value::String("Subject A edited".into())),
        "the edited subject did not reach the remote: {remote_solo:?}"
    );
    assert_converged(&project, "test", &prefix, "after pushing an email-template edit");

    // --- adoption: re-bind with no lockfile entries at all ---
    // Dropping the `email_templates` section is the documented recovery path
    // (delete lockfile state, re-sync) and it forces every template — ours by
    // NAME, the queue's system defaults by TYPE — back through
    // `pick_adoption_id`. Each local template must claim a DISTINCT remote id.
    let lock_path = project.path().join(".rdc/state/test.lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    lock["objects"]
        .as_object_mut()
        .expect("lockfile objects map")
        .remove("email_templates")
        .expect("the lockfile must have had email_templates recorded");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();

    let readopt = project.run_rdc(&["sync", "test"]);
    assert!(readopt.status.success(), "re-adoption sync failed: {}", combined(&readopt));

    // Nothing was orphaned: all three remote templates still exist...
    for (label, id) in [("solo", solo_id), ("dup-a", dup_a_id), ("dup-b", dup_b_id)] {
        assert!(
            client
                .find_listed_value("email_template", id)
                .await
                .expect("list email templates after re-adoption")
                .is_some(),
            "the {label} template (id {id}) was orphaned by re-adoption — two local \
             siblings collapsed onto one remote id"
        );
    }
    // ...and the local files still point at three distinct ids, one each.
    let readopted = our_templates(&project, &q_slug, &prefix);
    assert_eq!(
        ids_of(&readopted),
        created_ids,
        "after re-adoption the three local templates must still own three \
         DISTINCT remote ids"
    );
    // KNOWN DEFECT (narrowed, still open): the BASE CACHE keeps concrete env
    // URLs after an adoption, and the lockfile entry keeps no `content_hash`.
    //
    // Established by dumping the real post-adoption state:
    //   env file   url = "rdc://email_templates/<ws>/<q>/<slug>"   <- correct
    //   base cache url = "https://<host>/v1/email_templates/<id>"  <- stale form
    //   lockfile   { id, modified_at: null, content_hash: null }
    //
    // The env file is right because the post-pass (`pull::portabilize`)
    // rewrites it — but that pass walks the ENV TREE only and never mirrors to
    // the base cache, so the base keeps whatever the writer put there. The
    // writer could not do better: at portabilize time the lockfile has no
    // entry for the template, so its own `url` has no id->slug mapping to
    // resolve against. (The push drivers solve exactly this by registering the
    // id BEFORE portabilizing — see the "register the adopted id NOW" comment
    // in `push::email_templates`. The pull driver has no equivalent.)
    //
    // Cost: one extra cycle, self-healing, no data loss — but until it runs,
    // the 3-way merge base for these templates is in the wrong form.
    // Deliberately NOT fixed blind: the candidate fix touches
    // `refresh_lockfile_hashes` / the portabilize post-pass, whose own docs
    // warn that getting it wrong produces a "both diverged" prompt storm.
    let settle = project.run_rdc(&["sync", "test"]);
    assert!(settle.status.success(), "settling sync failed: {}", combined(&settle));

    // Churn — the other half of the collapse — surfaces here: a mismatched
    // binding re-PATCHes forever and never settles.
    assert_converged(&project, "test", &prefix, "after re-adopting the email templates");

    // --- delete one of the same-named pair ---
    // Delete the file bound to `dup_b_id`; its same-named sibling `dup_a_id`
    // must be untouched.
    let (dup_path, _) = readopted
        .iter()
        .find(|(_, t)| t.get("id").and_then(|i| i.as_u64()) == Some(dup_b_id))
        .expect("the second same-named template on disk");
    let survivor_id = dup_a_id;
    std::fs::remove_file(dup_path).expect("removing one template file");

    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "delete sync failed: {}", combined(&del));

    assert!(
        client
            .find_listed_value("email_template", dup_b_id)
            .await
            .expect("list after delete")
            .is_none(),
        "the deleted template (id {dup_b_id}) must be gone remotely"
    );
    assert!(
        client
            .find_listed_value("email_template", survivor_id)
            .await
            .expect("list after delete")
            .is_some(),
        "deleting one of a same-named pair must NOT take its sibling \
         (id {survivor_id}) with it"
    );
    assert_converged(&project, "test", &prefix, "after deleting one email template");

    drop(teardown);
}
