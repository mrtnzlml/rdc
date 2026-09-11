# Stage 2: the write-path seam, and the first two ports

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the half of the quirk seam that stage 2's foundation left open,
then use it by porting the two live scenarios that pay for themselves first.

**Architecture:** A `quirks::normalize_write` seam symmetric with the existing
`shape_response`, so "the server does X when it writes" stops being hand-wired
per fact. Then `ordering` — which pushes an entire hand-written graph and
asserts convergence, so every content gap in `kinds::defaults` surfaces as
non-convergence — and `organization`, which is what turns the foundation's
response-shaping work from *modelled* into *protective*.

**Tech Stack:** Rust, `wiremock` 0.6.5, `serde_json`, `chrono`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`

**Predecessors:** stage 1 (`c47b3fe`) and the stage-2 foundation (`b6fd6e6`),
both merged and shipped in `v0.10.0`. This plan implements the first three
items of that foundation's whole-branch review recommendation list.

## Global Constraints

- **No production-code changes.** Everything here lives under `tests/`. A step
  that seems to need a `src/` change is a finding to report, not a step.
- **Never set `RDC_LIVE_CAPTURE`**, and never modify anything under
  `testdata/`. The goldens and fixtures are artifacts captured from, or proven
  against, a real organization.
- **Provenance stays honest.** A `modelled: false` quirk states what documents
  it; a live citation must name a test that *actually asserts* the fact. Two
  invented citations were caught in stage 1 — do not add a third.
- Determinism: no sleeps, no wall clock, no randomness beyond the harness's
  `RunId`. Own port and own state per test. Test output pristine.
- **No customer names or customer-specific identifiers** anywhere.
- `cargo clippy --all-targets --locked -- -D warnings` clean; no `cargo fmt`.
- Commit to local `main`. **Never `git push`.**
- **The working tree is shared and actively moving.** Another person commits to
  it during this work, and has rebased it mid-session. Never run bare
  `git stash`, `git checkout`, `git reset` or `git clean`; never rewrite
  history; re-read a file before editing if a build surprises you.

## The invariants, re-measured at `b58d657`

The previous plan's numbers are stale — other work has landed, including a
release. These are current:

| invariant | value |
| --- | --- |
| `cargo test --locked` | **1752** passed, 0 failed (1748 + 4 new tests from this plan) |
| `cargo test --locked -- --skip fake` | **1685** passed, 0 failed |
| `cargo test --test live -- --ignored --list` | **exactly 23** |
| `cargo clippy --all-targets --locked -- -D warnings` | clean |

A task's new tests move the first number, and the second only if the test is
not fake-backed. The third must not move: porting a scenario **relocates** an
existing `#[ignore]` onto a `live_*` wrapper rather than adding or removing
one, which is what `tests/live/scenario_wrappers.rs` enforces.

## A known flake, so nobody re-diagnoses it

`cargo test --locked` contains **14 wall-clock concurrency assertions** of the
form "concurrent must beat sequential" (`grep -rn 'elapsed < std::time::Duration::from_millis' src/`),
budgets from 350ms to 1200ms. `cli::push::engines::tests::push_engines_patches_updates_concurrently`
(`src/cli/push/engines.rs:750`, four mocked 200ms PATCHes against a 650ms
budget) was **observed failing once** during this session's setup, while other
cargo processes were running; it then passed 6/6 in isolation at 0.28s and the
full suite passed 3/3 at 1748. So: pre-existing, load-dependent, not caused by
any work here. If a run goes red on one of those 14, re-run before
investigating. Fixing them is a `src/` change and out of scope for this plan.

## Verified facts this plan is built on

Read off the tree on 2026-09-11.

### The seam is half-built, and the foundation proved it

`quirks::shape_response(kind, method, &mut Value)` handles response shaping,
reached from a single `kind_response` wrapper that all seven of `route()`'s
body-building sites pass through. There is **no write-path counterpart**. When
the foundation discovered that the organization's `settings` normalization is a
*storage* fact rather than a response fact, the fix had to be hand-wired as
`if k == "settings"` inside `state.rs`'s `patch_organization`.

The foundation's whole-branch review found three facts now waiting on that
missing seam, and named them as sharing one root:

1. **`inbox.email` is derived on create and never re-derived on write.**
   `kinds.rs`'s `inbox_defaults` synthesizes the address; `state.rs`'s `patch`
   is an unconditional shallow merge, so a PATCH of `email_prefix` leaves a
   stale `email` in the store and in every later GET. `src/snapshot/limits.rs:468`
   documents the address as server-derived, which is why `strip_for_create`
   removes it — and `rdc` does PATCH `email_prefix`. A fake-backed inbox port
   would converge where a real org produces the exact phantom-drift cycle this
   instrument exists to catch.
2. **Back-reference growth does not bump the parent's `modified_at`.**
   `graph.rs`'s `add_ref`/`set_field` mutate a target and nothing else; only
   `state::patch` stamps the clock. Whether the real API bumps `modified_at`
   when `queue.hooks` grows is **unknown either way** — and that is the point:
   the fake has silently picked an answer with nothing recording the choice.
3. **`patch`'s shallow merge persists read-only server-owned keys.** `id` and
   `url` are not protected, and `rdc` does PATCH full objects containing both.

### What `ordering` needs, and already has

`tests/live/scenarios/ordering.rs` (360 lines) writes the 19-file
`testdata/live/snapshot/**` fixture to disk with `support::snapshot::write_snapshot`,
then pushes the whole graph in one sync. Its oracle is **server-side**: the
fixture queue binds the fixture engine, and `POST /queues` must refuse a queue
whose schema extracts a field the bound engine lacks —

> `'rdc-it-<run>-probe_field' is not present among names of engine fields`

The fake **already enforces exactly that** (`validate.rs:205`, rule 5 from
stage 1). Its second oracle is the `RDC_TRACE_HTTP` trace, read back through
`support::trace`, which is client-side and therefore backend-agnostic. It ends
in `assert_converged` (`ordering.rs:195`).

This is why the foundation's review recommended it as the first port over the
`collisions`/`cross_refs`/`sidecars` trio: it exercises `engines`,
`engine_fields` and `saved_views`, and every content gap in `kinds::defaults`
surfaces as non-convergence — with no live capture and no new oracle mechanism.
It also strands an engine per run against a real org (a bound engine is refused
deletion for up to 24 hours), a cost the fake does not have.

### What `organization` needs

`tests/live/scenarios/organization.rs` (172 lines) reads and restores the org's
`settings` out of band through `LiveClient::get_organization_settings` /
`patch_organization_settings`, pushes a settings change through `rdc`, and
asserts persistence plus second-cycle stability via
`assert_unprefixed_object_stable`. Both client methods already exist and are
backend-agnostic.

Porting it is what converts the foundation's response-shaping work from
*modelled* to *protective*: today the organization asymmetry is asserted only
over raw HTTP in the fake's own tests, and **nothing proves that regressing
`src/cli/push/organization.rs` back to writing the whole PATCH response makes a
fake-backed test go red.** That regression is the original incident.

## File Structure

| file | change |
| --- | --- |
| `tests/live/support/fake/quirks.rs` | add `normalize_write`; move the settings rule into it |
| `tests/live/support/fake/state.rs` | call the seam; drop the hand-wired `if k == "settings"` |
| `tests/live/support/fake/kinds.rs` | `inbox_defaults` keeps create-time derivation; the write rule lives in `quirks` |
| `tests/live/scenarios/ordering.rs` | extract body, add two wrappers |
| `tests/live/scenarios/organization.rs` | extract body, add two wrappers |

---

### Task 1: The write-path seam

**Files:**
- Modify: `tests/live/support/fake/quirks.rs`, `state.rs`, `kinds.rs`, `tests.rs`

**Interfaces:**
- Produces: `quirks::normalize_write(kind: &str, body: &mut Value)`, called
  from both `create_unchecked` and `patch` in `state.rs`.

- [x] **Step 1: Write the failing test**

The discriminating test is the inbox one — it is the fact that would otherwise
bless a real churn cycle:

```rust
    /// `inbox.email` is SERVER-DERIVED from `email_prefix`
    /// (`src/snapshot/limits.rs:468`, which is why `strip_for_create` removes
    /// it). So a PATCH that changes the prefix must change the address — in
    /// the STORE, not just in the response. A fake that derives it only on
    /// create leaves a stale address that every later GET repeats, and an
    /// inbox port would then converge where a real org produces exactly the
    /// phantom-drift cycle this instrument exists to catch.
    #[tokio::test]
    async fn an_inbox_email_is_re_derived_when_its_prefix_changes() {
        let fake = FakeOrg::start().await;
        let mut st = fake.state();
        let ws = st.create("workspaces", json!({ "name": "W" })).unwrap();
        let sc = st.create("schemas", json!({ "name": "S" })).unwrap();
        let q = st
            .create("queues", json!({ "name": "Q", "workspace": ws["url"], "schema": sc["url"] }))
            .unwrap();
        let inbox = st
            .create(
                "inboxes",
                json!({ "name": "In", "email_prefix": "before", "queues": [q["url"]] }),
            )
            .unwrap();
        assert_eq!(inbox["email"], json!("before@fake.rossum.invalid"));

        let id = inbox["id"].as_u64().unwrap();
        let patched = st.patch("inboxes", id, &json!({ "email_prefix": "after" })).unwrap();
        assert_eq!(
            patched["email"],
            json!("after@fake.rossum.invalid"),
            "a prefix change must re-derive the address"
        );
        assert_eq!(
            st.get("inboxes", id).unwrap()["email"],
            json!("after@fake.rossum.invalid"),
            "and the STORE must hold the new address, not just the response"
        );
    }
```

- [x] **Step 2: Run it and watch it fail**

Run: `cargo test --test live fake::tests::an_inbox_email -- --nocapture`
Expected: FAIL — the store keeps `before@…`.

- [x] **Step 3: Add the seam**

`quirks::normalize_write(kind, &mut Value)`, called from `state.rs` at **both**
write entrances — `create_unchecked` and `patch` — mirroring how
`shape_response` is called from exactly one response path. Move the
organization `settings` normalization into it as the first rule, deleting the
hand-wired `if k == "settings"`. Add the inbox rule as the second: when a body
carries `email_prefix`, (re-)derive `email` from it.

Two things to get right, and say how you did in your report:

- **`create` must keep working.** `inbox_defaults` derives the address at
  create time today. Decide whether the seam replaces that or complements it —
  either is defensible, but the create-path result must not change, and there
  must not be two places deriving the same field differently.
- **Order versus validation.** `create` validates before allocating an id, on
  purpose (a refused create must not consume one). Say where in that order the
  seam runs, and why that is right.

- [x] **Step 4: Record what is still not modelled**

The other two facts from the review remain unmodelled, and the registry is
where that belongs. Add `modelled: false` quirks for both, each citing what
documents it:

- back-reference growth does not bump the parent's `modified_at`, and **the
  real API's behavior here is unknown** — say that plainly rather than implying
  the fake is wrong; the defect is the unrecorded choice, not the choice;
- `patch`'s shallow merge persists read-only `id`/`url` a client may send.

- [x] **Step 5: Verify and commit**

The four invariants, plus: confirm no other kind's stored state changed, and
that the organization tests from the foundation still pass unaltered.

```bash
git add tests/live/support/fake
git commit -m "feat(fake): normalize on write, not just in the response

shape_response had no counterpart, so every 'the server does X when it
writes' fact was hand-wired — the organization settings normalization as
an `if k == \"settings\"` inside patch_organization, and inbox.email
derived on create and never again though rdc PATCHes email_prefix. Both
now run through quirks::normalize_write at both write entrances. The two
facts still unmodelled are recorded rather than left implicit.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Port `ordering`

**Files:**
- Modify: `tests/live/scenarios/ordering.rs`

- [x] **Step 1: Extract and wrap**

Rename `live_push_create_ordering` to `push_create_ordering`, take
`cfg: &LiveConfig`, drop the test attributes and the `from_env()` block, change
nothing else in the body. Add `fake_push_create_ordering` (no `#[ignore]`) and
`live_push_create_ordering` (keeping `#[ignore = "live: needs RDC_LIVE_* env"]`
and the gate), both `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`.

`tests/live/scenario_wrappers.rs` will check the pair exists and that the live
one keeps its `#[ignore]`; the ignored count must stay 23 because the attribute
**moves** rather than multiplies.

- [x] **Step 2: Run the fake twin and read the output**

Run: `cargo test --test live fake_push_create_ordering -- --nocapture`

**This is the step that earns the port.** Three outcomes, three responses:

- **It passes.** Then the fake already models everything this scenario needs —
  report that, and say which of its assertions were actually exercised (the
  engine-field refusal, the trace order, the convergence).
- **It fails because the fake is missing something** — an endpoint, a field, a
  server-owned default. Fix the fake, and report each gap with what named it.
  This is the expected case and the reason this port goes first.
- **It fails because the fake is faithful and `rdc` does not settle.** That is
  a real `rdc` defect found offline. **Do not work around it**, do not weaken
  an assertion, do not adjust the fixture. Gather the evidence — the plan
  lines, the byte diff, the trace, the object and field — write it up, leave
  the test failing, and report `DONE_WITH_CONCERNS`.

- [x] **Step 3: Verify and commit**

All four invariants, with the ignored count still exactly 23.

---

### Task 3: Port `organization`

**Files:**
- Modify: `tests/live/scenarios/organization.rs`

- [x] **Step 1: Extract and wrap**

Same shape as Task 2: `organization_settings_push(cfg: &LiveConfig)` plus
`fake_` and `#[ignore]`d `live_` wrappers.

Note this scenario's teardown restores the org's original `settings` through
`LiveClient::patch_organization_settings`. Against the fake that is harmless
but should still run — do not special-case it away.

- [x] **Step 2: Make the port protective**

The port itself only proves the scenario runs. What makes it *worth* porting is
that it should fail if `rdc` regressed to the naive write-back — which is the
original incident. Demonstrate that, and put the evidence in your report:
temporarily change `src/cli/push/organization.rs` to write the whole PATCH
response instead of just `settings`, confirm `fake_organization_settings_push`
goes **red**, then restore. **Restore precisely** — `git diff` must be empty
afterwards, and the tree is shared, so never use `git checkout` to do it.

If it does *not* go red, that is the finding: say so, and say what the fake
would need in order to catch it. A port that cannot catch the incident it was
chosen for is worth knowing about.

- [x] **Step 3: Verify and commit**

---

### Task 4: Make the edge table self-verifying

**Status: displaced, not started — carried forward to the next plan.**
While porting the two scenarios above, `tests/live/scenario_wrappers.rs` was
found to be silently blind: it keyed its per-scenario pairing and
`#[ignore]`-placement checks on a body name ending in `_core`, so
`push_create_ordering` and `organization_settings_push` — neither named that
way — were invisible to everything except the blanket total-ignored count,
which cannot name a broken wrapper or catch a missing twin at all. That is a
guard blind-spot that compounds with every future port, so fixing it
(`496319b`, keying `scenario_core_name` on the `(cfg: &LiveConfig)` signature
instead of the `_core` suffix) took this task's slot instead of this task.
The edge-table self-verification work below does not compound the same way —
it stays exactly as valuable a week from now — so it was deferred rather than
squeezed in alongside the guard fix and the two ports. Nothing else in this
plan recorded that swap before this note.

Before starting Step 1, carry forward these four findings from the review of
this plan's own branch — each is a concrete opening move, not just a
concern:

1. **`modelled: false` conflates two different meanings.** The flag is
   defined as "the real API's behavior isn't reproduced here," which is what
   `patch_persists_client_sent_id_and_url` and
   `inbox_patch_response_omits_fields_the_get_response_includes` mean by it.
   But `back_reference_growth_leaves_modified_at_unbumped` uses the same
   `false` to mean something else — the *fake's* choice is arbitrary and the
   real API's behavior is simply unknown. A reader scanning the table for
   "what does the fake not do yet?" reads that third entry backwards. Give
   the "fake picked an answer, real behavior unknown" case its own state
   (a third variant, or a second field alongside `modelled`) so the two
   meanings stop sharing one boolean.
2. **`has_modified_at` is a registry-invisible divergence, and its own
   justification contradicts a row added on this same branch.** Whatever
   reasoning currently keeps `has_modified_at` out of `QUIRKS` should be
   re-read against `back_reference_growth_leaves_modified_at_unbumped` — that
   row exists precisely because a `modified_at`-shaped fact was judged
   worth registering. This is also the first place the fake has been shaped
   to match *rdc's own contract* rather than the *real server's* behavior —
   worth deciding on purpose, and recording, before a later port crosses the
   same boundary without noticing it moved.
3. **`saved_views.queues_filter` has no `EDGES` row.** The ordering port
   exercises `saved_views`, but its coverage there is thinner than this plan
   advertised because that field never got an `EDGES` entry. The `EDGES` doc
   comment already anticipates this gap; adding the row is a one-line fix,
   worth doing as part of (or just before) Step 1's table-driven rewrite.
4. **`normalize_write`'s doc comment overclaims its own reach.** It says it
   is called from every place a stored body is updated, but `graph.rs`'s
   `relink`/`unlink` — which also update stored bodies, growing or shrinking
   a back-reference — call neither `normalize_write` nor go through
   `state.rs`'s `create_unchecked`/`patch` at all. Either narrow the doc
   comment to what is actually true today, or decide whether `relink`/`unlink`
   should route through the seam too (which would also bear on finding 2
   above, since a back-ref growing is exactly what leaves `modified_at`
   unbumped).

The foundation's review flagged that `kinds.rs`'s two edge tests are
table-*shape* assertions — they check rows exist in the same structure under
test, and would pass even if nothing read the table. The permanent proof is
missing; the delete-a-row experiment lives only in a report.

**Files:**
- Modify: `tests/live/support/fake/kinds.rs` (or wherever the loop reads best)

- [ ] **Step 1: Replace them with a table-driven behavioral loop**

For every row in `EDGES`: assert the ref-type check **rejects** a url of the
wrong kind for that field, and for every row carrying a `back_ref`, assert the
back-reference actually appears on the target after a create. Then a row that
stops being derived fails loudly, and every future row is self-verifying.

Keep the three guards the foundation added (`no_universal_row_carries_a_back_ref`
and the two coverage assertions) — those pin different properties.

- [ ] **Step 2: Prove it**

Remove one row, confirm the loop fails for that row specifically, restore.

- [ ] **Step 3: Verify and commit**

---

## Out of scope

- **Re-keying `shape_response` to `(kind, method, Endpoint)`.** It cannot
  currently distinguish a list GET from a detail GET, and a list body arrives
  as the pagination envelope. The foundation's review was explicit that this
  should land *with its first GET-side user* — the inbox port — rather than
  speculatively.
- **The remaining thirteen scenario ports**, MDH/Data Storage, and
  `POST /hooks/create` (a routing gap, not a shaping one).
- **The capture-based field-level oracle.** Still the right strategic move, and
  best designed after these two ports have shown how large the content gap is.
- **The 14 wall-clock concurrency assertions** and the base-cache defect in
  `src/cli/pull/portabilize.rs`. Both are `src/` changes and separate
  decisions.
