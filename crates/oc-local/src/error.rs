//! I/O errors with the path they happened on.
//!
//! `std::io::Error` doesn't say which file failed, but error messages must:
//! "You don't have permission to access /x" ([`oc_vfs::io_message`]). The
//! error kind is kept, so `VfsError::is_not_found` and friends still work.

use std::io;
use std::path::Path;

use oc_vfs::VfsError;

/// The error in words, naming `path`.
pub(crate) fn with_path(err: io::Error, path: &Path) -> VfsError {
    oc_vfs::io_error_at(err, &path.display().to_string())
}

/// For two-path calls (rename, copy, symlink): a problem with the target
/// (it exists, its disk is full or read-only) names the target, anything
/// else the source with the target after it.
pub(crate) fn with_paths(err: io::Error, src: &Path, dst: &Path) -> VfsError {
    use io::ErrorKind as K;
    if matches!(err.kind(), K::AlreadyExists | K::StorageFull | K::ReadOnlyFilesystem) {
        return with_path(err, dst);
    }
    let msg = format!("{} (to {})", oc_vfs::io_message(&err, &src.display().to_string()), dst.display());
    VfsError::Io(io::Error::new(err.kind(), msg))
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
    fn messages_name_the_path_and_keep_the_kind() {
        let e = with_path(io::Error::from_raw_os_error(2), Path::new("/nope"));
        assert!(e.is_not_found());
        assert_eq!(e.to_string(), "/nope not found");

        let e = with_path(io::Error::from(io::ErrorKind::PermissionDenied), Path::new("/x"));
        assert_eq!(e.to_string(), "You don't have permission to access /x");

        let e = with_paths(io::Error::other("boom"), Path::new("/a"), Path::new("/b"));
        assert_eq!(e.to_string(), "Boom: /a (to /b)");
        let e = with_paths(io::Error::from(io::ErrorKind::AlreadyExists), Path::new("/a"), Path::new("/b"));
        assert_eq!(e.to_string(), "/b already exists");
    }
}
