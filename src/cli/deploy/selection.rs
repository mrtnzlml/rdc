//! Per-invocation selection filter for `rdc migrate`. When the user passes
//! one or more `--only <selector>` flags, only the matching `(kind, slug)`
//! pairs participate in the migration.
//!
//! The selection lives in memory for the duration of the command — it
//! does not touch the mapping file, lockfile, overlay, or any other
//! persistent state. Re-running with the same flags on the same snapshots
//! yields a byte-identical plan.

// `--only` accepts any kind that migrate operates on. The dep-order list
// is the single source of truth so adding a kind extends what --only accepts.
use crate::paths::Paths;
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeSet;

/// Kinds we deploy/migrate, in dependency order (POST order). Workflows are
/// pull-only at the Rossum API (PATCH returns 405) and so are not
/// deployable; MDH is not yet writable.
pub(crate) const DEPLOYABLE_KINDS: &[&str] = &[
    "workspaces",
    "schemas",
    "queues",
    "inboxes",
    "email_templates",
    "hooks",
    "rules",
    "labels",
    "engines",
    "engine_fields",
];

#[derive(Debug, Default, Clone)]
pub(crate) struct Selection {
    pub(crate) items: BTreeSet<(String, String)>,
}

impl Selection {
    pub(crate) fn contains(&self, kind: &str, slug: &str) -> bool {
        self.items.contains(&(kind.to_string(), slug.to_string()))
    }
}

/// Resolve a list of `--only` flags against the candidate set built from
/// the src + tgt snapshots.
///
/// Returns `Ok(None)` when `raw_matchers` is empty (signalling
/// "no filter — whole-snapshot migrate"). Every matcher must hit at least
/// one `(kind, slug)` pair across both snapshots; otherwise we error so
/// typos can't silently produce no-op deploys.
pub(crate) fn resolve(
    raw_matchers: &[String],
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<Option<Selection>> {
    if raw_matchers.is_empty() {
        return Ok(None);
    }

    let matchers: Vec<Matcher> = raw_matchers
        .iter()
        .map(|s| Matcher::parse(s))
        .collect::<Result<_>>()?;

    let candidates = build_candidate_set(src_paths, tgt_paths)?;

    let mut items: BTreeSet<(String, String)> = BTreeSet::new();
    for m in &matchers {
        let mut hits = 0usize;
        for (kind, slug) in &candidates {
            if m.matches(kind, slug) {
                items.insert((kind.clone(), slug.clone()));
                hits += 1;
            }
        }
        if hits == 0 {
            bail!(
                "--only '{}' matched 0 objects across the local snapshots. \
                 (Check the spelling against `envs/<env>/` in your project tree.)",
                m.raw
            );
        }
    }

    Ok(Some(Selection { items }))
}

fn build_candidate_set(src_paths: &Paths, tgt_paths: &Paths) -> Result<BTreeSet<(String, String)>> {
    let mut set = BTreeSet::new();
    for kind in DEPLOYABLE_KINDS {
        for slug in list_slugs(src_paths, kind)? {
            set.insert((kind.to_string(), slug));
        }
        for slug in list_slugs(tgt_paths, kind)? {
            set.insert((kind.to_string(), slug));
        }
    }
    Ok(set)
}

/// One `--only` selector, parsed.
///
/// `kind = None` represents the `*/<slug-pattern>` form: match any kind
/// whose slug fits the pattern. Otherwise the matcher is scoped to a
/// single kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Matcher {
    pub(crate) kind: Option<String>,
    pub(crate) slug_pattern: String,
    pub(crate) raw: String,
}

impl Matcher {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let (kind_part, slug_part) = raw.split_once('/').ok_or_else(|| {
            anyhow!(
                "invalid --only '{raw}': expected '<kind>/<slug>' or '*/<slug>' \
                 (e.g. 'hooks/validator-invoices', 'schemas/cost-*', '*/cost-invoices')"
            )
        })?;

        if kind_part.is_empty() {
            bail!(
                "invalid --only '{raw}': kind segment is empty (use '*' for any kind, e.g. '*/cost-invoices')"
            );
        }

        if slug_part.is_empty() {
            bail!("invalid --only '{raw}': slug segment is empty");
        }

        // Reject glob syntax outside the supported `*` (zero-or-more chars
        // within the slug segment). The grammar is narrow on purpose; an
        // explicit error beats the generic "matched 0 objects" message.
        if slug_part.contains("**") {
            bail!(
                "invalid --only '{raw}': `**` is not supported (use a single `*` for zero-or-more chars). \
                 Example: 'hooks/*'."
            );
        }
        if slug_part.contains('?') {
            bail!(
                "invalid --only '{raw}': `?` is not supported (only `*` for zero-or-more chars). \
                 Example: 'schemas/cost-*'."
            );
        }

        let kind = if kind_part == "*" {
            None
        } else if DEPLOYABLE_KINDS.contains(&kind_part) {
            Some(kind_part.to_string())
        } else {
            bail!(
                "invalid --only '{raw}': unknown kind '{kind_part}'. \
                 Valid kinds: {}, or '*' for any kind.",
                DEPLOYABLE_KINDS.join(", ")
            );
        };

        Ok(Self {
            kind,
            slug_pattern: slug_part.to_string(),
            raw: raw.to_string(),
        })
    }

    pub(crate) fn matches(&self, kind: &str, slug: &str) -> bool {
        if let Some(k) = &self.kind
            && k != kind
        {
            return false;
        }
        glob_matches(&self.slug_pattern, slug)
    }
}

/// Glob match: `pattern` may contain `*` (zero or more characters); every
/// other byte is literal. No `?`, no character classes, no `**` — the
/// grammar is intentionally narrow so users learn it from one sentence
/// of help text.
pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let mut pi: usize = 0;
    let mut ti: usize = 0;
    let mut star: Option<(usize, usize)> = None; // (next_after_star, text_pos_at_star)
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi + 1, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((star_pi, star_ti)) = star {
            pi = star_pi;
            ti = star_ti + 1;
            star = Some((star_pi, star_ti + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// List slugs from the local snapshot. The layout is kind-specific; these
/// scanners mirror the cross-env auto-matching enumerators used by
/// `deploy::map`.
pub(crate) fn list_slugs(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    match kind {
        "workspaces" => list_workspace_slugs(paths),
        "schemas" | "queues" | "inboxes" => list_queue_nested(paths, kind),
        "email_templates" => list_email_template_keys(paths),
        "hooks" | "rules" | "labels" => list_flat_kind(paths, kind),
        "engines" => list_engine_slugs(paths),
        "engine_fields" => list_engine_field_slugs(paths),
        _ => Ok(Vec::new()),
    }
}

fn list_workspace_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.workspaces_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join("workspace.json").exists() {
            out.push(slug);
        }
    }
    out.sort();
    Ok(out)
}

fn list_queue_nested(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return Ok(Vec::new());
    }
    let file_name = match kind {
        "queues" => "queue.json",
        "schemas" => "schema.json",
        "inboxes" => "inbox.json",
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for ws_entry in std::fs::read_dir(&ws_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let queues_dir = ws_entry.path().join("queues");
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            if q_entry.path().join(file_name).exists() {
                out.push(q_entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn list_email_template_keys(paths: &Paths) -> Result<Vec<String>> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for ws_entry in std::fs::read_dir(&ws_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().into_owned();
        let queues_dir = ws_entry.path().join("queues");
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().into_owned();
            let et_dir = q_entry.path().join("email-templates");
            if !et_dir.exists() {
                continue;
            }
            for f in std::fs::read_dir(&et_dir)? {
                let f = f?;
                let name = f.file_name().to_string_lossy().into_owned();
                if crate::paths::is_shadow_artifact(&name, paths.env()) {
                    continue;
                }
                if let Some(stem) = name.strip_suffix(".json") {
                    out.push(format!("{ws_slug}/{q_slug}/{stem}"));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn list_flat_kind(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    let dir = match kind {
        "hooks" => paths.hooks_dir(),
        "rules" => paths.rules_dir(),
        "labels" => paths.labels_dir(),
        _ => return Ok(Vec::new()),
    };
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        if let Some(stem) = name.strip_suffix(".json") {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

fn list_engine_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.engines_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.path().join("engine.json").exists() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

fn list_engine_field_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.engines_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e_entry in std::fs::read_dir(&dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().into_owned();
        let fields_dir = paths.engine_fields_dir(&e_slug);
        if !fields_dir.exists() {
            continue;
        }
        for f_entry in std::fs::read_dir(&fields_dir)? {
            let f_entry = f_entry?;
            let name = f_entry.file_name().to_string_lossy().into_owned();
            if crate::paths::is_shadow_artifact(&name, paths.env()) {
                continue;
            }
            if let Some(stem) = name.strip_suffix(".json") {
                // Composite `<engine_slug>/<field_slug>` matches the
                // lockfile / mapping / overlay key shape.
                out.push(format!("{e_slug}/{stem}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod matcher_tests {
    use super::*;

    #[test]
    fn parse_literal() {
        let m = Matcher::parse("hooks/validator-invoices").unwrap();
        assert_eq!(m.kind.as_deref(), Some("hooks"));
        assert_eq!(m.slug_pattern, "validator-invoices");
        assert_eq!(m.raw, "hooks/validator-invoices");
    }

    #[test]
    fn parse_glob() {
        let m = Matcher::parse("schemas/cost-*").unwrap();
        assert_eq!(m.kind.as_deref(), Some("schemas"));
        assert_eq!(m.slug_pattern, "cost-*");
    }

    #[test]
    fn parse_cross_kind_glob() {
        let m = Matcher::parse("*/cost-invoices").unwrap();
        assert_eq!(m.kind, None);
        assert_eq!(m.slug_pattern, "cost-invoices");
    }

    #[test]
    fn parse_compound_email_template() {
        let m = Matcher::parse("email_templates/main/cost-invoices/rejection").unwrap();
        assert_eq!(m.kind.as_deref(), Some("email_templates"));
        assert_eq!(m.slug_pattern, "main/cost-invoices/rejection");
    }

    #[test]
    fn parse_unknown_kind_errors() {
        let err = Matcher::parse("hookz/foo").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("unknown kind 'hookz'"), "got: {s}");
        assert!(s.contains("Valid kinds:"), "got: {s}");
    }

    #[test]
    fn parse_missing_slash_errors() {
        let err = Matcher::parse("hooks").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("expected '<kind>/<slug>'"), "got: {s}");
    }

    #[test]
    fn parse_empty_slug_errors() {
        let err = Matcher::parse("hooks/").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("slug segment is empty"), "got: {s}");
    }

    #[test]
    fn parse_slash_only_errors() {
        let err = Matcher::parse("/").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("invalid --only '/'"), "got: {s}");
        assert!(s.contains("kind segment is empty"), "got: {s}");
    }

    #[test]
    fn parse_cross_kind_with_empty_slug_errors() {
        let err = Matcher::parse("*/").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("slug segment is empty"), "got: {s}");
    }

    #[test]
    fn parse_double_star_errors_with_example() {
        let err = Matcher::parse("hooks/**").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("`**` is not supported"), "got: {s}");
        assert!(s.contains("'hooks/*'"), "got: {s}");
    }

    #[test]
    fn parse_question_mark_errors_with_example() {
        let err = Matcher::parse("hooks/foo?bar").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("`?` is not supported"), "got: {s}");
        assert!(s.contains("'schemas/cost-*'"), "got: {s}");
    }

    #[test]
    fn matches_literal_respects_kind() {
        let m = Matcher::parse("hooks/validator-invoices").unwrap();
        assert!(m.matches("hooks", "validator-invoices"));
        assert!(!m.matches("hooks", "validator-credit"));
        assert!(!m.matches("rules", "validator-invoices"));
    }

    #[test]
    fn matches_glob_respects_kind() {
        let m = Matcher::parse("schemas/cost-*").unwrap();
        assert!(m.matches("schemas", "cost-invoices"));
        assert!(m.matches("schemas", "cost-credit"));
        assert!(!m.matches("queues", "cost-invoices"));
    }

    #[test]
    fn matches_cross_kind_glob_spans_all_kinds() {
        let m = Matcher::parse("*/cost-invoices").unwrap();
        assert!(m.matches("queues", "cost-invoices"));
        assert!(m.matches("schemas", "cost-invoices"));
        assert!(m.matches("inboxes", "cost-invoices"));
        assert!(!m.matches("queues", "credit-invoices"));
    }

    #[test]
    fn matches_compound_email_template() {
        let m = Matcher::parse("email_templates/main/cost-invoices/rejection").unwrap();
        assert!(m.matches("email_templates", "main/cost-invoices/rejection"));
        assert!(!m.matches("email_templates", "main/cost-invoices/confirmation"));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::paths::Paths;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"{}").unwrap();
    }

    /// Build a minimal snapshot under `root/envs/<env>/` covering several
    /// kinds and layouts so the scanners have something to find.
    fn make_env(root: &Path, env: &str) -> Paths {
        let env_root = root.join("envs").join(env);
        // flat hooks/rules/labels
        touch(&env_root.join("hooks/validator-invoices.json"));
        touch(&env_root.join("hooks/sftp-import.json"));
        touch(&env_root.join("rules/check-totals.json"));
        // workspace + queue + schema (queue-nested)
        touch(&env_root.join("workspaces/finance/workspace.json"));
        touch(&env_root.join("workspaces/finance/queues/cost-invoices/queue.json"));
        touch(&env_root.join("workspaces/finance/queues/cost-invoices/schema.json"));
        touch(&env_root.join("workspaces/finance/queues/credit-notes/queue.json"));
        // email template under cost-invoices
        touch(
            &env_root
                .join("workspaces/finance/queues/cost-invoices/email-templates/rejection.json"),
        );
        Paths::for_env(root, env)
    }

    #[test]
    fn resolve_returns_none_when_no_matchers() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let sel = resolve(&[], &p, &p).unwrap();
        assert!(sel.is_none());
    }

    #[test]
    fn resolve_literal_hits_exactly_one() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let sel = resolve(&["hooks/validator-invoices".to_string()], &p, &p)
            .unwrap()
            .unwrap();
        assert_eq!(sel.items.len(), 1);
        assert!(sel.contains("hooks", "validator-invoices"));
    }

    #[test]
    fn resolve_glob_within_kind() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let sel = resolve(&["hooks/*".to_string()], &p, &p).unwrap().unwrap();
        assert!(sel.contains("hooks", "validator-invoices"));
        assert!(sel.contains("hooks", "sftp-import"));
        assert!(!sel.contains("rules", "check-totals"));
    }

    #[test]
    fn resolve_cross_kind_glob_spans_kinds() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let sel = resolve(&["*/cost-invoices".to_string()], &p, &p)
            .unwrap()
            .unwrap();
        assert!(sel.contains("queues", "cost-invoices"));
        assert!(sel.contains("schemas", "cost-invoices"));
        assert!(!sel.contains("queues", "credit-notes"));
    }

    #[test]
    fn resolve_unions_multiple_matchers() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let sel = resolve(
            &[
                "hooks/validator-invoices".to_string(),
                "rules/check-totals".to_string(),
            ],
            &p,
            &p,
        )
        .unwrap()
        .unwrap();
        assert_eq!(sel.items.len(), 2);
        assert!(sel.contains("hooks", "validator-invoices"));
        assert!(sel.contains("rules", "check-totals"));
    }

    #[test]
    fn resolve_zero_match_errors_with_offending_selector() {
        let tmp = TempDir::new().unwrap();
        let p = make_env(tmp.path(), "test");
        let err = resolve(&["hooks/nonexistent".to_string()], &p, &p).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("hooks/nonexistent"), "got: {s}");
        assert!(s.contains("matched 0 objects"), "got: {s}");
    }

    #[test]
    fn resolve_candidate_set_unions_src_and_tgt() {
        // hooks/only-in-tgt is in tgt but not src; matcher should still find it.
        let tmp = TempDir::new().unwrap();
        let src_p = make_env(tmp.path(), "test");
        let tgt_root = tmp.path();
        let tgt_p = Paths::for_env(tgt_root, "prod");
        let env_root = tgt_root.join("envs/prod");
        fs::create_dir_all(env_root.join("hooks")).unwrap();
        fs::write(env_root.join("hooks/only-in-tgt.json"), b"{}").unwrap();

        let sel = resolve(&["hooks/only-in-tgt".to_string()], &src_p, &tgt_p)
            .unwrap()
            .unwrap();
        assert!(sel.contains("hooks", "only-in-tgt"));
    }

    #[test]
    fn resolve_parse_error_propagates_without_io() {
        // Use a path that doesn't exist; if resolve reaches build_candidate_set
        // before failing on the bad matcher, list_slugs would error on the
        // missing dir. The fail-fast guarantee says we never get there.
        let p = Paths::for_env(std::path::Path::new("/nonexistent"), "x");
        let err = resolve(&["hooks".to_string()], &p, &p).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("expected '<kind>/<slug>'"), "got: {s}");
    }
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn literal() {
        assert!(glob_matches("validator-invoices", "validator-invoices"));
        assert!(!glob_matches("validator-invoices", "validator-credit"));
        assert!(!glob_matches("validator-invoices", "validator-invoices-x"));
    }

    #[test]
    fn star_suffix() {
        assert!(glob_matches("cost-*", "cost-invoices"));
        assert!(glob_matches("cost-*", "cost-"));
        assert!(!glob_matches("cost-*", "credit-invoices"));
    }

    #[test]
    fn star_prefix() {
        assert!(glob_matches("*-invoices", "cost-invoices"));
        assert!(glob_matches("*-invoices", "-invoices"));
        assert!(!glob_matches("*-invoices", "cost-credit"));
    }

    #[test]
    fn star_only() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*", "with/slashes"));
    }

    #[test]
    fn star_middle() {
        assert!(glob_matches("cost-*-invoices", "cost-eu-invoices"));
        assert!(glob_matches("cost-*-invoices", "cost--invoices"));
        assert!(!glob_matches("cost-*-invoices", "cost-invoices"));
    }

    #[test]
    fn empty_pattern() {
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "abc"));
    }

    #[test]
    fn pattern_longer_than_text() {
        assert!(!glob_matches("abcdef", "abc"));
    }
}
