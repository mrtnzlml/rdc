# Per-kind `KindCodec` refactor — Design

- **Date:** 2026-06-01
- **Status:** Draft, pending review
- **Author:** Martin (with Claude)
- **Scope:** How rdc serializes, hashes, redacts, strips, and lays out each Rossum object kind across the `pull` / `sync` / `push` / `deploy` paths.

## 1. Problem & motivation

Per-kind behaviour is spread across ~9 independent *passive* registries that callers must each remember to invoke with the right `kind` string, and across ~30 call sites that hand-roll byte production. The two halves that must stay consistent — **what bytes land on disk** and **what bytes feed the lockfile `content_hash`** — are produced *independently* at every site. When a per-kind rule is added to a registry but not wired into one of the N sites, the on-disk bytes and the recorded hash silently diverge, and `sync`/`deploy` then report phantom drift or spurious conflicts.

This was confirmed empirically (the `agenda_id` bug: declared in `redact_on_pull` but never wired into the engine writer, so `sync` wrote the live id while a prior `deploy` had written the sentinel) and by a full-tree audit that found the **same class** live in several more places.

### The registries (the per-kind "knobs")

| Registry | Location | Purpose |
|---|---|---|
| `strip_for_create` / `kind_specific_strip` | `snapshot/create.rs:27,66` | fields removed from outbound within-env POST/PATCH bodies |
| `strip_for_cross_env_patch` | `snapshot/create.rs:157` | extra strips for cross-env deploy bodies/compares (`organization`; hooks `token_owner`; engine_fields `name`) |
| `redact_on_pull` / `redact_for_disk` | `snapshot/create.rs:91,115` | values replaced with a constant sentinel on disk (queues `counts`, hooks `status`, engines `agenda_id`) |
| `redacted_disk_bytes` | `snapshot/create.rs:141` | the *intended* single source of disk bytes — but only used by queues + engines |
| `HIDDEN_FIELDS` / `serialize_for_disk` | `snapshot/key_order.rs:34,95` | `modified_at` stripped on disk (only for kinds whose writer calls it) |
| `HOOK_KEY_ORDER` / `reorder_top_level` | `snapshot/key_order.rs:39,108` | cosmetic key ordering (hooks only) |
| `sort_queues` / `sort_string_arrays` | `snapshot/hook.rs:138`, `noise.rs:103` | set-like URL array ordering (two parallel impls) |
| `NOISE_FIELDS` + combined-hash builders + extra-strips | `noise.rs:14`, `state/lockfile.rs:163/209/232/265` | hash-input production, **deliberately decoupled** from disk bytes |
| derived gate `pull_redacts_kind` | `deploy/common.rs:302` | duplicates `redact_on_pull` for the deploy drift check (currently `"queues"` only) |

### Live divergences found by the audit (all the same bug class)

| # | Site | Symptom | Severity |
|---|---|---|---|
| a | `deploy/common.rs:302` `pull_redacts_kind` | engines phantom-drift on every `rdc deploy` once the pull baseline is redacted | active (introduced by the in-progress engine fix) |
| b | `sync/mod.rs:658` workspaces | pull strips `modified_at` recursively; the sync remote-hash recompute uses raw `to_vec_pretty` → spurious RemoteEdit/BothDiverged | active, pre-existing |
| c | `push/engines.rs:156` | post-PATCH write re-emits raw `agenda_id` | active |
| d | `sync/execute.rs` conflict + restore builders | `maybe_strip_overlay` never applied → objects with an overlay configured **never converge to Clean** | active, all overlay kinds |
| e | `sync/execute.rs:1917` schema restore | downgraded to Flat hash + formulas dropped → phantom drift + incomplete restore for schemas with formulas | active |
| f | ~9 flat kinds | `modified_at` written to disk (redacted_disk_bytes / raw paths skip `HIDDEN_FIELDS`) | cosmetic git churn (hash-neutral) |
| g | `snapshot/queue.rs`, `snapshot/engine.rs` | dead `write_queue` / `write_engine` serialize without redaction | latent footgun |

Engine `agenda_id` redaction **never** worked end-to-end through pull/sync in any commit (git-verified). The sentinel users sometimes saw came from `deploy` (which redacts on write-back); a later `sync` overwrote it. This is a *never-wired latent gap*, not a regression — which is precisely the fragility this refactor removes.

## 2. Goals / Non-goals

**Goals**
1. Make on-disk bytes and the lockfile hash **impossible to diverge** for a given kind, by construction.
2. Make the compiler enforce that every kind defines every behavioural dimension (no "added to a list, forgot to wire it").
3. Funnel every `pull`/`sync`/`push`/`deploy` call site through one per-kind codec; delete all inline `serde_json::to_vec_pretty(model)` byte production.
4. Fix the live divergences (a–g) as a consequence of #3, each guarded by a regression test.
5. Add a cross-kind invariant property test that fails CI on any future divergence.

**Non-goals**
- Changing the wire protocol, the lockfile *format*, the directory layout, or the conflict-resolution UX.
- Changing which fields are server-managed/redacted (the existing registry *contents* are preserved; only their *structure* changes).
- crates.io / release automation (separate track).

## 3. Architecture: the `KindCodec` trait

### 3.1 The canonical pipeline & the disk↔hash invariant

Every kind runs one pipeline; disk bytes and the hash both derive from the **same canonical `Value`**, so they cannot disagree on substance:

```
model ──to_value──▶ Value
                     │  (per-kind codec, in ONE place)
                     ├─ split sidecars (hook code / rule trigger_condition / schema formulas)
                     ├─ redact volatile → constant sentinel   (counts / status / agenda_id)
                     ├─ strip hidden (modified_at) recursively
                     └─ key-order (e.g. HOOK_KEY_ORDER)
                     ▼
            canonical Value'
              ├─ disk_bytes() = pretty(Value') + '\n'   (+ sidecar files)
              └─ base_hash()  = SHA256( canonicalize_for_hash(pretty(Value')) ⊕ sidecar bytes )
```

Because redaction replaces volatile fields with a **constant** sentinel, hashing the redacted bytes is already stable across server churn — so the per-kind hash *extra-strip* lists (e.g. hooks `["status"]` in `hook_combined_hash`) become **redundant and are removed**. `base_hash` keeps `canonicalize_for_hash` (strip `modifier`, sort keys) purely so cosmetic formatting never churns the hash. Net: the entire "hash-input production" registry family collapses to "hash the codec's own disk bytes, plus the codec's own sidecars."

This also fixes (b)/(e)/(f) for free: every site uses the *same* producer, so recursive-vs-top-level stripping or Flat-vs-combined hash mismatches can't happen.

### 3.2 Trait definition (illustrative)

```rust
/// Sidecars extracted from a kind's JSON (code/formulas live in separate files).
pub struct DiskArtifact {
    pub json: Vec<u8>,                       // canonical disk bytes (pretty + '\n')
    pub sidecars: Vec<(String, Vec<u8>)>,    // (relative path, bytes), e.g. "<slug>.py"
}

pub trait KindCodec: Sync {
    /// Stable kind string ("engines", "hooks", ...). Used for registry lookup + lockfile keys.
    fn kind(&self) -> &'static str;

    /// Canonical on-disk artifact for a remote object Value.
    /// Applies: sidecar split, redact→sentinel, hidden-field strip, key-order.
    fn disk_bytes(&self, value: &Value) -> Result<DiskArtifact>;

    /// Lockfile base hash, DERIVED from disk_bytes:
    ///   SHA256( canonicalize_for_hash(disk_bytes.json) ⊕ sidecar bytes ).
    /// Defaulted; individual codecs supply the pipeline (via disk_bytes) but
    /// NEVER override the hashing algorithm — that is what kills divergence.
    fn base_hash(&self, value: &Value) -> Result<String> { /* default; not overridden */ }

    /// Outbound within-env POST/PATCH body (strip server-managed fields).
    fn create_body(&self, value: &mut Value);

    /// Outbound cross-env (deploy) body / idempotency-compare form.
    fn cross_env_body(&self, value: &mut Value);

    /// Overlay paths for a given slug, if this kind supports overlays.
    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> { None }

    /// On-disk location for a slug (flat or nested).
    fn path(&self, paths: &Paths, slug: &str) -> PathBuf;
}
```

`base_hash` is a *defaulted* method computed from `disk_bytes` — individual codecs supply the per-kind *pipeline* (redaction set, sidecars, key order) but never re-implement hashing, which is the property that kills the divergence class.

### 3.3 Registry

```rust
/// The single dispatch point. Exhaustive: a missing arm is a compile error.
pub fn codec(kind: &str) -> Option<&'static dyn KindCodec> { /* match over the kind set */ }
```

Call sites do `codec(kind)?.disk_bytes(v)` / `.base_hash(v)` / `.create_body(v)` / `.cross_env_body(v)`. No site ever serializes a model directly.

### 3.4 Sidecar kinds

Hooks (`code` → `.py`/`.js`), rules (`trigger_condition` → `.py`), and schemas (formulas → `formulas/<id>.py`) return their sidecars in `DiskArtifact.sidecars`; `base_hash` folds them in using the existing framing (`\0 "code" \0 …`, `\0 "formulas/<id>.py" \0 …`) so recorded hashes for those kinds remain algorithmically identical aside from the redundant-strip removal.

## 4. Per-kind behaviour (consolidates all 9 registries)

| kind | disk path | sidecar | redact→sentinel | key order | overlay | create-strip extras | cross-env extras |
|---|---|---|---|---|---|---|---|
| engines | `engines/<slug>/engine.json` | — | `agenda_id` | — | yes | `agenda_id` | +`organization` |
| engine_fields | `engines/<eng>/fields/<slug>.json` | — | — | — | yes | — | +`organization`, `name` |
| queues | `workspaces/<ws>/queues/<slug>/queue.json` | — | `counts` | — | yes | `hooks,webhooks,rules,inbox,counts,users,workflows` | +`organization` |
| schemas | `…/queues/<q>/schema.json` + `formulas/*.py` | formulas | — | — | yes | `queues` | +`organization` |
| inboxes | `…/queues/<q>/inbox.json` | — | — | — | yes | `email` | +`organization` |
| hooks | `hooks/<slug>.json` + `<slug>.py/.js` | code | `status` | `HOOK_KEY_ORDER` | yes | `test,status` | +`organization`, `token_owner` |
| rules | `rules/<slug>.json` + `<slug>.py` | trigger_condition | — | — | yes | — | +`organization` |
| workspaces | `workspaces/<slug>/workspace.json` | — | — | — | no | `queues` | +`organization` |
| labels | `labels/<slug>.json` | — | — | — | yes | — | +`organization` |
| workflows | `workflows/<slug>/workflow.json` | — | — | — | no | — | +`organization` |
| workflow_steps | `workflows/<wf>/steps/<slug>.json` | — | — | — | no | — | +`organization` |
| email_templates | `…/queues/<q>/email_templates/<slug>.json` | — | — | — | yes | `triggers` | +`organization` |
| organization | `organization.json` | — | — | — | no | — (pull-only) | n/a |
| mdh / index_set | `mdh/…/<set>.json` | — | bespoke (`_id_`, `v`, search-index envelope via `strip_server_managed`) | — | no | bespoke | n/a |

Universal create-strip (all kinds): `id, url, created_at, created_by, modified_at, modified_by, status`. Universal disk-hidden: `modified_at` (recursive) — now applied to **all** kinds (closes #f). `modifier` continues to be stripped at hash time only.

> MDH keeps its bespoke `strip_server_managed`, exposed as that codec's `disk_bytes` implementation, so it joins the trait without forcing its idiosyncratic envelope handling onto others.

## 5. Call-site migration

Replace inline byte production with codec calls at every site the audit enumerated:

- **pull drivers** (`src/cli/pull/*.rs`, 13 files): `codec.disk_bytes` + `codec.base_hash`; overlay strip via `codec.overlay`.
- **sync adapter** (`sync/mod.rs`, per-kind remote-hash blocks): `codec.base_hash`.
- **sync executor** (`sync/execute.rs`): the conflict-refs builder, the remote-delete restore builder, and `try_auto_merge` all use `codec.disk_bytes`/`base_hash` and apply `codec.overlay` (fixes d, e, b).
- **push drivers** (`src/cli/push/*.rs`): all three byte sites (POST-write, drift-compare, post-PATCH write) use the codec (fixes c).
- **deploy** (`deploy/apply.rs`, `deploy/common.rs`): write-back uses `codec.disk_bytes`; the drift hash uses `codec.base_hash` — `pull_redacts_kind` is **deleted** (fixes a); cross-env compare/body uses `codec.cross_env_body`.
- Delete dead `snapshot/queue.rs::write_queue` and `snapshot/engine.rs::write_engine` (fixes g).

## 6. Outbound bodies

`create_body` (within-env) and `cross_env_body` (deploy) remain explicit codec methods rather than being folded into disk bytes, because they strip *more* (e.g. `status` removed entirely for the API, vs redacted-to-sentinel on disk) and must never carry the sentinel to the server. The cross-env compare path keeps stripping volatile fields entirely (as today), so it stays immune to redaction noise.

## 7. On-disk migration & backward compatibility

- **`modified_at` cleanup (~9 kinds):** disk files lose `modified_at` (top-level **and** nested).
  - `canonicalize_for_hash` only strips *top-level* `NOISE_FIELDS`, so: for kinds whose `modified_at` was top-level only, the recorded hash is **unchanged** → silent file rewrite, no conflict. For kinds carrying a **nested** `modified_at`, the recorded hash changes once.
  - **Migration requirement:** the first-run rehash must be absorbed as a benign rewrite, not surfaced as a `RemoteEdit`/conflict. Reuse/extend the existing `contains_hidden_fields` self-heal (`pull/common.rs:528`) so that a snapshot whose only delta is a now-stripped hidden field is rewritten-and-rebased silently.
- **engines / hooks:** their recorded hashes change once (redaction now participates in the hash). First `pull`/`sync` rewrites `engine.json` / hook `.json` to the sentinel and updates the lockfile base — a one-time, expected correction (the same migration already implied by the held fix), absorbed by the same self-heal.
- No lockfile schema change; entries are re-hashed in place on first run.

## 8. Testing strategy

1. **Cross-kind invariant property test (the keystone):** for every registered kind, assert `base_hash(v) == base_hash(parse(disk_bytes(v).json))` and a disk→parse→disk round-trip is stable, over a representative `Value` (including the volatile/hidden fields). This fails on the entire divergence class.
2. **Reproduce-then-fix each live bug** (a–e) with a focused integration test *before* its codec change lands — as done for `agenda_id`/`status` (`tests/cli_sync.rs`). Includes a deploy-drift test for engines (replacing the stale `tgt_drift_status_engine_with_unchanged_agenda_id_is_in_sync`), a workspaces sync test, an overlay-convergence test, and a schema-with-formula restore test.
3. **Keep all existing unit/integration tests green** (full `cargo test`), plus `cargo clippy -D warnings` (incl. `dead_code = "deny"`) and `cargo fmt --check`.
4. Carry forward the existing `serialize_*` unit tests by re-pointing them at the codecs.

## 9. Rollout (big-bang) & risks

**Approach (chosen): big-bang.** Introduce the trait + all codecs + the registry, convert every call site, delete the dead writers, in one change set; gate on the invariant test + full suite.

**Risks & mitigations**
- *Missed call site* → the invariant property test + the per-bug regression tests + `dead_code = "deny"` (orphaned helpers become hard errors) catch it; a grep sweep for residual `to_vec_pretty(<model>)` in `pull`/`sync`/`push`/`deploy` is part of the definition of done.
- *Behavioural change for a kind with no redaction* → covered by round-trip tests per kind.
- *Large review surface* → the trait keeps each kind's logic in a small dedicated module (`src/snapshot/codec/<kind>.rs`), so review is per-kind rather than per-call-site.
- *Migration churn for users* → §7: cosmetic-only hashes are unchanged; only engines/hooks rehash once.

## 10. Decisions (settled)

- Representation: **trait + `codec(kind)` registry** (per-kind modules; compiler-enforced completeness).
- Rollout: **big-bang**, gated by the invariant test + full suite.
- `modified_at` on-disk cleanup: **included**.
- The held engine/hook redaction edits + the deploy phantom-drift are **folded into this refactor** (not committed separately).

## 11. Open questions

- Exact module layout: `src/snapshot/codec/mod.rs` (registry + trait) with one file per kind — confirm during planning.
- Whether `organization` and `mdh` (pull-only, no push/cross-env) implement the full trait or a narrower sub-trait; leaning full trait with `create_body`/`cross_env_body` as no-ops to keep the registry uniform.
