//! Reading and writing the edited file (worker side; blocking).

use std::io::{Read, Write};
use std::path::Path;

use oc_preview::textfile::{self, TextFile};
use oc_vfs::{VPath, VfsError};

/// What loading produced.
pub enum Loaded {
    Text(TextFile),
    /// Not a text file (the editor shows why, read-only).
    Binary,
    Failed(String),
}

/// Read and decode `location`.
pub fn load(location: &VPath) -> Loaded {
    let mut data = Vec::new();
    let read = location.open_read().and_then(|mut f| f.read_to_end(&mut data).map_err(VfsError::from));
    if let Err(e) = read {
        return Loaded::Failed(e.to_string());
    }
    match textfile::decode(&data) {
        Ok(f) => Loaded::Text(f),
        Err(_) => Loaded::Binary,
    }
}

/// (modified time, size) of a local file, to tell our own saves from outside changes.
pub fn signature(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Save `data` to `location`: local files through a temporary file next to
/// it and an atomic replace (a crash can't leave half a file; permissions
/// are kept), others with `open_write`.
pub fn save(location: &VPath, data: &[u8]) -> Result<(), String> {
    match location.local_path() {
        Some(path) => write_atomically(&path, data).map_err(|e| e.to_string()),
        None => {
            let mut w = location.fs.open_write(&location.path).map_err(|e| e.to_string())?;
            w.write_all(data).map_err(|e| e.to_string())?;
            w.finish().map_err(|e| e.to_string())
        }
    }
}

/// Port of 1.x `_write_atomically`: write next to the file, then swap it in.
pub fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // A symlink (dotfiles kept in a repository…): replace the file it points
    // to, not the link itself.
    let resolved = std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_symlink())
        .then(|| std::fs::canonicalize(path).ok())
        .flatten();
    let path = resolved.as_deref().unwrap_or(path);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{name}.")).suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    if let Some(p) = perms {
        std::fs::set_permissions(tmp.path(), p)?;
    }
    // On failure the temporary file is removed when `tmp` drops.
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Create an empty file (Shift+F4); fails if the name is taken.
pub fn new_file(folder: &VPath, name: &str) -> Result<VPath, String> {
    let location = folder.child(&[name]);
    if location.exists() {
        return Err("a file with this name already exists".into());
    }
    let w = location.fs.open_write(&location.path).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(location)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn atomic_writes_keep_permissions_and_leave_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.txt");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomically(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777, 0o640);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        // Through a symlink: the target changes, the link stays a link.
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        write_atomically(&link, b"via link").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&p).unwrap(), b"via link");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
    }

    #[test]
    fn new_files_and_binary_detection() {
        let mem = oc_vfs::testing::MemFS::new();
        mem.add_file("bin.dat", b"\0\x01\x02".to_vec());
        let root = mem.root();
        let created = new_file(&root, "n.md").unwrap();
        assert!(created.exists());
        assert!(new_file(&root, "n.md").unwrap_err().contains("already exists"));
        assert!(matches!(load(&root.child(&["bin.dat"])), Loaded::Binary));
        assert!(matches!(load(&root.child(&["missing"])), Loaded::Failed(_)));
        save(&created, b"# hi\n").unwrap();
        match load(&created) {
            Loaded::Text(f) => assert_eq!(f.text, "# hi\n"),
            _ => panic!("text expected"),
        }
    }
}
