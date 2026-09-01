# Unified Change Rendering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render every change rdc shows the user through one row + expansion grammar, fixing the split-gutter and pair-interleaving defects and routing the destructive gates through `Log`.

**Architecture:** A new `src/cli/change_view.rs` owns the row renderer, the connector line and the diff body. `resolve.rs` keeps the prompts and delegates. `Log` gains `row()`. Every in-scope surface is rebuilt from `event line → row → expansion`.

**Tech Stack:** Rust 2024, `similar` v3 (Histogram + `inline`), existing `Log`/`ColorMode` plumbing.

**Spec:** `docs/superpowers/specs/2026-09-01-unified-change-rendering-design.md`

## Global Constraints

- Prompt keys (`k r e h s a`, `y/N`), exit codes, per-surface streams and the counted summaries (`Dry run: N would push, …`) are frozen — byte-identical.
- `ColorMode::Plain` renders identical glyphs and columns to `Color`, differing only in SGR. No `\x1b` may appear under `Plain`.
- Row columns (kind 15 / name 22 default): indent 9, verb 9–15, kind 16–31, name 32–54, `+N` 54–60, `-N` 60–66, note 69+.
- Widths: `MIN_KIND = 8`, `MIN_NAME = 12`, `MAX_NAME = 40`; longest registry kind is `email_templates` (15).
- Workspace lints set `dead_code = "deny"`. Deleted functions must lose every reference.
- No customer names or identifiers anywhere, including test fixtures and commit messages. Use `main`, `invoices`, `orders`, `dev`/`test`/`prod`.
- Never run repo-wide `cargo fmt` (pre-existing skew).

---

### Task 1: `change_view.rs` — row renderer

**Files:**
- Create: `src/cli/change_view.rs`
- Modify: `src/cli/mod.rs` (add `pub mod change_view;`)

**Interfaces:**
- Consumes: `crate::cli::resolve::ColorMode`
- Produces:
```rust
pub enum RowVerb { Patch, Post, Delete, Pull, Prompt, Drop }
impl RowVerb { pub fn token(self) -> &'static str; }   // 6 chars, space-padded
pub struct ChangeRow<'a> {
    pub verb: RowVerb, pub kind: &'a str, pub name: &'a str,
    pub added: Option<usize>, pub removed: Option<usize>, pub note: Option<&'a str>,
}
pub struct RowWidths { pub kind: usize, pub name: usize }
impl RowWidths {
    pub const MIN_KIND: usize = 8;
    pub const MIN_NAME: usize = 12;
    pub const MAX_NAME: usize = 40;
    pub fn fit<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(pairs: I) -> Self;
}
pub fn elide_name(name: &str, max: usize) -> String;
pub fn render_row(row: &ChangeRow<'_>, w: RowWidths, mode: ColorMode) -> String;
```

- [ ] **Step 1: Write failing tests** in `change_view.rs`'s `mod tests`:
  `row_columns_land_at_fixed_offsets`, `kind_is_dim_and_name_is_not`,
  `compound_name_dims_container_segments`, `absent_counts_render_blank`,
  `plain_is_color_minus_sgr`, `widths_fit_to_the_batch_with_minimums`,
  `long_name_middle_elides_preserving_leaf`.
- [ ] **Step 2:** `cargo test --lib change_view` → FAIL (module missing).
- [ ] **Step 3:** Implement. Row assembly:
  `{9sp}{verb:6} {kind:<kw} {name:<nw}{added:>6}{removed:>6}   {note}`, trailing
  spaces trimmed. Kind wrapped in dim; a name containing `/` splits at the last
  `/` with the head + separator dim and the leaf plain. `+N` uses add-bold, `-N`
  remove-bold, a literal `0` dim. `fit` takes `max(len)` clamped to the min/max.
  `elide_name` keeps the leaf and replaces the middle with `…`.
- [ ] **Step 4:** `cargo test --lib change_view` → PASS.
- [ ] **Step 5:** Commit `feat(cli): add the change-row renderer`.

---

### Task 2: `change_view.rs` — diff body

**Files:**
- Modify: `src/cli/change_view.rs`

**Interfaces:**
- Produces: `pub fn render_diff_body(left: &str, right: &str, is_json: bool, mode: ColorMode) -> String;`
  Returns `""` when the sides are byte-identical.

- [ ] **Step 1: Write failing tests:** `gutter_splits_old_and_new_columns`,
  `replace_pairs_interleave`, `identical_sides_render_empty`,
  `json_highlighting_only_for_json`, `body_carries_no_header_or_summary`,
  `plain_body_has_no_sgr`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement. Port the row/emphasis/JSON logic out of
  `resolve::render_styled_diff` (SGR constants move too). Layout per row:
  `{9sp}{old:>w} {new:>w} │ {marker} {content}`, `w = max(digits).max(3)`.
  Context rows fill both numbers, deletes only `old`, inserts only `new`.
  Interleave by buffering each op's changes, partitioning by tag and zipping
  `del[i]`/`ins[i]`, then emitting the surplus.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(cli): split the diff gutter and interleave changed pairs`.

---

### Task 3: connector line + `Log::row`

**Files:**
- Modify: `src/cli/change_view.rs`, `src/log.rs`

**Interfaces:**
- Produces:
```rust
pub fn render_connector(path: &std::path::Path, left: &str, right: &str, mode: ColorMode) -> String;
// "         ⎿ <dir dim>/<file>   - <left>  + <right>"; empty annotations omit the legend
impl Log { pub fn row(&self, body: &str); }   // writes body verbatim, clearing any status line
```

- [ ] **Step 1: Write failing tests:** `connector_dims_the_directory`,
  `connector_without_annotations_omits_legend`, `log_row_writes_verbatim`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement both.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(cli): add the diff connector line and Log::row`.

---

### Task 4: rewire the conflict, drift and hunk prompts (surfaces 1–3)

**Files:**
- Modify: `src/cli/resolve.rs`

- [ ] **Step 1:** Update the existing tests `render_styled_diff_plain_layout` and
  `render_styled_diff_color_backgrounds_and_highlight` to target
  `change_view::render_diff_body` and the new layout; add
  `conflict_prompt_emits_row_then_body`.
- [ ] **Step 2:** `cargo test --lib resolve` → FAIL.
- [ ] **Step 3:** Delete `render_styled_diff`, `diff_display_path` and
  `label_annotation`. `prompt_resolve_with_bytes_and_color` emits
  `render_row` + `render_connector` + `render_diff_body`;
  `prompt_single_hunk` calls `render_diff_body` on its synthetic slice.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `refactor(cli): render conflicts as row plus body`.

---

### Task 5: remote-deleted prompt (surface 4)

**Files:**
- Modify: `src/cli/resolve.rs`

- [ ] **Step 1:** Add `remote_delete_prompt_renders_a_one_sided_body` and
  `remote_delete_preview_still_elides_past_40_lines`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Replace the hand-rolled indented preview in
  `prompt_remote_delete_with_color` with row + connector + one-sided body,
  keeping the 40-line elision and the verbatim prompt string.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `refactor(cli): show remote deletions as a one-sided diff`.

---

### Task 6: dry-run plan (surface 5) + `MdhPlanItem`

**Files:**
- Modify: `src/cli/sync/mod.rs`, `src/cli/pull/mdh.rs`

**Interfaces:**
- `MdhPlanItem` loses `line: String` and gains
  `slug: String`, `verb: RowVerb`, `note: Option<String>`.

- [ ] **Step 1:** Add `dry_run_plan_emits_rows_without_section_headers` to `tests/cli_sync.rs`.
- [ ] **Step 2:** `cargo test --test cli_sync dry_run_plan_emits_rows` → FAIL.
- [ ] **Step 3:** Replace the three bullet blocks with `RowWidths::fit` over the
  classified + MDH + secret-only items, then one `log.row` per item. Delete the
  `would pull` / `would push` / `would prompt` event lines. Leave the diagnostic
  sections and the `Dry run: …` summary untouched.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(sync): render the dry-run plan as change rows`.

---

### Task 7: destructive gates (surfaces 6–8)

**Files:**
- Modify: `src/cli/push/deletes.rs`, `src/cli/push/mdh.rs`, `src/cli/push/mdh_data.rs`

- [ ] **Step 1:** Add `delete_gate_goes_through_the_log` and
  `index_drop_gate_goes_through_the_log`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Thread `&Arc<Log>` into `confirm_or_refuse`; replace every
  `eprintln!` with `log.event` / `log.row`. Tombstones and index names become
  rows. Keep both gates as separate prompts with verbatim prompt strings.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `fix(push): route the destructive gates through the log`.

---

### Task 8: delete the stdout path (surface 11) + fix the live parser

**Files:**
- Modify: `src/cli/resolve.rs`, `src/cli/push/deletes.rs`, `tests/live/support/converge.rs`

- [ ] **Step 1:** Rewrite `plan_lines_for` to read the verb column
  (`pull` → `would pull`; `patch`/`post`/`delete`/`drop` → `would push`;
  `prompt` → `would prompt`) and update its doctest-style fixtures.
- [ ] **Step 2:** `cargo test --test live` (compile check) → FAIL.
- [ ] **Step 3:** Delete `print_unified`, `print_new_file_diff`,
  `print_deleted_file_diff` and `preview_tombstone_bodies`.
- [ ] **Step 4:** `cargo test` full suite → PASS.
- [ ] **Step 5:** Commit `refactor(cli): drop the unreachable stdout diff path`.

---

## Self-review

- Spec coverage: surfaces 1–3 (T4), 4 (T5), 5 (T6), 6–8 (T7), 11 (T8), 12 (T1, by construction). Defects (T2). Plain invariant (T1, T2). Module layout (T1–T3). Backward compat (T8).
- Type consistency: `RowVerb`/`ChangeRow`/`RowWidths`/`render_row`/`render_connector`/`render_diff_body` are named identically in every task.
- Out of scope confirmed untouched: `doctor`, `migrate`, plan ordering, state glyph, diagnostic sections.
