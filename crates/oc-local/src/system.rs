//! Places, free space and handing files to other apps.

use std::io;
use std::path::{Path, PathBuf};

/// The user's home directory (`HOME` / `USERPROFILE`), "/" if unset.
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var_os(var) {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }),
    }
}

/// Places worth jumping to, as (label, path): drives on Windows, "/" and home
/// plus mounted volumes elsewhere.
pub fn locations() -> Vec<(String, PathBuf)> {
    let home = home_dir();
    #[cfg(windows)]
    {
        let mut result: Vec<(String, PathBuf)> =
            windows_drives().into_iter().map(|d| (d.trim_end_matches('\\').to_string(), PathBuf::from(d))).collect();
        result.push(("Home".to_string(), home));
        result
    }
    #[cfg(not(windows))]
    {
        let mut result = vec![("/".to_string(), PathBuf::from("/")), ("Home".to_string(), home)];
        let mount_roots: Vec<PathBuf> = if cfg!(target_os = "macos") {
            vec![PathBuf::from("/Volumes")]
        } else {
            let user = std::env::var_os("USER").unwrap_or_default();
            vec![Path::new("/media").join(&user), Path::new("/run/media").join(&user), PathBuf::from("/mnt")]
        };
        for root in mount_roots {
            result.extend(mounts_in(&root));
        }
        result
    }
}

/// Subdirectories of a mount root (e.g. /Volumes), sorted by path.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn mounts_in(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut children: Vec<PathBuf> = rd.filter_map(|de| de.ok().map(|de| de.path())).collect();
    children.sort();
    children
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| (p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), p))
        .collect()
}

/// The drive roots for [`crate::DrivesFS`]: Windows' drives, none elsewhere.
pub(crate) fn drive_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    return windows_drives().into_iter().map(PathBuf::from).collect();
    #[cfg(not(windows))]
    Vec::new()
}

/// "C:\\", "D:\\", … for every drive letter Windows reports, without touching
/// the drives (a stat of an empty card reader or floppy can hang).
#[cfg(windows)]
fn windows_drives() -> Vec<String> {
    // SAFETY: no arguments; returns a bitmask of available drive letters.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i) as char)).collect()
}

/// Free bytes on the volume holding `path` (like Python's `shutil.disk_usage().free`).
pub fn disk_free(path: &Path) -> Option<u64> {
    disk_free_impl(path).ok()
}

#[cfg(target_os = "macos")]
fn disk_free_impl(path: &Path) -> io::Result<u64> {
    // statfs, not statvfs: Darwin's statvfs has 32-bit block counts.
    let c = c_path(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: c is NUL-terminated; st is written by statfs on success.
    if unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so st is initialized.
    let st = unsafe { st.assume_init() };
    Ok(st.f_bavail.saturating_mul(u64::from(st.f_bsize)))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn disk_free_impl(path: &Path) -> io::Result<u64> {
    let c = c_path(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c is NUL-terminated; st is written by statvfs on success.
    if unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned 0, so st is initialized.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::useless_conversion)] // the field widths differ between targets
    Ok(u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize)))
}

#[cfg(windows)]
fn disk_free_impl(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: wide is NUL-terminated; the out pointers are valid u64s.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}

#[cfg(not(any(unix, windows)))]
fn disk_free_impl(_path: &Path) -> io::Result<u64> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(unix)]
pub(crate) fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Open `path` with the OS default application, without waiting for it.
pub fn open_with_default_app(path: &Path) -> io::Result<()> {
    // In words: callers show it after "Can't open <name>: ".
    open::that_detached(path).map_err(|e| io::Error::new(e.kind(), oc_vfs::io_reason(&e)))
}

/// Whether `item` (by the folder holding it, so a link counts where it
/// lies) is on the same volume as the folder `dir`: there a system file
/// manager moves a dragged item, elsewhere it copies. False when unsure.
pub fn same_volume(item: &Path, dir: &Path) -> bool {
    let holder = item.parent().unwrap_or(item);
    match (volume_id(holder), volume_id(dir)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(unix)]
fn volume_id(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev().to_string())
}

#[cfg(windows)]
fn volume_id(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 1024];
    // SAFETY: wide is NUL-terminated; buf's length is passed along.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]).to_lowercase())
}

#[cfg(not(any(unix, windows)))]
fn volume_id(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(disk_free(dir.path()).is_some_and(|n| n > 0));
        assert_eq!(disk_free(&dir.path().join("missing/deeper")), None);
    }

    #[test]
    fn locations_start_with_root_and_home() {
        let locs = locations();
        #[cfg(not(windows))]
        {
            assert_eq!(locs[0], ("/".to_string(), PathBuf::from("/")));
            assert_eq!(locs[1].0, "Home");
        }
        #[cfg(windows)]
        assert_eq!(locs.last().unwrap().0, "Home");
    }

    #[test]
    fn mount_roots_list_sorted_subdirectories_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Zeta")).unwrap();
        std::fs::create_dir(dir.path().join("Alpha")).unwrap();
        std::fs::write(dir.path().join("file"), b"").unwrap();
        let names: Vec<String> = mounts_in(dir.path()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Alpha", "Zeta"]);
        assert!(mounts_in(&dir.path().join("missing")).is_empty());
    }
}

