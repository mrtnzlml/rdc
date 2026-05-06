use crate::api::RossumClient;
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{anyhow, Result};

/// Shared state passed through every per-kind pull driver.
pub struct PullCtx<'a> {
    pub paths: &'a Paths,
    pub client: &'a RossumClient,
    pub lockfile: &'a mut Lockfile,
}

/// Compute the content hash of an object's serialized form. The pull drivers
/// hash the JSON bytes they're about to write to disk so the lockfile records
/// what was actually persisted.
pub fn hash_for_lockfile(bytes: &[u8]) -> String {
    content_hash(bytes)
}

/// Record an object in the lockfile under the given kind/slug.
pub fn record_object(
    lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    id: u64,
    url: Option<String>,
    modified_at: Option<String>,
    content_hash: Option<String>,
) {
    lockfile.upsert(
        kind,
        slug,
        ObjectEntry { id, url, modified_at, content_hash },
    );
}

/// Format `"<n> <noun>"` with correct singular/plural agreement.
/// Used by the pull summary line and any future count-aware UX.
pub fn pluralize(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Parse the trailing numeric ID out of a Rossum API URL, e.g.
/// `https://x.rossum.app/api/v1/schemas/1234` -> `1234`.
pub fn parse_id_from_url(url: &str) -> Result<u64> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("URL has no path segments: {url}"))?;
    last.parse::<u64>()
        .map_err(|e| anyhow!("URL trailing segment '{last}' is not a u64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_singular() {
        assert_eq!(pluralize(1, "hook", "hooks"), "1 hook");
        assert_eq!(pluralize(1, "inbox", "inboxes"), "1 inbox");
    }

    #[test]
    fn pluralize_plural() {
        assert_eq!(pluralize(0, "hook", "hooks"), "0 hooks");
        assert_eq!(pluralize(2, "hook", "hooks"), "2 hooks");
        assert_eq!(pluralize(0, "inbox", "inboxes"), "0 inboxes");
    }

    #[test]
    fn parse_id_basic() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/1234").unwrap(), 1234);
    }

    #[test]
    fn parse_id_with_trailing_slash() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/9/").unwrap(), 9);
    }

    #[test]
    fn parse_id_non_numeric_errors() {
        assert!(parse_id_from_url("https://x/api/v1/schemas/abc").is_err());
    }
}
