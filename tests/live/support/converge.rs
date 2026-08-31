//! Convergence assertions: the property the mock suite structurally cannot
//! prove.
//!
//! Almost every production bug this project has shipped was a *convergence*
//! bug, not a first-write bug: the first `sync` did the right thing and the
//! second one undid it, re-pushed it, or oscillated. Push write-back writing
//! the raw server response to disk, the base cache not tracking code sidecars,
//! matched-target env fields being stripped into 115 phantom PATCHes per run,
//! MDH index order ping-ponging with period 2 — every one of those needed a
//! SECOND cycle against a REAL server to become visible.
//!
//! wiremock can't see them because the mock replies with whatever the fixture
//! says; the bugs live in the difference between what the server echoes back
//! from a PATCH and what it returns from a later GET. Hence this module.
//!
//! [`assert_converged`] is the single assertion every scenario should make
//! after it finishes writing to a remote:
//!
//! 1. `sync <env> --dry-run` reports `0 would push, 0 would pull, 0 would
//!    prompt` — the planner agrees there is nothing left to do;
//! 2. the dry run wrote nothing (which also pins `--dry-run`'s no-write
//!    contract against a real API, not a mock);
//! 3. a real `sync <env>` reports `(0 changed`; and
//! 4. the tracked tree is **byte-identical** across that real sync — env tree,
//!    lockfile, base cache and conflict shadows alike.
//!
//! Step 4 is the strict one. A sync that plans nothing while quietly rewriting
//! a sidecar's trailing newline, reordering an MDH index array, or failing to
//! mirror a `.py` into the base cache still fails here, and the failure names
//! the exact file.
//!
//! # Everything here is scoped to one run
//!
//! The sandbox is a REAL org with unrelated content that other people change
//! while the suite runs, and `rdc` deliberately syncs the whole org. So the
//! org-wide summary counters (`N changed`, `N would pull`) can never be zero
//! and asserting on them is meaningless. Every check below is therefore
//! filtered to objects whose slug carries this run's `rdc-it-<id>-` prefix:
//! plan lines are matched by prefix, files by path, and the lockfile
//! entry-by-entry. Unrelated org drift passes straight through; a single byte
//! moving under one of THIS run's objects does not.

use crate::support::project::ProjectFixture;
use std::collections::BTreeMap;
use std::path::Path;

/// Every tracked byte for one env, keyed by project-relative path.
///
/// Contents are stored verbatim rather than hashed: these fixtures hold a few
/// dozen small files, and keeping the bytes lets [`TreeSnapshot::diff`] say
/// *how* a file changed instead of only *that* it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSnapshot {
    files: BTreeMap<String, Vec<u8>>,
}

/// The four trees a sync may legitimately write to for one env. A file outside
/// all of them (e.g. `rdc.toml`, `secrets/`) is not part of the convergence
/// contract and is deliberately not captured.
fn tracked_roots(env: &str) -> Vec<String> {
    vec![
        format!("envs/{env}"),
        format!(".rdc/state/{env}.base"),
        format!(".rdc/conflicts/{env}"),
    ]
}

impl TreeSnapshot {
    /// Capture the tracked files for `env` that belong to the run identified by
    /// `prefix` (their slug — and therefore their path — carries it).
    ///
    /// Missing directories are not an error: a project that has never
    /// conflicted has no `.rdc/conflicts/<env>/`, and its absence is itself a
    /// fact worth comparing across two snapshots.
    ///
    /// The lockfile is exploded entry-by-entry rather than stored whole, so a
    /// concurrent edit to somebody else's object doesn't register as a change
    /// to ours — and so a diff names the exact `(kind, slug)` that moved.
    pub fn capture(root: &Path, env: &str, prefix: &str) -> TreeSnapshot {
        let mut files = BTreeMap::new();
        for rel_root in tracked_roots(env) {
            collect_into(&mut files, root, &root.join(&rel_root));
        }
        files.retain(|path, _| path.contains(prefix));
        collect_lockfile_entries(&mut files, root, env, prefix);
        TreeSnapshot { files }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Human-readable difference, or `None` when the two snapshots are equal.
    ///
    /// Lists added / removed / modified paths, and for a modified text file
    /// includes the first differing line so a whitespace-only regression (the
    /// trailing-`\n` and trailing-space classes both bit this project) is
    /// visible rather than reported as an opaque "contents differ".
    pub fn diff(&self, other: &TreeSnapshot) -> Option<String> {
        let mut lines = Vec::new();
        for (path, bytes) in &other.files {
            match self.files.get(path) {
                None => lines.push(format!("  + added    {path} ({} bytes)", bytes.len())),
                Some(before) if before != bytes => {
                    lines.push(format!(
                        "  ~ modified {path} ({} -> {} bytes){}",
                        before.len(),
                        bytes.len(),
                        first_difference(before, bytes)
                    ));
                }
                Some(_) => {}
            }
        }
        for path in self.files.keys() {
            if !other.files.contains_key(path) {
                lines.push(format!("  - removed  {path}"));
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

/// How many files under `envs/<env>` have `slug` in their path.
///
/// Deliberately NOT `TreeSnapshot::capture`: that also walks the base cache
/// (`.rdc/state/<env>.base`) and then adds a synthetic entry per matching
/// LOCKFILE slug, so asking it "does this lockfile slug still have a file?"
/// is answered by the question itself and can never be false. Tombstone
/// detection needs the env tree alone.
#[allow(dead_code)]
pub fn env_files_matching(root: &Path, env: &str, slug: &str) -> usize {
    let mut files = BTreeMap::new();
    collect_into(&mut files, root, &root.join(format!("envs/{env}")));
    files.keys().filter(|path| path.contains(slug)).count()
}

/// Add one synthetic entry per lockfile row whose slug carries `prefix`, keyed
/// `lockfile:<kind>/<slug>`. Rows belonging to other runs (or to the org's own
/// pre-existing content) are skipped.
fn collect_lockfile_entries(
    out: &mut BTreeMap<String, Vec<u8>>,
    root: &Path,
    env: &str,
    prefix: &str,
) {
    let path = root.join(format!(".rdc/state/{env}.lock.json"));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(kinds) = lock.get("objects").and_then(|o| o.as_object()) else {
        return;
    };
    for (kind, rows) in kinds {
        let Some(rows) = rows.as_object() else { continue };
        for (slug, entry) in rows {
            if !slug.contains(prefix) {
                continue;
            }
            let bytes = serde_json::to_vec_pretty(entry).unwrap_or_default();
            out.insert(format!("lockfile:{kind}/{slug}"), bytes);
        }
    }
}

/// Recursively read every regular file under `dir` into `out`, keyed by its
/// path relative to `root` with forward slashes.
fn collect_into(out: &mut BTreeMap<String, Vec<u8>>, root: &Path, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_into(out, root, &path),
            Ok(t) if t.is_file() => {
                if let (Ok(rel), Ok(bytes)) = (path.strip_prefix(root), std::fs::read(&path)) {
                    out.insert(rel.to_string_lossy().replace('\\', "/"), bytes);
                }
            }
            _ => {}
        }
    }
}

/// `" — first differs at line N: ... vs ..."`, or `""` for binary content.
fn first_difference(before: &[u8], after: &[u8]) -> String {
    let (Ok(a), Ok(b)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        return String::new();
    };
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return format!("\n      line {}: {:?} -> {:?}", i + 1, la, lb);
        }
    }
    // Same prefix, different length: a pure append/truncate (the classic
    // trailing-newline churn) shows up here rather than as a line mismatch.
    if a.len() != b.len() {
        return format!(
            "\n      identical lines; trailing bytes differ ({:?} -> {:?})",
            tail(a),
            tail(b)
        );
    }
    String::new()
}

fn tail(s: &str) -> String {
    let start = s.len().saturating_sub(12);
    s[start..].to_string()
}

/// The per-item plan lines a `--dry-run` printed for objects carrying
/// `prefix`, each tagged with the section it appeared under, e.g.
/// `[would pull] - hooks/rdc-it-x-validator (new)`.
///
/// The org-wide counters in the `Dry run: …` summary are useless on a shared
/// sandbox — they count everybody's drift. The item lines carry the slug, so
/// they can be filtered down to the objects this run owns; carrying the
/// section along makes a convergence failure say which DIRECTION did not
/// settle, which is most of the diagnosis.
pub fn plan_lines_for(output: &str, prefix: &str) -> Vec<String> {
    let mut section = "unknown";
    let mut out = Vec::new();
    for line in output.lines().map(str::trim_end) {
        for name in ["would pull", "would push", "would prompt"] {
            if line.ends_with(name) && !line.starts_with("- ") {
                section = name;
            }
        }
        if line.starts_with("- ") && line.contains(prefix) {
            out.push(format!("[{section}] {line}"));
        }
    }
    out
}

/// True when the dry-run summary line is present at all, i.e. the planner ran
/// to completion rather than dying early.
pub fn planned_successfully(output: &str) -> bool {
    output.contains("Dry run: ")
}

/// Assert that `env` has fully converged: nothing left to plan, nothing left
/// to write, and a real sync that changes not one byte on disk.
///
/// `ctx` names the moment being checked (e.g. `"after pushing the label"`) and
/// is included in every failure message, because a scenario calls this more
/// than once.
///
/// `prefix` is a plain SUBSTRING match against paths, plan lines and lockfile
/// slugs — usually `RunId::list_prefix()` (`rdc-it-<id>-`). Pass the bare
/// `RunId::as_str()` when a kind slugs itself differently: MDH collection
/// names must be Mongo-safe, so they carry underscores (`rdc_it_<id>_mdh`)
/// and the dash form would match nothing, silently capturing an empty tree.
/// (The emptiness guard below turns that mistake into a failure rather than a
/// vacuous pass.)
///
/// # Panics
/// With a message naming the offending counts or the exact files that moved.
#[allow(dead_code)]
pub fn assert_converged(project: &ProjectFixture, env: &str, prefix: &str, ctx: &str) {
    let before = TreeSnapshot::capture(project.path(), env, prefix);
    assert!(
        !before.is_empty(),
        "assert_converged({ctx}): captured an EMPTY tree for env '{env}' and prefix \
         '{prefix}' — this run owns no files there, so convergence would pass vacuously"
    );

    // (1) What does the planner still want to do FOR THIS RUN'S OBJECTS?
    // Unrelated org drift is expected on a shared sandbox and is ignored.
    let dry = project.run_rdc(&["sync", env, "--dry-run"]);
    let dry_out = combined(&dry);
    assert!(
        dry.status.success(),
        "assert_converged({ctx}): sync {env} --dry-run failed:\n{dry_out}"
    );
    assert!(
        planned_successfully(&dry_out),
        "assert_converged({ctx}): the dry run printed no plan summary, so it cannot \
         have planned anything:\n{dry_out}"
    );
    let planned = plan_lines_for(&dry_out, prefix);

    // (2) --dry-run's no-write contract, against a real API rather than a mock.
    let after_dry = TreeSnapshot::capture(project.path(), env, prefix);
    if let Some(d) = before.diff(&after_dry) {
        panic!("assert_converged({ctx}): sync --dry-run WROTE to disk:\n{d}");
    }

    // (3) Run the real cycle even when (1) already found work: what that cycle
    // CHANGES is the actionable half of the diagnosis — a plan line says which
    // object failed to settle, the byte diff says which field did.
    let real = project.run_rdc(&["sync", env]);
    let real_out = combined(&real);
    assert!(
        real.status.success(),
        "assert_converged({ctx}): re-sync of {env} failed:\n{real_out}"
    );
    let after_real = TreeSnapshot::capture(project.path(), env, prefix);
    let drift = before.diff(&after_real);

    if planned.is_empty() && drift.is_none() {
        return;
    }
    let mut report = format!(
        "assert_converged({ctx}): env '{env}' has NOT converged — the previous cycle \
         left work behind for this run's objects."
    );
    if !planned.is_empty() {
        report.push_str(&format!("\n\nThe planner still wanted:\n{}", planned.join("\n")));
    }
    match drift {
        Some(d) => report.push_str(&format!("\n\nAnd a re-sync changed these files:\n{d}")),
        None => report.push_str(
            "\n\nA re-sync changed no bytes, so the plan is a phantom: something is \
             classified as drifted while both sides agree.",
        ),
    }
    panic!("{report}");
}

/// stdout + stderr as one string. `rdc` prints its event lines to stderr and
/// some payloads to stdout; scenarios care about neither distinction.
/// Assert that a re-sync leaves ONE named file (and its lockfile row)
/// byte-identical.
///
/// [`assert_converged`] filters everything by the run's `rdc-it-<id>-` prefix,
/// which is what makes it usable on a shared org — but it means an object
/// whose path carries no run id is invisible to it, and the emptiness guard
/// would fire rather than pass vacuously. The organization is the only such
/// object: a per-env singleton rdc PATCHes but never creates, living at
/// `envs/<env>/organization.json`.
///
/// That object is worth checking precisely because its push is the awkward
/// shape — the PATCH response is NOT the GET response, so the write-back is
/// deliberately settings-only, and "settings-only" is exactly the kind of
/// partial write-back that leaves the rest of the file a cycle behind.
#[allow(dead_code)]
pub fn assert_unprefixed_object_stable(
    project: &ProjectFixture,
    env: &str,
    rel: &str,
    kind: &str,
    ctx: &str,
) {
    let read = || -> (Option<Vec<u8>>, String) {
        let bytes = std::fs::read(project.path().join(rel)).ok();
        let lock = std::fs::read_to_string(
            project.path().join(format!(".rdc/state/{env}.lock.json")),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("objects").and_then(|o| o.get(kind)).cloned())
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        .unwrap_or_default();
        (bytes, lock)
    };

    let (before_bytes, before_lock) = read();
    assert!(
        before_bytes.is_some(),
        "assert_unprefixed_object_stable({ctx}): {rel} does not exist, so this \
         would pass vacuously"
    );

    let out = project.run_rdc(&["sync", env]);
    let combined_out = combined(&out);
    assert!(
        out.status.success(),
        "assert_unprefixed_object_stable({ctx}): re-sync failed:\n{combined_out}"
    );

    let (after_bytes, after_lock) = read();
    if before_bytes != after_bytes {
        let show = |b: &Option<Vec<u8>>| {
            b.as_deref()
                .map(|x| String::from_utf8_lossy(x).to_string())
                .unwrap_or_else(|| "<missing>".into())
        };
        panic!(
            "assert_unprefixed_object_stable({ctx}): a re-sync REWROTE {rel} — the \
             previous cycle did not converge.\n--- before ---\n{}\n--- after ---\n{}",
            show(&before_bytes),
            show(&after_bytes)
        );
    }
    assert_eq!(
        before_lock, after_lock,
        "assert_unprefixed_object_stable({ctx}): a re-sync changed the lockfile \
         rows for '{kind}'"
    );
}

#[allow(dead_code)]
pub fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PFX: &str = "rdc-it-abc-";

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn capture_reads_the_env_tree_base_cache_and_conflict_shadows() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "envs/test/labels/rdc-it-abc-a.json", "{}");
        write(root, ".rdc/state/test.base/labels/rdc-it-abc-a.json", "{}");
        write(root, ".rdc/conflicts/test/labels/rdc-it-abc-a.json", "{}");
        // Not captured: another env, project config, and — crucially — an
        // object belonging to somebody else in the shared org.
        write(root, "envs/prod/labels/rdc-it-abc-a.json", "{}");
        write(root, "rdc.toml", "x = 1");
        write(root, "envs/test/labels/someone-elses.json", "{}");

        let snap = TreeSnapshot::capture(root, "test", PFX);
        let keys: Vec<&str> = snap.files.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                ".rdc/conflicts/test/labels/rdc-it-abc-a.json",
                ".rdc/state/test.base/labels/rdc-it-abc-a.json",
                "envs/test/labels/rdc-it-abc-a.json",
            ]
        );
    }

    #[test]
    fn capture_explodes_only_this_runs_lockfile_rows() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".rdc/state/test.lock.json",
            r#"{"version":3,"objects":{
                 "labels":{"rdc-it-abc-a":{"id":1},"someone-elses":{"id":2}},
                 "hooks":{"rdc-it-abc-h":{"id":3}}}}"#,
        );
        let snap = TreeSnapshot::capture(dir.path(), "test", PFX);
        let keys: Vec<&str> = snap.files.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["lockfile:hooks/rdc-it-abc-h", "lockfile:labels/rdc-it-abc-a"]);
    }

    /// A concurrent edit to an unrelated object must not read as our drift —
    /// the sandbox is a live org and this happens routinely.
    #[test]
    fn a_change_to_another_runs_object_is_invisible() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "envs/test/labels/rdc-it-abc-a.json", "{}");
        write(dir.path(), "envs/test/labels/someone-elses.json", "{}");
        let before = TreeSnapshot::capture(dir.path(), "test", PFX);

        write(dir.path(), "envs/test/labels/someone-elses.json", "{\"changed\":true}");
        write(dir.path(), "envs/test/labels/another-persons.json", "{}");
        let after = TreeSnapshot::capture(dir.path(), "test", PFX);

        assert_eq!(before.diff(&after), None);
    }

    #[test]
    fn capture_of_a_missing_project_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(TreeSnapshot::capture(dir.path(), "test", PFX).is_empty());
    }

    /// The regression `assert_converged`'s emptiness guard is prone to and
    /// `env_files_matching` exists to avoid: a lockfile row and a base-cache
    /// file for a slug both survive a tombstone that only touched the env
    /// tree. `TreeSnapshot::capture` would still report a "file" for that
    /// slug (via the base cache) and would ALSO synthesize a
    /// `lockfile:<kind>/<slug>` entry from the very row being asked about —
    /// so the tombstone would be invisible to it. `env_files_matching` looks
    /// at the env tree alone and must come back 0.
    #[test]
    fn env_files_matching_ignores_base_cache_and_lockfile_survivors() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, ".rdc/state/test.base/labels/rdc-it-abc-a.json", "{}");
        write(
            root,
            ".rdc/state/test.lock.json",
            r#"{"version":3,"objects":{"labels":{"rdc-it-abc-a":{"id":1}}}}"#,
        );

        // No file under envs/test — this is the tombstoned state.
        assert_eq!(env_files_matching(root, "test", "rdc-it-abc-a"), 0);

        // Once a file exists under envs/test, it is found.
        write(root, "envs/test/labels/rdc-it-abc-a.json", "{}");
        assert_eq!(env_files_matching(root, "test", "rdc-it-abc-a"), 1);
    }

    #[test]
    fn diff_names_added_removed_and_modified_paths() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "envs/test/rdc-it-abc-keep.json", "{}");
        write(dir.path(), "envs/test/rdc-it-abc-gone.json", "{}");
        let before = TreeSnapshot::capture(dir.path(), "test", PFX);

        std::fs::remove_file(dir.path().join("envs/test/rdc-it-abc-gone.json")).unwrap();
        write(dir.path(), "envs/test/rdc-it-abc-new.json", "{}");
        write(dir.path(), "envs/test/rdc-it-abc-keep.json", "{\"a\":1}");
        let after = TreeSnapshot::capture(dir.path(), "test", PFX);

        let d = before.diff(&after).expect("trees differ");
        assert!(d.contains("+ added    envs/test/rdc-it-abc-new.json"), "{d}");
        assert!(d.contains("- removed  envs/test/rdc-it-abc-gone.json"), "{d}");
        assert!(d.contains("~ modified envs/test/rdc-it-abc-keep.json"), "{d}");
    }

    /// The regression this project actually shipped twice: a sidecar that
    /// gains or loses a trailing newline while every line stays identical.
    #[test]
    fn diff_reports_a_trailing_newline_only_change() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "envs/test/hooks/rdc-it-abc-h.py", "print(1)");
        let before = TreeSnapshot::capture(dir.path(), "test", PFX);
        write(dir.path(), "envs/test/hooks/rdc-it-abc-h.py", "print(1)\n");
        let after = TreeSnapshot::capture(dir.path(), "test", PFX);

        let d = before.diff(&after).expect("trailing newline is a difference");
        assert!(d.contains("rdc-it-abc-h.py"), "{d}");
        assert!(d.contains("trailing bytes differ"), "{d}");
    }

    #[test]
    fn plan_lines_keep_only_this_runs_items() {
        let out = "\
13:34:58 plan   would pull
- hooks/attach-rossum-url-cib (delete local; deleted on env)
- queues/ap-documents-header-level-taxation
- labels/rdc-it-abc-priority (new)
13:34:58 done   Dry run: 0 would push, 3 would pull, 0 would prompt (no writes)";
        assert_eq!(
            plan_lines_for(out, PFX),
            vec!["[would pull] - labels/rdc-it-abc-priority (new)"]
        );
        assert!(planned_successfully(out));
    }

    #[test]
    fn a_plan_naming_only_other_peoples_objects_reads_as_converged() {
        let out = "\
- hooks/attach-rossum-url-cib (delete local; deleted on env)
13:34:58 done   Dry run: 0 would push, 1 would pull, 0 would prompt (no writes)";
        assert!(plan_lines_for(out, PFX).is_empty());
    }

    #[test]
    fn a_truncated_run_is_not_mistaken_for_a_clean_plan() {
        assert!(!planned_successfully("13:00:00 err    connection reset"));
    }
}
