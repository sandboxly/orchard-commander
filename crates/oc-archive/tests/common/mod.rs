//! Test helpers: a minimal local file system (oc-local is built separately),
//! a "remote" variant without local paths, and archive builders.

#![allow(dead_code)]

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oc_vfs::{
    Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, sort_entries,
};

pub fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

/// Local disk; with `remote`, it hides local paths so archives on it must be copied first.
pub struct TestFs {
    pub remote: bool,
}

fn entry_of(path: &Path) -> Result<Entry> {
    let meta = std::fs::symlink_metadata(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    Ok(if meta.is_dir() {
        Entry::dir(name, mtime)
    } else {
        Entry::file(name, meta.len(), mtime)
    })
}

impl FileSystem for TestFs {
    fn type_name(&self) -> String {
        if self.remote {
            "test server".into()
        } else {
            "local disk".into()
        }
    }

    fn identity(&self) -> String {
        format!("testfs:{}", self.remote)
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            out.push(entry_of(&e?.path())?);
        }
        Ok(sort_entries(out, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        entry_of(Path::new(path)).map_err(|e| {
            if e.is_not_found() {
                VfsError::NotFound(path.into())
            } else {
                e
            }
        })
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut p = PathBuf::from(path);
        p.extend(names);
        p.to_string_lossy().into_owned()
    }

    fn basename(&self, path: &str) -> String {
        Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(File::open(path)?))
    }

    fn walk<'a>(
        &'a self,
        path: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        Ok(Box::new(std::iter::once(
            self.stat(path).map(|e| (Vec::new(), e)),
        )))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        (!self.remote).then(|| PathBuf::from(path))
    }
}

pub fn local(path: &Path) -> VPath {
    VPath::new(Arc::new(TestFs { remote: false }), path.to_string_lossy())
}

pub fn remote(path: &Path) -> VPath {
    VPath::new(Arc::new(TestFs { remote: true }), path.to_string_lossy())
}

/// docs/readme.txt, docs/deep/x.txt, top.txt, .hidden
pub fn content(root: &Path) -> PathBuf {
    let c = root.join("content");
    std::fs::create_dir_all(c.join("docs/deep")).unwrap();
    std::fs::write(c.join("docs/readme.txt"), "read me").unwrap();
    std::fs::write(c.join("docs/deep/x.txt"), "x".repeat(5000)).unwrap();
    std::fs::write(c.join("top.txt"), "top").unwrap();
    std::fs::write(c.join(".hidden"), "h").unwrap();
    c
}

fn files_of(content: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![content.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(content)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, p));
            }
        }
    }
    out.sort();
    out
}

/// Build an archive of `content` in `dest`. Zips and 7z get no directory
/// entries: those must be inferred.
pub fn make(kind: &str, content: &Path, dest: &Path) -> PathBuf {
    let files = files_of(content);
    match kind {
        "zip" => {
            let out = dest.join("a.zip");
            let mut z = zip::ZipWriter::new(File::create(&out).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (rel, p) in &files {
                z.start_file(rel.as_str(), opts).unwrap();
                z.write_all(&std::fs::read(p).unwrap()).unwrap();
            }
            z.finish().unwrap();
            out
        }
        "tar" | "tar.gz" | "tar.bz2" | "tar.xz" => {
            let out = dest.join(format!("a.{kind}"));
            let f = File::create(&out).unwrap();
            let w: Box<dyn Write> = match kind {
                "tar" => Box::new(f),
                "tar.gz" => Box::new(flate2::write::GzEncoder::new(
                    f,
                    flate2::Compression::default(),
                )),
                "tar.bz2" => Box::new(bzip2::write::BzEncoder::new(
                    f,
                    bzip2::Compression::default(),
                )),
                _ => Box::new(liblzma::write::XzEncoder::new(f, 6)),
            };
            let mut t = tar::Builder::new(w);
            t.append_dir_all(".", content).unwrap();
            t.into_inner().unwrap().flush().unwrap();
            out
        }
        "7z" => {
            let out = dest.join("a.7z");
            let mut z = sevenz_rust2::ArchiveWriter::create(&out).unwrap();
            for (rel, p) in &files {
                z.push_archive_entry(
                    sevenz_rust2::ArchiveEntry::new_file(rel),
                    Some(File::open(p).unwrap()),
                )
                .unwrap();
            }
            z.finish().unwrap();
            out
        }
        "7z-solid" => {
            let out = dest.join("solid.7z");
            let mut z = sevenz_rust2::ArchiveWriter::create(&out).unwrap();
            let entries = files
                .iter()
                .map(|(rel, _)| sevenz_rust2::ArchiveEntry::new_file(rel))
                .collect();
            let readers = files
                .iter()
                .map(|(_, p)| File::open(p).unwrap().into())
                .collect();
            z.push_archive_entries(entries, readers).unwrap();
            z.finish().unwrap();
            out
        }
        _ => panic!("unknown kind {kind}"),
    }
}

/// What `vfs.copy` does for an archive source, minus conflicts: walk and
/// stream everything below `src` into `dst`. Returns (files, bytes).
pub fn extract_tree(src: &VPath, dst: &Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    for item in src.fs.walk(&src.path).unwrap() {
        let (rel, entry) = item.unwrap();
        let mut target = dst.to_path_buf();
        target.extend(&rel);
        if entry.is_dir {
            std::fs::create_dir_all(&target).unwrap();
            continue;
        }
        let mut r = src
            .fs
            .open_read(&src.fs.join(
                &src.path,
                &rel.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .unwrap();
        let mut w = File::create(&target).unwrap();
        bytes += io::copy(&mut r, &mut w).unwrap();
        files += 1;
    }
    (files, bytes)
}

pub fn read_all(r: &mut dyn ReadStream) -> Vec<u8> {
    let mut v = Vec::new();
    r.read_to_end(&mut v).unwrap();
    v
}
