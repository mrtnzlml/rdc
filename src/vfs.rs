//! A scoped virtual filesystem. Every file access in the crate goes through
//! the free functions here (same names as `std::fs`), which dispatch to the
//! [`Store`] installed by [`scope`], or to [`NativeStore`] (plain `std::fs`)
//! when none is. The browser build scopes a [`MemStore`] so the same code
//! runs on an in-memory project tree.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a path is. Mirrors the parts of `std::fs::FileType` the crate uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    dir: bool,
    file: bool,
    symlink: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_file(&self) -> bool {
        self.file
    }
    pub fn is_symlink(&self) -> bool {
        self.symlink
    }
    fn of_dir() -> Self {
        Self { dir: true, file: false, symlink: false }
    }
    fn of_file() -> Self {
        Self { dir: false, file: true, symlink: false }
    }
}

impl From<std::fs::FileType> for FileType {
    fn from(t: std::fs::FileType) -> Self {
        Self { dir: t.is_dir(), file: t.is_file(), symlink: t.is_symlink() }
    }
}

/// One `read_dir` entry. The accessors keep `std::fs::DirEntry`'s shape so
/// call sites compile unchanged.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub path: PathBuf,
    pub file_name: OsString,
    pub file_type: FileType,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub fn file_name(&self) -> OsString {
        self.file_name.clone()
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.file_type)
    }
}

/// A filesystem. Semantics and error kinds follow `std::fs`.
pub trait Store: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    /// Follows symlinks.
    fn metadata(&self, path: &Path) -> io::Result<FileType>;
    /// Does not follow symlinks. A store without symlinks answers as `metadata`.
    fn symlink_metadata(&self, path: &Path) -> io::Result<FileType> {
        self.metadata(path)
    }
    /// Create `path`, failing with `AlreadyExists` if anything is there. `mode`
    /// is the unix permission bits for the new file; ignored elsewhere.
    fn write_new(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()>;
    /// `create_dir_all` with mode `0o700` natively (secrets directories).
    fn create_dir_all_private(&self, path: &Path) -> io::Result<()>;
    fn is_native(&self) -> bool;
}

/// The real disk: a thin wrapper over `std::fs`.
pub struct NativeStore;

impl Store for NativeStore {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            out.push(DirEntry {
                path: entry.path(),
                file_name: entry.file_name(),
                file_type: entry.file_type()?.into(),
            });
        }
        out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(out)
    }
    fn metadata(&self, path: &Path) -> io::Result<FileType> {
        Ok(std::fs::metadata(path)?.file_type().into())
    }
    fn symlink_metadata(&self, path: &Path) -> io::Result<FileType> {
        Ok(std::fs::symlink_metadata(path)?.file_type().into())
    }
    fn write_new(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts.open(path)?;
        f.write_all(bytes)?;
        f.sync_all()
    }
    fn create_dir_all_private(&self, path: &Path) -> io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(path)
    }
    fn is_native(&self) -> bool {
        true
    }
}

type OnChange = Box<dyn Fn(&str, Option<&[u8]>) + Send + Sync>;

#[derive(Default)]
struct Tree {
    files: BTreeMap<PathBuf, Vec<u8>>,
    /// Explicit directories. A directory with a file below it needs no entry.
    dirs: BTreeSet<PathBuf>,
}

impl Tree {
    fn is_dir(&self, root: &Path, p: &Path) -> bool {
        p == root
            || self.dirs.contains(p)
            || self.files.keys().any(|k| k != p && k.starts_with(p))
    }

    /// Record every ancestor of `p` below `root` as an explicit directory, so
    /// a directory outlives its last file the way it does on disk.
    fn mark_parents(&mut self, root: &Path, p: &Path) {
        let mut cur = p.parent();
        while let Some(d) = cur {
            if d == root || !d.starts_with(root) {
                break;
            }
            self.dirs.insert(d.to_path_buf());
            cur = d.parent();
        }
    }
}

/// An in-memory tree under a virtual `root`. Paths are normalized lexically
/// and anything outside the root is refused, so it cannot be steered at the
/// host's disk.
pub struct MemStore {
    root: PathBuf,
    tree: Mutex<Tree>,
    on_change: Mutex<Option<OnChange>>,
}

fn err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

impl MemStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root, tree: Mutex::new(Tree::default()), on_change: Mutex::new(None) }
    }

    /// `files` are keyed relative to `root`, with `/` separators. A key that
    /// is absolute or escapes the root is `PermissionDenied`.
    pub fn from_files(
        root: PathBuf,
        files: impl IntoIterator<Item = (String, Vec<u8>)>,
    ) -> io::Result<Self> {
        let store = Self::new(root);
        {
            let mut tree = store.tree.lock().unwrap();
            for (k, v) in files {
                if Path::new(&k).has_root() {
                    return Err(err(io::ErrorKind::PermissionDenied, "key is not relative"));
                }
                let p = store.norm(Path::new(&k))?;
                tree.mark_parents(&store.root, &p);
                tree.files.insert(p, v);
            }
        }
        Ok(store)
    }

    /// Every file, keyed relative to the root with `/` separators.
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let tree = self.tree.lock().unwrap();
        tree.files.iter().map(|(k, v)| (self.key(k), v.clone())).collect()
    }

    /// Called after each write, rename (once per side) and delete, with the
    /// root-relative key and the new bytes (`None` for a removal).
    pub fn set_on_change(&self, f: OnChange) {
        *self.on_change.lock().unwrap() = Some(f);
    }

    fn key(&self, p: &Path) -> String {
        let rel = p.strip_prefix(&self.root).unwrap_or(p);
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn norm(&self, path: &Path) -> io::Result<PathBuf> {
        let joined = if path.is_relative() { self.root.join(path) } else { path.to_path_buf() };
        let mut out = PathBuf::new();
        for c in joined.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                c => out.push(c),
            }
        }
        if out.starts_with(&self.root) {
            Ok(out)
        } else {
            Err(err(io::ErrorKind::PermissionDenied, "path is outside the virtual root"))
        }
    }

    /// Fire the callback for each event. Always called with the tree unlocked,
    /// so the callback may read the store.
    fn notify(&self, events: Vec<(String, Option<Vec<u8>>)>) {
        if let Some(f) = self.on_change.lock().unwrap().as_ref() {
            for (k, v) in &events {
                f(k, v.as_deref());
            }
        }
    }

    fn parent_must_be_dir(&self, tree: &Tree, p: &Path) -> io::Result<()> {
        match p.parent() {
            Some(parent) if tree.is_dir(&self.root, parent) => Ok(()),
            _ => Err(err(io::ErrorKind::NotFound, "parent directory does not exist")),
        }
    }

    fn put(&self, path: &Path, bytes: &[u8], must_be_new: bool) -> io::Result<()> {
        let p = self.norm(path)?;
        {
            let mut tree = self.tree.lock().unwrap();
            if tree.is_dir(&self.root, &p) {
                return Err(err(io::ErrorKind::IsADirectory, "is a directory"));
            }
            if must_be_new && tree.files.contains_key(&p) {
                return Err(err(io::ErrorKind::AlreadyExists, "file exists"));
            }
            self.parent_must_be_dir(&tree, &p)?;
            tree.mark_parents(&self.root, &p);
            tree.files.insert(p.clone(), bytes.to_vec());
        }
        self.notify(vec![(self.key(&p), Some(bytes.to_vec()))]);
        Ok(())
    }
}

impl Store for MemStore {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let p = self.norm(path)?;
        let tree = self.tree.lock().unwrap();
        match tree.files.get(&p) {
            Some(b) => Ok(b.clone()),
            None if tree.is_dir(&self.root, &p) => Err(err(io::ErrorKind::IsADirectory, "is a directory")),
            None => Err(err(io::ErrorKind::NotFound, "no such file")),
        }
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.put(path, bytes, false)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = self.norm(path)?;
        let mut tree = self.tree.lock().unwrap();
        // Validate the whole chain before recording any of it.
        let chain: Vec<&Path> = p.ancestors().take_while(|a| *a != self.root).collect();
        if chain.iter().any(|a| tree.files.contains_key(*a)) {
            return Err(err(io::ErrorKind::AlreadyExists, "a file is in the way"));
        }
        let chain: Vec<PathBuf> = chain.into_iter().map(Path::to_path_buf).collect();
        tree.dirs.extend(chain);
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let p = self.norm(path)?;
        {
            let mut tree = self.tree.lock().unwrap();
            if tree.files.remove(&p).is_none() {
                return Err(if tree.is_dir(&self.root, &p) {
                    err(io::ErrorKind::IsADirectory, "is a directory")
                } else {
                    err(io::ErrorKind::NotFound, "no such file")
                });
            }
        }
        self.notify(vec![(self.key(&p), None)]);
        Ok(())
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let p = self.norm(path)?;
        let mut tree = self.tree.lock().unwrap();
        if !tree.is_dir(&self.root, &p) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        let occupied = tree.files.keys().any(|k| k.starts_with(&p))
            || tree.dirs.iter().any(|d| d != &p && d.starts_with(&p));
        if occupied {
            return Err(err(io::ErrorKind::DirectoryNotEmpty, "directory not empty"));
        }
        tree.dirs.remove(&p);
        Ok(())
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = self.norm(path)?;
        let gone: Vec<PathBuf>;
        {
            let mut tree = self.tree.lock().unwrap();
            if !tree.is_dir(&self.root, &p) {
                return Err(err(io::ErrorKind::NotFound, "no such directory"));
            }
            gone = tree.files.keys().filter(|k| k.starts_with(&p)).cloned().collect();
            for k in &gone {
                tree.files.remove(k);
            }
            tree.dirs.retain(|d| !d.starts_with(&p));
        }
        self.notify(gone.iter().map(|k| (self.key(k), None)).collect());
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (src, dst) = (self.norm(from)?, self.norm(to)?);
        let mut events = Vec::new();
        {
            let mut tree = self.tree.lock().unwrap();
            self.parent_must_be_dir(&tree, &dst)?;
            if let Some(bytes) = tree.files.remove(&src) {
                if tree.is_dir(&self.root, &dst) {
                    tree.files.insert(src, bytes);
                    return Err(err(io::ErrorKind::IsADirectory, "target is a directory"));
                }
                events.push((self.key(&src), None));
                events.push((self.key(&dst), Some(bytes.clone())));
                tree.mark_parents(&self.root, &dst);
                tree.files.insert(dst, bytes);
            } else if tree.is_dir(&self.root, &src) {
                if src == dst {
                    return Ok(());
                }
                if tree.files.contains_key(&dst) {
                    return Err(err(io::ErrorKind::NotADirectory, "target is a file"));
                }
                if dst.starts_with(&src) {
                    return Err(err(io::ErrorKind::InvalidInput, "cannot move a directory into itself"));
                }
                let occupied = tree.files.keys().any(|k| k.starts_with(&dst))
                    || tree.dirs.iter().any(|d| d != &dst && d.starts_with(&dst));
                if occupied {
                    return Err(err(io::ErrorKind::DirectoryNotEmpty, "target directory not empty"));
                }
                let moved: Vec<PathBuf> =
                    tree.files.keys().filter(|k| k.starts_with(&src)).cloned().collect();
                for k in moved {
                    let bytes = tree.files.remove(&k).unwrap();
                    let new = dst.join(k.strip_prefix(&src).unwrap());
                    events.push((self.key(&k), None));
                    events.push((self.key(&new), Some(bytes.clone())));
                    tree.mark_parents(&self.root, &new);
                    tree.files.insert(new, bytes);
                }
                let dirs: Vec<PathBuf> =
                    tree.dirs.iter().filter(|d| d.starts_with(&src)).cloned().collect();
                for d in dirs {
                    tree.dirs.remove(&d);
                    tree.dirs.insert(dst.join(d.strip_prefix(&src).unwrap()));
                }
                tree.dirs.insert(dst);
            } else {
                return Err(err(io::ErrorKind::NotFound, "no such file or directory"));
            }
        }
        self.notify(events);
        Ok(())
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        let bytes = self.read(from)?;
        self.put(to, &bytes, false)?;
        Ok(bytes.len() as u64)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let p = self.norm(path)?;
        let tree = self.tree.lock().unwrap();
        if tree.files.contains_key(&p) {
            return Err(err(io::ErrorKind::NotADirectory, "not a directory"));
        }
        if !tree.is_dir(&self.root, &p) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        let mut found: BTreeMap<OsString, FileType> = BTreeMap::new();
        let below = tree.files.keys().map(|k| (k, true)).chain(tree.dirs.iter().map(|d| (d, false)));
        for (k, is_file) in below {
            let Ok(rel) = k.strip_prefix(&p) else { continue };
            let mut parts = rel.components();
            let Some(first) = parts.next() else { continue };
            let ty = if is_file && parts.next().is_none() {
                FileType::of_file()
            } else {
                FileType::of_dir()
            };
            found.insert(first.as_os_str().to_os_string(), ty);
        }
        Ok(found
            .into_iter()
            .map(|(name, file_type)| DirEntry { path: p.join(&name), file_name: name, file_type })
            .collect())
    }
    fn metadata(&self, path: &Path) -> io::Result<FileType> {
        let p = self.norm(path)?;
        let tree = self.tree.lock().unwrap();
        if tree.files.contains_key(&p) {
            Ok(FileType::of_file())
        } else if tree.is_dir(&self.root, &p) {
            Ok(FileType::of_dir())
        } else {
            Err(err(io::ErrorKind::NotFound, "no such file or directory"))
        }
    }
    fn write_new(&self, path: &Path, bytes: &[u8], _mode: Option<u32>) -> io::Result<()> {
        self.put(path, bytes, true)
    }
    fn create_dir_all_private(&self, path: &Path) -> io::Result<()> {
        self.create_dir_all(path)
    }
    fn is_native(&self) -> bool {
        false
    }
}

tokio::task_local! {
    static STORE: Arc<dyn Store>;
}

/// Run `f` with `store` as the filesystem behind every free function in this
/// module. Scoped to the current async TASK (`tokio::task_local!`), not the OS
/// thread, for the same reason as `cli::stdin_coord::with_route`: a
/// multi-thread runtime may resume the task's `.await` continuation on another
/// worker, and a thread-local would then vanish or read another task's store.
/// A task-local travels with the task and disappears the moment `f` resolves.
/// It does not follow `tokio::spawn`, which starts a new task.
pub async fn scope<F: Future>(store: Arc<dyn Store>, f: F) -> F::Output {
    STORE.scope(store, f).await
}

fn with_store<R>(f: impl FnOnce(&dyn Store) -> R) -> R {
    match STORE.try_with(Arc::clone) {
        Ok(s) => f(s.as_ref()),
        Err(_) => f(&NativeStore),
    }
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    with_store(|s| s.read(path.as_ref()))
}
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|_| err(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"))
}
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    with_store(|s| s.write(path.as_ref(), contents.as_ref()))
}
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.create_dir_all(path.as_ref()))
}
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.remove_file(path.as_ref()))
}
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.remove_dir(path.as_ref()))
}
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.remove_dir_all(path.as_ref()))
}
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.rename(from.as_ref(), to.as_ref()))
}
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    with_store(|s| s.copy(from.as_ref(), to.as_ref()))
}
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<DirEntry>> {
    with_store(|s| s.read_dir(path.as_ref()))
}
pub fn metadata(path: impl AsRef<Path>) -> io::Result<FileType> {
    with_store(|s| s.metadata(path.as_ref()))
}
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<FileType> {
    with_store(|s| s.symlink_metadata(path.as_ref()))
}
pub fn write_new(path: impl AsRef<Path>, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    with_store(|s| s.write_new(path.as_ref(), bytes, mode))
}
pub fn create_dir_all_private(path: impl AsRef<Path>) -> io::Result<()> {
    with_store(|s| s.create_dir_all_private(path.as_ref()))
}
pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|t| t.is_dir())
}
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|t| t.is_file())
}
pub fn is_native() -> bool {
    with_store(|s| s.is_native())
}

/// An open advisory-lock file. Always a real file: only meaningful when
/// [`is_native`], and callers skip locking otherwise.
pub type LockFile = std::fs::File;

/// Open (creating if needed) the file an OS-level lock is taken on. Native only.
pub fn open_lock_file(path: impl AsRef<Path>) -> io::Result<LockFile> {
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::sync::Mutex;

    /// Run one contract against both stores: a tempdir natively, a virtual
    /// root in memory.
    fn both(f: impl Fn(&dyn Store, &Path)) {
        let dir = tempfile::tempdir().unwrap();
        f(&NativeStore, dir.path());
        f(&MemStore::new(PathBuf::from("/rdc-mem")), Path::new("/rdc-mem"));
    }

    #[test]
    fn read_dir_contract() {
        both(|s, root| {
            assert_eq!(s.read_dir(&root.join("nope")).unwrap_err().kind(), ErrorKind::NotFound);
            s.create_dir_all(&root.join("b")).unwrap();
            s.write(&root.join("c.py"), b"x").unwrap();
            s.write(&root.join("a.json"), b"x").unwrap();
            let names: Vec<_> = s
                .read_dir(root)
                .unwrap()
                .iter()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["a.json", "b", "c.py"]);
            assert!(s.read_dir(&root.join("c.py")).is_err());
        });
    }

    #[test]
    fn rename_contract() {
        both(|s, root| {
            s.write(&root.join("a"), b"new").unwrap();
            s.write(&root.join("b"), b"old").unwrap();
            s.rename(&root.join("a"), &root.join("b")).unwrap();
            assert_eq!(s.read(&root.join("b")).unwrap(), b"new");
            assert!(s.read(&root.join("a")).is_err());

            s.create_dir_all(&root.join("d")).unwrap();
            s.write(&root.join("d/x"), b"1").unwrap();
            s.rename(&root.join("d"), &root.join("e")).unwrap();
            assert_eq!(s.read(&root.join("e/x")).unwrap(), b"1");
            assert_eq!(s.read(&root.join("d/x")).unwrap_err().kind(), ErrorKind::NotFound);
        });
    }

    #[test]
    fn binary_roundtrip() {
        both(|s, root| {
            s.write(&root.join("bin"), &[0xff, 0x00, 0xfe]).unwrap();
            assert_eq!(s.read(&root.join("bin")).unwrap(), [0xff, 0x00, 0xfe]);
        });
    }

    #[test]
    fn missing_read_is_not_found() {
        both(|s, root| {
            assert_eq!(s.read(&root.join("missing")).unwrap_err().kind(), ErrorKind::NotFound);
        });
    }

    #[test]
    fn write_new_refuses_existing() {
        both(|s, root| {
            s.write_new(&root.join("t"), b"1", Some(0o600)).unwrap();
            assert_eq!(
                s.write_new(&root.join("t"), b"2", None).unwrap_err().kind(),
                ErrorKind::AlreadyExists
            );
            assert_eq!(s.read(&root.join("t")).unwrap(), b"1");
        });
    }

    #[test]
    fn metadata_and_remove_contract() {
        both(|s, root| {
            s.create_dir_all_private(&root.join("p/q")).unwrap();
            s.write(&root.join("p/q/f"), b"x").unwrap();
            assert!(s.metadata(&root.join("p/q")).unwrap().is_dir());
            assert!(s.metadata(&root.join("p/q/f")).unwrap().is_file());
            assert!(!s.symlink_metadata(&root.join("p/q/f")).unwrap().is_symlink());
            assert!(s.remove_dir(&root.join("p/q")).is_err());
            s.remove_file(&root.join("p/q/f")).unwrap();
            s.remove_dir(&root.join("p/q")).unwrap();
            s.remove_dir_all(&root.join("p")).unwrap();
            assert_eq!(s.metadata(&root.join("p")).unwrap_err().kind(), ErrorKind::NotFound);
        });
    }

    #[cfg(unix)]
    #[test]
    fn native_keeps_symlinks_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = NativeStore;
        s.write(&dir.path().join("t"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("t"), dir.path().join("l")).unwrap();
        assert!(s.symlink_metadata(&dir.path().join("l")).unwrap().is_symlink());
        assert!(s.metadata(&dir.path().join("l")).unwrap().is_file());
        s.create_dir_all_private(&dir.path().join("sec/x")).unwrap();
        let mode = std::fs::metadata(dir.path().join("sec/x")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        s.write_new(&dir.path().join("w"), b"x", Some(0o600)).unwrap();
        let mode = std::fs::metadata(dir.path().join("w")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn mem_normalizes_paths() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        m.create_dir_all(Path::new("/rdc-mem/envs/dev")).unwrap();
        m.write(Path::new("/rdc-mem/envs/./dev/x"), b"1").unwrap();
        assert_eq!(m.read(Path::new("/rdc-mem/envs/dev/x")).unwrap(), b"1");
        assert_eq!(m.read(Path::new("/rdc-mem/envs/dev/../dev/x")).unwrap(), b"1");
    }

    #[test]
    fn mem_rejects_escape() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        let err = m.read(Path::new("/rdc-mem/../etc/x")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        let err = m.write(Path::new("/elsewhere/x"), b"").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn mem_from_files_and_files_are_root_relative() {
        let m = MemStore::from_files(
            PathBuf::from("/rdc-mem"),
            [("envs/dev/a.json".to_string(), b"1".to_vec())],
        )
        .unwrap();
        assert!(m.metadata(Path::new("/rdc-mem/envs/dev")).unwrap().is_dir());
        assert_eq!(m.files().keys().collect::<Vec<_>>(), ["envs/dev/a.json"]);
    }

    #[test]
    fn mem_from_files_rejects_escaping_keys() {
        for bad in ["../x", "a/../../x", "/etc/x"] {
            let r = MemStore::from_files(PathBuf::from("/rdc-mem"), [(bad.to_string(), vec![])]);
            assert_eq!(r.err().unwrap().kind(), ErrorKind::PermissionDenied, "{bad}");
        }
        let ok = MemStore::from_files(PathBuf::from("/rdc-mem"), [("./a/../b".to_string(), vec![1])]).unwrap();
        assert_eq!(ok.files().keys().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn dirs_outlive_their_last_file() {
        both(|s, root| {
            s.create_dir_all(&root.join("d")).unwrap();
            s.write(&root.join("d/x"), b"1").unwrap();
            s.remove_file(&root.join("d/x")).unwrap();
            s.write(&root.join("d/y"), b"2").unwrap();
            s.rename(&root.join("d/y"), &root.join("d/z")).unwrap();
            assert!(s.metadata(&root.join("d")).unwrap().is_dir());
            s.write(&root.join("d/w"), b"3").unwrap();
        });
        let m = MemStore::from_files(PathBuf::from("/rdc-mem"), [("d/x".to_string(), vec![])]).unwrap();
        m.remove_file(Path::new("/rdc-mem/d/x")).unwrap();
        m.write(Path::new("/rdc-mem/d/y"), b"").unwrap();
    }

    #[test]
    fn create_dir_all_over_a_file_leaves_nothing_behind() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        m.write(Path::new("/rdc-mem/f"), b"").unwrap();
        assert!(m.create_dir_all(Path::new("/rdc-mem/f/a/b")).is_err());
        assert!(m.metadata(Path::new("/rdc-mem/f/a")).is_err());
        assert!(m.read_dir(Path::new("/rdc-mem/f")).is_err());
    }

    #[test]
    fn write_onto_a_directory_fails() {
        both(|s, root| {
            s.create_dir_all(&root.join("d")).unwrap();
            assert!(s.write(&root.join("d"), b"x").is_err());
            assert!(s.read(&root.join("d")).is_err());
        });
    }

    #[test]
    fn rename_dir_onto_existing_dir() {
        both(|s, root| {
            s.create_dir_all(&root.join("a")).unwrap();
            s.write(&root.join("a/x"), b"1").unwrap();
            s.create_dir_all(&root.join("full")).unwrap();
            s.write(&root.join("full/y"), b"2").unwrap();
            assert!(s.rename(&root.join("a"), &root.join("full")).is_err());
            assert_eq!(s.read(&root.join("a/x")).unwrap(), b"1");
            assert_eq!(s.read(&root.join("full/y")).unwrap(), b"2");
            assert!(s.rename(&root.join("a"), &root.join("a/sub")).is_err());
            s.create_dir_all(&root.join("empty")).unwrap();
            s.rename(&root.join("a"), &root.join("empty")).unwrap();
            assert_eq!(s.read(&root.join("empty/x")).unwrap(), b"1");
        });
    }

    #[test]
    fn mem_on_change_for_dir_operations() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        let log = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let sink = log.clone();
        m.set_on_change(Box::new(move |k, v| sink.lock().unwrap().push((k.to_string(), v.is_some()))));
        m.create_dir_all(Path::new("/rdc-mem/d")).unwrap();
        m.write(Path::new("/rdc-mem/d/x"), b"1").unwrap();
        log.lock().unwrap().clear();
        m.rename(Path::new("/rdc-mem/d"), Path::new("/rdc-mem/e")).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [("d/x".to_string(), false), ("e/x".to_string(), true)]
        );
        log.lock().unwrap().clear();
        m.remove_dir_all(Path::new("/rdc-mem/e")).unwrap();
        assert_eq!(*log.lock().unwrap(), [("e/x".to_string(), false)]);
    }

    #[test]
    fn mem_norm_handles_relative_and_trailing_slash() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        m.create_dir_all(Path::new("/rdc-mem/d/")).unwrap();
        m.write(Path::new("d/x"), b"1").unwrap();
        assert_eq!(m.read(Path::new("/rdc-mem/d/x/")).unwrap(), b"1");
        assert_eq!(m.read(Path::new("d/../d/x")).unwrap(), b"1");
        assert_eq!(m.read(Path::new("../x")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn mem_on_change_reports_relative_keys() {
        let m = MemStore::new(PathBuf::from("/rdc-mem"));
        let log = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let sink = log.clone();
        m.set_on_change(Box::new(move |k, v| sink.lock().unwrap().push((k.to_string(), v.is_some()))));
        let a = Path::new("/rdc-mem/a");
        let b = Path::new("/rdc-mem/b");
        m.write(a, b"1").unwrap();
        m.rename(a, b).unwrap();
        m.remove_file(b).unwrap();
        let want = [("a", true), ("a", false), ("b", true), ("b", false)]
            .map(|(k, v)| (k.to_string(), v));
        assert_eq!(*log.lock().unwrap(), want);
    }

    #[tokio::test]
    async fn scope_routes_free_functions() {
        let mem = Arc::new(MemStore::new(PathBuf::from("/rdc-mem")));
        let dyn_mem: Arc<dyn Store> = mem.clone();
        assert!(is_native());
        scope(dyn_mem, async {
            assert!(!is_native());
            write(Path::new("/rdc-mem/in"), b"mem").unwrap();
            assert!(exists(Path::new("/rdc-mem/in")));
            assert!(is_file(Path::new("/rdc-mem/in")));
            assert!(!is_dir(Path::new("/rdc-mem/in")));
        })
        .await;
        assert_eq!(mem.files().get("in").map(Vec::as_slice), Some(&b"mem"[..]));

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out");
        write(&p, b"disk").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"disk");
        assert_eq!(read_to_string(&p).unwrap(), "disk");
        assert_eq!(mem.files().len(), 1);
    }
}
