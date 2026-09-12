# Stateful Fake Org — Stage 2 Scenario Ports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every remaining non-MDH live scenario a fake-backed twin, so the whole non-MDH live suite runs offline in `cargo test` and each scenario's convergence is gated on every commit.

**Architecture:** Each scenario's body is extracted verbatim into `async fn <name>(cfg: &LiveConfig)` and gains two wrappers — `fake_<name>()` (runs against `FakeOrg`, not ignored) and `live_<name>()` (`#[ignore]`d, env-gated, behaviour bit-for-bit unchanged). The extraction is mechanical; the real work is whatever gap in the fake each newly-offline scenario exposes. A gap is closed by modelling the behaviour **with a citation to the live test that observed it**, or — when no such observation exists — by declaring it in the fake's known-unmodelled list. Never by inventing an answer.

**Tech Stack:** Rust, `tokio` (multi-thread test flavor), `wiremock` 0.6.5, `serde_json`. All work is in `tests/`; this plan makes **no `src/` changes**.

**Spec:** `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md` — this plan implements its **Stage 2** (§G), using the wrapper shape fixed in §E and the two-org shape in §F.

## Global Constraints

- **No `src/` changes.** If a port appears to require one, stop and report it rather than making it. A scenario failing against the fake is evidence about the *fake*, until proven otherwise.
- **No customer names or customer-specific identifiers** anywhere — source, tests, docs, fixtures, **or commit messages**. Neutral placeholders only (`acme`, `main`, `invoices`, `dev`/`test`/`prod`). Absolute.
- **Do not make assumptions; every claim must be verified and grounded in fact.** Nine claims-stronger-than-their-evidence have been caught in this effort so far — by implementers, by reviewers, and in briefs. Before writing a sentence that asserts something about the real Rossum API or about another file, open that file and confirm it.
- **Cite a symbol whenever the target has one** — a function, a constant, a struct field, a test name — and spend a bare line number only where nothing does. A line citation into a file other commits edit rots; one in this tree rotted within two commits.
- **Backward compatibility:** `cargo test --test live -- --ignored` must keep selecting exactly the live set. Every `live_*` wrapper keeps its `#[ignore]` and its `LiveConfig::from_env()` gate unchanged.
- **Invariants**, measured before each task and re-measured after:
  - `cargo test --locked` — passes, 0 failed. The count rises by exactly the number of `fake_*` tests added.
  - `cargo test --locked -- --skip fake` — 0 failed. This is the pre-existing suite; a port must not move it except by tests added outside the `fake` module.
  - `cargo test --locked --test live -- --ignored --list | grep -c ': test'` — **exactly 23**, unchanged by every task in this plan. A port adds a non-ignored `fake_*` test and leaves the ignored `live_*` one in place; if this number moves, a live wrapper lost its `#[ignore]` and would silently pass having tested nothing.
  - `cargo clippy --all-targets --locked -- -D warnings` — clean.
- **Never rename a module or a test to make a count come out right.** If a number moves, it moves honestly and the report says why.

---

## The port procedure

Every task in this plan applies this same procedure to its own scenarios. It is written once, here, because it is identical each time and a reader arriving at any task needs it in full; each task names the scenarios, the body names, and the specific hazards that task is likely to meet.

**1. Extract the body.** The scenario today is `#[tokio::test(...)] #[ignore = "live: needs RDC_LIVE_* env"] async fn live_<name>() { … }`. Move its body verbatim into:

```rust
async fn <name>(cfg: &LiveConfig) { /* today's body, unchanged */ }
```

The body name is the live wrapper's name with `live_` stripped — `live_cross_refs` → `cross_refs`. Do not rename beyond that, and do not "improve" the body while moving it: a behaviour change smuggled into an extraction is invisible to every reviewer comparing the two.

**2. Add the two wrappers**, exactly this shape (`tests/live/scenarios/round_trip.rs` is the reference):

```rust
/// The fake-backed twin. Runs in a plain `cargo test`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_<name>() {
    let fake = crate::support::fake::FakeOrg::start().await;
    <name>(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_<name>() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    <name>(&cfg).await;
}
```

**3. If the scenario calls `capture_mode()`** (i.e. it reads or writes a golden under `testdata/live/expected/`), the fake wrapper must refuse to capture, exactly as `fake_round_trip_core` does:

```rust
assert!(
    !capture_mode(),
    "RDC_LIVE_CAPTURE is set: a fake-backed run must never capture a golden. \
     Capture only from the live invocation, e.g. \
     `RDC_LIVE_CAPTURE=1 cargo test --test live -- --ignored live_<name>`."
);
```

Those goldens are artifacts captured from a real organization. A fake-backed run blessing a divergence into one corrupts the live oracle *silently* — the "CAPTURED golden" notice goes to stderr, which cargo swallows without `--nocapture`.

**4. Run `cargo test --locked --test live` and watch the new `fake_*` test fail.** It usually will. That failure is the deliverable of the port — it is the fake telling you which piece of the real API it does not yet model.

**5. Close the gap honestly.** For each failure, exactly one of:
   - **Model it**, in `tests/live/support/fake/` — a route in `route()`, a field in `kinds.rs`'s `defaults`, an edge in `EDGES`, a refusal in `validate.rs`, a rule in `quirks.rs`. If the behaviour is one a live test has actually observed, add it to `quirks::QUIRKS` with `Provenance::Modelled { proven_by }` naming that test. If the fake has to pick an answer nobody has observed, use `Provenance::ChosenUnverified` and say what you weighed it against — that variant exists precisely so "we guessed" is never disguised as "we know".
   - **Declare it unmodelled**, in `validate.rs`'s known-unmodelled list, with the citation for what the real API does instead.
   - **Do not port the scenario**, and say why in the report. This is a legitimate outcome. A scenario forced green against a fake that had to invent three behaviours to get there is worse than no port at all — it manufactures false confidence, which is the exact failure this whole effort exists to remove.

**6. Guard tests.** `tests/live/scenario_wrappers.rs` enforces the wrapper contract automatically — both wrappers present, `#[ignore]` in the right place, the ignored-count unchanged, and that `fake_<name>` actually *calls* `<name>`. You do not add anything there; you make it pass.

**7. Verify and commit.** Run the four invariant commands. One commit per scenario ported, plus a separate commit per fake gap closed — a fake change is a claim about a server and deserves its own reviewable diff, not burial inside a refactor.

---

## File Structure

- `tests/live/scenarios/*.rs` — each gets its body extracted and two wrappers added. No scenario logic changes.
- `tests/live/support/fake/` — gains whatever each port proves is missing:
  - `mod.rs` — request routing; a missing endpoint lands here.
  - `state.rs` — the object graph, ids, urls, pagination, CRUD, pending deletes.
  - `kinds.rs` — per-kind table (`defaults`, `has_modified_at`, path) and `EDGES`.
  - `graph.rs` — back-references and cascade, derived from `EDGES`.
  - `validate.rs` — refusals, field caps, and the known-unmodelled list.
  - `quirks.rs` — the learned-facts registry and the two normalization seams.
- `tests/live/scenario_wrappers.rs` — **read-only for this plan.** It enforces the contract; it should need no edit. If a port seems to require changing a guard, that is a finding to report, not a step to take.

---

### Task 1: Ports A — the ref graph and sidecars

Three scenarios over kinds the fake already models. First because they exercise `graph.rs`'s back-references and the code-sidecar paths most heavily, so a defect there surfaces before three later tasks build on it.

**Files:**
- Modify: `tests/live/scenarios/cross_refs.rs` — body `cross_refs`
- Modify: `tests/live/scenarios/sidecars.rs` — body `sidecars_redaction`
- Modify: `tests/live/scenarios/collisions.rs` — body `collisions_identity`
- Modify (as gaps require): `tests/live/support/fake/*.rs`

**Interfaces:**
- Consumes: `FakeOrg::start()`, `FakeOrg::config()` (`tests/live/support/fake/mod.rs`); `assert_converged` (`tests/live/support/converge.rs`); the wrapper shape in `tests/live/scenarios/round_trip.rs`.
- Produces: bodies `cross_refs`, `sidecars_redaction`, `collisions_identity`, each `async fn <name>(cfg: &LiveConfig)`, plus their `fake_*` / `live_*` wrappers. Any fake capability added here is available to every later task.

**Hazards specific to this batch:** `collisions_identity` depends on slug derivation and identity across workspaces — the fake mints its own ids and urls, so a collision the live org produces by accident may not arise at all against the fake. If the scenario's premise cannot be reproduced without inventing server behaviour, take option 3 of step 5 and say so. `sidecars_redaction` touches hook/rule code sidecars; confirm the fake's write-back mirrors them the way `pull` does before assuming a mismatch is the scenario's fault.

- [ ] **Step 1: Read the reference port**

Read `tests/live/scenarios/round_trip.rs` lines 1-60 — the two wrappers and the body signature. This is the shape every port copies.

- [ ] **Step 2: Port `cross_refs`**

Apply **The port procedure** above to `tests/live/scenarios/cross_refs.rs`: body `cross_refs`, wrappers `fake_cross_refs` / `live_cross_refs`.

- [ ] **Step 3: Run it and record the failure**

Run: `cargo test --locked --test live fake_cross_refs -- --nocapture`
Expected: either PASS (record that it passed first try) or a failure naming a specific missing behaviour. Write the exact failure down before changing anything — it is the evidence for whatever you do next.

- [ ] **Step 4: Close the gap, per step 5 of the procedure**

Model it with a cited `Provenance`, declare it unmodelled, or decline the port with a reason. Commit the fake change separately from the port.

- [ ] **Step 5: Port `sidecars_redaction`, then `collisions_identity`**

Repeat steps 2-4 for each, in that order.

- [ ] **Step 6: Verify and commit**

```bash
cargo test --locked
cargo test --locked -- --skip fake
cargo test --locked --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```
Expected: 0 failed; `--skip fake` unmoved; the ignored count exactly 23; clippy clean.

---

### Task 2: Ports B — the newest-modelled kinds

Three scenarios over the kinds whose fake modelling is youngest and least exercised: engines and engine fields (`has_modified_at: false`), email templates (adopted by name, with a history of collision bugs), and saved views (whose `queues_filter` edge was added only in the previous plan).

**Files:**
- Modify: `tests/live/scenarios/engines.rs` — body `engines_round_trip`
- Modify: `tests/live/scenarios/email_templates.rs` — body `email_templates_round_trip`
- Modify: `tests/live/scenarios/saved_views.rs` — body `saved_views_round_trip`
- Modify (as gaps require): `tests/live/support/fake/*.rs`

**Interfaces:**
- Consumes: everything Task 1 produced, plus `kinds::EDGES`'s `saved_views.queues_filter` row and `validate::on_delete`'s two engine refusals.
- Produces: bodies `engines_round_trip`, `email_templates_round_trip`, `saved_views_round_trip` with their wrappers.

**Hazards specific to this batch:** `engines.rs` already observes a real `DELETE /engine_fields/{id}` succeeding, while `validate.rs` declares that the real API refuses that delete with `409 conflict_referenced` when a schema still uses the field — the fake models no such refusal. Expect that asymmetry to surface here; resolve it by reading what the scenario actually does, not by making the fake refuse and the scenario fail. Email templates are auto-created by a queue create in the fake; a port that creates its own may collide with one the fake already made.

- [ ] **Step 1: Port `engines_round_trip`**

Apply **The port procedure**: body `engines_round_trip`, wrappers `fake_engines_round_trip` / `live_engines_round_trip`.

- [ ] **Step 2: Run it and record the failure**

Run: `cargo test --locked --test live fake_engines_round_trip -- --nocapture`
Expected: PASS, or a failure naming a specific missing behaviour. Record the exact text.

- [ ] **Step 3: Close the gap, per step 5 of the procedure**

- [ ] **Step 4: Port `email_templates_round_trip`, then `saved_views_round_trip`**

Repeat steps 1-3 for each, in that order.

- [ ] **Step 5: Verify and commit**

```bash
cargo test --locked
cargo test --locked -- --skip fake
cargo test --locked --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```
Expected: 0 failed; `--skip fake` unmoved; the ignored count exactly 23; clippy clean.

---

### Task 3: Ports C — deletes and drift

Three scenarios over the delete and drift paths. These go third because both gaps found by the first three ports of this whole effort were about deletes and drift signals rather than pulled content — this is the highest-yield seam.

**Files:**
- Modify: `tests/live/scenarios/conflicts_deletes.rs` — body `conflicts_deletes`
- Modify: `tests/live/scenarios/janitor.rs` — body `janitor_sweep`
- Modify: `tests/live/scenarios/deploy_flow.rs` — body `deploy_flow`
- Modify (as gaps require): `tests/live/support/fake/*.rs`

**Interfaces:**
- Consumes: everything Tasks 1-2 produced; `OrgState`'s pending-delete state machine (a queue `DELETE` answers `202 deletion_requested` and the object survives exactly one more sighting); `validate::on_delete`.
- Produces: bodies `conflicts_deletes`, `janitor_sweep`, `deploy_flow` with their wrappers.

**Hazards specific to this batch:** the async queue-delete state machine is the fake's most stateful behaviour and the easiest place to write a test that passes for the wrong reason. `janitor_sweep` sweeps leftovers from *previous* runs — against a fresh fake org there are none, so check whether the scenario still asserts anything; a port that passes because it found nothing to do is a test that cannot fail, and must be reported as such rather than counted as a port.

- [ ] **Step 1: Port `conflicts_deletes`**

Apply **The port procedure**: body `conflicts_deletes`, wrappers `fake_conflicts_deletes` / `live_conflicts_deletes`.

- [ ] **Step 2: Run it and record the failure**

Run: `cargo test --locked --test live fake_conflicts_deletes -- --nocapture`
Expected: PASS, or a failure naming a specific missing behaviour. Record the exact text.

- [ ] **Step 3: Close the gap, per step 5 of the procedure**

- [ ] **Step 4: Port `janitor_sweep`, then `deploy_flow`**

Repeat steps 1-3 for each, in that order. For `janitor_sweep`, before declaring it ported, state explicitly what it asserts against a fresh fake org and whether that assertion can fail.

- [ ] **Step 5: Verify and commit**

```bash
cargo test --locked
cargo test --locked -- --skip fake
cargo test --locked --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```
Expected: 0 failed; `--skip fake` unmoved; the ignored count exactly 23; clippy clean.

---

### Task 4: Ports D — server truth

Four scenarios, all of which assert what the *server* does. This is where a fake is most dangerous: a fake that agrees with rdc, rather than with Rossum, turns these into tests that can only confirm what rdc already believes.

**Files:**
- Modify: `tests/live/scenarios/server_truth.rs` — bodies `field_limits_match_the_server`, `queue_engine_slot_counts_values_not_keys`, `preflight_refuses_over_length_before_touching_the_env`, `trailing_whitespace_handling_is_unchanged`
- Modify (as gaps require): `tests/live/support/fake/*.rs`

**Interfaces:**
- Consumes: everything Tasks 1-3 produced; `validate::field_caps`, which pins per-kind field length caps **independently of** `src/snapshot/limits::field_limits` on purpose.
- Produces: the four bodies above with their wrappers.

**Hazards specific to this batch, and the point of the task:** `validate::field_caps` is deliberately a *separate* pin from rdc's own `field_limits` table — importing one into the other would make `field_limits_match_the_server` tautological, since it would then compare a table with itself. Preserve that separation. If a cap mismatches, the correct move is to find out which of the two is wrong against the real API, not to make them agree. Likewise `trailing_whitespace_handling_is_unchanged` encodes a fact that was once asserted backwards in this repo and caused real data loss: the server trims trailing whitespace **only** in specific fields, not universally. Do not let the fake generalize it.

- [ ] **Step 1: Port `field_limits_match_the_server`**

Apply **The port procedure**: body `field_limits_match_the_server`, wrappers `fake_field_limits_match_the_server` / `live_field_limits_match_the_server`.

- [ ] **Step 2: Run it and prove it is not tautological**

Run: `cargo test --locked --test live fake_field_limits_match_the_server -- --nocapture`
Then **sabotage it**: change one cap in `validate::field_caps` to a wrong value, re-run, and confirm the test fails naming that field. Restore the cap. Report the exact failure message. A green run here proves nothing until you have seen it go red for the right reason.

- [ ] **Step 3: Close any gap, per step 5 of the procedure**

- [ ] **Step 4: Port the remaining three**

Repeat steps 1-3 for `queue_engine_slot_counts_values_not_keys`, `preflight_refuses_over_length_before_touching_the_env`, and `trailing_whitespace_handling_is_unchanged`, in that order. Sabotage-verify each one the same way: break the server behaviour it asserts, confirm the test fails and names it, restore.

- [ ] **Step 5: Verify and commit**

```bash
cargo test --locked
cargo test --locked -- --skip fake
cargo test --locked --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```
Expected: 0 failed; `--skip fake` unmoved; the ignored count exactly 23; clippy clean.

---

### Task 5: Ports E — promotion and the CLI surface

Four scenarios, last because they are the most likely to need a second fake org and the most likely to touch process-global state.

**Files:**
- Modify: `tests/live/scenarios/migrate_promotion.rs` — body `migrate_overlay_and_mirror`
- Modify: `tests/live/scenarios/cli_surface.rs` — bodies `auth_validates_before_writing`, `doctor_realign_after_a_remote_rename`, `sync_direction_flags`
- Modify (as gaps require): `tests/live/support/fake/*.rs`

**Interfaces:**
- Consumes: everything Tasks 1-4 produced, plus:
  - `FakeOrg::start_with_org(org_id: u64) -> FakeOrg` and `FakeOrg::paired_config(&self, target: &FakeOrg) -> LiveConfig` (`tests/live/support/fake/mod.rs`) — the two-org shape. **This is the first task to use them.**
  - `LiveConfig::target: Option<EnvCreds>` (`tests/live/support/config.rs`), which is what makes a promotion assertion honest.
- Produces: the four bodies above with their wrappers.

**Hazards specific to this batch:** a promotion scenario pointed at one org is not a promotion test — source and target become two snapshots of the same objects, `--mirror` reads the source's own objects as target-only extras, and convergence of the source after a deploy is unassertable. So `fake_migrate_overlay_and_mirror` must start **two** fakes with **different org ids** and use `paired_config`:

```rust
let src = crate::support::fake::FakeOrg::start_with_org(1).await;
let tgt = crate::support::fake::FakeOrg::start_with_org(2).await;
migrate_overlay_and_mirror(&src.paired_config(&tgt)).await;
```

`cli_surface.rs` also contains a helper, `async fn remote_color(client: &LiveClient, id: u64) -> String`, which is **not** a scenario body — the wrapper guard keys on the `(cfg: &LiveConfig` signature, so this helper is correctly invisible to it. Do not give it wrappers. `auth_validates_before_writing` asserts a refusal *before* any write; check that the fake's token handling actually rejects a bad token rather than ignoring it, or the port asserts nothing.

- [ ] **Step 1: Port `migrate_overlay_and_mirror` with two orgs**

Apply **The port procedure**, but with the two-fake wrapper shown above instead of the single-`FakeOrg::start()` form.

- [ ] **Step 2: Run it and record the failure**

Run: `cargo test --locked --test live fake_migrate_overlay_and_mirror -- --nocapture`
Expected: PASS, or a failure naming a specific missing behaviour. Record the exact text. This is the first offline coverage the promotion chain has ever had, so treat a first-try pass with suspicion and say what you did to convince yourself it is real.

- [ ] **Step 3: Close the gap, per step 5 of the procedure**

- [ ] **Step 4: Port the three `cli_surface` bodies**

Repeat steps 1-3 for `auth_validates_before_writing`, `doctor_realign_after_a_remote_rename`, and `sync_direction_flags`, in that order, using the single-org wrapper unless the scenario itself needs a target.

- [ ] **Step 5: Verify and commit**

```bash
cargo test --locked
cargo test --locked -- --skip fake
cargo test --locked --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```
Expected: 0 failed; `--skip fake` unmoved; the ignored count exactly 23; clippy clean.

---

## The live gate — performed by the repo owner, not by this plan

The spec asks for the live suite to be run once against the sandbox to prove the extraction changed nothing. **No task here can do that**: it needs real credentials, and the sandbox's tokens expire in roughly thirty minutes, so a burst of live failures is usually a `401` rather than a defect.

What the tasks *do* guarantee mechanically: every `live_*` wrapper keeps its `#[ignore]` and its `from_env()` gate (enforced by `tests/live/scenario_wrappers.rs`), the ignored-test count stays at 23, and each body is moved verbatim.

The command, for whenever the owner wants it:

```bash
cargo test --locked --test live -- --ignored
```

Run it with `RDC_LIVE_*` set. If it is green, the extraction is confirmed end-to-end. Until then, the extraction is guard-confirmed but not live-confirmed, and this plan says so rather than implying otherwise.

## Out of scope

- **MDH / Data Storage.** Absent from the fake by construction, not by stubbing; it needs its own module and its own design.
- **The capture-based field-level oracle.** Moved down the list on evidence: the gaps these ports keep finding are about deletes and refusals, not pulled content.
- **Stage 3** — the per-kind lifecycle matrix and the `proptest` fuzzer over edit sequences. Stages 1-2 preserve known facts; stage 3 is where unknown churn gets found.
- **The 14 wall-clock assertions** and the **base-cache defect** in `src/cli/pull/portabilize.rs` (the portabilize post-pass updates the lockfile hash but never the base cache). Both are `src/` changes and separate decisions.
