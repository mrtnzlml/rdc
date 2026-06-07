//! Filesystem helper shared with `rdc pull`'s portabilize pass.

use crate::paths::Paths;

/// Helper: find queue dir under either env's `workspaces/<ws>/queues/<q>/`.
pub fn locate_queue_dir(paths: &Paths, queue_slug: &str) -> Option<std::path::PathBuf> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return None;
    }
    for ws_entry in std::fs::read_dir(&ws_dir).ok()? {
        let Ok(ws_entry) = ws_entry else { continue };
        if !ws_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let queue_dir = ws_entry.path().join("queues").join(queue_slug);
        if queue_dir.join("queue.json").exists() || queue_dir.is_dir() {
            return Some(queue_dir);
        }
    }
    None
}
