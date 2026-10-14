//! What the info window shows about a local item beyond its listing entry:
//! creation and access times, permissions, owner, where a link points.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// More about an item than a listing shows. `None` where the system or the
/// file system doesn't say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    /// Seconds since the epoch, like [`oc_vfs::Entry::mtime`].
    pub created: Option<f64>,
    pub accessed: Option<f64>,
    /// "rwxr-xr-x" on Unix, the attributes on Windows ("read-only, hidden").
    pub permissions: Option<String>,
    /// "name (group)" on Unix.
    pub owner: Option<String>,
    /// A link's target as stored in the link.
    pub link_target: Option<PathBuf>,
}

/// [`Details`] of `path` (a link itself, not what it points to).
pub fn details(path: &Path) -> io::Result<Details> {
    let lstat = fs::symlink_metadata(path)?;
    let link_target = lstat.file_type().is_symlink().then(|| fs::read_link(path).ok()).flatten();
    Ok(Details {
        created: lstat.created().ok().and_then(secs),
        accessed: lstat.accessed().ok().and_then(secs),
        permissions: permissions(&lstat),
        owner: owner(&lstat),
        link_target,
    })
}

fn secs(t: SystemTime) -> Option<f64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs_f64()).filter(|s| *s > 0.0)
}

#[cfg(unix)]
fn permissions(md: &Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = md.permissions().mode();
    let bits = [(0o400, 'r'), (0o200, 'w'), (0o100, 'x'), (0o40, 'r'), (0o20, 'w'), (0o10, 'x'), (0o4, 'r'), (0o2, 'w'), (0o1, 'x')];
    Some(bits.iter().map(|&(bit, c)| if mode & bit != 0 { c } else { '-' }).collect())
}

#[cfg(windows)]
fn permissions(md: &Metadata) -> Option<String> {
    use std::os::windows::fs::MetadataExt;
    let attrs = md.file_attributes();
    let names = [(0x1, "read-only"), (0x2, "hidden"), (0x4, "system"), (0x20, "archive"), (0x800, "compressed"), (0x4000, "encrypted")];
    let set: Vec<&str> = names.iter().filter(|(bit, _)| attrs & bit != 0).map(|(_, n)| *n).collect();
    Some(if set.is_empty() { "none".to_string() } else { set.join(", ") })
}

#[cfg(not(any(unix, windows)))]
fn permissions(_md: &Metadata) -> Option<String> {
    None
}

#[cfg(unix)]
fn owner(md: &Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let user = user_name(md.uid()).unwrap_or_else(|| md.uid().to_string());
    Some(match group_name(md.gid()) {
        Some(group) => format!("{user} ({group})"),
        None => user,
    })
}

#[cfg(not(unix))]
fn owner(_md: &Metadata) -> Option<String> {
    None
}

/// Calls `get` with a growing buffer until it fits; `get` returns the
/// errno and whether an entry was found.
#[cfg(unix)]
fn lookup(mut get: impl FnMut(&mut [libc::c_char]) -> (i32, bool)) -> bool {
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        match get(&mut buf) {
            (libc::ERANGE, _) if buf.len() < 1 << 20 => buf.resize(buf.len() * 2, 0),
            (0, found) => return found,
            _ => return false,
        }
    }
}

#[cfg(unix)]
fn c_name(p: *const libc::c_char) -> Option<String> {
    // SAFETY: p points to a NUL-terminated name inside the caller's buffer.
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
    let mut name = None;
    lookup(|buf| {
        // SAFETY: pw and result are written by getpwuid_r; buf's length is passed along.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let err = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut result) };
        if err == 0 && !result.is_null() {
            name = c_name(pw.pw_name);
        }
        (err, !result.is_null())
    });
    name
}

#[cfg(unix)]
fn group_name(gid: u32) -> Option<String> {
    let mut name = None;
    lookup(|buf| {
        // SAFETY: gr and result are written by getgrgid_r; buf's length is passed along.
        let mut gr: libc::group = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let err = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut result) };
        if err == 0 && !result.is_null() {
            name = c_name(gr.gr_name);
        }
        (err, !result.is_null())
    });
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_of_a_file_and_a_link() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("f.txt");
        fs::write(&file, "x").unwrap();
        let d = details(&file).unwrap();
        assert!(d.accessed.is_some() && d.link_target.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
            assert_eq!(details(&file).unwrap().permissions.as_deref(), Some("rw-r-----"));
            assert!(d.owner.is_some_and(|o| !o.is_empty()));
            std::os::unix::fs::symlink("f.txt", t.path().join("l")).unwrap();
            assert_eq!(details(&t.path().join("l")).unwrap().link_target, Some(PathBuf::from("f.txt")));
        }
        #[cfg(target_os = "macos")]
        assert!(d.created.is_some());
        assert!(details(&t.path().join("missing")).is_err());
    }
}
