//! Filesystem helper shared with the pull phase's portabilize pass.

use crate::paths::Paths;

/// Helper: find queue dir under either env's `workspaces/<ws>/queues/<q>/`.
///
/// A dir holding a `queue.json` wins over a bare one: a queue deleted on the
/// env can leave its emptied dir behind in another workspace, and a namesake
/// queue may take the same slug.
pub fn locate_queue_dir(paths: &Paths, queue_slug: &str) -> Option<std::path::PathBuf> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return None;
    }
    let mut bare = None;
    for ws_entry in std::fs::read_dir(&ws_dir).ok()? {
        let Ok(ws_entry) = ws_entry else { continue };
        if !ws_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let queue_dir = ws_entry.path().join("queues").join(queue_slug);
        if queue_dir.join("queue.json").exists() {
            return Some(queue_dir);
        }
        if bare.is_none() && queue_dir.is_dir() {
            bare = Some(queue_dir);
        }
    }
    bare
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An emptied dir left by a deleted queue must not shadow a namesake
    /// queue in another workspace, whichever order `read_dir` yields.
    #[test]
    fn a_queue_json_beats_an_empty_namesake_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        for ws in ["a", "b", "c"] {
            std::fs::create_dir_all(paths.queue_dir(ws, "invoices").join("email-templates")).unwrap();
        }
        std::fs::write(paths.queue_dir("b", "invoices").join("queue.json"), "{}").unwrap();
        assert_eq!(locate_queue_dir(&paths, "invoices"), Some(paths.queue_dir("b", "invoices")));
    }
}
