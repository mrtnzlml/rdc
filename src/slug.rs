// Slug derivation. Stable, deterministic, ASCII-safe.
// Conversion rules:
//   - Lowercase the input.
//   - Replace any sequence of non-alphanumeric ASCII characters with a single hyphen.
//   - Strip leading/trailing hyphens.
//   - If the result is empty (input was all non-alphanumeric), use "_unnamed".
//
// Collision handling is the caller's responsibility (see `slugify_unique`).

pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_hyphen = true; // skip leading hyphens
    for ch in input.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "_unnamed".to_string()
    } else {
        out
    }
}

/// Given a base slug and a set of slugs already in use, return a slug that does
/// not collide. Adds `-2`, `-3`, ... as needed.
pub fn slugify_unique(input: &str, used: &std::collections::HashSet<String>) -> String {
    let base = slugify(input);
    if !used.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn simple_name() {
        assert_eq!(slugify("Validator: Invoices"), "validator-invoices");
    }

    #[test]
    fn collapses_runs_of_punctuation() {
        assert_eq!(slugify("Foo  ---  Bar!!!"), "foo-bar");
    }

    #[test]
    fn lowercases() {
        assert_eq!(slugify("UPPER lower"), "upper-lower");
    }

    #[test]
    fn empty_string() {
        assert_eq!(slugify(""), "_unnamed");
    }

    #[test]
    fn only_punctuation() {
        assert_eq!(slugify("!!! ???"), "_unnamed");
    }

    #[test]
    fn unicode_stripped() {
        // Non-ASCII characters are dropped (treated as non-alphanumeric).
        assert_eq!(slugify("Faktura č. 1"), "faktura-1");
    }

    #[test]
    fn trims_leading_and_trailing_hyphens() {
        assert_eq!(slugify("---hello---"), "hello");
    }

    #[test]
    fn unique_first_use_returns_base() {
        let used = HashSet::new();
        assert_eq!(slugify_unique("My Hook", &used), "my-hook");
    }

    #[test]
    fn unique_collision_appends_2() {
        let mut used = HashSet::new();
        used.insert("my-hook".to_string());
        assert_eq!(slugify_unique("My Hook", &used), "my-hook-2");
    }

    #[test]
    fn unique_collision_finds_next_free() {
        let mut used = HashSet::new();
        used.insert("my-hook".to_string());
        used.insert("my-hook-2".to_string());
        used.insert("my-hook-3".to_string());
        assert_eq!(slugify_unique("My Hook", &used), "my-hook-4");
    }
}
