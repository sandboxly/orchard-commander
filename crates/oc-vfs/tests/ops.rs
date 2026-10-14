//! Cross-file-system operations (port of tests/test_vfs.py, minus the parts
//! that need the local disk or archives — those live in oc-local/oc-archive)
//! plus the contract run against MemFS.

use std::cell::Cell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use oc_vfs::ops;
use oc_vfs::testing::{self, Faults, MemFS};
use oc_vfs::{
    Capabilities, Conflict, Entry, FileSystem, ListOptions, Progress, ReadStream, RelPath, Result, TransferCallbacks,
    VPath, VfsError,
};

fn mem() -> (Arc<MemFS>, VPath) {
    let fs = Arc::new(MemFS::new());
    let root = VPath::new(fs.clone(), "");
    (fs, root)
}

/// The Python suite's `tree` fixture: src/a.txt, src/b.bin, src/sub/c.txt.
fn source_tree() -> (Arc<MemFS>, VPath) {
    let (fs, root) = mem();
    fs.add_file("src/a.txt", "alpha");
    fs.add_file("src/b.bin", vec![0u8, 1, 2]);
    fs.add_file("src/sub/c.txt", "gamma");
    (fs, root)
}

fn vp(fs: &Arc<MemFS>, path: &str) -> VPath {
    VPath::new(fs.clone(), path)
}

fn read_only_fs() -> Arc<MemFS> {
    Arc::new(MemFS::with_capabilities(Capabilities::default()))
}

/// Run `f` with callbacks answering conflicts with `answer` (panicking if
/// `answer` is None), recording progress.
fn run(
    answer: Option<Conflict>,
    f: impl FnOnce(TransferCallbacks<'_>) -> Result<()>,
) -> (Result<()>, Vec<Progress>, Vec<(String, String)>) {
    let mut progress = Vec::new();
    let mut conflicts = Vec::new();
    let mut on_conflict = |s: &str, d: &str| {
        conflicts.push((s.to_string(), d.to_string()));
        answer.unwrap_or_else(|| panic!("unexpected conflict {s} -> {d}"))
    };
    let mut on_progress = |p: &Progress| progress.push(p.clone());
    let result = f(TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &|| false });
    (result, progress, conflicts)
}

fn copy(sources: &[VPath], target: &VPath, answer: Option<Conflict>) -> Result<()> {
    run(answer, |cb| ops::copy(sources, target, cb)).0
}

fn move_to(sources: &[VPath], target: &VPath, answer: Option<Conflict>) -> Result<()> {
    run(answer, |cb| ops::move_to(sources, target, cb)).0
}

// -- capabilities --------------------------------------------------------------

#[test]
fn read_only_targets_and_sources_are_refused() {
    let ro = read_only_fs();
    ro.add_file("top.txt", "top level");
    let ro_root = vp(&ro, "");
    let (_, src) = source_tree();

    let err = copy(&[src.child(&["src", "a.txt"])], &ro_root, None).unwrap_err();
    assert!(matches!(&err, VfsError::ReadOnly(m) if m == "memory is read-only"), "{err:?}");
    let (_, dst) = mem();
    assert!(matches!(move_to(&[ro_root.child(&["top.txt"])], &dst, None), Err(VfsError::ReadOnly(_))));
    assert!(matches!(ops::trash(&[ro_root.child(&["top.txt"])]), Err(VfsError::ReadOnly(_))));
    assert!(matches!(ops::mkdir(&ro_root, "new"), Err(VfsError::ReadOnly(_))));
    assert!(matches!(ops::delete(&[ro_root.child(&["top.txt"])]), Err(VfsError::ReadOnly(_))));
    assert!(ro.is_file("top.txt"));
    assert!(dst.list_dir(ListOptions::default()).unwrap().is_empty());
}

// -- copying ---------------------------------------------------------------------

#[test]
fn generic_copy_into_another_backend() {
    let (_, a) = source_tree();
    let other = Arc::new(MemFS::new());
    other.add_file("docs/readme.md", "# Hello\n");
    let (target_fs, target) = mem();
    let (result, progress, _) =
        run(None, |cb| ops::copy(&[a.child(&["src"]), vp(&other, "docs")], &target, cb));
    result.unwrap();
    assert_eq!(target_fs.read_file("src/sub/c.txt").unwrap(), b"gamma");
    assert_eq!(target_fs.read_file("src/b.bin").unwrap(), [0, 1, 2]);
    assert_eq!(target_fs.read_file("docs/readme.md").unwrap(), b"# Hello\n");
    assert!(target.child(&["src", "sub"]).is_dir());
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.files_total), (4, 4));
    assert_eq!(last.bytes_done, last.bytes_total);
    assert_eq!(last.bytes_total, 5 + 3 + 5 + 8);
    assert!(progress.iter().any(|p| p.current == "mem:/src/sub/c.txt"));
}

#[test]
fn copy_preserves_mtime() {
    let (fs, root) = source_tree();
    fs.set_mtime("src/a.txt", 1_500_000_000.0).unwrap();
    let (dst_fs, dst) = mem();
    copy(&[root.child(&["src", "a.txt"])], &dst, None).unwrap();
    assert_eq!(dst_fs.stat("a.txt").unwrap().mtime, 1_500_000_000.0);
}

#[test]
fn generic_move_and_conflicts() {
    let (src_fs, src) = source_tree();
    let (mem_fs, target) = mem();
    mem_fs.add_file("a.txt", "old");
    let a = src.child(&["src", "a.txt"]);

    let (result, _, conflicts) = run(Some(Conflict::Skip), |cb| ops::copy(std::slice::from_ref(&a), &target, cb));
    result.unwrap();
    assert_eq!(conflicts, [("mem:/src/a.txt".to_string(), "mem:/a.txt".to_string())]);
    assert_eq!(mem_fs.read_file("a.txt").unwrap(), b"old");

    copy(std::slice::from_ref(&a), &target, Some(Conflict::Overwrite)).unwrap();
    assert_eq!(mem_fs.read_file("a.txt").unwrap(), b"alpha");

    let (dst_fs, dst) = mem();
    dst_fs.add_dir("dst");
    move_to(&[vp(&mem_fs, "a.txt")], &dst.child(&["dst"]), None).unwrap();
    assert_eq!(dst_fs.read_file("dst/a.txt").unwrap(), b"alpha");
    assert!(!mem_fs.contains("a.txt"));
    assert!(src_fs.contains("src/a.txt"), "copy keeps the source");
}

#[test]
fn conflict_cancel_stops_everything() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("a.txt", "old");
    let err = copy(&[src.child(&["src", "a.txt"]), src.child(&["src", "b.bin"])], &dst, Some(Conflict::Cancel));
    assert!(matches!(err, Err(VfsError::Cancelled)));
    assert_eq!(dst_fs.read_file("a.txt").unwrap(), b"old");
    assert!(!dst_fs.contains("b.bin"));
}

#[test]
fn directories_merge_without_asking() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("src/keep.txt", "keep");
    copy(&[src.child(&["src"])], &dst, None).unwrap();
    assert_eq!(dst_fs.read_file("src/keep.txt").unwrap(), b"keep");
    assert_eq!(dst_fs.read_file("src/sub/c.txt").unwrap(), b"gamma");
}

#[test]
fn skipped_directory_skips_its_contents_and_counts_progress() {
    // A file where the source has a directory: skipping it skips the subtree.
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("src/sub", "a file in the way");
    let (result, progress, conflicts) = run(Some(Conflict::Skip), |cb| ops::copy(&[src.child(&["src"])], &dst, cb));
    result.unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(dst_fs.read_file("src/sub").unwrap(), b"a file in the way");
    assert!(dst_fs.is_file("src/a.txt"));
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.bytes_done), (last.files_total, last.bytes_total));
}

#[test]
fn overwriting_a_directory_with_a_file_replaces_it() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("a.txt/inner", "x");
    copy(&[src.child(&["src", "a.txt"])], &dst, Some(Conflict::Overwrite)).unwrap();
    assert_eq!(dst_fs.read_file("a.txt").unwrap(), b"alpha");
    assert!(!dst_fs.contains("a.txt/inner"));
}

#[test]
fn target_semantics() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    // A single source to a missing name is written under that name.
    copy(&[src.child(&["src", "a.txt"])], &dst.child(&["renamed.txt"]), None).unwrap();
    assert_eq!(dst_fs.read_file("renamed.txt").unwrap(), b"alpha");
    copy(&[src.child(&["src"])], &dst.child(&["tree"]), None).unwrap();
    assert_eq!(dst_fs.read_file("tree/sub/c.txt").unwrap(), b"gamma");
    // Several sources need an existing directory.
    let err = copy(&[src.child(&["src", "a.txt"]), src.child(&["src", "b.bin"])], &dst.child(&["nope"]), None)
        .unwrap_err();
    assert_eq!(err.to_string(), "Destination folder mem:/nope doesn't exist");
}

#[test]
fn copying_into_itself_is_refused() {
    let (fs, root) = source_tree();
    let err = copy(&[root.child(&["src"])], &root.child(&["src", "sub"]), None).unwrap_err();
    assert_eq!(err.to_string(), "Can't copy or move 'src' into itself");
    let err = move_to(&[root.child(&["src"])], &root, None).unwrap_err();
    assert_eq!(err.to_string(), "Can't copy or move 'src' into itself");
    let err = copy(&[root.child(&["src", "a.txt"])], &root.child(&["src"]), Some(Conflict::Overwrite)).unwrap_err();
    assert_eq!(err.to_string(), "Source and destination are the same file: mem:/src/a.txt");
    let err = move_to(&[root.child(&["src", "a.txt"])], &root.child(&["src"]), Some(Conflict::Overwrite)).unwrap_err();
    assert_eq!(err.to_string(), "Source and destination are the same: mem:/src/a.txt");
    assert_eq!(fs.read_file("src/a.txt").unwrap(), b"alpha");
    // A sibling whose name merely starts with the source's is fine.
    fs.add_dir("src2");
    copy(&[root.child(&["src", "sub"])], &root.child(&["src2"]), None).unwrap();
    assert!(fs.is_file("src2/sub/c.txt"));
}

#[test]
fn same_fs_move_uses_rename() {
    let (fs, root) = source_tree();
    fs.add_dir("dst");
    let (result, progress, _) = run(None, |cb| ops::move_to(&[root.child(&["src"])], &root.child(&["dst"]), cb));
    result.unwrap();
    assert!(fs.is_file("dst/src/sub/c.txt") && !fs.contains("src"));
    assert!(progress.is_empty(), "a rename streams nothing");
}

#[test]
fn same_fs_move_falls_back_when_rename_fails() {
    let (fs, root) = source_tree();
    fs.add_dir("dst");
    fs.set_faults(Faults { rename_fails: true, ..Faults::default() });
    move_to(&[root.child(&["src"])], &root.child(&["dst"]), None).unwrap();
    assert_eq!(fs.read_file("dst/src/sub/c.txt").unwrap(), b"gamma");
    assert!(!fs.contains("src"));
}

#[test]
fn same_fs_move_onto_existing_merges_by_copying() {
    let (fs, root) = source_tree();
    fs.add_file("dst/src/a.txt", "old");
    move_to(&[root.child(&["src"])], &root.child(&["dst"]), Some(Conflict::Overwrite)).unwrap();
    assert_eq!(fs.read_file("dst/src/a.txt").unwrap(), b"alpha");
    assert!(!fs.contains("src"));
}

#[test]
fn move_keeps_skipped_items_in_the_source() {
    let (src_fs, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("src/sub/c.txt", "old");
    move_to(&[src.child(&["src"])], &dst, Some(Conflict::Skip)).unwrap();
    assert_eq!(dst_fs.read_file("src/a.txt").unwrap(), b"alpha");
    assert_eq!(dst_fs.read_file("src/sub/c.txt").unwrap(), b"old");
    assert_eq!(src_fs.paths(), ["src", "src/sub", "src/sub/c.txt"]);
}

#[test]
fn partial_file_is_removed_when_writing_fails() {
    let (src_fs, src) = mem();
    src_fs.add_file("big.bin", vec![7u8; 3 * oc_vfs::CHUNK]);
    let (dst_fs, dst) = mem();
    dst_fs.set_faults(Faults { write_limit: Some(oc_vfs::CHUNK + 10), ..Faults::default() });
    let err = copy(&[src.child(&["big.bin"])], &dst, None).unwrap_err();
    assert!(matches!(err, VfsError::Io(_)), "{err:?}");
    assert!(!dst_fs.contains("big.bin"));
}

#[test]
fn cancelling_mid_file_removes_the_partial_file() {
    let (src_fs, src) = mem();
    src_fs.add_file("big.bin", vec![1u8; 3 * oc_vfs::CHUNK]);
    let (dst_fs, dst) = mem();
    let chunks = Cell::new(0);
    let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
    let mut on_progress = |p: &Progress| {
        if p.bytes_done > 0 {
            chunks.set(chunks.get() + 1);
        }
    };
    let is_cancelled = || chunks.get() >= 1;
    let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &is_cancelled };
    assert!(matches!(ops::copy(&[src.child(&["big.bin"])], &dst, cb), Err(VfsError::Cancelled)));
    assert!(!dst_fs.contains("big.bin"));
    assert!(src_fs.is_file("big.bin"));
}

#[test]
fn cancelled_before_start_writes_nothing() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
    let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: None, is_cancelled: &|| true };
    assert!(matches!(ops::copy(&[src.child(&["src"])], &dst, cb), Err(VfsError::Cancelled)));
    assert!(dst_fs.paths().is_empty());
}

#[test]
fn missing_source_fails_before_copying_anything() {
    let (_, src) = source_tree();
    let (dst_fs, dst) = mem();
    let err = copy(&[src.child(&["src", "a.txt"]), src.child(&["gone"])], &dst, None).unwrap_err();
    assert!(err.is_not_found());
    assert!(dst_fs.paths().is_empty());
}

// -- delete / trash / mkdir / measure -------------------------------------------

#[test]
fn delete_and_mkdir() {
    let (fs, root) = source_tree();
    ops::delete(&[root.child(&["src", "sub"]), root.child(&["src", "a.txt"])]).unwrap();
    assert_eq!(fs.paths(), ["src", "src/b.bin"]);
    let made = ops::mkdir(&root, "new").unwrap();
    assert_eq!(made, root.child(&["new"]));
    assert!(made.is_dir());
    assert!(matches!(ops::mkdir(&root, "new"), Err(VfsError::AlreadyExists(_))));
}

#[test]
fn trash_is_grouped_by_file_system() {
    let caps = Capabilities { writable: true, can_trash: true, can_rename: true, watchable: false };
    let a = Arc::new(MemFS::with_capabilities(caps));
    let b = Arc::new(MemFS::with_capabilities(caps));
    for fs in [&a, &b] {
        fs.add_file("x", "1");
        fs.add_file("y", "2");
    }
    ops::trash(&[vp(&a, "x"), vp(&b, "y"), vp(&a, "y"), vp(&b, "x")]).unwrap();
    assert_eq!(a.trashed(), ["x", "y"]);
    assert_eq!(b.trashed(), ["y", "x"]);
    // One call per file system: the OS trash handles a batch as one operation.
    assert_eq!(a.trash_calls(), [["x", "y"]]);
    assert_eq!(b.trash_calls(), [["y", "x"]]);
    assert!(a.paths().is_empty() && b.paths().is_empty());
    // One file system without a trash refuses the whole batch up front.
    let (plain, _) = mem();
    plain.add_file("z", "3");
    a.add_file("w", "4");
    assert!(matches!(ops::trash(&[vp(&a, "w"), vp(&plain, "z")]), Err(VfsError::ReadOnly(_))));
    assert!(a.contains("w"));
}

#[test]
fn trash_waits_for_a_slow_backend_and_reports_its_failure() {
    let caps = Capabilities { writable: true, can_trash: true, can_rename: true, watchable: false };
    let fs = Arc::new(MemFS::with_capabilities(caps));
    fs.add_file("a", "1");
    fs.add_file("b", "2");
    let gate = testing::Gate::new();
    fs.set_trash_gate(Some(gate.clone()));
    let paths = [vp(&fs, "a"), vp(&fs, "b")];
    let worker = std::thread::spawn(move || ops::trash(&paths));
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    assert!(fs.contains("a") && fs.trash_calls().is_empty(), "held by the gate");
    gate.open();
    worker.join().unwrap().unwrap();
    assert!(fs.paths().is_empty());
    // A failing trash moves nothing.
    fs.add_file("c", "3");
    fs.set_faults(Faults { trash_fails: true, ..Faults::default() });
    assert!(ops::trash(&[vp(&fs, "c")]).is_err());
    assert!(fs.contains("c"));
}

#[test]
fn measure_counts_files_and_bytes() {
    let (_, root) = source_tree();
    assert_eq!(ops::measure(&[root.child(&["src"])]), (3, 13));
    assert_eq!(ops::measure(&[root.child(&["src", "a.txt"]), root.child(&["missing"])]), (1, 5));
    assert!(matches!(ops::measure_cancellable(std::slice::from_ref(&root), &|| true), Err(VfsError::Cancelled)));
}

// -- materialize ---------------------------------------------------------------------

#[test]
fn materialize_non_local_file() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, root) = mem();
    fs.add_file("x.txt", "payload");
    let x = root.child(&["x.txt"]);
    let path = ops::materialize(&x, cache.path()).unwrap();
    assert_eq!(path.file_name().unwrap(), "x.txt");
    assert!(path.starts_with(cache.path().join("vfs")));
    assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    // Cached: the same path again, and no .part files left behind.
    assert_eq!(ops::materialize(&x, cache.path()).unwrap(), path);
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1);
    // A changed file gets a fresh copy.
    fs.add_file("x.txt", "changed!");
    let fresh = ops::materialize(&x, cache.path()).unwrap();
    assert_ne!(fresh, path);
    assert_eq!(std::fs::read(&fresh).unwrap(), b"changed!");
}

#[test]
fn materialize_cancelled_leaves_nothing() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, root) = mem();
    fs.add_file("x.txt", "payload");
    let err = ops::materialize_cancellable(&root.child(&["x.txt"]), cache.path(), &|| true).unwrap_err();
    assert!(matches!(err, VfsError::Cancelled));
    let dir = std::fs::read_dir(cache.path().join("vfs")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
}

#[test]
fn materialize_missing_file_fails() {
    let cache = tempfile::tempdir().unwrap();
    let (_, root) = mem();
    assert!(ops::materialize(&root.child(&["nope"]), cache.path()).unwrap_err().is_not_found());
}

/// A read-only FS whose items are real files: materialize must hand back
/// the real path instead of copying.
struct DiskStub(PathBuf);

impl FileSystem for DiskStub {
    fn type_name(&self) -> String {
        "disk stub".into()
    }
    fn list_dir(&self, _: &str, _: ListOptions) -> Result<Vec<Entry>> {
        Ok(Vec::new())
    }
    fn stat(&self, path: &str) -> Result<Entry> {
        let md = std::fs::metadata(self.0.join(path))?;
        Ok(Entry::file(path, md.len(), 0.0))
    }
    fn parent_path(&self, _: &str) -> Option<String> {
        None
    }
    fn join(&self, path: &str, names: &[&str]) -> String {
        std::iter::once(path).chain(names.iter().copied()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
    }
    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }
    fn display(&self, path: &str) -> String {
        path.to_string()
    }
    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(std::fs::File::open(self.0.join(path))?))
    }
    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        Ok(Box::new(std::iter::once(self.stat(path).map(|e| (RelPath::new(), e)))))
    }
    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(self.0.join(path))
    }
}

#[test]
fn materialize_prefers_local_path_and_streams_into_other_backends() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::File::create(dir.path().join("f.txt")).unwrap().write_all(b"real").unwrap();
    let disk = VPath::new(Arc::new(DiskStub(dir.path().to_path_buf())), "");
    assert_eq!(ops::materialize(&disk.child(&["f.txt"]), cache.path()).unwrap(), dir.path().join("f.txt"));
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
    // And a copy from it into MemFS works through the generic path.
    let (dst_fs, dst) = mem();
    copy(&[disk.child(&["f.txt"])], &dst, None).unwrap();
    assert_eq!(dst_fs.read_file("f.txt").unwrap(), b"real");
}

// -- the contract ----------------------------------------------------------------------

#[test]
fn memfs_passes_the_contract() {
    testing::check_filesystem(&MemFS::new().root());
}

#[test]
fn memfs_passes_the_contract_in_a_subdirectory() {
    let fs = Arc::new(MemFS::new());
    fs.add_dir("some/empty/dir");
    testing::check_filesystem(&VPath::new(fs, "some/empty/dir"));
}

#[test]
fn read_only_memfs_passes_the_contract() {
    let fs = read_only_fs();
    for (rel, data) in testing::sample_tree() {
        fs.add_file(rel, data);
    }
    testing::check_filesystem(&VPath::new(fs, ""));
}

#[test]
#[should_panic(expected = "writable should be false")]
fn contract_catches_wrong_writable_flag() {
    testing::check_filesystem_with(&MemFS::new().root(), false, &testing::sample_tree());
}

#[test]
fn memfs_is_shareable_across_threads() {
    let fs = Arc::new(MemFS::new());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let fs = fs.clone();
            std::thread::spawn(move || {
                let mut w = fs.open_write(&format!("f{i}")).unwrap();
                w.write_all(&[i as u8; 100]).unwrap();
                w.finish().unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(fs.paths().len(), 8);
    assert_eq!(fs.read_file("f3").unwrap(), [3u8; 100]);
}

#[test]
fn memfs_write_is_committed_only_on_finish() {
    let (fs, _) = mem();
    let mut w = fs.open_write("a").unwrap();
    w.write_all(b"data").unwrap();
    assert_eq!(fs.read_file("a").unwrap(), b"", "open_write truncates immediately");
    drop(w);
    assert_eq!(fs.read_file("a").unwrap(), b"");
    assert!(fs.open_write("missing/a").err().unwrap().is_not_found());
}

// -- review additions --------------------------------------------------------------

/// A writable FS over a real directory. Every instance is a separate handle
/// (different identity), like two connections to the same disk.
struct DiskRw(PathBuf);

struct FileWriter(std::fs::File);

impl Write for FileWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.write(data)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl oc_vfs::WriteStream for FileWriter {
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(self.0.sync_all()?)
    }
}

impl FileSystem for DiskRw {
    fn type_name(&self) -> String {
        "disk".into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, ..Capabilities::default() }
    }
    fn list_dir(&self, _: &str, _: ListOptions) -> Result<Vec<Entry>> {
        Ok(Vec::new())
    }
    fn stat(&self, path: &str) -> Result<Entry> {
        let md = std::fs::metadata(self.0.join(path))?;
        let name = path.rsplit('/').next().unwrap_or("");
        Ok(if md.is_dir() { Entry::dir(name, 0.0) } else { Entry::file(name, md.len(), 0.0) })
    }
    fn parent_path(&self, path: &str) -> Option<String> {
        (!path.is_empty()).then(|| path.rsplit_once('/').map_or("", |(p, _)| p).to_string())
    }
    fn join(&self, path: &str, names: &[&str]) -> String {
        std::iter::once(path).chain(names.iter().copied()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
    }
    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }
    fn display(&self, path: &str) -> String {
        self.0.join(path).display().to_string()
    }
    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(std::fs::File::open(self.0.join(path))?))
    }
    fn walk<'a>(&'a self, path: &str) -> Result<oc_vfs::WalkIter<'a>> {
        Ok(Box::new(std::iter::once(self.stat(path).map(|e| (RelPath::new(), e)))))
    }
    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(self.0.join(path))
    }
    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        let p = self.0.join(path);
        if exist_ok { std::fs::create_dir_all(p)? } else { std::fs::create_dir(p)? }
        Ok(())
    }
    fn remove(&self, path: &str) -> Result<()> {
        let p = self.0.join(path);
        if p.is_dir() { std::fs::remove_dir_all(p)? } else { std::fs::remove_file(p)? }
        Ok(())
    }
    fn open_write(&self, path: &str) -> Result<Box<dyn oc_vfs::WriteStream>> {
        Ok(Box::new(FileWriter(std::fs::File::create(self.0.join(path))?)))
    }
}

#[test]
fn same_file_through_two_handles_is_refused() {
    // Without the check, "Overwrite" would delete the only copy before reading it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/f.txt"), "precious").unwrap();
    let a = VPath::new(Arc::new(DiskRw(dir.path().to_path_buf())), "");
    let b = VPath::new(Arc::new(DiskRw(dir.path().to_path_buf())), "");
    assert!(!a.same_fs(&b));
    let f = a.child(&["sub", "f.txt"]);
    let err = copy(std::slice::from_ref(&f), &b.child(&["sub"]), Some(Conflict::Overwrite)).unwrap_err();
    assert!(err.to_string().starts_with("Source and destination are the same file: "), "{err}");
    let err = move_to(std::slice::from_ref(&f), &b.child(&["sub"]), Some(Conflict::Overwrite)).unwrap_err();
    assert!(err.to_string().starts_with("Source and destination are the same: "), "{err}");
    assert_eq!(std::fs::read(dir.path().join("sub/f.txt")).unwrap(), b"precious");
    // A different name in the same place is an ordinary copy.
    copy(&[f], &b.child(&["sub", "g.txt"]), None).unwrap();
    assert_eq!(std::fs::read(dir.path().join("sub/g.txt")).unwrap(), b"precious");
}

#[test]
fn moving_a_folder_into_itself_through_two_handles_is_refused() {
    // Without the check, the move would copy into the subfolder and then
    // remove the source, copy included.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
    std::fs::write(dir.path().join("src/f.txt"), "keep").unwrap();
    let a = VPath::new(Arc::new(DiskRw(dir.path().to_path_buf())), "");
    let b = VPath::new(Arc::new(DiskRw(dir.path().to_path_buf())), "");
    let err = move_to(&[a.child(&["src"])], &b.child(&["src", "inner"]), None).unwrap_err();
    assert_eq!(err.to_string(), "Can't copy or move 'src' into itself");
    let err = copy(&[a.child(&["src"])], &b.child(&["src", "new"]), None).unwrap_err();
    assert_eq!(err.to_string(), "Can't copy or move 'src' into itself");
    assert_eq!(std::fs::read(dir.path().join("src/f.txt")).unwrap(), b"keep");
    assert!(!dir.path().join("src/new").exists());
}

#[test]
fn cross_fs_move_of_a_tree_removes_the_source() {
    let (src_fs, src) = source_tree();
    let (dst_fs, dst) = mem();
    let (result, progress, _) = run(None, |cb| ops::move_to(&[src.child(&["src"])], &dst, cb));
    result.unwrap();
    assert_eq!(dst_fs.paths(), ["src", "src/a.txt", "src/b.bin", "src/sub", "src/sub/c.txt"]);
    assert!(src_fs.paths().is_empty());
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.files_total, last.bytes_done, last.bytes_total), (3, 3, 13, 13));
}

#[test]
fn moving_a_skipped_file_keeps_it() {
    let (src_fs, src) = source_tree();
    let (dst_fs, dst) = mem();
    dst_fs.add_file("a.txt", "old");
    let (result, progress, _) = run(Some(Conflict::Skip), |cb| ops::move_to(&[src.child(&["src", "a.txt"])], &dst, cb));
    result.unwrap();
    assert_eq!(src_fs.read_file("src/a.txt").unwrap(), b"alpha");
    assert_eq!(dst_fs.read_file("a.txt").unwrap(), b"old");
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.bytes_done), (1, 5), "a skip still completes the bar");
}

#[test]
fn same_fs_move_with_refused_rename_keeps_skipped_items() {
    let (fs, root) = source_tree();
    fs.add_file("dst/src/a.txt", "old");
    fs.set_faults(Faults { rename_fails: true, ..Faults::default() });
    move_to(&[root.child(&["src"])], &root.child(&["dst"]), Some(Conflict::Skip)).unwrap();
    assert_eq!(fs.read_file("dst/src/a.txt").unwrap(), b"old");
    assert_eq!(fs.read_file("dst/src/sub/c.txt").unwrap(), b"gamma");
    assert_eq!(fs.read_file("src/a.txt").unwrap(), b"alpha");
    assert!(!fs.contains("src/b.bin") && !fs.contains("src/sub"));
}

#[test]
fn no_sources_and_no_target_directory_is_an_error() {
    let (_, root) = mem();
    let err = copy(&[], &root.child(&["nope"]), None).unwrap_err();
    assert_eq!(err.to_string(), "Destination folder mem:/nope doesn't exist");
    copy(&[], &root, None).unwrap();
}

#[test]
fn materialize_of_a_directory_fails_cleanly() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, root) = mem();
    fs.add_dir("d");
    assert!(ops::materialize(&root.child(&["d"]), cache.path()).is_err());
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
}

#[test]
fn memfs_add_file_replaces_a_directory() {
    let (fs, _) = mem();
    fs.add_file("a/b", "x");
    fs.add_file("a", "now a file");
    assert_eq!(fs.paths(), ["a"]);
}
