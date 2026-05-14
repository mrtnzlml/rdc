//! Per-invocation selection filter for `rdc deploy`. When the user passes
//! one or more `--only <selector>` flags, only the matching `(kind, slug)`
//! pairs participate in the create / update / delete phases.
//!
//! The selection lives in memory for the duration of the command — it
//! does not touch the mapping file, lockfile, overlay, or any other
//! persistent state. Re-running with the same flags on the same snapshots
//! yields a byte-identical plan.

use anyhow::{anyhow, bail, Result};

/// The kinds `rdc deploy` operates on. Must stay in sync with
/// `cli::deploy::run::KINDS_IN_DEP_ORDER`.
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
