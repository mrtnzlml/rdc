use crate::api::RossumClient;
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};

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
