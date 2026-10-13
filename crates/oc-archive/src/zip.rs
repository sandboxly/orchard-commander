//! Zip archives. Stored and deflated members (nearly all real-world zips)
//! are read straight from the file with positional reads — no lock, and
//! stored members are truly seekable. Other methods (bzip2, zstd, lzma, …)
//! go through the zip crate into a temp file.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use ::zip::{CompressionMethod, ZipArchive};
use oc_vfs::{ReadStream, Result, VfsError};

use crate::archive::{Archive, Format, ListError, Listing, Member, Source};
use crate::io::{Decoded, HandleSlot, SharedFile, Window, read_exact_at};
use crate::{local_time_to_epoch, normalize};

pub(crate) fn list(path: &Path) -> std::result::Result<Listing, ListError> {
    let slot = Arc::new(HandleSlot::new(path)?);
    let zip = ZipArchive::new(SharedFile::new(slot.clone()))?;
    let mut members = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let e = zip.by_index_data(i)?;
        let raw = match e.name() {
            Ok(n) => n.into_owned(),
            Err(_) => String::from_utf8_lossy(e.name_raw()).into_owned(),
        };
        let Some(name) = normalize(&raw) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mtime = e.last_modified().map_or(0.0, |t| {
            local_time_to_epoch(
                i32::from(t.year()),
                u32::from(t.month()),
                u32::from(t.day()),
                u32::from(t.hour()),
                u32::from(t.minute()),
                u32::from(t.second()),
            )
        });
        members.push((
            Member::new(name, raw.ends_with('/'), e.size(), mtime),
            Source::Zip(i),
        ));
    }
    Ok(Listing {
        format: Box::new(ZipFormat { slot, zip }),
        members,
    })
}

struct ZipFormat {
    slot: Arc<HandleSlot>,
    zip: ZipArchive<SharedFile>,
}

/// Offset of a member's data: the local header has its own (variable) name
/// and extra field lengths, so the central directory alone doesn't tell.
fn data_start(file: &std::fs::File, header_start: u64) -> io::Result<u64> {
    let mut h = [0u8; 30];
    read_exact_at(file, &mut h, header_start)?;
    if h[..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Bad magic number for file header",
        ));
    }
    let name_len = u64::from(u16::from_le_bytes([h[26], h[27]]));
    let extra_len = u64::from(u16::from_le_bytes([h[28], h[29]]));
    Ok(header_start + 30 + name_len + extra_len)
}

impl Format for ZipFormat {
    fn open(&self, archive: &Archive, idx: usize) -> Result<Box<dyn ReadStream>> {
        let Source::Zip(i) = *archive.source(idx) else {
            return Ok(crate::io::empty_stream());
        };
        let e = self
            .zip
            .by_index_data(i)
            .map_err(|e| VfsError::Operation(e.to_string()))?;
        if e.encrypted() {
            return Err(VfsError::Operation(
                "Encrypted zip entries are not supported yet".into(),
            ));
        }
        let (size, compressed, crc) = (e.size(), e.compressed_size(), e.crc32());
        match e.compression() {
            CompressionMethod::Stored => {
                let file = self.slot.get()?;
                let start = data_start(&file, e.header_start())?;
                let name = archive.member_name(idx).to_owned();
                Ok(Box::new(StoredMember {
                    check: CrcCheck::new(Window::new(file, start, size), crc, size, name),
                    pos: 0,
                }))
            }
            CompressionMethod::Deflated => {
                let file = self.slot.get()?;
                let start = data_start(&file, e.header_start())?;
                let name = archive.member_name(idx).to_owned();
                let opener = Box::new(move || {
                    let raw = Window::new(file.clone(), start, compressed);
                    let dec = flate2::read::DeflateDecoder::new(raw);
                    Ok(Box::new(CrcCheck::new(dec, crc, size, name.clone()))
                        as Box<dyn Read + Send>)
                });
                Ok(Box::new(Decoded::new(opener, Some(size))))
            }
            _ => archive.open_via_temp(idx),
        }
    }

    fn decode_to(
        &self,
        archive: &Archive,
        idx: usize,
        out: &Path,
        on_chunk: crate::archive::OnChunk<'_>,
    ) -> Result<()> {
        let Source::Zip(i) = *archive.source(idx) else {
            std::fs::File::create(out)?;
            return Ok(());
        };
        let mut dst = std::fs::File::create(out)?;
        let method = self
            .zip
            .by_index_data(i)
            .map_err(|e| VfsError::Operation(e.to_string()))?
            .compression();
        if matches!(
            method,
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            let mut src = self.open(archive, idx)?;
            return crate::archive::copy_chunked(&mut src, &mut dst, on_chunk);
        }
        // Everything else: the zip crate (it verifies the CRC itself).
        let mut zip = self.zip.clone();
        let mut src = zip.by_index(i).map_err(|e| match e {
            ::zip::result::ZipError::UnsupportedArchive(m) if m.contains("assword") => {
                VfsError::Operation("Encrypted zip entries are not supported yet".into())
            }
            e => VfsError::Operation(e.to_string()),
        })?;
        crate::archive::copy_chunked(&mut src, &mut dst, on_chunk)
    }

    fn close(&self) {
        self.slot.close();
    }
}

/// Verifies the CRC-32 once the whole member has been read, like zipfile.
struct CrcCheck<R> {
    inner: R,
    crc: flate2::Crc,
    expected: u32,
    size: u64,
    name: String,
    enabled: bool,
}

impl<R> CrcCheck<R> {
    fn new(inner: R, expected: u32, size: u64, name: String) -> Self {
        CrcCheck {
            inner,
            crc: flate2::Crc::new(),
            expected,
            size,
            name,
            enabled: true,
        }
    }
}

impl<R: Read> Read for CrcCheck<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if !self.enabled {
            return Ok(n);
        }
        self.crc.update(&buf[..n]);
        if n == 0
            && !buf.is_empty()
            && (u64::from(self.crc.amount()) != self.size & 0xFFFF_FFFF
                || self.crc.sum() != self.expected)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Bad CRC-32 for file '{}'", self.name),
            ));
        }
        Ok(n)
    }
}

/// A stored member: a byte range of the archive, truly seekable. Like
/// zipfile, the CRC is checked while it's read straight through; a seek
/// elsewhere turns the check off (back to the start restarts it).
struct StoredMember {
    check: CrcCheck<Window>,
    pos: u64,
}

impl Read for StoredMember {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.check.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StoredMember {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = self.check.inner.seek(pos)?;
        if new != self.pos {
            self.check.enabled = new == 0;
            self.check.crc.reset();
        }
        self.pos = new;
        Ok(new)
    }
}

impl ReadStream for StoredMember {}
