use std::collections::HashSet;

/// Derive a folder slug from a user-visible Connection name.
///
/// Delegates to `rdc::slug::slugify_unique`, which (a) lowercases, (b)
/// drops non-ASCII characters, (c) collapses non-alphanumeric runs to a
/// single hyphen, (d) appends `-2`, `-3`, ... when the base slug
/// already exists in `used`.
pub fn derive_slug(name: &str, used: &HashSet<String>) -> String {
    rdc::slug::slugify_unique(name, used)
}
