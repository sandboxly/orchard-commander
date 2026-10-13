//! Directory listings and entries on the local disk (the listing half of
//! `fs.py`, plus `LocalFS.walk`).

use std::collections::VecDeque;
use std::fs::{self, DirEntry, Metadata};
#[cfg(target_os = "macos")]
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use oc_vfs::{Entry, ListOptions, RelPath, sort_entries};
use rayon::prelude::*;

/// Above this many entries a listing stats in parallel. Below it the thread
/// hand-off costs more than it saves (a cached stat is ~1–2 µs).
const PARALLEL_THRESHOLD: usize = 512;

// ---------------------------------------------------------------------------
// Platform flags
// ---------------------------------------------------------------------------

/// macOS (APFS): File Provider placeholder (Dropbox, iCloud Drive, OneDrive).
pub const SF_DATALESS: u32 = 0x4000_0000;
/// macOS: hidden in Finder (`chflags hidden`).
pub const UF_HIDDEN: u32 = 0x8000;
/// Windows: RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE.
pub const WINDOWS_CLOUD_ATTRIBUTES: u32 = 0x0040_0000 | 0x0004_0000 | 0x0000_1000;
/// Windows: HIDDEN | SYSTEM.
pub const WINDOWS_HIDDEN_ATTRIBUTES: u32 = 0x2 | 0x4;

#[cfg(target_os = "macos")]
fn st_flags(md: &Metadata) -> u32 {
    use std::os::macos::fs::MetadataExt;
    md.st_flags()
}

#[cfg(windows)]
fn attributes(md: &Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

fn is_hidden(name: &str, md: &Metadata) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        st_flags(md) & UF_HIDDEN != 0
    }
    #[cfg(windows)]
    {
        attributes(md) & WINDOWS_HIDDEN_ATTRIBUTES != 0
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = md;
        false
    }
}

fn is_executable(name: &str, md: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = name;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        let _ = md;
        matches!(oc_vfs::ext_of(name).as_str(), "exe" | "bat" | "cmd" | "com" | "ps1")
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (name, md);
        false
    }
}

/// A cloud placeholder: its data isn't on disk yet, reading it downloads it.
fn is_online_only(md: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        st_flags(md) & SF_DATALESS != 0
    }
    #[cfg(windows)]
    {
        attributes(md) & WINDOWS_CLOUD_ATTRIBUTES != 0
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = md;
        false
    }
}

/// Seconds since the epoch as a float, like Python's `st_mtime`.
pub(crate) fn mtime_of(md: &Metadata) -> f64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.mtime() as f64 + md.mtime_nsec() as f64 * 1e-9
    }
    #[cfg(not(unix))]
    {
        use std::time::UNIX_EPOCH;
        match md.modified() {
            Ok(t) => match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            },
            Err(_) => 0.0,
        }
    }
}

/// Non-UTF-8 names (rare, Linux only) are shown lossily; see the crate docs.
pub(crate) fn name_string(name: std::ffi::OsString) -> String {
    name.into_string().unwrap_or_else(|s| s.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// Build an Entry from lstat and link-following stat (None for dangling links).
fn entry_from(name: String, lstat: &Metadata, st: Option<&Metadata>) -> Entry {
    let is_link = lstat.file_type().is_symlink();
    let eff = st.unwrap_or(lstat);
    let is_dir = eff.is_dir();
    Entry {
        is_hidden: is_hidden(&name, eff),
        is_executable: !is_dir && is_executable(&name, eff),
        size: if is_dir { 0 } else { eff.len() },
        mtime: mtime_of(eff),
        is_online_only: !is_dir && is_online_only(eff),
        name,
        is_dir,
        is_link,
    }
}

/// Links need a second, following stat; everything else is done with one.
fn entry_at(name: String, path: &Path, lstat: &Metadata) -> Entry {
    if lstat.file_type().is_symlink() {
        let st = fs::metadata(path).ok();
        entry_from(name, lstat, st.as_ref())
    } else {
        entry_from(name, lstat, None)
    }
}

fn entry_from_dirent(de: DirEntry) -> Entry {
    let name = name_string(de.file_name());
    match de.metadata() {
        Ok(lstat) => entry_at(name, &de.path(), &lstat),
        // Vanished or unreadable between readdir and stat: show it anyway.
        Err(_) => {
            let hidden = name.starts_with('.');
            Entry { is_hidden: hidden, ..Entry::file(name, 0, 0.0) }
        }
    }
}

/// Entry for a single path. Errors if it doesn't exist.
pub fn stat_entry(path: &Path) -> io::Result<Entry> {
    let lstat = fs::symlink_metadata(path)?;
    let name = match path.file_name() {
        Some(n) => name_string(n.to_os_string()),
        None => path.to_string_lossy().into_owned(),
    };
    Ok(entry_at(name, path, &lstat))
}

/// All entries of `path`, unsorted and unfiltered. On macOS names and
/// attributes come in bulk (`getattrlistbulk`, what Finder uses: ~1/3 faster
/// than readdir + stat even in parallel); elsewhere huge folders are stat-ed
/// in parallel, as the per-entry stat dominates (and on network folders each
/// one can take milliseconds).
pub fn read_entries(path: &Path) -> io::Result<Vec<Entry>> {
    #[cfg(target_os = "macos")]
    if let Some(entries) = bulk::read_entries(path)? {
        return Ok(entries);
    }
    read_entries_generic(path)
}

/// readdir + stat, in parallel above [`PARALLEL_THRESHOLD`].
fn read_entries_generic(path: &Path) -> io::Result<Vec<Entry>> {
    let dirents = fs::read_dir(path)?.collect::<io::Result<Vec<DirEntry>>>()?;
    Ok(if dirents.len() >= PARALLEL_THRESHOLD {
        dirents.into_par_iter().map(entry_from_dirent).collect()
    } else {
        dirents.into_iter().map(entry_from_dirent).collect()
    })
}

/// List `path`: directories first, then files, each group sorted per `opts`.
pub fn list_dir(path: &Path, opts: ListOptions) -> io::Result<Vec<Entry>> {
    Ok(sort_entries(read_entries(path)?, opts))
}

/// The parent directory, or None at a root.
pub fn parent_of(path: &Path) -> Option<PathBuf> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => Some(p.to_path_buf()),
        // "a" → "." like Python's Path("a").parent; "." and "" have none.
        Some(_) if path != Path::new(".") => Some(PathBuf::from(".")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// macOS bulk listing
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod bulk {
    use super::*;
    use std::os::fd::AsRawFd;

    const VDIR: u32 = 2;
    const VLNK: u32 = 5;
    const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FLAGS;
    /// Mount points are listed with the attributes of the directory they cover,
    /// but stat (and so the Python version) reports the mounted volume's root.
    const DIR: u32 = libc::ATTR_DIR_MOUNTSTATUS;
    /// Same for firmlinks (/Users, /Applications, … in "/"): listed as the
    /// system volume's placeholder, stat-ed as the data volume's folder.
    const SF_FIRMLINK: u32 = 0x0080_0000;
    /// Record layout with FSOPT_PACK_INVAL_ATTRS: length 4, returned set 20,
    /// name ref 8, objtype 4, modtime 16, mask 4, flags 4 — then only the group
    /// for the object's type: a directory's mount status (4) or everything
    /// else's data length (8). The name's bytes follow.
    const TYPED: usize = 60;
    /// Enough of a record to read its name.
    const NAME_END: usize = 32;

    fn rd<T: Copy>(b: &[u8], off: usize) -> T {
        assert!(off + std::mem::size_of::<T>() <= b.len());
        // SAFETY: bounds checked above; read_unaligned has no alignment needs.
        unsafe { std::ptr::read_unaligned(b.as_ptr().add(off).cast::<T>()) }
    }

    /// Ok(None) when the file system can't do bulk reads (the caller falls
    /// back to readdir + stat); real errors (missing, no permission, not a
    /// directory) are returned as such.
    pub(super) fn read_entries(path: &Path) -> io::Result<Option<Vec<Entry>>> {
        let dir = File::open(path)?;
        // SAFETY: attrlist is plain old data; zero is a valid starting value.
        let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
        al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        al.commonattr = COMMON;
        al.dirattr = DIR;
        al.fileattr = libc::ATTR_FILE_DATALENGTH;
        let mut buf = vec![0u8; 256 * 1024];
        let mut entries = Vec::new();
        let mut slow = Vec::new();
        loop {
            // SAFETY: al and buf are valid for the call; buf.len() is its size.
            let n = unsafe {
                libc::getattrlistbulk(
                    dir.as_raw_fd(),
                    (&raw mut al).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::ENOTDIR | libc::EACCES | libc::EPERM | libc::ENOENT | libc::EIO) => Err(err),
                    _ => Ok(None),
                };
            }
            if n == 0 {
                break;
            }
            let mut off = 0usize;
            for _ in 0..n {
                let Some(len) = buf.get(off..off + 4).map(|_| rd::<u32>(&buf, off) as usize) else { return Ok(None) };
                let Some(rec) = buf.get(off..off + len).filter(|r| r.len() >= NAME_END) else { return Ok(None) };
                off += len;
                let Some(name) = record_name(rec) else { return Ok(None) };
                let returned_common: u32 = rd(rec, 4);
                let returned_dir: u32 = rd(rec, 4 + 8);
                let returned_file: u32 = rd(rec, 4 + 12);
                let objtype: u32 = if rec.len() >= TYPED { rd(rec, 32) } else { 0 };
                let is_dir = objtype == VDIR;
                let complete = rec.len() >= TYPED + if is_dir { 4 } else { 8 }
                    && returned_common & COMMON == COMMON
                    && if is_dir {
                        returned_dir & DIR != 0
                    } else {
                        returned_file & libc::ATTR_FILE_DATALENGTH != 0
                    };
                let mount_point = complete && is_dir && rd::<u32>(rec, TYPED) & libc::DIR_MNTSTATUS_MNTPOINT != 0;
                if objtype == VLNK || mount_point || !complete {
                    // Links need a following stat, mount points the mounted
                    // root's; odd records go the generic way too.
                    slow.push(name);
                    continue;
                }
                let flags: u32 = rd(rec, 56);
                if flags & SF_FIRMLINK != 0 {
                    slow.push(name);
                    continue;
                }
                let sec: i64 = rd(rec, 36);
                let nsec: i64 = rd(rec, 44);
                let mask: u32 = rd(rec, 52);
                let size: i64 = if is_dir { 0 } else { rd(rec, TYPED) };
                entries.push(Entry {
                    is_hidden: name.starts_with('.') || flags & UF_HIDDEN != 0,
                    is_executable: !is_dir && mask & 0o111 != 0,
                    size: if is_dir { 0 } else { size.max(0) as u64 },
                    mtime: sec as f64 + nsec as f64 * 1e-9,
                    is_online_only: !is_dir && flags & SF_DATALESS != 0,
                    is_dir,
                    is_link: false,
                    name,
                });
            }
        }
        entries.extend(slow.into_iter().map(|name| {
            stat_entry(&path.join(&name)).unwrap_or_else(|_| {
                let hidden = name.starts_with('.');
                Entry { is_hidden: hidden, ..Entry::file(name, 0, 0.0) }
            })
        }));
        Ok(Some(entries))
    }

    fn record_name(rec: &[u8]) -> Option<String> {
        let rel: i32 = rd(rec, 24);
        let len: u32 = rd(rec, 28);
        let start = usize::try_from(24 + i64::from(rel)).ok()?;
        let bytes = rec.get(start..start + (len as usize).checked_sub(1)?)?;
        Some(match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(bytes).into_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

/// Iterator behind `LocalFS::walk`: the root, then each directory's children
/// (subdirectories first, each group by name) before descending into the
/// subdirectories one by one — the order of Python's `os.walk` top-down.
/// Links to directories are reported as non-directory links and not entered,
/// so copies reproduce the link instead of its target. Unreadable
/// subdirectories are skipped silently, as `os.walk` does.
pub(crate) struct Walk {
    pending: VecDeque<oc_vfs::Result<(RelPath, Entry)>>,
    /// Directories still to list, the next one last.
    stack: Vec<(RelPath, PathBuf)>,
}

impl Walk {
    pub(crate) fn new(root: &Path) -> io::Result<Walk> {
        let entry = stat_entry(root)?;
        let descend = entry.is_dir && !entry.is_link;
        let mut pending = VecDeque::new();
        pending.push_back(Ok((RelPath::new(), entry)));
        let stack = if descend { vec![(RelPath::new(), root.to_path_buf())] } else { Vec::new() };
        Ok(Walk { pending, stack })
    }

    fn expand(&mut self, rel: RelPath, dir: PathBuf) {
        let Ok(rd) = fs::read_dir(&dir) else { return };
        let mut dirs: Vec<(String, Entry)> = Vec::new();
        let mut files: Vec<(String, Entry)> = Vec::new();
        for de in rd {
            let Ok(de) = de else { continue };
            let name = name_string(de.file_name());
            let path = de.path();
            match stat_entry(&path) {
                Ok(mut entry) => {
                    if entry.is_dir {
                        if entry.is_link {
                            // Don't descend; copy the link itself.
                            entry.is_dir = false;
                            entry.is_executable = false;
                            entry.size = 0;
                            entry.is_online_only = false;
                        }
                        dirs.push((name, entry));
                    } else {
                        files.push((name, entry));
                    }
                }
                // Vanished since readdir: say which one.
                Err(e) => self.pending.push_back(Err(crate::error::with_path(e, &path))),
            }
        }
        dirs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let child_rel = |name: &str| {
            let mut r = Vec::with_capacity(rel.len() + 1);
            r.extend_from_slice(&rel);
            r.push(name.to_string());
            r
        };
        // Real subdirectories, pushed in reverse so the first is listed next.
        for (name, entry) in dirs.iter().rev() {
            if !entry.is_link {
                self.stack.push((child_rel(name), dir.join(name)));
            }
        }
        for (name, entry) in dirs.into_iter().chain(files) {
            self.pending.push_back(Ok((child_rel(&name), entry)));
        }
    }
}

impl Iterator for Walk {
    type Item = oc_vfs::Result<(RelPath, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Some(item);
            }
            let (rel, dir) = self.stack.pop()?;
            self.expand(rel, dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        assert_eq!(parent_of(Path::new("/a/b")), Some(PathBuf::from("/a")));
        assert_eq!(parent_of(Path::new("/a/b/")), Some(PathBuf::from("/a")));
        assert_eq!(parent_of(Path::new("/a")), Some(PathBuf::from("/")));
        assert_eq!(parent_of(Path::new("/")), None);
        assert_eq!(parent_of(Path::new("a")), Some(PathBuf::from(".")));
        assert_eq!(parent_of(Path::new(".")), None);
    }

    /// "/" has short records (a directory named "bin"; these used to make the
    /// whole listing fall back to readdir) and firmlinks; /System/Volumes
    /// holds mount points. All must show what stat shows.
    #[cfg(target_os = "macos")]
    #[test]
    fn bulk_listing_of_system_folders_and_mount_points() {
        use std::os::unix::fs::MetadataExt;
        for dir in ["/", "/System/Volumes"] {
            let d = Path::new(dir);
            let Ok(dev) = fs::metadata(d).map(|m| m.dev()) else { continue };
            let mut bulk = bulk::read_entries(d).unwrap().unwrap_or_else(|| panic!("bulk read of {dir} gave up"));
            let mut generic = read_entries_generic(d).unwrap();
            bulk.sort_by(|a, b| a.name.cmp(&b.name));
            generic.sort_by(|a, b| a.name.cmp(&b.name));
            assert_eq!(bulk.len(), generic.len(), "{dir}");
            for (b, g) in bulk.iter().zip(&generic) {
                if dir == "/" && b.name == "dev" {
                    // Its time moves whenever a device node comes or goes.
                    assert!(b.is_dir && g.is_dir);
                    continue;
                }
                let mount_point = !b.is_link && fs::metadata(d.join(&b.name)).is_ok_and(|m| m.dev() != dev);
                assert_eq!(b, g, "{dir}/{} (mount point: {mount_point})", b.name);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bulk_listing_matches_readdir_and_stat() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        fs::write(d.join("plain.txt"), "12345").unwrap();
        fs::write(d.join("run"), "#!").unwrap();
        fs::set_permissions(d.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(d.join("flagged"), "").unwrap();
        assert!(std::process::Command::new("chflags").arg("hidden").arg(d.join("flagged")).status().unwrap().success());
        fs::write(d.join(".dot"), "x").unwrap();
        fs::create_dir(d.join("folder")).unwrap();
        // Short names make the shortest records (a directory's is 64 bytes).
        fs::create_dir(d.join("go")).unwrap();
        fs::write(d.join("Ünïcödé ✓.txt"), "u").unwrap();
        symlink("folder", d.join("dirlink")).unwrap();
        symlink("plain.txt", d.join("filelink")).unwrap();
        symlink("nowhere", d.join("dangling")).unwrap();
        let key = |mut v: Vec<Entry>| {
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        };
        let bulk = key(bulk::read_entries(d).unwrap().expect("APFS supports bulk reads"));
        let generic = key(read_entries_generic(d).unwrap());
        assert_eq!(bulk.len(), 10);
        for (b, g) in bulk.iter().zip(&generic) {
            assert_eq!(b.name, g.name);
            assert!((b.mtime - g.mtime).abs() < 1e-6, "{}", b.name);
            let (mut b, mut g) = (b.clone(), g.clone());
            b.mtime = 0.0;
            g.mtime = 0.0;
            assert_eq!(b, g);
        }
        assert!(bulk.iter().find(|e| e.name == "flagged").unwrap().is_hidden);
        assert!(bulk::read_entries(&d.join("plain.txt")).is_err());
        assert!(bulk::read_entries(&d.join("missing")).unwrap_err().kind() == io::ErrorKind::NotFound);
    }
}
