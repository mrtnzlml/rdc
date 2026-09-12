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

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. `field_limits_match_the_server` never calls
/// `capture_mode` / `load_or_compare` — it has no golden — so, like
/// `fake_engines_round_trip`, there is nothing here to refuse.
///
/// **What this twin can and cannot establish.** Against the fake, the
/// "server" side of the comparison is `fake::validate::field_caps`, which is
/// pinned independently of `snapshot::limits::field_limits` and deliberately
/// not imported from it. So a green run here proves the two tables still
/// AGREE — it is a drift detector, and a real one: change either table alone
/// and this goes red naming the field. It does NOT prove either table matches
/// Rossum, because both were written from the same live probes. Only
/// `live_field_limits_match_the_server` can say that. Making the fake import
/// `field_limits` would collapse even the drift detection into a tautology;
/// see `field_caps`' doc comment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_field_limits_match_the_server() {
    let fake = crate::support::fake::FakeOrg::start().await;
    field_limits_match_the_server(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_field_limits_match_the_server() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    field_limits_match_the_server(&cfg).await;
}

/// Every `max_length` rdc enforces offline must be the one the server really
/// enforces: exactly at the limit is accepted, one code point over is refused.
async fn field_limits_match_the_server(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

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

/// A queue's three engine-binding fields are mutually exclusive — but the
/// server counts VALUES, not KEYS.
///
/// `cli::migrate::reconcile_engine_slot` leans on exactly this: it resolves a
/// double binding by setting the losers to `null` rather than removing them,
/// so a migrated queue keeps the shape a pulled one has (all three keys) and
/// `git diff` stays honest. And rdc's within-env push re-serializes the whole
/// on-disk body, so every queue PATCH carries all three keys whatever the
/// binding. If the API ever started counting keys, both would break at once:
/// every queue push would 400 with `Only one of dedicated_engine,
/// generic_engine or engine can be set.` — the very error this reconcile
/// exists to prevent — and nothing offline would notice.
///
/// The probe only covers the `generic_engine`-set direction, because creating
/// an `engine` is 403 on the sandbox token. That is the load-bearing half:
/// what is being tested is whether an explicit `null` counts as "set", and the
/// answer cannot depend on which of the three carries the value.
/// # Not ported to the fake, on purpose
///
/// Unlike its two neighbours, this scenario never runs `rdc`: it is a client
/// talking straight to a server, so a fake-backed twin would have the fake
/// assert the fake's own rule, with no second party anywhere in the test.
/// `field_limits_match_the_server` above survives the same treatment only
/// because it reads `snapshot::limits::field_limits` — rdc's table — and
/// compares it against the fake's independently pinned one. There is no
/// equivalent here. `state.rs::a_queue_patch_counts_engine_values_not_engine_keys`
/// already pins the fake's own conduct, at the layer where that is the honest
/// claim.
///
/// Two gaps would have to be invented to get a twin green at all, both
/// observed by running the port before reverting it: `LiveClient::get_value`
/// rejects the plural kind (fixed below — it panicked "unsupported kind
/// 'queues'" before the first probe, which also means this LIVE scenario has
/// never run green), and the fake's `kinds::queue_defaults` binds no generic
/// engine, so the assertion below fails with "a freshly created queue is
/// expected to be generic-engine bound". Modelling that second one means
/// inventing a whole `/generic_engines/<id>` URL space the fake's
/// `kinds::EDGES` currently mis-points at `engines` — three inventions for a
/// test with no independent oracle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_queue_engine_slot_counts_values_not_keys() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let queue_id = index.id("queue-invoices-main").expect("seeded queue id");

    // Whatever binding the seeded queue came up with — a fresh queue is on the
    // built-in generic engine.
    let before = client.get_value("queue", queue_id).await.expect("GET queue");
    let generic = before.get("generic_engine").cloned().unwrap_or(serde_json::Value::Null);
    assert!(
        !generic.is_null(),
        "a freshly created queue is expected to be generic-engine bound; got {before:#}"
    );

    // The shape every rdc queue PATCH sends: all three keys, one value.
    client
        .patch_fields(
            "queue",
            queue_id,
            serde_json::json!({
                "engine": serde_json::Value::Null,
                "dedicated_engine": serde_json::Value::Null,
                "generic_engine": generic,
            }),
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the server REFUSED a queue PATCH carrying all three engine keys with only                  one value. rdc's push sends exactly this on every queue, and                  `reconcile_engine_slot` clears a losing binding by nulling it — both now                  400 on every run. Clear the losers by REMOVING the keys instead: {e:#}"
            )
        });

    // Two values at once is the state that must stay refused — the premise
    // behind refusing it offline (`ChangeList::queue_engine_conflicts`).
    // Pointing `engine` at a made-up id is enough: the mutual-exclusion check
    // must fire before any hyperlink is resolved. Any error is a pass; what
    // would fail this is a 200.
    let bogus_engine = format!("{}/engines/999999999", cfg.api_base.trim_end_matches('/'));
    let res = client
        .patch_fields(
            "queue",
            queue_id,
            serde_json::json!({ "engine": bogus_engine, "generic_engine": generic }),
        )
        .await;
    assert!(
        res.is_err(),
        "the server ACCEPTED two engine bindings at once; rdc refuses that offline          (`queue_engine_conflicts`) and would now be blocking work the server allows"
    );

    drop(teardown);
}

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. No golden, so nothing to refuse capturing.
///
/// This is the one scenario in this file whose assertions are mostly about
/// **rdc**, not about the server: the pre-flight it exercises is offline, so
/// a fake backend is a perfectly good stand-in for the env it must not touch.
/// The server-side half is the closing assertion — the hook's remote
/// `description` is still short — and that one really does read the fake's
/// stored state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_preflight_refuses_over_length_before_touching_the_env() {
    let fake = crate::support::fake::FakeOrg::start().await;
    preflight_refuses_over_length_before_touching_the_env(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_preflight_refuses_over_length_before_touching_the_env() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    preflight_refuses_over_length_before_touching_the_env(&cfg).await;
}

/// rdc's offline pre-flight must refuse an over-long field before it opens a
/// single connection — the property that keeps one bad field from wedging a
/// whole project's syncs half-applied.
async fn preflight_refuses_over_length_before_touching_the_env(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let hook_id = index.id("hook-validator").expect("validator hook id");

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
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
