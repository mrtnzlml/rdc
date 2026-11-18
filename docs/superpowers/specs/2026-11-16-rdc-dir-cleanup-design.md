# Automatic `.rdc/` cleanup — design

## Intent

Housekeeping. Removing an env from `rdc.toml` leaves its state under `.rdc/`
forever, and nothing removes it. People forget to run `rdc doctor`, so the
cleanup runs inside commands they run anyway. This is a step towards doing as
much as possible outside `doctor`, which may be deprecated later.

## Goals

- `rdc sync` and `rdc init` remove `.rdc/` artifacts of envs that are not in
  `rdc.toml`, plus known-obsolete leftovers.
- No extra command, flag or prompt.

## Non-goals

- `.rdc/map/*.toml` (legacy per-pair maps): `rdc migrate` still converts
  them, so deleting them would lose data. They stay.
- `.rdc/mapping.toml` rows naming an unknown env: the file is hand-edited, and
  today's warning stays.
- Files under `.rdc/` whose shape rdc does not own: rdc does not know what they
  are, so it leaves them.
- Moving `doctor`'s in-env `.base/` prune into sync: a possible follow-up.
- wasm: the host owns persistence there, and state lives in per-env snapshots.
  The cleanup runs on the CLI paths only.

## Anti-goals

- Deleting state of an env that **is** in `rdc.toml`.
- Deleting anything on `--dry-run`.

## Approaches

1. **Every `sync` / `init` prunes all orphans (chosen).** Fully automatic.
   Accepted cost: if an env is removed from `rdc.toml` by mistake, the next
   sync deletes its committed `<env>.lock.json`. Re-adding the env then
   starts without its slug→id mapping. Git history still has the file.
2. `doctor` prunes. Rejected: people forget to run it.
3. Split: auto-prune only gitignored artifacts, and `doctor` prunes the
   lockfile. Rejected for the same reason.

## Design

What counts as orphaned, for an env name `E` not in `rdc.toml`:

- `.rdc/state/E.lock.json`, `.rdc/state/E.lock`, `.rdc/state/E.base/`
- `.rdc/conflicts/E/`

Obsolete regardless of envs: `.rdc/cache` (nothing writes it any more).

Rules:

- One function, `prune_orphans(root, cfg, dry_run) -> Vec<PathBuf>`, that lists
  `.rdc/state/` and `.rdc/conflicts/` and goes through `crate::vfs`.
- It runs once per `rdc sync` invocation, before the first cycle (one-shot and
  watch), and once per `rdc init`. It does not run in `run_cycle`, so wasm
  embedders do not reach it.
- **My call:** an env name matches case-insensitively. On macOS's
  case-insensitive filesystem, `Dev.lock.json` *is* the lockfile of env `dev`,
  so an exact comparison would delete live state.
- **My call:** if `E.lock` is held by another process (`EnvLock::try_acquire`
  returns `None`), skip env `E`. A sync that started before `rdc.toml` was
  edited is still using it.
- `--dry-run`: print `would remove <path>`, and change nothing.
- Otherwise: print one `removed <path>` line per path, then remove
  `.rdc/state/` and `.rdc/conflicts/` if they are now empty.
- A failure to remove a path is a warning, never a sync failure.
- Docs: add one line to the `sync` long-about in `src/cli/mod.rs`, and to the
  scaffolded `CLAUDE.md` text in `src/cli/init.rs` that lists `.rdc/` contents.

## Left to the builder

- The exact output wording and the log channel.
- The test layout: unit tests for the matcher, plus one CLI test per entry
  point (sync, init, dry-run, held lock, case-insensitive name).
