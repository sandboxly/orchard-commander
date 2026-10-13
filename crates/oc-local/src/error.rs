//! I/O errors with the path they happened on.
//!
//! `std::io::Error` doesn't say which file failed, but error dialogs must;
//! the Python version showed `OSError`'s text ("[Errno 13] Permission
//! denied: '/x'"), so we produce the same shape. The error kind is kept, so
//! `VfsError::is_not_found` and friends still work.

use std::io;
use std::path::Path;

use oc_vfs::VfsError;

/// `[Errno N] reason: 'path'`, like Python's `OSError.__str__`.
pub(crate) fn with_path(err: io::Error, path: &Path) -> VfsError {
    let msg = match err.raw_os_error() {
        Some(code) => format!("[Errno {code}] {}: '{}'", strerror(&err), path.display()),
        None => format!("{err}: '{}'", path.display()),
    };
    VfsError::Io(io::Error::new(err.kind(), msg))
}

/// `[Errno N] reason: 'src' -> 'dst'` for two-path calls (rename, symlink).
pub(crate) fn with_paths(err: io::Error, src: &Path, dst: &Path) -> VfsError {
    let msg = match err.raw_os_error() {
        Some(code) => format!("[Errno {code}] {}: '{}' -> '{}'", strerror(&err), src.display(), dst.display()),
        None => format!("{err}: '{}' -> '{}'", src.display(), dst.display()),
    };
    VfsError::Io(io::Error::new(err.kind(), msg))
}

/// The OS's description without std's " (os error N)" suffix.
fn strerror(err: &io::Error) -> String {
    let s = err.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// `.at(path)` / `.at2(src, dst)` on io results.
pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> oc_vfs::Result<T>;
    fn at2(self, src: &Path, dst: &Path) -> oc_vfs::Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> oc_vfs::Result<T> {
        self.map_err(|e| with_path(e, path))
    }

    fn at2(self, src: &Path, dst: &Path) -> oc_vfs::Result<T> {
        self.map_err(|e| with_paths(e, src, dst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_like_messages_keep_the_kind() {
        let e = with_path(io::Error::from_raw_os_error(2), Path::new("/nope"));
        assert!(e.is_not_found());
        let msg = e.to_string();
        assert!(msg.starts_with("[Errno 2] "), "{msg}");
        assert!(msg.ends_with(": '/nope'"), "{msg}");
        assert!(!msg.contains("os error"), "{msg}");

        let e = with_paths(io::Error::other("boom"), Path::new("/a"), Path::new("/b"));
        assert_eq!(e.to_string(), "boom: '/a' -> '/b'");
    }
}
