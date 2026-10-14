//! Tar archives, plain or compressed (gz/bz2/xz, detected by magic bytes).
//!
//! Plain tars are read in place: each member is a byte range of the file, so
//! streams are seekable and listing skips data by seeking. Compressed tars
//! have to be decoded from the start to reach a member; their streams seek
//! by decoding again (see [`Decoded`]).

use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ::tar::EntryType;
use oc_vfs::{ReadStream, Result, VfsError};

use crate::archive::{Archive, Format, ListError, Listing, Member, Source};
use crate::io::{Decoded, HandleSlot, TempFileStream, Window, empty_stream};
use crate::single::Compression;
use crate::{cache_dir, dirname, normalize, normpath};

#[derive(Clone, Debug)]
pub(crate) enum TarSource {
    /// Contiguous data at `offset` in the (decompressed) tar stream.
    Data { offset: u64, size: u64 },
    /// A GNU sparse file: decoded by the tar crate; `ordinal` counts entries.
    Sparse { ordinal: usize },
    /// A symlink or hard link, resolved to an archive path (None: points outside).
    Link { target: Option<String> },
    /// Devices, fifos, directories: reading gives nothing.
    Empty,
}

/// Follow at most this many links (cycles).
const MAX_LINK_DEPTH: usize = 16;

pub(crate) fn list(path: &Path) -> std::result::Result<Listing, ListError> {
    let slot = Arc::new(HandleSlot::new(path)?);
    let file = slot.get()?;
    let compression = Compression::sniff(&file);
    let mut members = Vec::new();
    if compression == Compression::None {
        let mut ar = ::tar::Archive::new(Window::whole(file));
        for (ordinal, entry) in ar.entries_with_seek()?.enumerate() {
            add_entry(&entry?, ordinal, &mut members)?;
        }
    } else {
        let mut ar = ::tar::Archive::new(compression.decoder(Window::whole(file))?);
        for (ordinal, entry) in ar.entries()?.enumerate() {
            add_entry(&entry?, ordinal, &mut members)?;
        }
    }
    Ok(Listing {
        format: Box::new(TarFormat {
            slot,
            compression,
            parked: Arc::new(Mutex::new(Vec::new())),
        }),
        members,
    })
}

fn add_entry<R: Read>(
    entry: &::tar::Entry<'_, R>,
    ordinal: usize,
    out: &mut Vec<(Member, Source)>,
) -> io::Result<()> {
    let header = entry.header();
    let type_byte = header.as_bytes()[156];
    let mut kind = header.entry_type();
    if matches!(
        kind,
        EntryType::XGlobalHeader
            | EntryType::XHeader
            | EntryType::GNULongName
            | EntryType::GNULongLink
    ) || type_byte == b'X'
    // Solaris extended header: tarfile reads it as pax
    {
        return Ok(());
    }
    let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
    // Old V7 tars store directories as regular files ending in "/" (type
    // NUL, which the tar crate folds into Regular); tarfile turns them back.
    if type_byte == 0 && raw.ends_with('/') {
        kind = EntryType::Directory;
    }
    let Some(name) = normalize(&raw) else {
        return Ok(());
    };
    if name.is_empty() {
        return Ok(());
    }
    let mtime = header.mtime().unwrap_or(0) as f64;
    let link = || {
        entry
            .link_name_bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let (member, src) = match kind {
        EntryType::Directory => (Member::new(name, true, 0, mtime), TarSource::Empty),
        EntryType::Regular | EntryType::Continuous => {
            let size = entry.size();
            (
                Member::new(name, false, size, mtime),
                TarSource::Data {
                    offset: entry.raw_file_position(),
                    size,
                },
            )
        }
        EntryType::GNUSparse => (
            Member::new(name, false, entry.size(), mtime),
            TarSource::Sparse { ordinal },
        ),
        EntryType::Symlink => {
            // Relative to the link's directory, like tarfile's _find_link_target.
            let target = normpath(&format!("{}/{}", dirname(&name), link()));
            (
                Member {
                    is_link: true,
                    ..Member::new(name, false, 0, mtime)
                },
                TarSource::Link { target },
            )
        }
        EntryType::Link => {
            let target = normpath(&link());
            (
                Member::new(name, false, 0, mtime),
                TarSource::Link { target },
            )
        }
        // Devices and fifos: nothing to read.
        EntryType::Char | EntryType::Block | EntryType::Fifo => {
            (Member::new(name, false, 0, mtime), TarSource::Empty)
        }
        // Unknown types are read as plain data, like tarfile (which still
        // lists them with size 0, as they aren't "regular").
        _ => (
            Member::new(name, false, 0, mtime),
            TarSource::Data {
                offset: entry.raw_file_position(),
                size: entry.size(),
            },
        ),
    };
    out.push((member, Source::Tar(src)));
    Ok(())
}

struct TarFormat {
    slot: Arc<HandleSlot>,
    compression: Compression,
    /// Decoders of a compressed tar left where their last member ended.
    parked: Parked,
}

/// Reaching a member of a compressed tar means decoding everything before
/// it. Python kept one decompressor that only ever seeked forward, so copying
/// a whole folder out of a .tar.gz was linear; opening a fresh decoder per
/// member would make it quadratic. Instead, a finished member stream parks
/// its decoder here and the next member at or after that point continues
/// from it. Each stream still owns its decoder while reading, so threads
/// never share one.
type Parked = Arc<Mutex<Vec<ParkedDecoder>>>;

/// How many decoders to keep per archive (xz ones can hold a large
/// dictionary). Two lets a copy and a preview each keep their place.
const MAX_PARKED: usize = 2;

struct ParkedDecoder {
    /// Position in the decompressed tar stream.
    pos: u64,
    dec: Box<dyn Read + Send>,
}

/// One member's bytes from a (possibly parked) decoder of the whole stream.
struct PooledMember {
    dec: Option<Box<dyn Read + Send>>,
    pos: u64,
    end: u64,
    parked: Parked,
}

impl PooledMember {
    /// A decoder at `offset`: the parked one closest before it, else a new one.
    fn open(
        parked: &Parked,
        compression: Compression,
        file: &Arc<std::fs::File>,
        offset: u64,
        size: u64,
    ) -> io::Result<Self> {
        let reuse = {
            let mut pool = parked.lock().unwrap_or_else(|p| p.into_inner());
            pool.iter()
                .enumerate()
                .filter(|(_, p)| p.pos <= offset)
                .max_by_key(|(_, p)| p.pos)
                .map(|(i, _)| i)
                .map(|i| pool.remove(i))
        };
        let (mut dec, pos) = match reuse {
            Some(p) => (p.dec, p.pos),
            None => (compression.decoder(Window::whole(file.clone()))?, 0),
        };
        let want = offset - pos;
        let skipped = io::copy(&mut (&mut dec).take(want), &mut io::sink())?;
        if skipped < want {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive is incomplete or damaged",
            ));
        }
        Ok(PooledMember {
            dec: Some(dec),
            pos: offset,
            end: offset.saturating_add(size),
            parked: parked.clone(),
        })
    }
}

impl Read for PooledMember {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(dec) = self.dec.as_mut() else {
            return Ok(0);
        };
        let want = (buf.len() as u64).min(self.end - self.pos) as usize;
        if want == 0 {
            return Ok(0);
        }
        match dec.read(&mut buf[..want]) {
            Ok(n) => {
                self.pos += n as u64;
                Ok(n)
            }
            Err(e) => {
                // A failed decoder's state is unknown: never park it.
                if e.kind() != io::ErrorKind::Interrupted {
                    self.dec = None;
                }
                Err(e)
            }
        }
    }
}

impl Drop for PooledMember {
    fn drop(&mut self) {
        if let Some(dec) = self.dec.take() {
            let mut pool = self.parked.lock().unwrap_or_else(|p| p.into_inner());
            pool.push(ParkedDecoder { pos: self.pos, dec });
            if pool.len() > MAX_PARKED {
                pool.remove(0); // the longest parked
            }
        }
    }
}

impl TarFormat {
    fn open_depth(
        &self,
        archive: &Archive,
        idx: usize,
        depth: usize,
    ) -> Result<Box<dyn ReadStream>> {
        let Source::Tar(src) = archive.source(idx) else {
            return Ok(empty_stream());
        };
        match *src {
            TarSource::Data { offset, size } => {
                let file = self.slot.get()?;
                if self.compression == Compression::None {
                    return Ok(Box::new(Window::new(file, offset, size)));
                }
                let compression = self.compression;
                let parked = self.parked.clone();
                let opener = Box::new(move || {
                    PooledMember::open(&parked, compression, &file, offset, size)
                        .map(|m| Box::new(m) as Box<dyn Read + Send>)
                });
                Ok(Box::new(Decoded::new(opener, Some(size))))
            }
            TarSource::Sparse { ordinal } => self.open_sparse(ordinal),
            TarSource::Link { ref target } => {
                let name = archive.member_name(idx);
                let target_idx = target
                    .as_deref()
                    .and_then(|t| archive.index_of(t))
                    .filter(|_| depth < MAX_LINK_DEPTH)
                    .ok_or_else(|| {
                        crate::archive::missing_link_target(name)
                    })?;
                if archive.member_at(target_idx).is_dir {
                    return Ok(empty_stream());
                }
                self.open_depth(archive, target_idx, depth + 1)
            }
            TarSource::Empty => Ok(empty_stream()),
        }
    }

    /// The tar crate expands sparse files only while iterating, so decode
    /// into a temp file.
    fn open_sparse(&self, ordinal: usize) -> Result<Box<dyn ReadStream>> {
        let file = self.slot.get()?;
        let mut tmp = tempfile::tempfile_in(cache_dir()?)?;
        let mut ar = ::tar::Archive::new(self.compression.decoder(Window::whole(file))?);
        let mut entry = ar
            .entries()?
            .nth(ordinal)
            .ok_or_else(|| VfsError::Operation("sparse member vanished".into()))??;
        io::copy(&mut entry, &mut tmp)?;
        Ok(Box::new(TempFileStream::new(tmp, None)?))
    }
}

impl Format for TarFormat {
    fn open(&self, archive: &Archive, idx: usize) -> Result<Box<dyn ReadStream>> {
        self.open_depth(archive, idx, 0)
    }

    fn close(&self) {
        // Parked decoders hold the file too (it must not stay locked on Windows).
        self.parked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.slot.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gz_file(data: &[u8]) -> (tempfile::NamedTempFile, Arc<std::fs::File>) {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        w.write_all(data).unwrap();
        t.write_all(&w.finish().unwrap()).unwrap();
        let f = Arc::new(std::fs::File::open(t.path()).unwrap());
        (t, f)
    }

    fn read(m: &mut PooledMember) -> Vec<u8> {
        let mut v = Vec::new();
        m.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn finished_members_park_their_decoder_for_the_next_one() {
        let data: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
        let (_t, file) = gz_file(&data);
        let parked: Parked = Arc::default();
        let open = |off, len| PooledMember::open(&parked, Compression::Gzip, &file, off, len);

        let mut a = open(0, 1000).unwrap();
        assert_eq!(read(&mut a), &data[..1000]);
        drop(a);
        assert_eq!(
            parked
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.pos)
                .collect::<Vec<_>>(),
            [1000]
        );

        // A later member continues from the parked decoder (taking it out).
        let mut b = open(5000, 1000).unwrap();
        assert!(parked.lock().unwrap().is_empty());
        assert_eq!(read(&mut b), &data[5000..6000]);
        drop(b);

        // An earlier one can't use it: a fresh decoder; both stay parked.
        let mut c = open(100, 10).unwrap();
        assert_eq!(read(&mut c), &data[100..110]);
        drop(c);
        let mut pos: Vec<u64> = parked.lock().unwrap().iter().map(|p| p.pos).collect();
        pos.sort();
        assert_eq!(pos, [110, 6000]);

        // The closest one before the offset is picked; the pool stays bounded.
        let mut d = open(7000, 5).unwrap();
        assert_eq!(read(&mut d), &data[7000..7005]);
        assert_eq!(
            parked
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.pos)
                .collect::<Vec<_>>(),
            [110]
        );
        let e = open(200, 5).unwrap();
        let f = open(300, 5).unwrap();
        drop((d, e, f));
        assert_eq!(parked.lock().unwrap().len(), MAX_PARKED);

        // Past the end of the stream: an error, not a short member.
        assert!(open(40_000, 5).is_err());
    }
}
