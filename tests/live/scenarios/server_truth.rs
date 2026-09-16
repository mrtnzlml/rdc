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
use crate::support::expected::{capture_mode, load_or_compare, CapturedState, Golden};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;
use rdc::kinds::PUSH_CAPABLE;
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

/// What a probe value for a given field has to LOOK like, beyond its length.
///
/// A cap probe sends one value at exactly the cap and one a single code point
/// over, so for most fields any string of the right length will do. `color`
/// is the exception in this fixture: it is capped at 7, which is also the
/// length of a `#rrggbb` literal, so its two probe values are colour-shaped
/// and the panic messages below name the shape — a refusal there could be the
/// cap or could be format validation, and this test cannot tell them apart.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// Free-form text: a run-id-prefixed string, so teardown can still find
    /// whatever it is set on.
    Text,
    /// `#` plus hex digits, the only shape a colour field takes.
    Color,
}

/// One `(kind, field)` cap to probe on one object.
///
/// `kind` keys `snapshot::limits::field_limits`; `endpoint` is the singular
/// kind `LiveClient::patch_fields` takes. They differ (`labels` / `label`),
/// and both are needed.
struct Probe {
    kind: &'static str,
    endpoint: &'static str,
    id: u64,
    field: &'static str,
    shape: Shape,
}

/// A value of exactly `len` code points, shaped so the SERVER's only reason to
/// refuse it is the length.
fn probe_value(shape: Shape, prefix: &str, len: usize) -> String {
    match shape {
        Shape::Text => padded(prefix, len),
        Shape::Color => format!("#{}", "f".repeat(len.saturating_sub(1))),
    }
}

/// What to leave the probed field as, so later probes and teardown are
/// unaffected: something short, and still shaped like the field.
fn restore_value(shape: Shape, run_id: &RunId) -> String {
    match shape {
        Shape::Text => run_id.prefix("probe"),
        // The seeded label's own colour (`testdata/live/bodies/labels/priority.json`).
        Shape::Color => "#ff0000".to_string(),
    }
}

/// The `(kind, field)` caps this scenario does NOT probe, each with the reason
/// it cannot be probed here rather than an implicit shrug.
///
/// Two reasons, and they are different in kind:
///
/// - **No object of that kind is in front of the API here.** `engines` and
///   `engine_fields` are absent from `testdata/live/manifest.toml`, and this
///   scenario does not create them. It creates the other two kinds the
///   manifest omits (an email template, a saved view) because both are
///   cheap: they are created and swept like every other object. An engine is
///   not. `teardown_by_prefix`'s engine sweep is best-effort by design — an
///   engine that was ever bound to a queue is refused deletion for up to 24
///   hours, which is why `janitor_sweep` reports an engine backlog instead of
///   asserting one empty — so seeding one per run of a cap probe trades a
///   cheap test for a slow leak. `live_engines_round_trip` owns that
///   lifecycle instead, unbound and deleted in the same run.
/// - **The value is shape-constrained past what a length probe can build.**
///   `extension_image_url` and `read_more_url` are URL fields: a run-id-padded
///   string is not a URL at any length, and whether a synthetic 200-character
///   URL is accepted for other reasons has never been observed, so a probe
///   here could report a moved cap when the server had merely rejected the
///   shape. `rir_params` is a legacy extraction-parameters string with no
///   observed value anywhere in this repo — there is no value known to be
///   valid AT the cap, which is exactly what the accepted-at-the-limit half
///   needs.
///
/// Note the two reasons are not interchangeable, and `engine_fields` would
/// carry both: even with an engine seeded, `subtype` and
/// `pre_trained_field_id` take catalogue values (`"amount"` and the like, per
/// `live_engines_round_trip`'s seed body), so a 50-character filler is not a
/// valid value for them at any length either.
///
/// [`assert_coverage_is_accounted_for`] keeps this list honest: every cap
/// `field_limits` declares is either probed or named here, so a cap added to
/// either table cannot quietly go unwatched.
const UNPROBED_CAPS: &[(&str, &str)] = &[
    ("hooks", "extension_image_url"),
    ("hooks", "read_more_url"),
    ("queues", "rir_params"),
    ("engines", "name"),
    ("engine_fields", "name"),
    ("engine_fields", "label"),
    ("engine_fields", "pre_trained_field_id"),
    ("engine_fields", "subtype"),
];

/// Every cap `field_limits` declares for a `kinds::PUSH_CAPABLE` kind is
/// either probed by this run or listed in [`UNPROBED_CAPS`], and never both.
///
/// Runs inside the scenario, against the probe list it is about to execute,
/// rather than as a standalone unit test: what needs pinning is the coverage
/// of the run that actually happens, and a separate test would drift from it
/// the first time someone edited one and not the other.
fn assert_coverage_is_accounted_for(probes: &[Probe]) {
    let mut probed: Vec<(&str, &str)> = probes.iter().map(|p| (p.kind, p.field)).collect();
    probed.sort_unstable();
    probed.dedup();
    for kind in PUSH_CAPABLE {
        for (field, _) in field_limits(kind) {
            let pair = (*kind, *field);
            let is_probed = probed.contains(&pair);
            let is_excused = UNPROBED_CAPS.contains(&pair);
            assert!(
                is_probed || is_excused,
                "snapshot::limits caps {kind}.{field}, but nothing probes it and \
                 UNPROBED_CAPS does not say why — add a probe, or add the pair with its \
                 reason"
            );
            assert!(
                !(is_probed && is_excused),
                "{kind}.{field} is both probed and listed in UNPROBED_CAPS"
            );
        }
    }
    for (kind, field) in UNPROBED_CAPS {
        assert!(
            field_limits(kind).iter().any(|(f, _)| f == field),
            "UNPROBED_CAPS excuses {kind}.{field}, which snapshot::limits no longer caps"
        );
    }
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
/// AGREE on every pair it probes — it is a drift detector, and a real one:
/// change either table's entry for a probed pair and this goes red naming the
/// field. It does NOT prove either table matches Rossum, because both were
/// written from the same live probes. Only `live_field_limits_match_the_server`
/// can say that. Making the fake import `field_limits` would collapse even the
/// drift detection into a tautology; see `field_caps`' doc comment.
///
/// "Every pair it probes" is load-bearing and used to be the whole gap here:
/// the probe set was three pairs of the twenty-one the two tables carry, so
/// changing `rules.description` in either one went unnoticed — measured, by
/// moving that exact cap in `field_caps` to 256: green before, red naming
/// `rules.description` now. The probe set covers thirteen of the twenty-one,
/// and [`assert_coverage_is_accounted_for`] fails the run if a cap is neither
/// probed nor listed in [`UNPROBED_CAPS`] with a reason, so the eight that are
/// left say why in one place.
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
///
/// Probes every cap in `snapshot::limits::field_limits` except the ones
/// [`UNPROBED_CAPS`] names — thirteen of twenty-one — and asserts that
/// accounting itself before it sends anything.
async fn field_limits_match_the_server(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let prefix = run_id.list_prefix();

    // Two kinds carry caps but have no object in the seeding manifest. Create
    // one each, rather than leave their caps unwatched: both creates are the
    // shapes already proven live elsewhere in this suite — the email template
    // by `trailing_whitespace_handling_is_unchanged` below, the shared saved
    // view by `saved_views_round_trip`. Teardown sweeps both kinds by name
    // prefix.
    let queue_url = index.url("queue", "queue-invoices-main").expect("queue url").to_string();
    let (template_id, _) = client
        .create(
            "email_template",
            &serde_json::json!({
                "name": run_id.prefix("cap-probe-template"),
                "type": "custom",
                "subject": run_id.prefix("cap-probe-subject"),
                "message": "<p>cap probe</p>",
                "automate": false,
                "queue": queue_url,
            }),
        )
        .await
        .expect("create the cap-probe email template");
    let (view_id, _) = client
        .create(
            "saved_view",
            &serde_json::json!({
                "name": run_id.prefix("cap-probe-view"),
                "shared": true,
                // An empty `$and` is refused (`saved_views.rs`); use a real
                // condition.
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            }),
        )
        .await
        .expect("create the cap-probe saved view");

    let hook_id = index.id("hook-validator").expect("validator hook id");
    let label_id = index.id("label-priority").expect("label id");
    let queue_id = index.id("queue-invoices-main").expect("queue id");
    let schema_id = index.id("schema-invoices-main").expect("schema id");
    let workspace_id = index.id("ws-main").expect("workspace id");
    let inbox_id = index.id("inbox-invoices-main").expect("inbox id");
    let rule_id = index.id("rule-totals").expect("rule id");

    let probes = [
        Probe { kind: "labels", endpoint: "label", id: label_id, field: "name", shape: Shape::Text },
        Probe { kind: "labels", endpoint: "label", id: label_id, field: "color", shape: Shape::Color },
        Probe { kind: "hooks", endpoint: "hook", id: hook_id, field: "name", shape: Shape::Text },
        Probe { kind: "hooks", endpoint: "hook", id: hook_id, field: "description", shape: Shape::Text },
        Probe { kind: "queues", endpoint: "queue", id: queue_id, field: "name", shape: Shape::Text },
        Probe { kind: "schemas", endpoint: "schema", id: schema_id, field: "name", shape: Shape::Text },
        Probe { kind: "workspaces", endpoint: "workspace", id: workspace_id, field: "name", shape: Shape::Text },
        Probe { kind: "inboxes", endpoint: "inbox", id: inbox_id, field: "name", shape: Shape::Text },
        Probe { kind: "rules", endpoint: "rule", id: rule_id, field: "name", shape: Shape::Text },
        Probe { kind: "rules", endpoint: "rule", id: rule_id, field: "description", shape: Shape::Text },
        Probe { kind: "email_templates", endpoint: "email_template", id: template_id, field: "name", shape: Shape::Text },
        Probe { kind: "email_templates", endpoint: "email_template", id: template_id, field: "subject", shape: Shape::Text },
        Probe { kind: "saved_views", endpoint: "saved_view", id: view_id, field: "name", shape: Shape::Text },
    ];
    assert_coverage_is_accounted_for(&probes);

    for Probe { kind, endpoint, id, field, shape } in probes {
        let limit = limit_for(kind, field);

        // Exactly at the limit: accepted.
        let at = probe_value(shape, &prefix, limit);
        client
            .patch_fields(endpoint, id, serde_json::json!({ field: at }))
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "the server REFUSED {kind}.{field} at exactly {limit} chars \
                     ({shape:?}-shaped probe value), but snapshot::limits says that is \
                     allowed — rdc's pre-flight is too loose and will let a push 400 \
                     mid-flight: {e:#}"
                )
            });

        // One over: refused.
        let over = probe_value(shape, &prefix, limit + 1);
        let res = client
            .patch_fields(endpoint, id, serde_json::json!({ field: over }))
            .await;
        assert!(
            res.is_err(),
            "the server ACCEPTED {kind}.{field} at {} chars ({shape:?}-shaped probe \
             value), but snapshot::limits caps it at {limit} — rdc's pre-flight is too \
             strict and will refuse work the server would take",
            limit + 1
        );

        // Restore a sane value so later probes and teardown are unaffected.
        client
            .patch_fields(endpoint, id, serde_json::json!({ field: restore_value(shape, &run_id) }))
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
/// The probe only covers the `generic_engine`-set direction — but NOT because
/// engines cannot be created. `POST /engines` with type `extractor` succeeds
/// on the sandbox token: `live_engines_round_trip` creates engines and engine
/// fields on every run, and a direct probe confirmed it again on 2026-09-16.
/// An earlier version of this comment asserted a 403 here. Engine creation is
/// gated by an organization feature flag, so that was either a flag state
/// that has since changed or a misattribution of some other refusal; either
/// way it is not true of this org now, and it must not be repeated as a
/// reason.
///
/// The real cost is BINDING, not creating. Pointing a queue's `engine` at a
/// real engine strands both objects: the engine cannot be deleted while the
/// queue is draining — "up to 24 hours", with no unbind escape hatch — which
/// is why `live_push_create_ordering` leaks one engine per run (see the
/// module doc in `tests/live.rs`, and the stranded `rdc-it-*` engines any
/// sandbox sweep still finds). Paying that to cover the `engine`-set
/// direction buys nothing: what is being tested is whether an explicit `null`
/// counts as "set", and the answer cannot depend on which of the three keys
/// carries the value.
///
/// # Not ported to the fake, on purpose
///
/// Unlike its two neighbours, this scenario never runs `rdc`: it is a client
/// talking straight to a server, so a fake-backed twin would have the fake
/// assert the fake's own rule, with no second party anywhere in the test.
///
/// The criterion, stated the same way it is stated at
/// `fake_trailing_whitespace_handling_is_unchanged` below: a fake-backed twin
/// earns its place when something in the test is not the fake's own opinion.
/// Its two neighbours each have one. `field_limits_match_the_server` above
/// reads `snapshot::limits::field_limits` — rdc's table — and compares it
/// against the fake's independently pinned one. The whitespace twin below
/// must reproduce a golden captured from a real organization, which is a
/// weaker second party than it looks (the fake's trim table was written from
/// that golden, so three of its four rows check the wiring rather than the
/// fact) but is still an artifact the fake did not author. This scenario has
/// neither: no rdc code, no captured artifact.
/// `state.rs::a_queue_patch_counts_engine_values_not_engine_keys` already
/// pins the fake's own conduct, at the layer where that is the honest claim.
///
/// Two gaps would have to be invented to get a twin green at all, both
/// observed by running the port before reverting it. `LiveClient::get_value`
/// rejected the plural kind: `client.get_value("queues", ...)` panicked
/// "unsupported kind 'queues'" before the first probe, which is also how we
/// know this LIVE scenario has never been observed green. That one was
/// repaired in `0d9d61f` — the body below passes the singular — but the
/// second stands: the fake's `kinds::queue_defaults` binds no generic engine,
/// so the assertion below fails with "a freshly created queue is expected to
/// be generic-engine bound". Modelling it means inventing a whole
/// `/generic_engines/<id>` URL space the fake's `kinds::EDGES` currently
/// mis-points at `engines` — an invention in service of a test with no second
/// party.
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

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
///
/// **What a green run here is worth, row by row.** The golden
/// `testdata/live/expected/server_truth.toml` was captured from a real
/// organization — but `fake::quirks::TRIMMED_ON_WRITE` was written FROM that
/// golden (its doc comment says so: "Every row is a line of
/// `testdata/live/expected/server_truth.toml`"), in `d6607be`, the commit
/// immediately before this port. So for the three trimming rows —
/// `hook.description`, `email_template.subject`, `email_template.message` —
/// this twin compares a table against its own source. What it still
/// establishes for them is not nothing and is not proof: that the table is
/// WIRED (`quirks::trim_stored_text` really runs on the write path, and the
/// trim survives a round trip through the HTTP layer), and that the fake has
/// not drifted off the live capture. Measured, by deleting rows from the
/// table: drop `("hooks", "description")` and this goes red, drop both email
/// rows and this goes red.
///
/// The fourth row, `label.name.at_limit_plus_newline = "accepted"`, is the
/// one genuine two-party check in the body: it reads rdc's cap through
/// `limit_for` and the fake answers from `validate::field_caps`, pinned
/// independently — the same shape that makes `field_limits_match_the_server`
/// above worth running against the fake at all.
///
/// The criterion, stated once so it reads the same here and at
/// `live_queue_engine_slot_counts_values_not_keys`'s "# Not ported to the
/// fake" note below: a fake-backed twin earns its place when something in the
/// test is not the fake's own opinion — rdc's table, or a live-captured
/// artifact it must reproduce. The engine-slot scenario has neither, which is
/// why it stayed live-only. This one has the artifact for every row and rdc's
/// table for the fourth, which is why it was ported. Neither is "an
/// independent oracle" for the rule it names; only `live_*` can be that.
///
/// That is also why this backend must never WRITE the golden: a capture from
/// the fake would replace live evidence with the fake's own opinion, silently
/// (the "CAPTURED golden" notice goes to stderr, which cargo swallows without
/// `--nocapture`). The refusal is the `Golden::Compare` handed to the body
/// below — `load_or_compare` reads no environment variable at all — and it is
/// structural rather than a race the assert below happened to win. See
/// `support::expected::Golden` for the run that proved the difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_trailing_whitespace_handling_is_unchanged() {
    assert!(
        !capture_mode(),
        "RDC_LIVE_CAPTURE is set: a fake-backed run must never capture a golden. \
         Capture only from the live invocation, e.g. \
         `RDC_LIVE_CAPTURE=1 cargo test --test live -- --ignored live_trailing_whitespace_handling_is_unchanged`."
    );
    let fake = crate::support::fake::FakeOrg::start().await;
    trailing_whitespace_handling_is_unchanged(&fake.config(), Golden::Compare).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_trailing_whitespace_handling_is_unchanged() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    trailing_whitespace_handling_is_unchanged(&cfg, Golden::from_env()).await;
}

/// Characterize what the server does with trailing whitespace, per field.
///
/// Recorded, not predicted — see the module docs. A change in any recorded
/// value means an rdc premise about trimming needs revisiting.
async fn trailing_whitespace_handling_is_unchanged(cfg: &LiveConfig, golden: Golden) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

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

    let golden_path = static_dir().join("expected/server_truth.toml");
    load_or_compare(&golden_path, &captured, golden)
        .expect("server whitespace behavior matches the golden");

    drop(teardown);
}

/// Raw-wire JSON reader.
///
/// Every [`LiveClient`] read goes through rdc's typed models — `get_value`
/// ends in `serde_json::to_value(typed)` — which re-serializes in STRUCT
/// DECLARATION order and would launder away the exact property under test.
/// This talks to the API directly, like `support::mdh::MdhRaw` does.
struct WireReader {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl WireReader {
    fn new(cfg: &LiveConfig) -> WireReader {
        WireReader {
            http: reqwest::Client::builder().build().expect("building reqwest client"),
            base: cfg.api_base.trim_end_matches('/').to_string(),
            token: cfg.token.clone(),
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> serde_json::Value {
        let res = req
            // `token <t>`, not `Bearer <t>` — matching `RossumClient`. The
            // live API accepts both; the fake accepts only this one.
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await
            .expect("live request");
        let status = res.status();
        let body = res.text().await.expect("reading response body");
        assert!(status.is_success(), "request failed {status}: {body}");
        serde_json::from_str(&body).expect("response is JSON")
    }

    async fn get(&self, path: &str) -> serde_json::Value {
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        self.send(self.http.post(format!("{}{path}", self.base)).json(&body)).await
    }

    async fn patch(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        self.send(self.http.patch(format!("{}{path}", self.base)).json(&body)).await
    }
}

/// Top-level keys of a JSON object, in wire order. `serde_json` is built with
/// `preserve_order`, so `Value::Object` is an `IndexMap` and this IS the order
/// the server sent.
fn wire_keys(v: &serde_json::Value) -> Vec<String> {
    v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// Both key lists reduced to the keys they SHARE, each kept in its own order.
/// Equality of the two results means "same relative order".
///
/// Filtering rather than comparing key SETS is the whole point: objects of one
/// kind legitimately differ in membership — a `function` hook carries a
/// `status` a `webhook` hook does not, and a schema LIST omits the `content`
/// its detail response carries. Those are not permutations and must not fail.
fn shared_order(a: &[String], b: &[String]) -> (Vec<String>, Vec<String>) {
    let in_a: std::collections::HashSet<&String> = a.iter().collect();
    let in_b: std::collections::HashSet<&String> = b.iter().collect();
    (
        a.iter().filter(|k| in_b.contains(*k)).cloned().collect(),
        b.iter().filter(|k| in_a.contains(*k)).cloned().collect(),
    )
}

/// The wire key-ORDER premise behind rdc's on-disk byte stability.
///
/// Two of rdc's writers decide "did this file change?" by comparing RAW
/// BYTES: `cli::migrate::settle` (`Ok(existing) if existing == bytes`) and
/// `snapshot::writer::write_atomic` (the same test). Everything else is
/// already immune — `state::lockfile::content_hash` canonicalizes through
/// `snapshot::noise::sort_keys_recursive`, and `decide_pull_action`
/// short-circuits on `local_hash == remote_hash` to `PullAction::NoChange`
/// without writing. So a key permutation can never produce phantom DRIFT. It
/// can produce phantom WRITES, and a file two writers disagree about is a
/// project that never settles — which is why this premise is worth a test.
///
/// What reaches disk is a hybrid, and that is why the assertion is narrow.
/// Pull deserializes into rdc's typed models and re-serializes
/// (`cli::pull::queues.rs:209`, `serde_json::to_value(q)`), so the TYPED
/// fields emit in struct-declaration order, which no server can move. Every
/// model then carries `#[serde(flatten)] extra: IndexMap<String, Value>`, and
/// an `IndexMap` preserves insertion order — so the flattened TAIL lands in
/// wire order. That tail is the entire exposure, and it is what this pins.
///
/// Measured against a real org on 2026-09-16: the API's key order is
/// deterministic and positional per kind. Responses differ by MEMBERSHIP,
/// never by permutation — a `function` hook appends a `status` key a
/// `webhook` hook lacks; a schema LIST omits the `content` its detail
/// carries. So every comparison below is over the keys two responses SHARE.
/// Demanding equal key sets would fail on facts that are perfectly fine.
///
/// # Not ported to the fake, on purpose
///
/// This is the one gap the fake structurally cannot cover, and the reason the
/// scenario exists at all. `quirks::impose_field_order` makes the fake serve
/// ONE order per kind for every endpoint and every method, so a fake-backed
/// twin would assert the fake's own simplification and pass by construction,
/// while the real risk — Rossum changing this — went unnoticed. Same
/// criterion as `live_queue_engine_slot_counts_values_not_keys` above: a twin
/// earns its place when something in the test is not the fake's own opinion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_wire_key_order_is_positional_not_permuted() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let wire = WireReader::new(&cfg);
    let run_id = RunId::new();
    let teardown = Teardown::new(LiveClient::connect(&cfg).expect("connect"), run_id.clone());

    // (1) and (2): within one LIST response, and LIST versus DETAIL.
    let mut probed = 0usize;
    for kind in ["queues", "hooks", "engines", "workspaces", "email_templates", "schemas"] {
        let list = wire.get(&format!("/{kind}?page_size=20")).await;
        let results =
            list.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        let Some(first) = results.first() else {
            continue; // nothing of this kind in the org; not this test's problem
        };
        probed += 1;
        let base_keys = wire_keys(first);

        for other in results.iter().skip(1) {
            let (a, b) = shared_order(&base_keys, &wire_keys(other));
            assert_eq!(
                a, b,
                "{kind}: two objects in ONE list response disagree on the relative order of \
                 the keys they share. rdc's flattened `extra` tail is written to disk in wire \
                 order, so two objects of one kind would now serialize differently and every \
                 byte-comparing writer (`migrate::settle`, `write_atomic`) would rewrite them \
                 forever. See this test's doc comment."
            );
        }

        let Some(id) = first.get("id").and_then(|i| i.as_u64()) else {
            continue;
        };
        let detail = wire.get(&format!("/{kind}/{id}")).await;
        let (a, b) = shared_order(&base_keys, &wire_keys(&detail));
        assert_eq!(
            a, b,
            "{kind}: the LIST and DETAIL responses disagree on the relative order of the keys \
             they share. rdc pulls some kinds by list and re-reads others by id, so the same \
             object would land on disk in two different orders depending on the path taken."
        );
    }
    assert!(
        probed >= 4,
        "only {probed} kind(s) had any objects to probe — this org is too empty for the \
         assertion to mean anything; point RDC_LIVE_* at an org with content"
    );

    // (3) POST / PATCH / GET on one throwaway object. This is the push
    // write-back path: `cli::push::*` writes the CREATE/PATCH response to
    // disk (portabilized), while pull writes the GET/list response. If those
    // two orders ever diverge, sync and pull would each rewrite the file into
    // the other's order on every cycle — the exact "never converges" shape.
    let org = format!("{}/organizations/{}", cfg.api_base.trim_end_matches('/'), cfg.org_id);
    let name = run_id.prefix("keyorder");
    let created = wire
        .post("/workspaces", serde_json::json!({ "name": name, "organization": org }))
        .await;
    let id = created.get("id").and_then(|i| i.as_u64()).expect("created workspace id");
    let fetched = wire.get(&format!("/workspaces/{id}")).await;
    let patched = wire
        .patch(
            &format!("/workspaces/{id}"),
            serde_json::json!({ "name": format!("{name}-renamed") }),
        )
        .await;

    let get_keys = wire_keys(&fetched);
    for (label, other) in [("POST", &created), ("PATCH", &patched)] {
        let (a, b) = shared_order(&get_keys, &wire_keys(other));
        assert_eq!(
            a, b,
            "the {label} response and the GET response disagree on the relative order of the \
             keys they share. Push write-back writes the {label} body to disk and pull writes \
             the GET body, so the file would flip between the two orders on every cycle."
        );
    }

    drop(teardown);
}
