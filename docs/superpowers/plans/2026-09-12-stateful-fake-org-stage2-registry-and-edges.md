# Stage 2: registry semantics, and a self-verifying edge table

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the four Important findings the previous whole-branch review
left, so the fake's honesty machinery says what it means and the edge table
proves itself.

**Architecture:** Three tasks. A third provenance state, so "the fake chose an
answer and the real API's is unknown" stops being squeezed into a flag that
means something else. A table-*driven* behavioural loop over `EDGES`, replacing
assertions that merely restate the table — which mechanically surfaces the
missing `saved_views.queues_filter` row. Then the documentation and guard
corrections that the same review itemised.

**Tech Stack:** Rust, `wiremock` 0.6.5, `serde_json`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`

**Predecessor:** `docs/superpowers/plans/2026-09-11-stateful-fake-org-stage2-writes-and-first-ports.md`,
complete at `65bcb69` and reviewed "ready to merge, 0 Critical". Its own Task 4
was displaced by a guard fix and is Task 2 here.

## Global Constraints

- **No production-code changes.** Everything lives under `tests/`. A step that
  seems to need a `src/` change is a finding to report, not a step.
- **Never set `RDC_LIVE_CAPTURE`**, and never modify anything under `testdata/`.
- **Provenance is the point of this plan.** A claim must not be stronger than
  its evidence; four such overclaims have already been caught across this
  effort and each was treated as a real defect. If the honest answer is
  "unknown", say unknown.
- Determinism: no sleeps, no wall clock, no randomness beyond `RunId`. Output
  pristine.
- **No customer names or customer-specific identifiers** anywhere.
- `cargo clippy --all-targets --locked -- -D warnings` clean; no `cargo fmt`.
- Commit to local `main`. **Never `git push`.**
- **The tree is shared with another person who commits to it and has rebased it
  mid-session.** Never run bare `git stash`, `git checkout`, `git restore`,
  `git reset` or `git clean`. Never rewrite history. Re-read before editing if
  a build surprises you.

## The invariants, measured at `65bcb69`

| invariant | value |
| --- | --- |
| `cargo test --locked` | **1752** passed, 0 failed |
| `cargo test --locked -- --skip fake` | **1685** passed, 0 failed |
| `cargo test --test live -- --ignored --list` | **exactly 23** |
| `cargo clippy --all-targets --locked -- -D warnings` | clean |

New tests move the first. A new test whose path does **not** contain `fake`
also moves the second — report the arithmetic rather than renaming anything to
keep a number still; that exact trick was caught and reverted once already.

## A known flake, so nobody re-diagnoses it

`src/` carries 14 wall-clock concurrency assertions ("concurrent must beat
sequential", budgets 350ms–1200ms). One was observed failing under load, then
passed 6/6 in isolation. If a full run reds on one, **re-run before
investigating** — pre-existing, out of scope, and no commit here touches `src/`.

## Verified facts this plan is built on

### `modelled: false` currently means two contradictory things

`quirks.rs` defines it as "a real API fact this registry records but the fake
does not yet reproduce." Three rows carry it and they do not agree:

| row | named after | matches the definition? |
| --- | --- | --- |
| `inbox_patch_response_omits_fields_the_get_response_includes` | the **real API's** behaviour | yes |
| `patch_persists_client_sent_id_and_url` | the **fake's** behaviour | no |
| `back_reference_growth_leaves_modified_at_unbumped` | the **fake's** behaviour, and its comment says the real API's is *unknown* | no |

So a reader scanning the table for "what does the fake not do yet?" reads the
third row as *"the fake bumps `modified_at` on back-ref growth"* — the exact
opposite of the truth.

### `has_modified_at` is a registry-invisible divergence

`kinds.rs` argues it is "deliberately not a `quirks::QUIRKS` entry" because the
registry is for real-API behaviours and this is an internal-consistency
requirement. But `back_reference_growth_leaves_modified_at_unbumped` is *also*
not an observed server fact — its own comment says so — and it *was* added to
the registry. Same category, opposite treatment, same branch.

There is a second half. The doc calls the real behaviour "unobserved", but
`live_push_create_ordering` is evidence and stronger than the branch claims: it
asserts the warning `"engines/{slug} delete failed (skipped)"`, which is only
reachable if the engine DELETE is actually **issued**, which requires remote
and lockfile to agree — i.e. requires the real API to give rdc no engine
`modified_at`. A green live run of that test *is* an observation.

### The edge tests restate the table instead of proving it

`kinds.rs`'s two coverage tests assert that rows exist in the same structure
under test. They would pass even if nothing read the table. The permanent proof
is missing: the delete-a-row experiment that demonstrated the derivation lives
only in a task report that has since been deleted.

### `saved_views.queues_filter` has no row

`testdata/live/snapshot/saved-views/rdc-it-{{RUN}}-view.json` carries
`"queues_filter": ["rdc://queues/rdc-it-{{RUN}}-invoices"]`, and the `EDGES`
doc comment already anticipates the row ("including the
`saved_views.queues_filter` one stage 2 needs next"). Without it, a regressed
`rdc` POSTing an unresolved url there gets `400 Invalid hyperlink` from a real
org and silence from the fake.

### `normalize_write`'s doc overstates its reach

It says it is reached from "every place `state.rs` produces or updates a STORED
object body". `graph.rs`'s `relink`/`unlink` (`add_ref`, `set_field`,
`remove_ref`, `remove_field`) also update stored bodies and bypass the seam.
Today's rules do not care — relink touches `queues`/`inbox`/`hooks`/`rules`,
never `settings` or `email_prefix` — but the doc currently denies that the
bypass exists, which is the honest answer to "where could the fake silently
diverge without a test noticing".

---

### Task 1: A third provenance state

**Files:** `tests/live/support/fake/quirks.rs`, `kinds.rs`

- [ ] **Step 1: Introduce the state**

Replace the `modelled: bool` flag with a three-way distinction. The three
categories that actually exist:

1. **Modelled** — the fake reproduces a real-API behaviour. Carries a citation.
2. **Not modelled** — a real API fact the fake does not reproduce yet. Carries
   a citation to what documents the fact.
3. **Chosen, unverified** — the fake had to pick an answer and the real API's
   behaviour is *unknown*. Carries what evidence exists, explicitly labelled as
   not proof.

Shape is yours — an enum is the obvious fit. Whatever you choose, the guards
must keep working and must not demand a live citation from a row that cannot
have one.

- [ ] **Step 2: Re-sort the three existing rows, and rename two**

`inbox_patch_response_omits_fields_the_get_response_includes` stays category 2.
The other two are category 3 — and both are **named after the fake's own
behaviour**, which is what makes the table read backwards. Rename them to state
the API fact they are not modelling, so every row in the table reads as a
statement about the server.

- [ ] **Step 3: Add `has_modified_at` as a row**

Category 3, for the reasons in Verified facts. Cite
`ordering.rs::live_push_create_ordering` as **corroborating** evidence and spell
out the reasoning — that the warning it asserts is only reachable if the DELETE
is issued, which requires the real API to send no engine `modified_at`. Say
plainly that this is corroboration, not proof.

Then correct `kinds.rs`'s "deliberately not a QUIRKS entry" paragraph, which no
longer holds.

- [ ] **Step 4: Guard the new state**

A row in category 3 must not claim a live citation. A row in category 1 must
have one that resolves. Extend the existing guards rather than replacing them,
and make sure none of them can pass vacuously — the "iterates an empty set"
failure has already happened once in this registry.

- [ ] **Step 5: Verify and commit**

---

### Task 2: Make the edge table prove itself

**Files:** `tests/live/support/fake/kinds.rs` (and wherever the loop reads best)

- [ ] **Step 1: Add the row that is missing**

`saved_views.queues_filter → queues`, array-shaped, no back-reference. Add it
**first**, so the loop you write next covers it from the start.

- [ ] **Step 2: Replace restatement with behaviour**

For every row in `EDGES`, drive the fake and assert what the row *claims*:

- the ref-type check **rejects** a url of the wrong kind for that field;
- for a row carrying a `back_ref`, the back-reference actually appears on the
  target after a create — `Push` appends to the named array, `Set` writes the
  named scalar.

Keep the three existing guards (`no_universal_row_carries_a_back_ref` and the
two coverage assertions) — they pin different properties. What you are
replacing is the pair that merely asserts rows exist.

- [ ] **Step 3: Prove the loop is load-bearing**

Remove one row; confirm the loop fails **naming that row**; restore. Then break
one `back_ref` field name; confirm the loop fails for that row; restore.
Neither may be committed, and the tree must be clean afterwards.

- [ ] **Step 4: Verify and commit**

---

### Task 3: The documentation and guard corrections

Small, itemised, and all from the same review.

**Files:** `quirks.rs`, `state.rs`, `validate.rs`, `tests/live/scenario_wrappers.rs`

- [ ] **Step 1: `normalize_write`'s reach**

Correct the doc: name `graph.rs`'s `relink`/`unlink` as a write path that
bypasses the seam, say why today's rules do not care, and say what would have
to change if a future rule did.

- [ ] **Step 2: `queues_bound_to_engine`'s doc**

It says `validate::on_delete` "intersects this with `queues_awaiting_deletion`".
It does not — the two checks run sequentially and the draining branch wins
outright when an engine is bound to both. Say what actually happens.

- [ ] **Step 3: The unmodelled-rejections list**

`validate.rs`'s list correctly lost the engine-delete entry when it was
modelled, but never gained its sibling: `DELETE /engine_fields/{id}` →
`409 conflict_referenced` while a schema still uses the field
(`tests/live/support/teardown.rs:51-57`). That gap is now *reachable* —
`has_modified_at: false` means engine-field deletes really do reach the fake in
`fake_push_create_ordering`, where they succeed and would be refused live. Add
it.

- [ ] **Step 4: Two guard hardenings**

In `tests/live/scenario_wrappers.rs`:

- `scenario_core_name` still requires the parameter list to close immediately
  (`"(cfg: &LiveConfig)"`). A future body taking `(cfg: &LiveConfig, seed: &Seed)`
  reproduces exactly the false negative just fixed, in a new disguise. Either
  accept the prefix followed by `)` or `,`, or add it to the documented
  blind-spot list — your call, say which and why.
- The guard never checks that `fake_X` actually **calls** `X`. An empty or
  stubbed wrapper satisfies the pairing check and shows up as a green test that
  ran nothing. Require the wrapper's body to mention the body name.

- [ ] **Step 5: A uniqueness test for quirk names**

`kinds.rs` has `kind_paths_are_unique`; `quirks.rs` has no analogue, so a
duplicated quirk `name` would compile and go unnoticed. Add one.

- [ ] **Step 6: Verify and commit**

---

## Out of scope

- The remaining **13 scenario ports**, which come next and should be ordered by
  *new kinds reached* rather than scenario size.
- **MDH / Data Storage**, which needs its own module and its own design.
- The **capture-based field-level oracle**. The previous review moved it down
  the list on evidence: both gaps the first ports found were about deletes and
  drift signals, not pulled content, which suggests per-kind delete/refusal
  modelling is the higher-yield seam right now.
- The **14 wall-clock assertions** and the **base-cache defect** in
  `src/cli/pull/portabilize.rs`. Both are `src/` changes and separate decisions.
