//! Port of `tests/test_fs.py` plus edge cases of the native operations and
//! the `LocalFS` backend. Everything happens in temp dirs; trash is stubbed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oc_local::{LocalFS, copy_paths, list_dir, local, move_paths};
use oc_vfs::testing::MemFS;
use oc_vfs::{Conflict, FileSystem, ListOptions, Progress, SortKey, TransferCallbacks, VPath, VfsError};
use tempfile::TempDir;

/// src/{a.txt, b.bin, sub/c.txt, .hidden}, dst/
fn tree() -> TempDir {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), "alpha").unwrap();
    fs::write(src.join("b.bin"), vec![0u8; 3000]).unwrap();
    fs::write(src.join("sub/c.txt"), "gamma").unwrap();
    fs::write(src.join(".hidden"), "h").unwrap();
    fs::create_dir(t.path().join("dst")).unwrap();
    t
}

fn never(src: &str, dst: &str) -> Conflict {
    panic!("unexpected conflict {src} -> {dst}")
}

fn not_cancelled() -> bool {
    false
}

/// Runs `f` with callbacks answering conflicts with `answer` (None = must not
/// happen) and collects every progress snapshot.
fn with_callbacks<R>(
    answer: Option<Conflict>,
    cancelled: &dyn Fn() -> bool,
    f: impl FnOnce(TransferCallbacks<'_>) -> R,
) -> (R, Vec<Progress>, Vec<(String, String)>) {
    let mut progress = Vec::new();
    let mut asked = Vec::new();
    let mut on_conflict = |s: &str, d: &str| {
        asked.push((s.to_string(), d.to_string()));
        match answer {
            Some(a) => a,
            None => never(s, d),
        }
    };
    let mut on_progress = |p: &Progress| progress.push(p.clone());
    let r = f(TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: cancelled });
    (r, progress, asked)
}

fn cp(sources: &[PathBuf], target: &Path, answer: Option<Conflict>) -> oc_vfs::Result<()> {
    with_callbacks(answer, &not_cancelled, |cb| copy_paths(sources, target, cb)).0
}

fn mv(sources: &[PathBuf], target: &Path, answer: Option<Conflict>) -> oc_vfs::Result<()> {
    with_callbacks(answer, &not_cancelled, |cb| move_paths(sources, target, cb)).0
}

fn names(dir: &Path, opts: ListOptions) -> Vec<String> {
    list_dir(dir, opts).unwrap().into_iter().map(|e| e.name).collect()
}

fn sorted_dir(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|d| d.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Ported from test_fs.py
// ---------------------------------------------------------------------------

#[test]
fn list_dir_dirs_first_and_hidden() {
    let t = tree();
    assert_eq!(names(&t.path().join("src"), ListOptions::default()), ["sub", "a.txt", "b.bin"]);
    let all = ListOptions { show_hidden: true, ..Default::default() };
    assert!(names(&t.path().join("src"), all).contains(&".hidden".to_string()));
}

#[test]
fn list_dir_sort_by_size_reverse() {
    let t = tree();
    let opts = ListOptions { sort_key: SortKey::Size, reverse: true, ..Default::default() };
    let files: Vec<String> =
        list_dir(&t.path().join("src"), opts).unwrap().into_iter().filter(|e| !e.is_dir).map(|e| e.name).collect();
    assert_eq!(files, ["b.bin", "a.txt"]);
}

#[test]
fn copy_into_directory() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    let (r, progress, _) =
        with_callbacks(None, &not_cancelled, |cb| copy_paths(&[src.join("a.txt"), src.join("sub")], &dst, cb));
    r.unwrap();
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
    assert_eq!(fs::read_to_string(dst.join("sub/c.txt")).unwrap(), "gamma");
    assert!(src.join("a.txt").exists());
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.files_total), (2, 2));
    assert_eq!((last.bytes_done, last.bytes_total), (10, 10));
}

#[test]
fn copy_single_file_to_new_name() {
    let t = tree();
    cp(&[t.path().join("src/a.txt")], &t.path().join("dst/renamed.txt"), None).unwrap();
    assert_eq!(fs::read_to_string(t.path().join("dst/renamed.txt")).unwrap(), "alpha");
}

#[test]
fn copy_conflict_skip_and_overwrite() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    fs::write(dst.join("a.txt"), "old").unwrap();
    let (r, progress, asked) = with_callbacks(Some(Conflict::Skip), &not_cancelled, |cb| copy_paths(&[src.join("a.txt")], &dst, cb));
    r.unwrap();
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "old");
    assert_eq!(asked, [(src.join("a.txt").display().to_string(), dst.join("a.txt").display().to_string())]);
    assert_eq!(progress.last().unwrap().files_done, 1); // skipped files still count as done
    cp(&[src.join("a.txt")], &dst, Some(Conflict::Overwrite)).unwrap();
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
}

#[test]
fn copy_conflict_cancel() {
    let t = tree();
    fs::write(t.path().join("dst/a.txt"), "old").unwrap();
    let r = cp(&[t.path().join("src/a.txt")], &t.path().join("dst"), Some(Conflict::Cancel));
    assert!(matches!(r, Err(VfsError::Cancelled)), "{r:?}");
    assert_eq!(fs::read_to_string(t.path().join("dst/a.txt")).unwrap(), "old");
}

#[test]
fn copy_cancel_removes_partial_file() {
    let t = tree();
    let (r, _, _) = with_callbacks(None, &|| true, |cb| copy_paths(&[t.path().join("src/b.bin")], &t.path().join("dst"), cb));
    assert!(matches!(r, Err(VfsError::Cancelled)), "{r:?}");
    assert!(!t.path().join("dst/b.bin").exists());
}

#[test]
fn copy_dir_into_itself_rejected() {
    let t = tree();
    let r = cp(&[t.path().join("src")], &t.path().join("src/sub"), None);
    match r {
        Err(VfsError::Operation(msg)) => assert_eq!(msg, "Cannot copy or move 'src' into itself"),
        other => panic!("{other:?}"),
    }
    // Also into a not-yet-existing folder below itself, and onto itself.
    assert!(matches!(cp(&[t.path().join("src")], &t.path().join("src/new/deeper"), None), Err(VfsError::Operation(_))));
    assert!(matches!(mv(&[t.path().join("src")], &t.path().join("src"), None), Err(VfsError::Operation(_))));
    // A sibling whose name starts the same is fine.
    fs::create_dir(t.path().join("src2")).unwrap();
    cp(&[t.path().join("src/sub")], &t.path().join("src2"), None).unwrap();
}

#[test]
fn copy_multiple_to_missing_dir_rejected() {
    let t = tree();
    let nope = t.path().join("nope");
    let r = cp(&[t.path().join("src/a.txt"), t.path().join("src/b.bin")], &nope, None);
    match r {
        Err(VfsError::Operation(msg)) => assert_eq!(msg, format!("Target directory does not exist: {}", nope.display())),
        other => panic!("{other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn copy_preserves_symlink() {
    let t = tree();
    let link = t.path().join("src/link");
    std::os::unix::fs::symlink("a.txt", &link).unwrap();
    cp(&[link], &t.path().join("dst"), None).unwrap();
    let copied = t.path().join("dst/link");
    assert!(copied.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(copied).unwrap(), PathBuf::from("a.txt"));
}

#[test]
fn move_and_rename() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    mv(&[src.join("sub")], &dst, None).unwrap();
    assert!(dst.join("sub/c.txt").exists() && !src.join("sub").exists());
    mv(&[src.join("a.txt")], &src.join("z.txt"), None).unwrap();
    assert_eq!(fs::read_to_string(src.join("z.txt")).unwrap(), "alpha");
    assert!(!src.join("a.txt").exists());
}

#[test]
fn move_merges_directories() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    fs::create_dir(dst.join("sub")).unwrap();
    fs::write(dst.join("sub/keep.txt"), "k").unwrap();
    mv(&[src.join("sub")], &dst, None).unwrap();
    assert_eq!(sorted_dir(&dst.join("sub")), ["c.txt", "keep.txt"]);
    assert!(!src.join("sub").exists());
}

#[test]
fn move_conflict_skip_keeps_source() {
    let t = tree();
    fs::write(t.path().join("dst/a.txt"), "old").unwrap();
    let (r, progress, _) =
        with_callbacks(Some(Conflict::Skip), &not_cancelled, |cb| move_paths(&[t.path().join("src/a.txt")], &t.path().join("dst"), cb));
    r.unwrap();
    assert!(t.path().join("src/a.txt").exists());
    assert_eq!(fs::read_to_string(t.path().join("dst/a.txt")).unwrap(), "old");
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.bytes_done), (1, 5), "a skipped move still counts as done");
}

/// A real move across file systems (copy, then delete), where the machine has
/// a second one at hand: tmpfs at /dev/shm on Linux. Skipped elsewhere; the
/// unit tests cover the same code with a simulated EXDEV.
#[cfg(unix)]
#[test]
fn move_across_real_devices() {
    use std::os::unix::fs::MetadataExt;
    let t = tree();
    let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
    let Some(other) = ["/dev/shm"].iter().map(Path::new).find(|p| {
        dev(p).is_some_and(|d| Some(d) != dev(t.path())) && tempfile::tempdir_in(p).is_ok()
    }) else {
        eprintln!("no second file system; skipped");
        return;
    };
    let far = tempfile::tempdir_in(other).unwrap();
    let src = t.path().join("src");
    std::os::unix::fs::symlink("sub", src.join("sublink")).unwrap();
    mv(std::slice::from_ref(&src), far.path(), None).unwrap();
    let moved = far.path().join("src");
    assert!(!src.exists());
    assert_eq!(fs::read_to_string(moved.join("sub/c.txt")).unwrap(), "gamma");
    assert!(fs::symlink_metadata(moved.join("sublink")).unwrap().file_type().is_symlink());
    // And back, onto an existing folder: merge across devices.
    fs::create_dir(&src).unwrap();
    fs::write(src.join("keep.txt"), "k").unwrap();
    mv(&[moved], t.path(), None).unwrap();
    assert_eq!(sorted_dir(&src), [".hidden", "a.txt", "b.bin", "keep.txt", "sub", "sublink"]);
    assert_eq!(sorted_dir(far.path()), Vec::<String>::new());
}

#[test]
fn trash_goes_through_the_trasher() {
    let t = tree();
    let trashed: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let seen = trashed.clone();
    let lfs = LocalFS::with_trasher(move |paths| {
        seen.lock().unwrap().extend_from_slice(paths);
        Ok(())
    });
    let a = t.path().join("src/a.txt");
    lfs.trash(&[a.display().to_string()]).unwrap();
    assert_eq!(*trashed.lock().unwrap(), [a]);
    assert!(lfs.capabilities().can_trash);
}

#[test]
fn human_size() {
    assert_eq!(oc_local::human_size(512), "512 B");
    assert_eq!(oc_local::human_size(2048), "2.0 K");
    assert_eq!(oc_local::human_size(5 * 1024u64.pow(3)), "5.0 G");
}

// ---------------------------------------------------------------------------
// More edge cases
// ---------------------------------------------------------------------------

#[test]
fn same_file_is_an_error_not_a_conflict() {
    let t = tree();
    let src = t.path().join("src");
    match cp(&[src.join("a.txt")], &src, None) {
        Err(VfsError::Operation(msg)) => {
            assert_eq!(msg, format!("Source and destination are the same file: {}", src.join("a.txt").display()))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "alpha");
    match mv(&[src.join("a.txt")], &src.join("a.txt"), None) {
        Err(VfsError::Operation(msg)) => {
            assert_eq!(msg, format!("Source and destination are the same: {}", src.join("a.txt").display()))
        }
        other => panic!("{other:?}"),
    }
    #[cfg(unix)]
    {
        // A hard link is the same file too.
        fs::hard_link(src.join("a.txt"), t.path().join("dst/a.txt")).unwrap();
        assert!(matches!(cp(&[src.join("a.txt")], &t.path().join("dst"), None), Err(VfsError::Operation(_))));
    }
}

#[test]
fn overwrite_replaces_file_with_directory_and_back() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    fs::write(dst.join("sub"), "a file in the way").unwrap();
    cp(&[src.join("sub")], &dst, Some(Conflict::Overwrite)).unwrap();
    assert_eq!(fs::read_to_string(dst.join("sub/c.txt")).unwrap(), "gamma");

    fs::create_dir_all(dst.join("a.txt/inner")).unwrap();
    cp(&[src.join("a.txt")], &dst, Some(Conflict::Overwrite)).unwrap();
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
}

#[test]
fn copy_merges_directories_and_asks_per_file() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    fs::create_dir(dst.join("sub")).unwrap();
    fs::write(dst.join("sub/c.txt"), "old").unwrap();
    fs::write(dst.join("sub/keep.txt"), "k").unwrap();
    let (r, progress, asked) = with_callbacks(Some(Conflict::Skip), &not_cancelled, |cb| copy_paths(&[src.join("sub")], &dst, cb));
    r.unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(sorted_dir(&dst.join("sub")), ["c.txt", "keep.txt"]);
    assert_eq!(fs::read_to_string(dst.join("sub/c.txt")).unwrap(), "old");
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.files_total), (1, 1));
}

#[test]
fn skipping_a_directory_counts_its_files() {
    let t = tree();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    fs::write(dst.join("sub"), "file in the way").unwrap();
    let (r, progress, _) =
        with_callbacks(Some(Conflict::Skip), &not_cancelled, |cb| copy_paths(&[src.join("sub"), src.join("a.txt")], &dst, cb));
    r.unwrap();
    let last = progress.last().unwrap();
    assert_eq!((last.files_done, last.files_total, last.bytes_done, last.bytes_total), (2, 2, 10, 10));
    assert_eq!(fs::read_to_string(dst.join("sub")).unwrap(), "file in the way");
}

#[cfg(unix)]
#[test]
fn copy_keeps_mtime_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    let a = t.path().join("src/a.txt");
    fs::set_permissions(&a, fs::Permissions::from_mode(0o751)).unwrap();
    filetime::set_file_mtime(&a, filetime::FileTime::from_unix_time(1_500_000_000, 0)).unwrap();
    let sub = t.path().join("src/sub");
    filetime::set_file_mtime(&sub, filetime::FileTime::from_unix_time(1_400_000_000, 0)).unwrap();
    cp(&[a, sub], &t.path().join("dst"), None).unwrap();
    let meta = fs::metadata(t.path().join("dst/a.txt")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o751);
    assert_eq!(filetime::FileTime::from_last_modification_time(&meta).unix_seconds(), 1_500_000_000);
    let dmeta = fs::metadata(t.path().join("dst/sub")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&dmeta).unix_seconds(), 1_400_000_000);
}

#[cfg(unix)]
#[test]
fn links_to_directories_are_copied_and_moved_as_links() {
    let t = tree();
    let src = t.path().join("src");
    std::os::unix::fs::symlink("sub", src.join("sublink")).unwrap();
    std::os::unix::fs::symlink("missing", src.join("dangling")).unwrap();
    fs::create_dir(t.path().join("copy")).unwrap();
    cp(std::slice::from_ref(&src), &t.path().join("copy"), None).unwrap();
    let copied = t.path().join("copy/src");
    for name in ["sublink", "dangling"] {
        assert!(copied.join(name).symlink_metadata().unwrap().file_type().is_symlink(), "{name}");
    }
    assert_eq!(fs::read_link(copied.join("sublink")).unwrap(), PathBuf::from("sub"));
    // Measured: a.txt, b.bin, .hidden, sub/c.txt, sublink, dangling.
    assert_eq!(oc_local::measure(&[&src]).0, 6);

    // Copying the link itself onto an existing dir asks instead of merging.
    fs::create_dir(t.path().join("dst/sublink")).unwrap();
    cp(&[src.join("sublink")], &t.path().join("dst"), Some(Conflict::Overwrite)).unwrap();
    assert!(t.path().join("dst/sublink").symlink_metadata().unwrap().file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn listing_entries_carry_link_hidden_and_exec_flags() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    let src = t.path().join("src");
    std::os::unix::fs::symlink("sub", src.join("dirlink")).unwrap();
    std::os::unix::fs::symlink("nowhere", src.join("dangling")).unwrap();
    fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
    let all = ListOptions { show_hidden: true, ..Default::default() };
    let entries = list_dir(&src, all).unwrap();
    let get = |n: &str| entries.iter().find(|e| e.name == n).unwrap().clone();

    let dirlink = get("dirlink");
    assert!(dirlink.is_dir && dirlink.is_link && dirlink.size == 0);
    let dangling = get("dangling");
    assert!(!dangling.is_dir && dangling.is_link);
    assert!(get("run.sh").is_executable && !get("a.txt").is_executable);
    assert!(!get("sub").is_executable, "directories are never executable");
    assert!(get(".hidden").is_hidden && !get("a.txt").is_hidden);
    assert_eq!(get("b.bin").size, 3000);
    assert!(get("a.txt").mtime > 1_000_000_000.0);
    let order: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(order, ["dirlink", "sub", ".hidden", "a.txt", "b.bin", "dangling", "run.sh"]);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_hidden_flag_hides() {
    let t = tree();
    let f = t.path().join("src/a.txt");
    let status = std::process::Command::new("chflags").arg("hidden").arg(&f).status().unwrap();
    assert!(status.success());
    assert!(!names(&t.path().join("src"), ListOptions::default()).contains(&"a.txt".to_string()));
    assert!(oc_local::stat_entry(&f).unwrap().is_hidden);
    // Copies keep the flag (Finder would show them otherwise).
    cp(&[f], &t.path().join("dst"), None).unwrap();
    assert!(oc_local::stat_entry(&t.path().join("dst/a.txt")).unwrap().is_hidden);
}

#[test]
fn listing_errors_name_the_path() {
    let t = tree();
    let missing = t.path().join("missing");
    let err = LocalFS::new().list_dir(&missing.display().to_string(), ListOptions::default()).unwrap_err();
    assert!(err.is_not_found());
    assert!(err.to_string().contains(&missing.display().to_string()), "{err}");
    assert!(LocalFS::new().stat(&missing.display().to_string()).unwrap_err().is_not_found());
}

#[test]
fn walk_order_and_links() {
    let t = tree();
    let src = t.path().join("src");
    fs::create_dir_all(src.join("sub/inner")).unwrap();
    fs::write(src.join("sub/inner/d.txt"), "d").unwrap();
    fs::create_dir(src.join("adir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("sub", src.join("link")).unwrap();
    let lfs = LocalFS::new();
    let walked: Vec<(Vec<String>, oc_vfs::Entry)> =
        lfs.walk(&src.display().to_string()).unwrap().collect::<oc_vfs::Result<_>>().unwrap();
    let rels: Vec<String> = walked.iter().map(|(r, _)| r.join("/")).collect();
    // Directories (a link to one among them, as a non-directory), then files;
    // then each real subdirectory's contents.
    let mut expected = vec!["", "adir"];
    #[cfg(unix)]
    expected.push("link");
    expected.extend(["sub", ".hidden", "a.txt", "b.bin", "sub/inner", "sub/c.txt", "sub/inner/d.txt"]);
    assert_eq!(rels, expected);
    assert!(walked[0].1.is_dir);
    #[cfg(unix)]
    {
        let link = &walked.iter().find(|(r, _)| r.join("/") == "link").unwrap().1;
        assert!(link.is_link && !link.is_dir && link.size == 0);
    }
    // A file walks as itself only.
    let one: Vec<_> = lfs.walk(&src.join("a.txt").display().to_string()).unwrap().collect();
    assert_eq!(one.len(), 1);
    assert!(lfs.walk(&src.join("missing").display().to_string()).is_err());
}

#[test]
fn local_fs_passes_the_vfs_contract() {
    let t = tempfile::tempdir().unwrap();
    oc_vfs::testing::check_filesystem(&local(t.path()));
}

#[test]
fn local_fs_paths() {
    let lfs = LocalFS::new();
    assert_eq!(lfs.type_name(), "local disk");
    let caps = lfs.capabilities();
    assert!(caps.writable && caps.can_trash && caps.can_rename && caps.watchable);
    #[cfg(unix)]
    {
        assert_eq!(lfs.basename("/a/b.txt"), "b.txt");
        assert_eq!(lfs.basename("/a/b/"), "b");
        assert_eq!(lfs.basename("/"), "/");
        assert_eq!(lfs.parent_path("/a/b"), Some("/a".to_string()));
        assert_eq!(lfs.parent_path("/"), None);
        assert_eq!(lfs.join("/a", &["b", "c"]), "/a/b/c");
        assert_eq!(lfs.display("/a/b"), "/a/b");
        assert_eq!(local("/a/./b/../c").path, "/a/c");
        assert_eq!(local("/").name(), "/");
    }
    // Every LocalFS is the same file system.
    let other: Arc<dyn FileSystem> = Arc::new(LocalFS::new());
    assert_eq!(VPath::new(other, local("/x").path.clone()), local("/x"));
    let t = tempfile::tempdir().unwrap();
    assert!(lfs.free_space(&t.path().display().to_string()).unwrap() > 0);
    assert_eq!(lfs.local_path("/x/y"), Some(PathBuf::from("/x/y")));
}

#[test]
fn local_fs_writes() {
    let t = tempfile::tempdir().unwrap();
    let lfs = LocalFS::new();
    let p = |rel: &str| t.path().join(rel).display().to_string();
    lfs.mkdir(&p("x/y"), false).unwrap();
    assert!(lfs.mkdir(&p("x/y"), false).is_err());
    lfs.mkdir(&p("x/y"), true).unwrap();
    fs::write(t.path().join("file"), "f").unwrap();
    assert!(lfs.mkdir(&p("file"), true).is_err(), "exist_ok doesn't excuse a file in the way");

    let mut w = lfs.open_write(&p("x/y/out.txt")).unwrap();
    std::io::Write::write_all(&mut w, b"written").unwrap();
    w.finish().unwrap();
    assert_eq!(fs::read_to_string(t.path().join("x/y/out.txt")).unwrap(), "written");

    lfs.set_mtime(&p("x/y/out.txt"), 1_234_567_890.5).unwrap();
    let mtime = lfs.stat(&p("x/y/out.txt")).unwrap().mtime;
    assert!((mtime - 1_234_567_890.5).abs() < 1e-3, "{mtime}");
    lfs.set_mtime(&p("x/y/out.txt"), -10.25).unwrap();
    assert!((lfs.stat(&p("x/y/out.txt")).unwrap().mtime + 10.25).abs() < 1e-3);

    #[cfg(unix)]
    {
        // remove() deletes a link, never what it points to.
        std::os::unix::fs::symlink(t.path().join("x"), t.path().join("xlink")).unwrap();
        assert!(lfs.exists(&p("xlink")));
        lfs.remove(&p("xlink")).unwrap();
        assert!(t.path().join("x/y/out.txt").exists());
        std::os::unix::fs::symlink("gone", t.path().join("dangling")).unwrap();
        assert!(lfs.exists(&p("dangling")), "dangling links exist");
        assert!(!lfs.is_dir(&p("dangling")));
    }
    lfs.rename(&p("x"), &p("z")).unwrap();
    lfs.remove(&p("z")).unwrap();
    assert!(!t.path().join("z").exists());
    assert!(lfs.remove(&p("z")).unwrap_err().is_not_found());
}

#[test]
fn vfs_copy_dispatches_local_and_streams_elsewhere() {
    let t = tree();
    // Local to local: native (symlinks etc.), same semantics.
    let mut c = never;
    oc_local::copy(
        &[local(t.path().join("src/a.txt"))],
        &local(t.path().join("dst")),
        false,
        TransferCallbacks { on_conflict: &mut c, on_progress: None, is_cancelled: &not_cancelled },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(t.path().join("dst/a.txt")).unwrap(), "alpha");

    // Local to memory: streamed through oc_vfs::ops.
    let mem = Arc::new(MemFS::new());
    let root = VPath::new(mem.clone(), "");
    oc_local::copy(
        &[local(t.path().join("src/sub"))],
        &root,
        false,
        TransferCallbacks { on_conflict: &mut c, on_progress: None, is_cancelled: &not_cancelled },
    )
    .unwrap();
    assert_eq!(mem.read_file("sub/c.txt").as_deref(), Some(&b"gamma"[..]));

    // Memory to local, as a move.
    oc_local::copy(
        &[root.child(&["sub"])],
        &local(t.path().join("dst")),
        true,
        TransferCallbacks { on_conflict: &mut c, on_progress: None, is_cancelled: &not_cancelled },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(t.path().join("dst/sub/c.txt")).unwrap(), "gamma");
    assert!(!mem.contains("sub"));

    // Read-only targets are refused up front.
    let ro = MemFS::with_capabilities(oc_vfs::Capabilities::default()).root();
    let r = oc_local::copy(
        &[local(t.path().join("src/a.txt"))],
        &ro,
        false,
        TransferCallbacks { on_conflict: &mut c, on_progress: None, is_cancelled: &not_cancelled },
    );
    assert!(matches!(r, Err(VfsError::ReadOnly(_))), "{r:?}");
}

/// `cargo test -p oc-local --release -- --ignored --nocapture bench_list_100k`
#[test]
#[ignore]
fn bench_list_100k() {
    let t = tempfile::tempdir().unwrap();
    let n = 100_000;
    let t0 = std::time::Instant::now();
    for i in 0..n {
        fs::File::create(t.path().join(format!("file-{i:06}.txt"))).unwrap();
    }
    eprintln!("created {n} files in {:?}", t0.elapsed());
    for round in 0..3 {
        let t0 = std::time::Instant::now();
        let entries = list_dir(t.path(), ListOptions::default()).unwrap();
        eprintln!("round {round}: listed + sorted {} entries in {:?}", entries.len(), t0.elapsed());
        assert_eq!(entries.len(), n);
    }
    let t0 = std::time::Instant::now();
    let raw = oc_local::read_entries(t.path()).unwrap();
    eprintln!("read_entries (no sort) {} in {:?}", raw.len(), t0.elapsed());
}
