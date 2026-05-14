//! Per-invocation selection filter for `rdc deploy`. When the user passes
//! one or more `--only <selector>` flags, only the matching `(kind, slug)`
//! pairs participate in the create / update / delete phases.
//!
//! The selection lives in memory for the duration of the command — it
//! does not touch the mapping file, lockfile, overlay, or any other
//! persistent state. Re-running with the same flags on the same snapshots
//! yields a byte-identical plan.

// `--only` accepts any kind that `rdc deploy` operates on. Reuse the
// dep-order list as the single source of truth so adding a kind in
// run.rs automatically extends what --only accepts.
use crate::cli::deploy::run::{list_slugs, KINDS_IN_DEP_ORDER as DEPLOYABLE_KINDS};
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone)]
pub(crate) struct Selection {
    pub(crate) items: BTreeSet<(String, String)>,
}

impl Selection {
    pub(crate) fn contains(&self, kind: &str, slug: &str) -> bool {
        self.items
            .contains(&(kind.to_string(), slug.to_string()))
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Resolve a list of `--only` flags against the candidate set built from
/// the src + tgt snapshots.
///
/// Returns `Ok(None)` when `raw_matchers` is empty (signalling
/// "no filter — whole-snapshot deploy"). Every matcher must hit at least
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

fn build_candidate_set(
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<BTreeSet<(String, String)>> {
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
            bail!("invalid --only '{raw}': kind segment is empty (use '*' for any kind, e.g. '*/cost-invoices')");
        }

        if slug_part.is_empty() {
            bail!("invalid --only '{raw}': slug segment is empty");
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
        if let Some(k) = &self.kind {
            if k != kind {
                return false;
            }
        }
        glob_matches(&self.slug_pattern, slug)
    }
}

/// URL-typed fields per kind. Mirrors the field list rewritten by
/// `cli::deploy::common::rewrite_urls` — when that list changes, this
/// one must change too.
pub(crate) fn extract_refs(kind: &str, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match kind {
        "queues" => {
            push_str(&mut out, value.get("workspace"));
            push_str(&mut out, value.get("schema"));
        }
        "email_templates" => {
            push_str(&mut out, value.get("queue"));
        }
        "hooks" => {
            push_str_array(&mut out, value.get("queues"));
            push_str_array(&mut out, value.get("run_after"));
        }
        "inboxes" => {
            push_str_array(&mut out, value.get("queues"));
        }
        "rules" => {
            push_str_array(&mut out, value.get("queues"));
        }
        // workspaces, schemas, labels, engines, engine_fields:
        // no outbound URL refs we promote across envs.
        _ => {}
    }
    out
}

fn push_str(out: &mut Vec<String>, v: Option<&Value>) {
    if let Some(s) = v.and_then(|v| v.as_str()) {
        out.push(s.to_string());
    }
}

fn push_str_array(out: &mut Vec<String>, v: Option<&Value>) {
    if let Some(arr) = v.and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                out.push(s.to_string());
            }
        }
    }
}

/// One outbound reference whose target is missing from both the
/// selection and the tgt lockfile. The `from` and `to` pairs are
/// `(kind, slug)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unresolved {
    pub(crate) from: (String, String),
    pub(crate) to: (String, String),
}

/// Classify a list of `(selected_object, [referenced_urls])` tuples.
/// Returns only the references whose target is neither in the selection
/// nor in the tgt lockfile (i.e., the user has to decide what to do).
pub(crate) fn classify_unresolved(
    refs_per_object: &[((String, String), Vec<String>)],
    selection: &Selection,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
) -> Vec<Unresolved> {
    let mut seen: std::collections::BTreeSet<(String, String, String, String)> =
        std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (from, urls) in refs_per_object {
        for url in urls {
            // Translate the src URL into a (kind, slug) pair via the src
            // lockfile. If the URL doesn't resolve to a known src object,
            // we don't treat it as an unresolved dep — URL rewrite skips
            // it the same way at execute time.
            let Some((kind, slug)) = src_lockfile.lookup_url(url) else {
                continue;
            };
            let to = (kind.to_string(), slug.to_string());

            if selection.contains(&to.0, &to.1) {
                continue;
            }
            // Honor the mapping: a rename pairs src_slug → tgt_slug.
            // Falling back to src_slug handles the common same-slug case.
            let tgt_slug = mapping
                .lookup_tgt_slug(&to.0, &to.1)
                .unwrap_or(&to.1);
            if tgt_lockfile
                .objects
                .get(&to.0)
                .and_then(|m| m.get(tgt_slug))
                .is_some()
            {
                continue;
            }
            let key = (from.0.clone(), from.1.clone(), to.0.clone(), to.1.clone());
            if seen.insert(key) {
                out.push(Unresolved {
                    from: from.clone(),
                    to,
                });
            }
        }
    }
    out
}

/// Read the on-disk JSON for `(kind, slug)` from a snapshot. Returns the
/// raw `serde_json::Value` so the caller can pass it to `extract_refs`.
///
/// Returns an empty object when the file doesn't exist (the selection
/// may include tgt-only slugs whose src file is absent — that's fine,
/// just means no outbound refs to walk).
fn read_src_value(
    paths: &Paths,
    kind: &str,
    slug: &str,
) -> Result<Value> {
    let path = match kind {
        "hooks" => paths.hooks_dir().join(format!("{slug}.json")),
        "rules" => paths.rules_dir().join(format!("{slug}.json")),
        "labels" => paths.labels_dir().join(format!("{slug}.json")),
        "workspaces" => paths.workspace_dir(slug).join("workspace.json"),
        "engines" => paths.engines_dir().join(slug).join("engine.json"),
        "engine_fields" => {
            // engine_fields: walk engines/<engine>/fields/<field>.json
            let engines_dir = paths.engines_dir();
            if !engines_dir.exists() {
                return Ok(Value::Object(serde_json::Map::new()));
            }
            for e_entry in std::fs::read_dir(&engines_dir)? {
                let e = e_entry?;
                if !e.file_type()?.is_dir() { continue; }
                let candidate = e.path().join("fields").join(format!("{slug}.json"));
                if candidate.exists() {
                    let bytes = std::fs::read(&candidate)
                        .with_context(|| format!("reading {}", candidate.display()))?;
                    let v: Value = serde_json::from_slice(&bytes)
                        .with_context(|| format!("parsing {}", candidate.display()))?;
                    return Ok(v);
                }
            }
            return Ok(Value::Object(serde_json::Map::new()));
        }
        "queues" | "schemas" | "inboxes" => {
            // queue-nested: walk workspaces/<ws>/queues/<slug>/<file>.
            let ws_dir = paths.workspaces_dir();
            if !ws_dir.exists() {
                return Ok(Value::Object(serde_json::Map::new()));
            }
            let fname = match kind {
                "queues" => "queue.json",
                "schemas" => "schema.json",
                "inboxes" => "inbox.json",
                _ => unreachable!(),
            };
            for ws_entry in std::fs::read_dir(&ws_dir)? {
                let ws = ws_entry?;
                if !ws.file_type()?.is_dir() { continue; }
                let candidate = ws.path().join("queues").join(slug).join(fname);
                if candidate.exists() {
                    let bytes = std::fs::read(&candidate)
                        .with_context(|| format!("reading {}", candidate.display()))?;
                    let v: Value = serde_json::from_slice(&bytes)
                        .with_context(|| format!("parsing {}", candidate.display()))?;
                    return Ok(v);
                }
            }
            return Ok(Value::Object(serde_json::Map::new()));
        }
        "email_templates" => {
            // slug is `<ws>/<q>/<template>`
            let parts: Vec<&str> = slug.splitn(3, '/').collect();
            if parts.len() != 3 {
                return Ok(Value::Object(serde_json::Map::new()));
            }
            paths
                .queue_email_templates_dir(parts[0], parts[1])
                .join(format!("{}.json", parts[2]))
        }
        _ => return Ok(Value::Object(serde_json::Map::new())),
    };

    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Iterative dep check.
///
/// On each pass: collect refs for every object currently in the selection,
/// classify, and either (a) return `Ok` if there are no unresolved deps,
/// (b) prompt + extend the selection if `interactive` and the prompt
/// returns true, (c) return an error otherwise.
///
/// `prompt` is invoked with the current unresolved list and returns
/// `true` to fold them into the selection, `false` to abort. In
/// production the prompt is a y/N reader; in tests it's a closure.
pub(crate) fn dep_check(
    selection: &mut Selection,
    src_paths: &Paths,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
    interactive: bool,
    prompt: &mut dyn FnMut(&[Unresolved]) -> bool,
) -> Result<()> {
    loop {
        let mut refs_per_object: Vec<((String, String), Vec<String>)> = Vec::new();
        for (kind, slug) in &selection.items {
            let v = read_src_value(src_paths, kind, slug)?;
            let urls = extract_refs(kind, &v);
            refs_per_object.push(((kind.clone(), slug.clone()), urls));
        }

        let unresolved = classify_unresolved(
            &refs_per_object,
            selection,
            src_lockfile,
            tgt_lockfile,
            mapping,
        );

        if unresolved.is_empty() {
            return Ok(());
        }

        if !interactive {
            let mut msg = String::from(
                "selection has unresolved dependencies (not in --only, not yet on tgt):\n",
            );
            for u in &unresolved {
                msg.push_str(&format!(
                    "  {}/{} -> {}/{}\n",
                    u.from.0, u.from.1, u.to.0, u.to.1,
                ));
            }
            msg.push_str("Re-run with these added to --only, e.g.:\n");
            let mut deduped: std::collections::BTreeSet<(String, String)> =
                std::collections::BTreeSet::new();
            for u in &unresolved {
                deduped.insert(u.to.clone());
            }
            for (k, s) in &deduped {
                msg.push_str(&format!("  --only {k}/{s}\n"));
            }
            bail!("{}", msg.trim_end());
        }

        let proceed = prompt(&unresolved);
        if !proceed {
            bail!("dep check aborted by user; selection not modified");
        }
        for u in &unresolved {
            selection.items.insert(u.to.clone());
        }
        // Loop: re-classify in case the newly-added deps themselves have
        // unresolved peers (transitive).
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
        touch(&env_root.join("workspaces/finance/queues/cost-invoices/email-templates/rejection.json"));
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
mod refs_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_extracts_queue_and_run_after_urls() {
        let body = json!({
            "name": "validator",
            "queues": [
                "https://test.example/api/v1/queues/600",
                "https://test.example/api/v1/queues/601"
            ],
            "run_after": ["https://test.example/api/v1/hooks/700"]
        });
        let urls = extract_refs("hooks", &body);
        assert_eq!(urls.len(), 3);
        assert!(urls.iter().any(|u| u.ends_with("/queues/600")));
        assert!(urls.iter().any(|u| u.ends_with("/queues/601")));
        assert!(urls.iter().any(|u| u.ends_with("/hooks/700")));
    }

    #[test]
    fn queue_extracts_workspace_and_schema_urls() {
        let body = json!({
            "name": "cost-invoices",
            "workspace": "https://test.example/api/v1/workspaces/3",
            "schema": "https://test.example/api/v1/schemas/5"
        });
        let urls = extract_refs("queues", &body);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn schema_has_no_refs() {
        let body = json!({"name": "cost-invoices-schema", "content": []});
        let urls = extract_refs("schemas", &body);
        assert!(urls.is_empty());
    }

    #[test]
    fn email_template_extracts_queue() {
        let body = json!({
            "name": "rejection",
            "queue": "https://test.example/api/v1/queues/600"
        });
        let urls = extract_refs("email_templates", &body);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn rule_extracts_queue_list() {
        let body = json!({
            "name": "check-totals",
            "queues": ["https://test.example/api/v1/queues/600"]
        });
        let urls = extract_refs("rules", &body);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn inbox_extracts_queue_list() {
        let body = json!({
            "name": "cost-invoices-inbox",
            "queues": ["https://test.example/api/v1/queues/600"],
            "email": "invoices@example.com"
        });
        let urls = extract_refs("inboxes", &body);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].ends_with("/queues/600"));
    }

    #[test]
    fn null_urls_ignored() {
        let body = json!({"workspace": null, "schema": null});
        let urls = extract_refs("queues", &body);
        assert!(urls.is_empty());
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use crate::mapping::Mapping;
    use crate::state::Lockfile;

    fn lockfile_with(kind: &str, slug: &str, id: u64, url: &str) -> Lockfile {
        let mut lf = Lockfile::default();
        lf.objects
            .entry(kind.to_string())
            .or_default()
            .insert(
                slug.to_string(),
                crate::state::lockfile::ObjectEntry {
                    id,
                    url: Some(url.to_string()),
                    modified_at: None,
                    content_hash: None,
                },
            );
        lf
    }

    #[test]
    fn ref_in_selection_is_ok() {
        let src_lf = lockfile_with(
            "queues",
            "cost-invoices",
            600,
            "https://test.example/api/v1/queues/600",
        );
        let tgt_lf = Lockfile::default();
        let mut sel = Selection::default();
        sel.items
            .insert(("queues".to_string(), "cost-invoices".to_string()));

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "validator-invoices".to_string()),
                vec!["https://test.example/api/v1/queues/600".to_string()],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &Mapping::default(),
        );
        assert!(unresolved.is_empty(), "{unresolved:?}");
    }

    #[test]
    fn ref_in_tgt_lockfile_is_ok() {
        let src_lf = lockfile_with(
            "queues",
            "cost-invoices",
            600,
            "https://test.example/api/v1/queues/600",
        );
        let tgt_lf = lockfile_with(
            "queues",
            "cost-invoices",
            900,
            "https://prod.example/api/v1/queues/900",
        );
        let sel = Selection::default();

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "validator-invoices".to_string()),
                vec!["https://test.example/api/v1/queues/600".to_string()],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &Mapping::default(),
        );
        assert!(unresolved.is_empty(), "{unresolved:?}");
    }

    #[test]
    fn ref_missing_both_is_unresolved() {
        let src_lf = lockfile_with(
            "queues",
            "cost-invoices",
            600,
            "https://test.example/api/v1/queues/600",
        );
        let tgt_lf = Lockfile::default();
        let sel = Selection::default();

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "validator-invoices".to_string()),
                vec!["https://test.example/api/v1/queues/600".to_string()],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &Mapping::default(),
        );
        assert_eq!(unresolved.len(), 1);
        let u = &unresolved[0];
        assert_eq!(u.from, ("hooks".to_string(), "validator-invoices".to_string()));
        assert_eq!(u.to, ("queues".to_string(), "cost-invoices".to_string()));
    }

    #[test]
    fn unknown_url_in_src_is_silently_ignored() {
        // Some URLs don't resolve to any known src object (e.g.
        // workflow_step refs we don't track). They're not "unresolved
        // deps" — they're just out-of-scope strings. URL rewrite leaves
        // them alone too.
        let src_lf = Lockfile::default();
        let tgt_lf = Lockfile::default();
        let sel = Selection::default();

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "x".to_string()),
                vec!["https://test.example/api/v1/queues/600".to_string()],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &Mapping::default(),
        );
        assert!(unresolved.is_empty());
    }

    #[test]
    fn dedup_same_from_to_pair() {
        // A hook references the same missing queue twice — should produce
        // exactly one Unresolved entry, not two.
        let src_lf = lockfile_with(
            "queues",
            "cost-invoices",
            600,
            "https://test.example/api/v1/queues/600",
        );
        let tgt_lf = Lockfile::default();
        let sel = Selection::default();
        let mapping = Mapping::default();

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "h".to_string()),
                vec![
                    "https://test.example/api/v1/queues/600".to_string(),
                    "https://test.example/api/v1/queues/600".to_string(),
                ],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &mapping,
        );
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn multiple_distinct_targets_each_reported() {
        // One hook referencing two distinct missing queues — both must be
        // reported.
        let mut src_lf = Lockfile::default();
        src_lf.objects.entry("queues".into()).or_default().insert(
            "q-a".into(),
            crate::state::lockfile::ObjectEntry {
                id: 1,
                url: Some("https://test.example/api/v1/queues/1".into()),
                modified_at: None,
                content_hash: None,
            },
        );
        src_lf.objects.entry("queues".into()).or_default().insert(
            "q-b".into(),
            crate::state::lockfile::ObjectEntry {
                id: 2,
                url: Some("https://test.example/api/v1/queues/2".into()),
                modified_at: None,
                content_hash: None,
            },
        );
        let tgt_lf = Lockfile::default();
        let sel = Selection::default();
        let mapping = Mapping::default();

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "h".to_string()),
                vec![
                    "https://test.example/api/v1/queues/1".to_string(),
                    "https://test.example/api/v1/queues/2".to_string(),
                ],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &mapping,
        );
        assert_eq!(unresolved.len(), 2);
    }

    #[test]
    fn mapping_rename_treats_renamed_tgt_as_present() {
        // src has queues/cost-invoices; tgt has queues/cost-invoices-prod
        // under a mapping rename. Classifier must NOT flag this as unresolved.
        let src_lf = lockfile_with(
            "queues",
            "cost-invoices",
            600,
            "https://test.example/api/v1/queues/600",
        );
        let mut tgt_lf = Lockfile::default();
        tgt_lf.objects.entry("queues".into()).or_default().insert(
            "cost-invoices-prod".into(),
            crate::state::lockfile::ObjectEntry {
                id: 900,
                url: Some("https://prod.example/api/v1/queues/900".into()),
                modified_at: None,
                content_hash: None,
            },
        );
        let sel = Selection::default();
        let mut mapping = Mapping::default();
        mapping.queues.insert("cost-invoices".to_string(), "cost-invoices-prod".to_string());

        let unresolved = classify_unresolved(
            &[(
                ("hooks".to_string(), "h".to_string()),
                vec!["https://test.example/api/v1/queues/600".to_string()],
            )],
            &sel,
            &src_lf,
            &tgt_lf,
            &mapping,
        );
        assert!(unresolved.is_empty(), "{unresolved:?}");
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

#[cfg(test)]
mod dep_check_tests {
    use super::*;
    use crate::mapping::Mapping;
    use crate::paths::Paths;
    use crate::state::Lockfile;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn touch_json(path: &Path, body: &serde_json::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(body).unwrap()).unwrap();
    }

    fn make_src_with_hook_referencing_queue(root: &Path) -> (Paths, Lockfile) {
        let env_root = root.join("envs/test");
        touch_json(
            &env_root.join("hooks/validator-invoices.json"),
            &serde_json::json!({
                "name": "validator",
                "queues": ["https://test.example/api/v1/queues/600"]
            }),
        );
        touch_json(
            &env_root.join("workspaces/finance/workspace.json"),
            &serde_json::json!({"name": "finance"}),
        );
        touch_json(
            &env_root.join("workspaces/finance/queues/cost-invoices/queue.json"),
            &serde_json::json!({"name": "cost-invoices"}),
        );

        let mut lf = Lockfile::default();
        lf.objects.entry("queues".into()).or_default().insert(
            "cost-invoices".into(),
            crate::state::lockfile::ObjectEntry {
                id: 600,
                url: Some("https://test.example/api/v1/queues/600".into()),
                modified_at: None,
                content_hash: None,
            },
        );
        (Paths::for_env(root, "test"), lf)
    }

    #[test]
    fn no_unresolved_no_prompt_called() {
        let tmp = TempDir::new().unwrap();
        let (src_paths, src_lf) = make_src_with_hook_referencing_queue(tmp.path());
        let mut tgt_lf = Lockfile::default();
        tgt_lf.objects.entry("queues".into()).or_default().insert(
            "cost-invoices".into(),
            crate::state::lockfile::ObjectEntry {
                id: 900,
                url: Some("https://prod.example/api/v1/queues/900".into()),
                modified_at: None,
                content_hash: None,
            },
        );

        let mut sel = Selection::default();
        sel.items
            .insert(("hooks".into(), "validator-invoices".into()));

        let mapping = Mapping::default();
        let mut prompt_calls = 0usize;
        dep_check(
            &mut sel,
            &src_paths,
            &src_lf,
            &tgt_lf,
            &mapping,
            true,
            &mut |_unresolved| {
                prompt_calls += 1;
                true
            },
        )
        .unwrap();

        assert_eq!(prompt_calls, 0);
        assert_eq!(sel.items.len(), 1);
    }

    #[test]
    fn unresolved_non_tty_refuses() {
        let tmp = TempDir::new().unwrap();
        let (src_paths, src_lf) = make_src_with_hook_referencing_queue(tmp.path());
        let tgt_lf = Lockfile::default();

        let mut sel = Selection::default();
        sel.items
            .insert(("hooks".into(), "validator-invoices".into()));

        let mapping = Mapping::default();
        let mut prompt_calls = 0usize;
        let err = dep_check(
            &mut sel,
            &src_paths,
            &src_lf,
            &tgt_lf,
            &mapping,
            false,
            &mut |_unresolved| {
                prompt_calls += 1;
                true
            },
        )
        .unwrap_err();
        assert_eq!(prompt_calls, 0);
        let s = format!("{err:#}");
        assert!(s.contains("queues/cost-invoices"), "got: {s}");
        assert!(s.contains("--only"), "got: {s}");
    }

    #[test]
    fn unresolved_tty_yes_includes_dep() {
        let tmp = TempDir::new().unwrap();
        let (src_paths, src_lf) = make_src_with_hook_referencing_queue(tmp.path());
        let tgt_lf = Lockfile::default();

        let mut sel = Selection::default();
        sel.items
            .insert(("hooks".into(), "validator-invoices".into()));

        let mapping = Mapping::default();
        let mut prompt_calls = 0usize;
        dep_check(
            &mut sel,
            &src_paths,
            &src_lf,
            &tgt_lf,
            &mapping,
            true,
            &mut |unresolved| {
                prompt_calls += 1;
                assert_eq!(unresolved.len(), 1);
                assert_eq!(unresolved[0].to, ("queues".into(), "cost-invoices".into()));
                true
            },
        )
        .unwrap();
        assert_eq!(prompt_calls, 1);
        assert!(sel.contains("queues", "cost-invoices"));
    }

    #[test]
    fn unresolved_tty_no_aborts_cleanly() {
        let tmp = TempDir::new().unwrap();
        let (src_paths, src_lf) = make_src_with_hook_referencing_queue(tmp.path());
        let tgt_lf = Lockfile::default();

        let mut sel = Selection::default();
        sel.items
            .insert(("hooks".into(), "validator-invoices".into()));

        let mapping = Mapping::default();
        let err = dep_check(
            &mut sel,
            &src_paths,
            &src_lf,
            &tgt_lf,
            &mapping,
            true,
            &mut |_unresolved| false,
        )
        .unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("aborted"), "got: {s}");
    }

    #[test]
    fn transitive_dep_picked_up_on_re_check() {
        // hook → queue (missing) → workspace (also missing).
        // First prompt offers the queue; we accept; second prompt must
        // offer the workspace.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let env_root = root.join("envs/test");
        touch_json(
            &env_root.join("hooks/validator-invoices.json"),
            &serde_json::json!({
                "name": "v",
                "queues": ["https://test.example/api/v1/queues/600"]
            }),
        );
        touch_json(
            &env_root.join("workspaces/finance/workspace.json"),
            &serde_json::json!({"name": "finance"}),
        );
        touch_json(
            &env_root.join("workspaces/finance/queues/cost-invoices/queue.json"),
            &serde_json::json!({
                "name": "cost-invoices",
                "workspace": "https://test.example/api/v1/workspaces/3",
                "schema": null
            }),
        );

        let mut src_lf = Lockfile::default();
        src_lf.objects.entry("queues".into()).or_default().insert(
            "cost-invoices".into(),
            crate::state::lockfile::ObjectEntry {
                id: 600,
                url: Some("https://test.example/api/v1/queues/600".into()),
                modified_at: None,
                content_hash: None,
            },
        );
        src_lf.objects.entry("workspaces".into()).or_default().insert(
            "finance".into(),
            crate::state::lockfile::ObjectEntry {
                id: 3,
                url: Some("https://test.example/api/v1/workspaces/3".into()),
                modified_at: None,
                content_hash: None,
            },
        );

        let tgt_lf = Lockfile::default();
        let src_paths = Paths::for_env(root, "test");
        let mapping = Mapping::default();

        let mut sel = Selection::default();
        sel.items
            .insert(("hooks".into(), "validator-invoices".into()));

        let mut prompt_calls = 0usize;
        dep_check(
            &mut sel,
            &src_paths,
            &src_lf,
            &tgt_lf,
            &mapping,
            true,
            &mut |_unresolved| {
                prompt_calls += 1;
                true
            },
        )
        .unwrap();
        assert!(prompt_calls >= 2, "expected at least two prompts; got {prompt_calls}");
        assert!(sel.contains("queues", "cost-invoices"));
        assert!(sel.contains("workspaces", "finance"));
    }
}
