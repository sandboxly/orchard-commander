//! Single compressed files (foo.txt.gz), shown as a folder holding foo.txt,
//! and the stream decompressors shared with compressed tarballs.

use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use oc_vfs::{ReadStream, Result};

use crate::archive::{Archive, Format, ListError, Listing, Member, Source};
use crate::io::{Decoded, HandleSlot, Window, read_exact_at};
use crate::{ArchiveKind, file_mtime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compression {
    None,
    Gzip,
    Bzip2,
    /// xz, or legacy .lzma (liblzma's auto decoder takes both).
    Xz,
}

impl Compression {
    /// Detect by magic bytes, like tarfile's "r:*".
    pub(crate) fn sniff(file: &std::fs::File) -> Compression {
        let mut head = [0u8; 6];
        let n = crate::io::read_at(file, &mut head, 0).unwrap_or(0);
        let head = &head[..n];
        if head.starts_with(&[0x1f, 0x8b]) {
            Compression::Gzip
        } else if head.starts_with(b"BZh") {
            Compression::Bzip2
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Compression::Xz
        } else {
            Compression::None
        }
    }

    /// A decompressing reader. Multi-stream files (concatenated .gz, pbzip2
    /// output, multi-stream .xz) decode as one, like Python's modules.
    pub(crate) fn decoder<R: Read + Send + 'static>(
        self,
        r: R,
    ) -> io::Result<Box<dyn Read + Send>> {
        Ok(match self {
            Compression::None => Box::new(r),
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(r)),
            Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(r)),
            Compression::Xz => {
                let stream = liblzma::stream::Stream::new_auto_decoder(
                    u64::MAX,
                    liblzma::stream::CONCATENATED,
                )
                .map_err(io::Error::other)?;
                Box::new(liblzma::read::XzDecoder::new_stream(r, stream))
            }
        })
    }
}

pub(crate) fn list(
    path: &Path,
    kind: ArchiveKind,
    meta: &std::fs::Metadata,
) -> std::result::Result<Listing, ListError> {
    let compression = match kind {
        ArchiveKind::Gz => Compression::Gzip,
        ArchiveKind::Bz2 => Compression::Bzip2,
        _ => Compression::Xz,
    };
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let stem = match file_name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s.to_owned(),
        _ => "data".to_owned(),
    };
    let slot = Arc::new(HandleSlot::new(path)?);
    let file = slot.get()?;
    let mut size = 0;
    if compression == Compression::Gzip {
        // gzip stores the size (mod 4 GiB) in its trailer.
        let len = file.metadata()?.len();
        let mut tail = [0u8; 4];
        if len < 4 {
            return Err(ListError::Other("file too short".into()));
        }
        read_exact_at(&file, &mut tail, len - 4)?;
        size = u64::from(u32::from_le_bytes(tail));
    }
    // Validate the format up front.
    let _ = compression
        .decoder(Window::whole(file))?
        .read(&mut [0u8; 1])?; // empty is fine too
    let member = Member::new(stem, false, size, file_mtime(meta));
    Ok(Listing {
        format: Box::new(SingleFormat { slot, compression }),
        members: vec![(member, Source::Single)],
    })
}

struct SingleFormat {
    slot: Arc<HandleSlot>,
    compression: Compression,
}

impl Format for SingleFormat {
    fn open(&self, _archive: &Archive, _idx: usize) -> Result<Box<dyn ReadStream>> {
        let file = self.slot.get()?;
        let compression = self.compression;
        let opener = Box::new(move || compression.decoder(Window::whole(file.clone())));
        // The gzip size field wraps at 4 GiB, so the length is learned by reading.
        Ok(Box::new(Decoded::new(opener, None)))
    }

    fn close(&self) {
        self.slot.close();
    }
}
