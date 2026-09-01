# Unified change rendering

Status: approved 2026-09-01. Supersedes the per-surface printers described below.

## Problem

rdc has twelve surfaces that tell the user something changed. Three share
`resolve::render_styled_diff`; the rest each hand-roll their own shape.

| # | Surface | Today |
|---|---------|-------|
| 1 | pull conflict (`BothDiverged`) | `render_styled_diff` |
| 2 | hunk-by-hunk walker | `render_styled_diff` |
| 3 | push drift | `render_styled_diff` (via `prompt_resolve`) |
| 4 | remote-deleted prompt | hand-rolled raw body, 2-space indent, 40-line cap, no colour, no line numbers |
| 5 | `sync --dry-run` plan | `log.block()` bullet list, `- kind/slug PATCH` |
| 6 | destructive delete gate | raw `eprintln!` — bypasses `Log` |
| 7 | MDH index-drop gate | raw `eprintln!` — bypasses `Log` |
| 8 | MDH row-delete gate | raw `eprintln!`, count only — bypasses `Log` |
| 9 | `doctor` | count only |
| 10 | `migrate` | nothing; defers to `git diff` |
| 11 | `print_unified` + 2 wrappers | shared renderer, but to **stdout**; sole caller `preview_tombstone_bodies` is unreachable |
| 12 | per-object execution lines | **singular** kind (`rule/`, `queue/`) while the plan/classifier use the **plural** (`rules`, `queues`) |

Consequences, all verified in-tree:

- Surfaces 6/7/8 bypass `Log`, so an embedder consuming `Log::for_sink` (the
  desktop app) never receives any destructive confirmation.
- Surface 12 means the plan announces `hooks/legacy-export` and the executor
  reports `hook/legacy-export` a few lines later, for the same object, because
  each call site hand-builds the string.
- `-` means "bullet" in the plan and "removed line" in a diff, often on adjacent
  screens.

Two defects in the shared renderer itself, visible in captured output:

- **One gutter, two numberings.** `-` rows print `old_index` and `+` rows print
  `new_index` into the same column, so line `3` appears twice in a row.
- **Pairs are not adjacent.** `similar` emits a Replace as every delete followed
  by every insert, separating the old and new spelling of one field.

## Decisions

Settled with the user before design:

1. **Scope**: surfaces 1–8 and 11. `doctor` (9) and `migrate` (10) are out —
   giving them output they do not have is a behaviour change, not a
   presentational one.
2. **Behaviour is frozen.** Prompt keys (`k r e h s a`, `y/N`), exit codes, the
   stream each surface writes to, and the counted summaries stay as they are.
   Layout is free to change, including in `ColorMode::Plain`.
3. **Both renderer defects are fixed**: split gutter, interleaved pairs.
4. **Kind and name are separate columns** — kind dimmed, name at full weight.
   `ClassifiedItem` already stores them separately; today's plan line joins them
   with `format!("- {}/{}", it.kind, it.slug)`. This removes a join.
5. **No `would pull` / `would push` / `would prompt` section headers.** The verb
   column carries the direction on every row, so the headers are redundant.
6. **No `Update(<path>)` expansion header and no `Added N lines, removed M
   lines` line.** The row above states the verb, the object and the ± counts.
7. **No state-glyph column.** Considered (it was the ledger proposal's idea) and
   deliberately not taken.

## The grammar

Three parts. Every in-scope surface is built from them and nothing else.

```
<event line>     HH:MM:SS <action> <prose>          existing Log::event
  <row>          one per object                     NEW
    <expansion>  the diff body                      render_diff_body
```

### Row

Fixed columns. Byte offsets assume the default widths (kind 15, name 22):

| Col | Field | Weight | Source |
|-----|-------|--------|--------|
| 0–9 | indent | — | 9 spaces, so the verb lands in the action column |
| 9–15 | verb | existing `ActionColor` bucket | sync class (below) |
| 16–31 | kind | **dim** | `ClassifiedItem.kind`, verbatim from `kinds.rs` |
| 32–54 | name | full weight | `ClassifiedItem.slug`; container segments dimmed |
| 54–60 | `+N` | add green; `0` dim | added line count; blank when meaningless |
| 60–66 | `-N` | remove red; `0` dim | removed line count; blank when meaningless |
| 69+ | note | dim | the existing tag strings, verbatim |

Verb mapping, exhaustive over `SyncClass` plus the two non-classifier sources:

| Source | Verb | Colour bucket |
|--------|------|---------------|
| `LocalEdit` | `patch` | Write |
| `LocalCreate` | `post` | Write |
| `LocalDelete`, tombstone | `delete` | Destructive |
| `RemoteEdit`, `RemoteCreate` | `pull` | Read |
| `RemoteDelete` | `pull` | Read (note: `delete local; deleted on env`) |
| `BothDiverged`, `LocalEditRemoteDelete`, `LocalDeleteRemoteEdit` | `prompt` | Warn |
| MDH index drop | `drop` | Destructive |

**Compound names.** `email_templates` slugs are `<ws>/<queue>/<name>` and
`engine_fields` are `<engine>/<field>`. Container segments and their separators
render dim; the leaf renders at full weight. Same rule as kind-vs-name, applied
one level down.

**Column widths** are sized to the widest entry in the cycle, not fixed. This is
possible because `classified` is fully built before `execute::run` receives it,
so the plan, the interactive prompts and the executed rows share one width and
line up. Minimum kind width 8, minimum name width 12. Name is capped at 40 and
middle-elided past that, preserving the leaf (`main/…/rejection-default`).
Longest kind in the registry is `email_templates` (15).

### Expansion

`render_diff_body(left, right, is_json, mode)`. No header, no summary line.

- **Split gutter**: `old` and `new` get their own right-aligned sub-column.
  Context rows show both; a delete shows old only; an insert shows new only.
  Shared width = digits of `max(max_old, max_new)`, minimum 3.
- **Interleaved pairs**: within one `Replace` op, buffer the changes, split by
  tag and zip — `del[0], ins[0], del[1], ins[1], …` — emitting any surplus at
  the end. `iter_inline_changes` already pairs by position, so zipping by index
  preserves the intra-line emphasis it computed.
- Layout: 9 spaces, `old` (w, right), space, `new` (w, right), ` │ `, marker,
  space, content. The gutter therefore starts at the same column as a row's
  verb, giving the whole block one left edge.
- Row backgrounds, intra-line emphasis and JSON syntax highlighting are
  unchanged, including the `\x1b[K` fill trick.

The caller renders the connector line that used to be the header:

```
         ⎿ <dir dimmed>/<filename>   - local  + test
```

### Plain mode

New invariant: **`Plain` is `Color` minus SGR — identical glyphs and columns.**
Today Plain silently drops the `⎿` marker, so a CI log does not match what the
same command printed locally. One rendering path, escape codes gated at emit.

## Module layout

`resolve.rs` is 3,852 lines and already carries the prompts, the editor loop,
the hunk walker, the token-owner picker and the colour helpers. The new
renderer goes in `src/cli/change_view.rs`:

- `RowVerb`, `ChangeRow`, `RowWidths`
- `RowWidths::fit(rows)` — one pass over the cycle's objects
- `render_row(&ChangeRow, RowWidths, ColorMode) -> String`
- `render_connector(path, left_ann, right_ann, ColorMode) -> String`
- `render_diff_body(left, right, is_json, ColorMode) -> String`

`resolve.rs` keeps the prompts and delegates. `Log` gains `row()` so a row goes
through the same sink as everything else.

## Per-surface changes

| Surface | Change |
|---------|--------|
| 1, 3 | prompt emits event line + row + connector + body; header/summary lines dropped |
| 2 | hunk walker's synthetic slice goes through `render_diff_body` |
| 4 | replaces the hand-rolled preview with a row + one-sided body; keeps the 40-line elision |
| 5 | rows instead of bullets; the three section headers removed; `Dry run: …` summary unchanged |
| 6, 7, 8 | `eprintln!` → `Log`; tombstone/index lists become rows; prompt strings verbatim; the two gates stay **two separate prompts** |
| 11 | `print_unified`, `print_new_file_diff`, `print_deleted_file_diff` and `preview_tombstone_bodies` deleted |
| 12 | fixed by construction — the kind column is fed from `ClassifiedItem.kind` |

`MdhPlanItem` currently carries a pre-formatted `line: String`. It gains
`slug`, `verb` and `note` so it can render as a row; `line` is removed.

## Backward compatibility

- `tests/live/support/converge.rs::plan_lines_for` parses the section headers
  and `- ` item lines. It must be rewritten to read the verb column, which also
  removes its `section` state variable — the direction is now on every row.
- Unit tests pinning `Update(<path>)`, `Added 1 line, removed 1 line` and
  `- local   + remote` are rewritten against the new layout.
- Assertions on `"N would pull"` etc. match the `Dry run: …` summary, which is
  unchanged, and keep passing.
- `NO_COLOR` and non-TTY detection are untouched.
- The desktop bridge does not reference any renamed symbol.

## Out of scope

`doctor` still reports a count. `migrate` still defers to `git diff`. No
reordering of plan items. No state glyph. The diagnostic sections of the
dry-run plan (parse errors, field limit errors, missing required fields,
organization settings problems, unshared saved views, dangling secrets) keep
their existing shape — they are not object rows.
