//! Test helpers for every crate: [`MemFS`], an in-memory writable file
//! system standing in for servers, and [`check_filesystem`], the contract
//! every [`FileSystem`] backend should pass. Port of `testing.py`.
//!
//! ```ignore
//! let root = VPath::new(Arc::new(MyFs::new(...)), "/some/empty/dir");
//! oc_vfs::testing::check_filesystem(&root);          // writable: writes the sample tree itself
//! ```
//!
//! The checks only use the generic interface, so they hold for any backend.
//! They panic (like `assert!`) with a message naming the broken rule.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, WalkIter, WriteStream,
    sort_entries,
};

// ---------------------------------------------------------------------------
// MemFS
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Node {
    Dir,
    File(Arc<[u8]>),
}

#[derive(Clone, Debug)]
struct Item {
    node: Node,
    mtime: f64,
}

/// Failure injection, so callers can test their error paths (partial-file
/// cleanup, servers refusing renames) without a flaky real backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Writes fail once a single stream has received this many bytes.
    pub write_limit: Option<usize>,
    /// `rename` always fails (like servers that refuse renames).
    pub rename_fails: bool,
}

#[derive(Debug)]
struct State {
    /// POSIX-style paths without leading slash; "" is the root (always a dir).
    /// A BTreeMap keeps a directory's descendants contiguous (`dir/` prefix)
    /// and parents before their children, which `walk` relies on.
    items: BTreeMap<String, Item>,
    trashed: Vec<String>,
    faults: Faults,
}

/// A thread-safe, in-memory file system. Paths are POSIX paths without a
/// leading slash, "" is the root; `display` shows them as `mem:/path`.
///
/// Like a local disk, `open_write` creates (truncates) the file immediately
/// and `finish` commits the contents, so a failed copy leaves something
/// behind that the copier must clean up — exactly what real backends do.
#[derive(Debug)]
pub struct MemFS {
    /// Shared with open writers, which commit after `open_write` returned.
    state: Arc<Mutex<State>>,
    caps: Capabilities,
}

impl Default for MemFS {
    fn default() -> Self {
        Self::new()
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // A panicking test thread must not poison other tests' views.
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn parent_of(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rfind('/').map_or("", |i| &path[..i]))
}

fn base_of(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

fn not_found(path: &str) -> VfsError {
    VfsError::NotFound(format!("mem:/{path}"))
}

fn io_err(kind: io::ErrorKind, msg: String) -> VfsError {
    VfsError::Io(io::Error::new(kind, msg))
}

/// `dir/` (or "" for the root): every descendant of `dir` starts with it.
fn prefix_of(dir: &str) -> String {
    if dir.is_empty() { String::new() } else { format!("{dir}/") }
}

impl MemFS {
    /// Writable, can rename, no trash.
    pub fn new() -> Self {
        Self::with_capabilities(Capabilities { writable: true, can_trash: false, can_rename: true, watchable: false })
    }

    /// With explicit capability flags (e.g. read-only, or with a trash).
    /// The flags are enforced: writes on a non-writable MemFS fail.
    pub fn with_capabilities(caps: Capabilities) -> Self {
        let mut items = BTreeMap::new();
        items.insert(String::new(), Item { node: Node::Dir, mtime: 0.0 });
        MemFS { state: Arc::new(Mutex::new(State { items, trashed: Vec::new(), faults: Faults::default() })), caps }
    }

    /// Convenience: wrap in an `Arc` and return the root `VPath`.
    pub fn root(self) -> VPath {
        VPath::new(Arc::new(self), "")
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    pub fn set_faults(&self, faults: Faults) {
        self.lock().faults = faults;
    }

    /// Put a file in place regardless of capabilities, creating parents
    /// (replacing whatever was at `path`, including a directory's contents).
    pub fn add_file(&self, path: &str, data: impl Into<Vec<u8>>) {
        assert!(!path.is_empty(), "MemFS: the root is always a directory");
        let mut st = self.lock();
        Self::remove_tree(&mut st, path);
        Self::make_parents(&mut st, path);
        st.items.insert(path.to_string(), Item { node: Node::File(data.into().into()), mtime: now() });
    }

    /// Create a directory (and its parents) regardless of capabilities.
    pub fn add_dir(&self, path: &str) {
        let mut st = self.lock();
        Self::make_parents(&mut st, path);
        st.items.entry(path.to_string()).or_insert(Item { node: Node::Dir, mtime: now() });
    }

    /// Contents of a file, if there is one at `path`.
    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        match &self.lock().items.get(path)?.node {
            Node::File(data) => Some(data.to_vec()),
            Node::Dir => None,
        }
    }

    pub fn is_file(&self, path: &str) -> bool {
        matches!(self.lock().items.get(path), Some(Item { node: Node::File(_), .. }))
    }

    pub fn contains(&self, path: &str) -> bool {
        self.lock().items.contains_key(path)
    }

    /// Every path except the root, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.lock().items.keys().filter(|k| !k.is_empty()).cloned().collect()
    }

    /// Paths passed to `trash`, in order.
    pub fn trashed(&self) -> Vec<String> {
        self.lock().trashed.clone()
    }

    fn make_parents(st: &mut State, path: &str) {
        let mut p = parent_of(path);
        while let Some(dir) = p {
            if st.items.contains_key(dir) {
                break;
            }
            st.items.insert(dir.to_string(), Item { node: Node::Dir, mtime: now() });
            p = parent_of(dir);
        }
    }

    fn check_writable(&self) -> Result<()> {
        if self.caps.writable { Ok(()) } else { Err(VfsError::ReadOnly(self.read_only_reason())) }
    }

    fn entry(path: &str, item: &Item) -> Entry {
        match &item.node {
            Node::Dir => Entry::dir(base_of(path), item.mtime),
            Node::File(data) => Entry::file(base_of(path), data.len() as u64, item.mtime),
        }
    }

    fn remove_tree(st: &mut State, path: &str) {
        st.items.remove(path);
        let prefix = prefix_of(path);
        let doomed: Vec<String> = st.items.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
        for k in doomed {
            st.items.remove(&k);
        }
    }
}

/// Read stream sharing the stored bytes instead of copying them.
struct MemReader(io::Cursor<Arc<[u8]>>);

impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for MemReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl ReadStream for MemReader {}

struct MemWriter {
    state: Arc<Mutex<State>>,
    path: String,
    buf: Vec<u8>,
    limit: Option<usize>,
}

impl Write for MemWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.limit.is_some_and(|limit| self.buf.len() + data.len() > limit) {
            return Err(io::Error::other("simulated write failure"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteStream for MemWriter {
    fn finish(self: Box<Self>) -> Result<()> {
        let mut st = lock(&self.state);
        match st.items.get_mut(&self.path) {
            Some(Item { node: node @ Node::File(_), mtime }) => {
                *node = Node::File(self.buf.into());
                *mtime = now();
                Ok(())
            }
            // Removed (or replaced by a directory) while being written.
            _ => Err(not_found(&self.path)),
        }
    }
}

impl FileSystem for MemFS {
    fn type_name(&self) -> String {
        "memory".into()
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let st = self.lock();
        match st.items.get(path) {
            None => return Err(not_found(path)),
            Some(Item { node: Node::File(_), .. }) => {
                return Err(io_err(io::ErrorKind::NotADirectory, format!("not a directory: mem:/{path}")));
            }
            Some(_) => {}
        }
        let prefix = prefix_of(path);
        let entries = st
            .items
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k.is_empty() && !k[prefix.len()..].contains('/'))
            .map(|(k, item)| Self::entry(k, item))
            .collect();
        Ok(sort_entries(entries, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        self.lock().items.get(path).map(|item| Self::entry(path, item)).ok_or_else(|| not_found(path))
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        parent_of(path).map(str::to_string)
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut out = String::from(path);
        for name in names.iter().filter(|n| !n.is_empty()) {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(name);
        }
        out
    }

    fn basename(&self, path: &str) -> String {
        base_of(path).to_string()
    }

    fn display(&self, path: &str) -> String {
        format!("mem:/{path}")
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        match &self.lock().items.get(path).ok_or_else(|| not_found(path))?.node {
            Node::File(data) => Ok(Box::new(MemReader(io::Cursor::new(data.clone())))),
            Node::Dir => Err(io_err(io::ErrorKind::IsADirectory, format!("is a directory: mem:/{path}"))),
        }
    }

    fn walk<'a>(&'a self, path: &str) -> Result<WalkIter<'a>> {
        let st = self.lock();
        let root = st.items.get(path).ok_or_else(|| not_found(path))?;
        let mut out = vec![Ok((RelPath::new(), Self::entry(path, root)))];
        if matches!(root.node, Node::Dir) {
            let prefix = prefix_of(path);
            out.extend(
                st.items
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .filter(|(k, _)| !k.is_empty())
                    .map(|(k, item)| Ok((k[prefix.len()..].split('/').map(str::to_string).collect(), Self::entry(k, item)))),
            );
        }
        Ok(Box::new(out.into_iter()))
    }

    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        self.check_writable()?;
        let mut st = self.lock();
        match st.items.get(path) {
            Some(Item { node: Node::Dir, .. }) if exist_ok => return Ok(()),
            Some(_) => return Err(VfsError::AlreadyExists(format!("mem:/{path}"))),
            None => {}
        }
        // Missing parents are created; a file in the way is an error.
        let mut missing = vec![path.to_string()];
        let mut p = parent_of(path);
        while let Some(dir) = p {
            match st.items.get(dir) {
                Some(Item { node: Node::Dir, .. }) => break,
                Some(_) => return Err(io_err(io::ErrorKind::NotADirectory, format!("not a directory: mem:/{dir}"))),
                None => missing.push(dir.to_string()),
            }
            p = parent_of(dir);
        }
        let t = now();
        for dir in missing {
            st.items.insert(dir, Item { node: Node::Dir, mtime: t });
        }
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        self.check_writable()?;
        if path.is_empty() {
            return Err(VfsError::Operation("Cannot remove the root of mem:/".into()));
        }
        let mut st = self.lock();
        if !st.items.contains_key(path) {
            return Err(not_found(path));
        }
        Self::remove_tree(&mut st, path);
        Ok(())
    }

    fn trash(&self, paths: &[String]) -> Result<()> {
        if !self.caps.can_trash {
            return Err(VfsError::ReadOnly(self.read_only_reason()));
        }
        let mut st = self.lock();
        for path in paths {
            if !st.items.contains_key(path.as_str()) || path.is_empty() {
                return Err(not_found(path));
            }
            Self::remove_tree(&mut st, path);
            st.trashed.push(path.clone());
        }
        Ok(())
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        if !self.caps.can_rename {
            return Err(VfsError::ReadOnly(self.read_only_reason()));
        }
        self.check_writable()?;
        let mut st = self.lock();
        if st.faults.rename_fails {
            return Err(io_err(io::ErrorKind::PermissionDenied, "simulated rename failure".into()));
        }
        if src.is_empty() || !st.items.contains_key(src) {
            return Err(not_found(src));
        }
        if st.items.contains_key(dst) {
            return Err(VfsError::AlreadyExists(format!("mem:/{dst}")));
        }
        if dst.starts_with(&prefix_of(src)) {
            return Err(io_err(io::ErrorKind::InvalidInput, format!("cannot move mem:/{src} into itself")));
        }
        match parent_of(dst).and_then(|p| st.items.get(p)) {
            Some(Item { node: Node::Dir, .. }) => {}
            _ => return Err(not_found(dst)),
        }
        let src_prefix = prefix_of(src);
        let moved: Vec<String> =
            st.items.range(src_prefix.clone()..).take_while(|(k, _)| k.starts_with(&src_prefix)).map(|(k, _)| k.clone()).collect();
        let root = st.items.remove(src).expect("checked above");
        st.items.insert(dst.to_string(), root);
        for old in moved {
            let item = st.items.remove(&old).expect("listed above");
            st.items.insert(format!("{dst}/{}", &old[src_prefix.len()..]), item);
        }
        Ok(())
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn WriteStream>> {
        self.check_writable()?;
        let limit = {
            let mut st = self.lock();
            if path.is_empty() || matches!(st.items.get(path), Some(Item { node: Node::Dir, .. })) {
                return Err(io_err(io::ErrorKind::IsADirectory, format!("is a directory: mem:/{path}")));
            }
            match parent_of(path).and_then(|p| st.items.get(p)) {
                Some(Item { node: Node::Dir, .. }) => {}
                _ => return Err(not_found(path)),
            }
            // Truncate now, like `open(path, "wb")`.
            st.items.insert(path.to_string(), Item { node: Node::File(Arc::from(&[][..])), mtime: now() });
            st.faults.write_limit
        };
        Ok(Box::new(MemWriter { state: self.state.clone(), path: path.to_string(), buf: Vec::new(), limit }))
    }

    fn set_mtime(&self, path: &str, mtime: f64) -> Result<()> {
        self.check_writable()?;
        let mut st = self.lock();
        st.items.get_mut(path).ok_or_else(|| not_found(path))?.mtime = mtime;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------

/// The sample tree: relative path -> contents; directories are implied.
/// `docs/deep/data.bin` (76.8 KB) takes more than one read chunk on most backends.
pub fn sample_tree() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("hello.txt", b"hello world\n".to_vec()),
        ("empty.bin", Vec::new()),
        ("docs/readme.md", b"# Readme\n".to_vec()),
        ("docs/deep/data.bin", (0..=255u8).collect::<Vec<_>>().repeat(300)),
        (".hidden", b"dot file\n".to_vec()),
    ]
}

/// Create `tree` below `root` using only the file system's write API.
pub fn write_tree(root: &VPath, tree: &[(&str, Vec<u8>)]) -> Result<()> {
    for (rel, data) in tree {
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.len() > 1 {
            root.fs.mkdir(&root.child(&parts[..parts.len() - 1]).path, true)?;
        }
        let mut w = root.fs.open_write(&root.child(&parts).path)?;
        w.write_all(data)?;
        w.finish()?;
    }
    Ok(())
}

/// Run the whole contract with [`sample_tree`]. A writable file system's
/// `root` must be an empty directory (the tree is written into it); a
/// read-only one must already contain the sample tree.
pub fn check_filesystem(root: &VPath) {
    let writable = root.fs.capabilities().writable;
    check_filesystem_with(root, writable, &sample_tree());
}

/// [`check_filesystem`] with an explicit expectation and tree.
pub fn check_filesystem_with(root: &VPath, writable: bool, tree: &[(&str, Vec<u8>)]) {
    let fs = &root.fs;
    assert_eq!(fs.capabilities().writable, writable, "{}: writable should be {writable}", fs.type_name());
    if writable {
        write_tree(root, tree).unwrap_or_else(|e| panic!("{}: writing the sample tree: {e}", fs.type_name()));
    }
    check_reading(root, tree);
    if writable {
        check_writing(root);
    } else {
        check_read_only(root);
    }
}

/// A private, self-deleting cache directory for `materialize` checks, so the
/// contract never touches the user's real cache.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oc-vfs-contract-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        ScratchDir(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_all_chunked(stream: &mut dyn Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 10_000];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

pub fn check_reading(root: &VPath, tree: &[(&str, Vec<u8>)]) {
    let fs = &root.fs;
    let tn = fs.type_name();
    assert!(root.is_dir() && root.exists(), "{tn}: root must be an existing directory");

    // Listing: names, kinds, sizes; hidden entries filtered on request.
    let top_files: Vec<&str> = tree.iter().map(|(r, _)| *r).filter(|r| !r.contains('/')).collect();
    let mut top_dirs: Vec<&str> = tree.iter().filter_map(|(r, _)| r.split_once('/').map(|(d, _)| d)).collect();
    top_dirs.sort_unstable();
    top_dirs.dedup();
    let all = ListOptions { show_hidden: true, ..ListOptions::default() };
    let listing = root.list_dir(all).unwrap_or_else(|e| panic!("{tn}: list_dir: {e}"));
    let mut names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = top_files.iter().chain(&top_dirs).copied().collect();
    expected.sort_unstable();
    assert_eq!(names, expected, "{tn}: listing");
    for e in &listing {
        if top_dirs.contains(&e.name.as_str()) {
            assert!(e.is_dir, "{tn}: {} should be a directory", e.name);
        } else {
            let data = &tree.iter().find(|(r, _)| *r == e.name).expect("listed above").1;
            assert!(!e.is_dir && e.size == data.len() as u64, "{tn}: {}: size", e.name);
        }
    }
    let visible = root.list_dir(ListOptions::default()).unwrap_or_else(|e| panic!("{tn}: list_dir: {e}"));
    assert!(visible.iter().all(|e| !e.name.starts_with('.')), "{tn}: hidden entries listed with show_hidden=false");
    assert!(listing.windows(2).all(|w| w[0].is_dir || !w[1].is_dir), "{tn}: dirs first");

    let cache = ScratchDir::new();
    for (rel, data) in tree {
        let parts: Vec<&str> = rel.split('/').collect();
        let path = root.child(&parts);
        // Paths: join / parent / basename agree.
        assert_eq!(path.name(), *parts.last().expect("non-empty"), "{tn}: basename of {rel}");
        let expected_parent = if parts.len() > 1 { root.child(&parts[..parts.len() - 1]) } else { root.clone() };
        assert_eq!(path.parent().as_ref(), Some(&expected_parent), "{tn}: parent of {rel}");
        assert!(!path.display().is_empty(), "{tn}: display of {rel}");
        // Metadata.
        let entry = path.stat().unwrap_or_else(|e| panic!("{tn}: stat {rel}: {e}"));
        assert!(path.exists() && !path.is_dir() && entry.size == data.len() as u64, "{tn}: stat {rel}");
        // Contents: streamed reads (in several chunks) and a local copy.
        let mut stream = fs.open_read(&path.path).unwrap_or_else(|e| panic!("{tn}: open_read {rel}: {e}"));
        assert!(read_all_chunked(&mut stream) == *data, "{tn}: contents of {rel}");
        let local = crate::ops::materialize(&path, &cache.0).unwrap_or_else(|e| panic!("{tn}: materialize {rel}: {e}"));
        assert!(std::fs::read(&local).expect("read materialized file") == *data, "{tn}: materialize {rel}");
    }

    let missing = root.child(&["does-not-exist"]);
    assert!(!missing.exists() && !missing.is_dir(), "{tn}: missing path exists");
    match missing.stat() {
        Err(e) if e.is_not_found() => {}
        other => panic!("{tn}: stat() of a missing path must fail with NotFound, got {other:?}"),
    }

    // walk(): the start first, parents before children, every file once.
    if top_dirs.contains(&"docs") {
        let docs = root.child(&["docs"]);
        let walked: Vec<(RelPath, Entry)> = fs
            .walk(&docs.path)
            .unwrap_or_else(|e| panic!("{tn}: walk: {e}"))
            .collect::<Result<_>>()
            .unwrap_or_else(|e| panic!("{tn}: walk: {e}"));
        assert!(walked[0].0.is_empty() && walked[0].1.is_dir, "{tn}: walk starts with the start");
        let mut seen_dirs: Vec<&[String]> = vec![&[]];
        for (rel, entry) in &walked[1..] {
            assert!(!rel.is_empty(), "{tn}: walk yields the start only once");
            assert!(seen_dirs.contains(&&rel[..rel.len() - 1]), "{tn}: walk: {rel:?} before its parent");
            if entry.is_dir {
                seen_dirs.push(rel);
            }
        }
        let mut walked_files: Vec<String> =
            walked.iter().filter(|(_, e)| !e.is_dir).map(|(r, _)| format!("docs/{}", r.join("/"))).collect();
        walked_files.sort_unstable();
        let mut expected: Vec<String> = tree.iter().map(|(r, _)| *r).filter(|r| r.starts_with("docs/")).map(String::from).collect();
        expected.sort_unstable();
        assert_eq!(walked_files, expected, "{tn}: walk files");
    }
}

pub fn check_writing(root: &VPath) {
    let fs = &root.fs;
    let tn = fs.type_name();
    let new_dir = root.child(&["new", "nested"]);
    fs.mkdir(&new_dir.path, false).unwrap_or_else(|e| panic!("{tn}: mkdir with missing parents: {e}"));
    assert!(new_dir.is_dir(), "{tn}: mkdir");
    fs.mkdir(&new_dir.path, true).unwrap_or_else(|e| panic!("{tn}: mkdir exist_ok: {e}"));
    match fs.mkdir(&new_dir.path, false) {
        Err(VfsError::AlreadyExists(_)) => {}
        Err(VfsError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {}
        other => panic!("{tn}: mkdir of an existing directory must fail with AlreadyExists, got {other:?}"),
    }

    let target = new_dir.child(&["file.bin"]);
    let payload: Vec<u8> = (0..32_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let write = |data: &[u8]| {
        let mut w = fs.open_write(&target.path).unwrap_or_else(|e| panic!("{tn}: open_write: {e}"));
        w.write_all(data).unwrap_or_else(|e| panic!("{tn}: write: {e}"));
        w.finish().unwrap_or_else(|e| panic!("{tn}: finish: {e}"));
    };
    write(&payload);
    let mut r = fs.open_read(&target.path).unwrap_or_else(|e| panic!("{tn}: open_read: {e}"));
    assert!(read_all_chunked(&mut r) == payload, "{tn}: written contents");
    drop(r);
    write(b"short"); // overwrite truncates
    assert_eq!(target.stat().map(|e| e.size).ok(), Some(5), "{tn}: overwrite must truncate");

    let before = target.stat().map(|e| e.mtime).unwrap_or(0.0);
    fs.set_mtime(&target.path, 1_600_000_000.0).unwrap_or_else(|e| panic!("{tn}: set_mtime: {e}"));
    let mtime = target.stat().map(|e| e.mtime).unwrap_or(0.0);
    // Best effort: not every server can set it.
    assert!(mtime == 0.0 || mtime == before || (mtime - 1_600_000_000.0).abs() < 2.0, "{tn}: set_mtime: {mtime}");

    if fs.capabilities().can_rename {
        let renamed = root.child(&["new", "renamed.bin"]);
        fs.rename(&target.path, &renamed.path).unwrap_or_else(|e| panic!("{tn}: rename: {e}"));
        assert!(renamed.exists() && !target.exists(), "{tn}: rename");
    }

    fs.remove(&root.child(&["new"]).path).unwrap_or_else(|e| panic!("{tn}: remove directory: {e}"));
    assert!(!root.child(&["new"]).exists(), "{tn}: remove directory");
    let hello = root.child(&["hello.txt"]);
    if hello.exists() {
        fs.remove(&hello.path).unwrap_or_else(|e| panic!("{tn}: remove file: {e}"));
        assert!(!hello.exists(), "{tn}: remove file");
    }
}

pub fn check_read_only(root: &VPath) {
    let fs = &root.fs;
    let tn = fs.type_name();
    let x = root.child(&["x"]).path;
    let hello = root.child(&["hello.txt"]).path;
    let attempts: [(&str, Result<()>); 3] = [
        ("mkdir", fs.mkdir(&x, false)),
        ("open_write", fs.open_write(&x).map(|_| ())),
        ("remove", fs.remove(&hello)),
    ];
    for (what, result) in attempts {
        assert!(
            matches!(result, Err(VfsError::ReadOnly(_) | VfsError::Operation(_))),
            "{tn}: {what} on a read-only file system must fail with ReadOnly, got {result:?}"
        );
    }
}
