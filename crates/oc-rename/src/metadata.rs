//! File metadata for multi-rename placeholders like `[=artist]` or `[=height]`.
//!
//! Classic two-panel file managers get these from content plugins
//! (`[=plugin.field]`); here
//! they come from [`MetadataProvider`]s — built-ins for audio tags (lofty),
//! video and image properties (container headers) and photo EXIF
//! (kamadak-exif) — plus whatever the application [`register`]s:
//!
//! ```ignore
//! struct Pages;
//! impl MetadataProvider for Pages {
//!     fn id(&self) -> &str { "pages" }
//!     fn label(&self) -> &str { "Documents" }
//!     fn fields(&self) -> &[Field] { const F: &[Field] = &[Field::new("pages", "Number of pages")]; F }
//!     fn handles(&self, name: &str) -> bool { name.ends_with(".doc") }
//!     fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError> { todo!() }
//! }
//! oc_rename::metadata::register(Arc::new(Pages));
//! ```
//!
//! Field names are shared: several providers may offer "title" or "width";
//! for a file, the first provider that handles it and has a value wins.
//! Values are strings, already cleaned up for use in file names.
//!
//! The providers and the registry are shared with the other Orchard apps
//! (`orchard_metadata`, re-exported here); reading a file from any file
//! system ([`read`], [`read_with`]) is this crate's.

use std::io::{self, Read, Seek, SeekFrom};

use oc_vfs::VPath;

pub use orchard_metadata::*;

/// All metadata fields for a file from the application's registry (blocking).
pub fn read(location: &VPath, is_dir: bool, online_only: bool) -> Result<Metadata, MetadataUnavailable> {
    read_with(&registry(), location, is_dir, online_only)
}

/// All metadata fields for a file from `registry` (blocking).
///
/// Files elsewhere than on a local disk are streamed (and buffered if
/// they can't seek); cloud-only placeholders are not read — that would
/// download them.
pub fn read_with(
    registry: &Registry,
    location: &VPath,
    is_dir: bool,
    online_only: bool,
) -> Result<Metadata, MetadataUnavailable> {
    let name = location.name();
    if !registry.handled(&name) || is_dir {
        return Ok(Metadata::new());
    }
    if online_only {
        return Err(MetadataUnavailable("cloud-only file, not downloaded".into()));
    }
    let local = location.local_path();
    let mut file: Box<dyn ReadSeek> = match &local {
        // Like the providers opening the path themselves: an unreadable
        // local file just has no metadata.
        Some(path) => match std::fs::File::open(path) {
            Ok(f) => Box::new(f),
            Err(_) => return Ok(Metadata::new()),
        },
        None => {
            let stream = location.open_read().map_err(|e| MetadataUnavailable(e.to_string()))?;
            if stream.seekable() {
                Box::new(stream)
            } else {
                spool(stream).map_err(|e| MetadataUnavailable(e.to_string()))?
            }
        }
    };
    Ok(registry.read_source(&name, local.as_deref(), &mut *file))
}

/// Files that can't seek (some archive members) up to this size are kept in
/// memory; bigger ones (a film in a ZIP) go to an anonymous temporary file.
const SPOOL_IN_MEMORY: u64 = 16 << 20;

/// A seekable copy of `stream`.
fn spool(stream: Box<dyn oc_vfs::ReadStream>) -> io::Result<Box<dyn ReadSeek>> {
    let mut data = Vec::new();
    let mut stream = stream.take(SPOOL_IN_MEMORY + 1);
    stream.read_to_end(&mut data)?;
    if data.len() as u64 <= SPOOL_IN_MEMORY {
        return Ok(Box::new(io::Cursor::new(data)));
    }
    let mut file = tempfile::tempfile()?;
    io::Write::write_all(&mut file, &data)?;
    drop(data);
    stream.set_limit(u64::MAX);
    io::copy(&mut stream, &mut file)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Box::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoSeek(io::Cursor<Vec<u8>>);
    impl Read for NoSeek {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Seek for NoSeek {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
    impl oc_vfs::ReadStream for NoSeek {
        fn seekable(&self) -> bool {
            false
        }
    }

    #[test]
    fn unseekable_streams_are_spooled() {
        for len in [0, 10, SPOOL_IN_MEMORY as usize, SPOOL_IN_MEMORY as usize + 3] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut copy = spool(Box::new(NoSeek(io::Cursor::new(data.clone())))).unwrap();
            assert_eq!(copy.seek(SeekFrom::End(0)).unwrap(), len as u64);
            copy.seek(SeekFrom::Start(0)).unwrap();
            let mut back = Vec::new();
            copy.read_to_end(&mut back).unwrap();
            assert!(back == data, "length {len}");
        }
    }
}
