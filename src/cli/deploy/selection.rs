//! Per-invocation selection filter for `rdc deploy`. When the user passes
//! one or more `--only <selector>` flags, only the matching `(kind, slug)`
//! pairs participate in the create / update / delete phases.
//!
//! The selection lives in memory for the duration of the command — it
//! does not touch the mapping file, lockfile, overlay, or any other
//! persistent state. Re-running with the same flags on the same snapshots
//! yields a byte-identical plan.

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
}
