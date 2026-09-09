# Stateful fake org — Stage 2 foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape the fake's seams *before* fifteen more scenario ports land on
top of them, so the invariants that matter become structural rather than
reviewed by hand.

**Architecture:** Four changes, in dependency order: split the two oversized
modules; collapse the duplicated graph-edge knowledge into one table in
`kinds.rs` and derive both the ref-type check and the back-reference
maintenance from it; add the response-shaping seam the design named but never
built, and use it for the one asymmetry the spec quotes as its reason to exist;
then make the scenario-wrapper contract a guard test instead of a review habit.

**Tech Stack:** Rust, `wiremock` 0.6.5, `serde_json`, `chrono`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`

**Predecessor:** `docs/superpowers/plans/2026-09-07-stateful-fake-org-stage1.md`
(stage 1, complete at `c47b3fe`). This plan implements the "Stage 2, in this
order" recommendations of stage 1's final whole-branch review, which are
deliberately front-loaded: every one of them is cheaper at one instance than at
fifteen.

## Global Constraints

- **No production-code changes.** Everything here lives under `tests/`. A step
  that seems to need a `src/` change is a finding to report, not a step. (The
  base-cache defect stage 1 found is a separate decision, not this plan's work.)
- **Determinism:** no sleeps, no wall clock, no randomness beyond the harness's
  `RunId`. Own port and own state per test; the suite must need no
  `--test-threads=1`. `cargo test --locked` is the weekly release gate.
- **Never set `RDC_LIVE_CAPTURE`**, and never modify anything under `testdata/`.
  The goldens are artifacts captured from a real organization.
- **Every quirk carries its provenance honestly.** A `modelled: false` entry
  states what documents it; a live citation must name a test that actually
  asserts the fact, not merely a test in the right file.
- **No customer names or customer-specific identifiers** anywhere — code,
  tests, or commit messages. Neutral placeholders only.
- `cargo clippy --all-targets --locked -- -D warnings` must stay clean, and
  test output must stay pristine.
- Do **not** run `cargo fmt` — this checkout is not fmt-clean under the local
  rustfmt. Match surrounding style by hand.
- Commit to local `main`. **Never `git push`.**
- The working tree is shared with another person. Never run bare `git stash`,
  `git checkout`, `git reset` or `git clean`.

## The invariants every task must preserve

Measured at `c47b3fe`. Any task that moves one of these has broken something:

| invariant | value |
| --- | --- |
| `cargo test --locked` | 1718 passed, 0 failed |
| `cargo test --locked -- --skip fake` | **exactly 1660** (the pre-stage-1 baseline) |
| `cargo test --test live -- --ignored --list` | **exactly 23** |
| `git diff <stage-1 base>..HEAD -- src testdata` | empty |

## Verified facts this plan is built on

Read off the tree on 2026-09-09.

### File sizes

`state.rs` 1277, `mod.rs` 863, `quirks.rs` 466, `validate.rs` 250, `kinds.rs`
135. `mod.rs`'s `tests` submodule is roughly two thirds of that file.

### The graph-edge duplication — the whole table, in one place

The same edges are declared twice today: as `(kind, field, target)` triples in
`validate::REF_FIELDS` (the type check) and as hand-written match arms in
`state::relink` / `state::unlink` (the back-reference maintenance). Neither
knows about the other. This is the full set:

| owner | field | shape | target | back-ref on the target |
| --- | --- | --- | --- | --- |
| `queues` | `workspace` | single | `workspaces` | push onto `queues` |
| `queues` | `schema` | single | `schemas` | push onto `queues` |
| `queues` | `engine` | single | `engines` | none |
| `queues` | `generic_engine` | single | `engines` | none |
| `email_templates` | `queue` | single | `queues` | none |
| `labels` | `organization` | single | `organizations` | none |
| `workspaces` | `organization` | single | `organizations` | none |
| `inboxes` | `queues` | array | `queues` | **set** scalar `inbox` |
| `hooks` | `queues` | array | `queues` | push onto `hooks` |
| `rules` | `queues` | array | `queues` | push onto `rules` |
| *(any kind)* | `queues` | array | `queues` | none — type check only |
| *(any kind)* | `run_after` | array | `hooks` | none — type check only |

The last two rows are **load-bearing and deliberate**. `validate.rs:130-134`
explains why they are not scoped per kind: those two field names mean the same
thing on whatever kind carries them. Narrowing them to declared owners would
silently stop validating refs on any kind without a row.

### Two facts that make the unified table safe

- **Validation runs before defaults.** `create` calls `validate::on_write`
  and only then `create_unchecked`, which applies `kinds.rs`'s `defaults`
  (`state.rs`, `pub fn create`). So the presence-driven check only ever sees
  fields a client actually sent — never the `queues: []` that `defaults`
  seeds onto five kinds.
- **`saved_views.queues_filter` is a real ref array that nothing checks
  today** (`testdata/live/snapshot/saved-views/…view.json` carries
  `["rdc://queues/…"]`). It is the first edge stage 2's saved-views port will
  need, and a good witness that the table is extensible.

### The response-shaping gap

`OrgState::patch_organization` merges the patch and returns the whole
organization — the fake answers GET and PATCH with the same body, which is
verbatim the deficiency `src/cli/push/organization.rs:161` describes and the
spec quotes as its motivation. The documented real asymmetry, from
`src/cli/push/organization.rs:161-177`:

- the PATCH response carries `rir_key`, which `GET /organizations/{id}` omits
  entirely;
- it returns `users` in a different order;
- it normalizes inside `settings`: `width: 140` comes back `140.0`, and
  `annotation_list_table: {}` comes back `columns: []`.

Stage 1 recorded this in `QUIRKS` as `modelled: false`. `route()` builds
response bodies at several sites with no seam to shape them, which is why the
spec's second quirk shape (`shape_organization_patch_response`) was never
built.

### Two obstacles in the way of Task 3's test, found before dispatch

- **`authed_request` cannot carry a body.** Its signature is
  `async fn authed_request(method: reqwest::Method, url: &str) -> reqwest::Response`
  — it *sends* immediately and hands back the response, so nothing can be
  chained onto it. A PATCH test needs a variant that takes a JSON body (or an
  inline builder). Extending the helper is preferable to duplicating the
  client construction a third time.
- **The fake's organization has `users: []`** (`state.rs`, `OrgState::new`).
  Reversing an empty array is a no-op, so a "PATCH reorders `users`"
  assertion would be vacuous — which is why Task 3 deliberately does not
  model that third difference. See its step 3.

### The scenario-wrapper contract

`round_trip.rs` establishes the template the remaining fifteen ports follow: a
shared `async fn <name>_core(cfg: &LiveConfig)` body, a `fake_<name>_core`
wrapper with no `#[ignore]`, and a `live_<name>_core` wrapper carrying
`#[ignore = "live: needs RDC_LIVE_* env"]` plus the `LiveConfig::from_env()`
gate.

Stage 1's final review established which half of that is actually load-bearing,
and it is worth knowing before writing a guard:

- **`#[ignore]` on the live wrapper is critical.** Forget it and the live twin
  runs under a plain `cargo test`, early-returns from `from_env()`, silently
  "passes", and breaks the 23-count contract.
- **`worker_threads = 2` is NOT load-bearing for the fake.** wiremock 0.6.5
  runs its server on its own `std::thread::spawn`ed thread with its own
  current-thread runtime, and `Teardown::drop` likewise spawns its own thread,
  so a forgotten `multi_thread` cannot deadlock a port. It is a consistency
  convention with the live twin, nothing more — and the guard should say so
  rather than enforcing it as though a hang depended on it.

## File Structure

| file | responsibility after this plan |
| --- | --- |
| `tests/live/support/fake/mod.rs` | `FakeOrg`, `route()`, creds/config — no tests |
| `tests/live/support/fake/tests.rs` | the fake's own integration tests |
| `tests/live/support/fake/state.rs` | the store: ids, urls, clock, pagination, CRUD, pending deletes |
| `tests/live/support/fake/graph.rs` | back-reference maintenance and the delete cascade |
| `tests/live/support/fake/kinds.rs` | the per-kind table **and the edge table** |
| `tests/live/support/fake/quirks.rs` | the learned-facts registry, its guards, and response shaping |
| `tests/live/support/fake/validate.rs` | request rejections, deriving ref checks from `kinds`' edges |

---

### Task 1: Split the two oversized modules

Pure mechanical refactor. No behavior change, and the test counts are the proof.

**Files:**
- Create: `tests/live/support/fake/tests.rs`, `tests/live/support/fake/graph.rs`
- Modify: `tests/live/support/fake/mod.rs`, `tests/live/support/fake/state.rs`

**Interfaces:**
- Consumes: everything as it stands at `c47b3fe`.
- Produces: no new public API. Visibility may widen from private to
  `pub(super)` where a moved item needs it — note every such widening in the
  report.

- [ ] **Step 1: Move the tests out of `mod.rs`**

Move `mod.rs`'s entire `#[cfg(test)] mod tests { … }` body into a new
`tests.rs`, and declare it in `mod.rs` as:

```rust
#[cfg(test)]
mod tests;
```

Fix the imports the move breaks: paths that were `super::…` inside an inline
module resolve differently from a file module. Prefer
`crate::support::fake::…` for cross-module references, matching what the
existing tests already do.

- [ ] **Step 2: Move the graph half out of `state.rs`**

Move `relink`, `unlink`, `add_ref`, `remove_ref`, `set_field`, `remove_field`
and `cascade_queue_delete` into `graph.rs`. They are `OrgState` methods, so
either keep them as an `impl OrgState` block in `graph.rs` (Rust allows
multiple `impl` blocks for a type across files in the same crate) or make them
free functions taking `&mut OrgState` — your call, but say which you chose and
why. Keep `state.rs`'s tests with the code they exercise: a test for `relink`
belongs beside `relink`.

- [ ] **Step 3: Verify nothing moved but the code**

Run, and paste the output into your report:

```
cargo test --locked
cargo test --locked -- --skip fake
cargo test --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```

Expected: **1718 passed / 0 failed**, **exactly 1660** with `--skip fake`,
**exactly 23** ignored, clippy clean. A refactor that changes any of those
numbers has changed behavior — stop and report rather than adjusting a test.

- [ ] **Step 4: Commit**

```bash
git add tests/live/support/fake
git commit -m "refactor(fake): split the tests and the graph out of two large modules

Pure code motion before fifteen scenario ports and MDH land on top: mod.rs
keeps the server and the router, state.rs keeps the store, and the
back-reference/cascade machinery moves to graph.rs. Test counts unchanged
(1718 total, 1660 with --skip fake, 23 ignored), which is the proof.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: One edge table, two derived behaviors

The mis-cut stage 1's final review identified: the same graph knowledge lives in
`validate::REF_FIELDS` and in `graph.rs`'s match arms, and stage 2 roughly
doubles the edge count.

**Files:**
- Modify: `tests/live/support/fake/kinds.rs` (the table)
- Modify: `tests/live/support/fake/validate.rs` (derive the type check)
- Modify: `tests/live/support/fake/graph.rs` (derive relink/unlink)
- Modify: `tests/live/support/fake/tests.rs` or `state.rs` tests as needed

**Interfaces:**
- Produces: `kinds::EDGES` plus whatever types it needs, and a lookup by owner
  kind. Exact shape is yours to design — see the constraints below.

- [ ] **Step 1: Write the failing tests**

Add to `kinds.rs`'s tests:

```rust
    /// The edge table must cover every edge the two derived behaviors used to
    /// hand-write, or one of them silently stops working. These are the ten
    /// owner-scoped edges as they stood at c47b3fe.
    #[test]
    fn the_edge_table_covers_every_edge_the_fake_used_to_hand_write() {
        for (owner, field, target) in [
            ("queues", "workspace", "workspaces"),
            ("queues", "schema", "schemas"),
            ("queues", "engine", "engines"),
            ("queues", "generic_engine", "engines"),
            ("email_templates", "queue", "queues"),
            ("labels", "organization", "organizations"),
            ("workspaces", "organization", "organizations"),
            ("inboxes", "queues", "queues"),
            ("hooks", "queues", "queues"),
            ("rules", "queues", "queues"),
        ] {
            assert!(
                edges_for(owner).any(|e| e.field == field && e.target == target),
                "edge table is missing {owner}.{field} -> {target}"
            );
        }
    }

    /// The two universal rows are deliberate, not an oversight: `queues` and
    /// `run_after` mean the same thing on whatever kind carries them, so the
    /// TYPE check must still apply to a kind with no row of its own.
    #[test]
    fn the_universal_ref_fields_are_still_universal() {
        for (field, target) in [("queues", "queues"), ("run_after", "hooks")] {
            assert!(
                universal_edges().any(|e| e.field == field && e.target == target),
                "the universal type check lost {field} -> {target}"
            );
        }
    }

    /// Only the four owners that maintained a back-reference before may carry
    /// one now — an accidental extra back-ref would mutate objects the real
    /// server does not touch.
    #[test]
    fn only_the_documented_owners_maintain_a_back_reference() {
        let with_back_ref: std::collections::BTreeSet<&str> = EDGES
            .iter()
            .filter(|e| e.back_ref.is_some() && e.owner.is_some())
            .map(|e| e.owner.unwrap())
            .collect();
        assert_eq!(
            with_back_ref,
            ["hooks", "inboxes", "queues", "rules"].into_iter().collect(),
        );
    }
```

Adjust the helper names to whatever your design actually calls them, but keep
all three assertions.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --test live fake::kinds -- --nocapture`
Expected: FAIL — no edge table exists yet.

- [ ] **Step 3: Design and add the table**

Put it in `kinds.rs`. It must express, per edge: the owner kind (or that the
field is universal), the field name, whether the field holds one url or an
array of them, the target kind, and the back-reference to maintain on the
target — which is a *push onto an array* for three owners and a *scalar set*
for `inboxes.inbox`.

Constraints, in priority order:

1. **Behavior must not change.** The ten owner-scoped edges and the two
   universal ones are the complete current set — no edge gained, none lost, and
   no back-reference added to an owner that did not have one. The three tests
   above pin this; the suite's unchanged counts confirm it.
2. **The universal rows stay universal.** Do not narrow them to declared
   owners. `validate.rs:130-134`'s comment explains why, and that reasoning
   must survive the refactor in some form.
3. **Adding an edge must be a one-line change in one file.** That is the whole
   point. `saved_views.queues_filter → queues` is the next one stage 2 needs —
   do NOT add it here (it would change behavior), but satisfy yourself that
   adding it later touches only the table, and say so in your report.

- [ ] **Step 4: Derive both behaviors from it**

Rewrite `validate::on_write`'s rule 1 and its universal-array loop to walk the
table, and rewrite `graph.rs`'s `relink`/`unlink` to do the same. Delete
`REF_FIELDS` and the hand-written match arms. Neither derived site may keep a
private list of edges.

- [ ] **Step 5: Verify**

Run and paste into the report:

```
cargo test --test live fake:: -- --nocapture
cargo test --locked
cargo test --locked -- --skip fake
cargo test --test live -- --ignored --list | grep -c ': test'
cargo clippy --all-targets --locked -- -D warnings
```

Expected: the invariants above, unchanged, plus your three new tests.

Then prove the derivation is real, and put the evidence in your report:
temporarily delete the `queues.schema` row from the table, and confirm that
**both** derived behaviors break — a ref-type test and a back-reference test —
rather than only one. Restore it. If only one breaks, the two sites are not
genuinely deriving from the table.

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/fake
git commit -m "refactor(fake): derive ref checks and back-refs from one edge table

The same graph edges were declared twice — as triples in validate's
REF_FIELDS and as hand-written match arms in relink/unlink — with nothing
tying them together, and stage 2 roughly doubles the edge count. Both now
derive from one table in kinds.rs, so adding an edge is a one-line change
in one file. The two universal rows (`queues`, `run_after`) stay universal
on purpose: those field names mean the same thing on whatever kind carries
them.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: The response-shaping seam, and the asymmetry it exists for

**Files:**
- Modify: `tests/live/support/fake/quirks.rs` (the seam and the shaping rule)
- Modify: `tests/live/support/fake/mod.rs` (call the seam where bodies are built)
- Modify: `tests/live/support/fake/state.rs` (the `patch_organization` doc)
- Modify: `tests/live/support/fake/tests.rs` (tests)

**Interfaces:**
- Produces: `quirks::shape_response(kind: &str, method: &str, body: &mut Value)`,
  called from `route()` at the single point where a response body is built.

- [ ] **Step 1: Write the failing test**

The fake must stop answering GET and PATCH with the same organization body:

```rust
    /// The fact the whole design exists for. `src/cli/push/organization.rs:161`
    /// records what it cost: rdc wrote the PATCH response to disk, the response
    /// was not GET-shaped, and every sync afterwards re-pulled the org to
    /// correct itself — "one phantom '1 changed' cycle after every settings
    /// push". A fake that answers both the same way would BLESS that bug.
    #[tokio::test]
    async fn the_organization_patch_response_is_not_get_shaped() {
        let fake = FakeOrg::start().await;
        let base = fake.api_base();
        let get_body: serde_json::Value = authed_request("GET", &format!("{base}/organizations/1"))
            .send().await.expect("get").json().await.expect("json");
        assert!(
            get_body.get("rir_key").is_none(),
            "GET /organizations/{{id}} omits rir_key entirely"
        );

        // NOTE: `authed_request` sends immediately and cannot carry a body —
        // use the body-carrying variant this task adds.
        let patched: serde_json::Value = authed_json(
            reqwest::Method::PATCH,
            &format!("{base}/organizations/1"),
            &serde_json::json!({ "settings": {
                "annotation_list_table": {},
                "some_width": { "width": 140 },
            }}),
        )
        .await
        .json()
        .await
        .expect("json");
        assert!(
            patched.get("rir_key").is_some(),
            "the PATCH response carries rir_key, which GET omits"
        );
        assert_eq!(
            patched["settings"]["annotation_list_table"],
            serde_json::json!({ "columns": [] }),
            "the server normalizes an empty annotation_list_table to columns: []"
        );
        assert_eq!(
            patched["settings"]["some_width"]["width"],
            serde_json::json!(140.0),
            "the server normalizes an integer width to a float"
        );
    }
```

`authed_json` is a body-carrying sibling of the existing `authed_request`,
which you add in this task — see the obstacle noted in Verified facts.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test live fake::tests::the_organization_patch -- --nocapture`
Expected: FAIL — the fake returns the same body for both.

- [ ] **Step 3: Add the seam, then the rule**

Add `quirks::shape_response(kind, method, &mut Value)` and call it from
`route()` at the one place a response body is built, so every kind and method
passes through it. Implement exactly **two** transformations, for
`("organizations", "PATCH")` only:

- insert `rir_key` (any stable placeholder value — it is the key's *presence*
  that matters, and its absence from GET);
- inside `settings`, recursively normalize an integer `width` to a float, and
  an empty `annotation_list_table` object to `{ "columns": [] }`.

**Deliberately not modelled: the `users` reorder.** The source documents it as
the third difference, but the fake's organization carries `users: []`, so
reversing it is a no-op and an assertion on it would be vacuous. Seeding
synthetic users to make it observable would change the organization body that
*every* pull sees, for a purely cosmetic difference — and the two
transformations above are already sufficient to make a naive write-back
detectably wrong, which is the point. Record the omission in the quirk's doc
alongside what is modelled, in the same register the rest of this registry
uses.

Do not shape anything else. Every transformation must cite
`src/cli/push/organization.rs:161-177`.

- [ ] **Step 4: Flip the quirk's provenance — honestly**

`organization_patch_response_is_not_get_shaped` is currently
`modelled: false`. It is now modelled, so it needs a citation — and the
citation rules bind you: it must name a live test that *actually asserts the
fact*, not merely a test in a plausible file.

Check `tests/live/scenarios/organization.rs::live_organization_settings_push`
and `src/cli/sync/mod.rs`'s
`sync_organization_write_back_keeps_the_shape_a_pull_would_produce` (an offline
test in the lib, not a live scenario). Then choose:

- if a live scenario really does assert the asymmetry, cite it;
- if the honest answer is that the fact is proven by an offline lib test plus
  the source comment, say exactly that, and keep the source-citation form.

**Do not manufacture a live citation.** Stage 1's reviews caught two invented
ones; that is the failure mode this whole layer exists to prevent. Report which
you chose and the evidence you checked.

- [ ] **Step 5: Correct the `patch_organization` doc**

It currently says the shaping is not modelled. Update it to say what is now
modelled and what still is not.

- [ ] **Step 6: Verify**

The full invariant set, plus: confirm no other kind's responses changed, by
checking the suite's unchanged counts.

- [ ] **Step 7: Commit**

```bash
git add tests/live/support/fake
git commit -m "feat(fake): shape responses, starting with the one fact the design exists for

route() had no seam for per-(kind, method) response shaping, so the quirk
category the design named second had nowhere to attach — and the fake
answered GET and PATCH with the same organization body, which is verbatim
the deficiency src/cli/push/organization.rs:161 describes. It now carries
rir_key on PATCH only, reorders users, and normalizes width and
annotation_list_table inside settings.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Make the wrapper contract structural

**Files:**
- Create: `tests/live/scenario_wrappers.rs` (or add to an existing guard test
  file — your call, say which and why)

**Interfaces:**
- Produces: a guard test. No runtime API.

- [ ] **Step 1: Write the failing test**

A guard that scans `tests/live/scenarios/*.rs` and, for every shared scenario
body it finds, asserts both wrappers exist with the right attributes:

```rust
//! Every ported scenario must keep both of its wrappers, and the live one must
//! keep its `#[ignore]`.
//!
//! The failure this prevents is silent. A `live_*` wrapper that loses its
//! `#[ignore]` runs under a plain `cargo test`, finds no `RDC_LIVE_*`
//! credentials, early-returns from `LiveConfig::from_env()` — and PASSES,
//! having tested nothing. It also breaks the documented contract that
//! `cargo test --test live -- --ignored` selects exactly the live set
//! (`README.md:522`).
//!
//! Note what is deliberately NOT enforced here: `worker_threads = 2`. wiremock
//! runs its server on its own thread with its own current-thread runtime, and
//! `Teardown::drop` spawns its own thread too, so a forgotten `multi_thread`
//! cannot deadlock a port. It is a consistency convention with the live twin,
//! not a hang waiting to happen.
```

The test must: find every `async fn <name>_core(cfg: &LiveConfig)`; assert a
`fake_<name>_core` exists; assert a `live_<name>_core` exists and that the
`#[ignore]` attribute appears immediately above it; and assert the count of
`#[ignore]`d live wrappers plus the un-ported `#[ignore]`d scenarios equals
the 23 the live binary reports. Keep the parsing deliberately simple and
line-based — this repo's `tests/command_references.rs` is the house pattern for
a source-scanning guard, including its habit of a module-level `//!` doc
explaining the real failure it prevents.

- [ ] **Step 2: Run it — it should PASS immediately**

Only `round_trip.rs` is ported so far, and it is correct, so a passing run is
expected. That makes step 3 the important one.

- [ ] **Step 3: Prove it is load-bearing**

Temporarily remove the `#[ignore]` from `live_round_trip_core`, confirm the
guard FAILS with a message naming that wrapper, then restore. Also
temporarily rename `fake_round_trip_core` and confirm the guard catches the
missing twin. Both demonstrations go in the report; neither may be committed.

- [ ] **Step 4: Verify and commit**

The full invariant set, then:

```bash
git add tests/live
git commit -m "test(live): guard the scenario-wrapper contract

Fifteen more ports will follow round_trip's shape, and the invariant that
matters is silent when broken: a live_* wrapper that loses its #[ignore]
runs under a plain cargo test, early-returns from from_env(), and passes
having tested nothing — while breaking the documented 23-scenario contract.
This guard scans the scenario sources and refuses that. It deliberately does
not enforce worker_threads, which stage 1 established is a convention rather
than a hang waiting to happen.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Out of scope for this plan

- **Porting scenarios.** That is the next plan, and it starts once these four
  seams are in place. The final review's suggested first ports are the ones
  whose fake support already exists: `collisions`, `cross_refs`, `sidecars`.
- **The field-level tree oracle** — capturing one live pull as a tree and
  diffing the fake's pulled tree against it modulo ids, urls, run prefix and
  env-specific addresses. The final review called this the change that would
  move fidelity from shape to content, and it deserves its own design: it needs
  a live run to capture, and a decision about what counts as an acceptable
  difference.
- **MDH / Data Storage.** Its own module when it comes, per the final review.
- **PATCH-time validation.** Still the documented gap in `validate.rs`; it
  lands with whichever port first needs it (`migrate_promotion` or `engines`).
- **The base-cache defect stage 1 found** (`src/cli/pull/portabilize.rs`
  updating the lockfile hash but not the base cache). A production fix, and a
  separate decision.
