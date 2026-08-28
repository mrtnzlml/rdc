//! Facts about the *server* that rdc's local logic is built on.
//!
//! These are premises, not behaviours: rdc refuses an over-long field offline,
//! before any network call, using a table of `max_length` values probed by hand
//! from `OPTIONS` and written into `snapshot::limits`. Nothing re-checks them.
//! If Rossum ever moves a limit, the table silently becomes either too strict
//! (rdc refuses work the server would accept, wedging a project's syncs) or too
//! loose (rdc waves through a payload that 400s mid-push).
//!
//! The whitespace half is here for a sharper reason: the premise "the server
//! always strips trailing whitespace before storing" was once written into the
//! hashing and migrate paths, turned out to be **false**, and cost real data
//! (MDH `$concat` separators). It is now scoped to email subject/message only.
//! A premise that has already been wrong once deserves a test.
//!
//! The whitespace checks CHARACTERIZE rather than predict: the first run
//! records what the server actually does into a reviewed golden
//! (`RDC_LIVE_CAPTURE=1`), and later runs alarm when that changes. Guessing the
//! answer in an assertion would just re-encode the mistake this exists to
//! catch.
//!
//! What the first capture recorded, and why it is not a contradiction: on the
//! CORE API the server trims trailing whitespace broadly — hook `description`
//! as well as email `subject` / `message`. The data-loss incident that made
//! this worth testing was in **Data Storage** (MDH `$concat` separators),
//! which is a different service and does NOT trim. Both facts hold at once,
//! and the golden pins only the core-API half; the MDH half is covered by the
//! MDH scenarios. Anyone widening rdc's trimming beyond email subject/message
//! on the strength of this golden would be repeating the original mistake.

use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;
use rdc::snapshot::limits::field_limits;

/// A string of exactly `len` code points that still starts with `prefix`, so
/// teardown can find whatever it is set on.
fn padded(prefix: &str, len: usize) -> String {
    assert!(
        prefix.chars().count() < len,
        "the run-id prefix ({}) must fit inside a {len}-char value",
        prefix.chars().count()
    );
    let pad = len - prefix.chars().count();
    format!("{prefix}{}", "a".repeat(pad))
}

/// Look up rdc's declared limit for `(kind, field)`, failing loudly if the
/// table stops carrying it — a silent `None` here would make the test pass by
/// checking nothing.
fn limit_for(kind: &str, field: &str) -> usize {
    field_limits(kind)
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, l)| *l)
        .unwrap_or_else(|| panic!("snapshot::limits no longer declares a limit for {kind}.{field}"))
}

/// Every `max_length` rdc enforces offline must be the one the server really
/// enforces: exactly at the limit is accepted, one code point over is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_field_limits_match_the_server() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let prefix = run_id.list_prefix();

    // (kind, endpoint kind, id, field) triples to probe. Deliberately small:
    // one tight cap and one generous one, on objects the seed already made.
    let hook_id = index.id("hook-validator").expect("validator hook id");
    let label_id = index.id("label-priority").expect("label id");

    let probes: [(&str, &str, u64, &str); 3] = [
        ("labels", "label", label_id, "name"),
        ("hooks", "hook", hook_id, "name"),
        ("hooks", "hook", hook_id, "description"),
    ];

    for (kind, endpoint, id, field) in probes {
        let limit = limit_for(kind, field);

        // Exactly at the limit: accepted.
        let at = padded(&prefix, limit);
        client
            .patch_fields(endpoint, id, serde_json::json!({ field: at }))
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "the server REFUSED {kind}.{field} at exactly {limit} chars, but \
                     snapshot::limits says that is allowed — rdc's pre-flight is too \
                     loose and will let a push 400 mid-flight: {e:#}"
                )
            });

        // One over: refused.
        let over = padded(&prefix, limit + 1);
        let res = client
            .patch_fields(endpoint, id, serde_json::json!({ field: over }))
            .await;
        assert!(
            res.is_err(),
            "the server ACCEPTED {kind}.{field} at {} chars, but snapshot::limits caps \
             it at {limit} — rdc's pre-flight is too strict and will refuse work the \
             server would take",
            limit + 1
        );

        // Restore a sane value so later probes and teardown are unaffected.
        client
            .patch_fields(endpoint, id, serde_json::json!({ field: run_id.prefix("probe") }))
            .await
            .expect("restoring the probed field");
    }

    drop(teardown);
}

/// rdc's offline pre-flight must refuse an over-long field before it opens a
/// single connection — the property that keeps one bad field from wedging a
/// whole project's syncs half-applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_preflight_refuses_over_length_before_touching_the_env() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let hook_id = index.id("hook-validator").expect("validator hook id");

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    // Find the validator hook on disk and give it an over-length description.
    let lf = crate::support::assert_local::load_lockfile(project.path(), "test").expect("lockfile");
    let hslug = lf
        .slug_for_id("hooks", hook_id)
        .expect("the validator hook must be tracked")
        .to_string();
    let hrel = format!("envs/test/hooks/{hslug}.json");
    let mut hv = project.read_json(&hrel);
    let limit = limit_for("hooks", "description");
    hv["description"] = serde_json::Value::String("d".repeat(limit + 1));
    std::fs::write(
        project.path().join(&hrel),
        serde_json::to_vec_pretty(&hv).unwrap(),
    )
    .unwrap();

    let out = project.run_rdc(&["sync", "test"]);
    let combined = crate::support::converge::combined(&out);
    assert!(
        !out.status.success(),
        "sync must refuse an over-length field rather than push it: {combined}"
    );
    assert!(
        combined.contains("length limit"),
        "the refusal must name the length limit so the offending field is findable: \
         {combined}"
    );

    // The env must be untouched: the pre-flight runs before any write.
    let remote = client
        .find_listed_value("hook", hook_id)
        .await
        .expect("list hooks")
        .expect("the hook must still exist");
    let remote_desc = remote.get("description").and_then(|d| d.as_str()).unwrap_or("");
    assert!(
        remote_desc.chars().count() <= limit,
        "the over-length description reached the env despite the pre-flight"
    );

    drop(teardown);
}

/// Characterize what the server does with trailing whitespace, per field.
///
/// Recorded, not predicted — see the module docs. A change in any recorded
/// value means an rdc premise about trimming needs revisiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_trailing_whitespace_handling_is_unchanged() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let queue_url = index.url("queue", "queue-invoices-main").expect("queue url").to_string();

    let mut captured = CapturedState::default();

    /// `"preserved"` / `"trimmed"` / `"altered"` for one round-tripped value.
    fn verdict(sent: &str, got: Option<&str>) -> String {
        match got {
            None => "absent".to_string(),
            Some(g) if g == sent => "preserved".to_string(),
            Some(g) if g == sent.trim_end() => "trimmed".to_string(),
            Some(_) => "altered".to_string(),
        }
    }

    // --- a hook's `description`: rdc does NOT trim this one ---
    let hook_id = index.id("hook-validator").expect("hook id");
    let desc = format!("{}   ", run_id.prefix("desc"));
    client
        .patch_fields("hook", hook_id, serde_json::json!({ "description": desc }))
        .await
        .expect("patch hook description");
    let hook = client
        .find_listed_value("hook", hook_id)
        .await
        .expect("list hooks")
        .expect("hook exists");
    captured.refs.insert(
        "hook.description.trailing_spaces".into(),
        verdict(&desc, hook.get("description").and_then(|d| d.as_str())),
    );

    // --- an email template's `subject` and `message`: rdc DOES trim these ---
    let subject = format!("{}  ", run_id.prefix("subj"));
    let message = "<p>body</p>   ".to_string();
    let (tpl_id, _) = client
        .create(
            "email_template",
            &serde_json::json!({
                "name": run_id.prefix("ws-probe"),
                "type": "custom",
                "subject": subject,
                "message": message,
                "automate": false,
                "queue": queue_url,
            }),
        )
        .await
        .expect("create the probe email template");
    let tpl = client
        .find_listed_value("email_template", tpl_id)
        .await
        .expect("list email templates")
        .expect("the probe template exists");
    captured.refs.insert(
        "email_template.subject.trailing_spaces".into(),
        verdict(&subject, tpl.get("subject").and_then(|s| s.as_str())),
    );
    captured.refs.insert(
        "email_template.message.trailing_spaces".into(),
        verdict(&message, tpl.get("message").and_then(|s| s.as_str())),
    );

    // --- a value at exactly the limit PLUS a trailing newline ---
    // `snapshot::limits` counts after trimming, on the stated grounds that the
    // server trims before validating. If that stopped being true, every field
    // sitting exactly at its cap would start 400ing.
    let name_limit = limit_for("labels", "name");
    let label_id = index.id("label-priority").expect("label id");
    let at_limit_plus_newline = format!("{}\n", padded(&run_id.list_prefix(), name_limit));
    let res = client
        .patch_fields("label", label_id, serde_json::json!({ "name": at_limit_plus_newline }))
        .await;
    captured.refs.insert(
        "label.name.at_limit_plus_newline".into(),
        if res.is_ok() { "accepted".into() } else { "rejected".to_string() },
    );

    let golden = static_dir().join("expected/server_truth.toml");
    load_or_compare(&golden, &captured).expect("server whitespace behavior matches the golden");

    drop(teardown);
}
