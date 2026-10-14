//! 7z archives (pure Rust, sevenz-rust2). A member can only be decoded as
//! part of its block (in solid archives, after everything before it), so
//! members are decoded into a file: straight to the target when extracting,
//! into a temp file when streamed.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use oc_vfs::{ReadStream, Result, VfsError};
use sevenz_rust2::{BlockDecoder, Password};

use crate::archive::{Archive, Format, ListError, Listing, Member, OnChunk, Source};
use crate::io::{HandleSlot, Window};
use crate::normalize;

const ENCRYPTED: &str = "Password-protected 7z archives aren't supported yet";
const AES_METHOD_ID: &[u8] = &[0x06, 0xF1, 0x07, 0x01];
/// 100 ns ticks between 1601-01-01 and the Unix epoch.
const NT_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

pub(crate) fn list(path: &Path) -> std::result::Result<Listing, ListError> {
    let slot = Arc::new(HandleSlot::new(path)?);
    let meta =
        match sevenz_rust2::Archive::read(&mut Window::whole(slot.get()?), &Password::empty()) {
            Ok(m) => m,
            Err(
                sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_),
            ) => {
                return Err(ListError::User(ENCRYPTED.into()));
            }
            Err(e) => return Err(e.into()),
        };
    if meta.blocks.iter().any(|b| {
        b.coders
            .iter()
            .any(|c| c.encoder_method_id() == AES_METHOD_ID)
    }) {
        return Err(ListError::User(ENCRYPTED.into()));
    }
    let mut members = Vec::with_capacity(meta.files.len());
    for (i, f) in meta.files.iter().enumerate() {
        if f.is_anti_item {
            continue;
        }
        let Some(name) = normalize(&f.name) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mtime = if f.has_last_modified_date {
            let ticks = u64::from(f.last_modified_date);
            (ticks as i128 - NT_UNIX_EPOCH as i128) as f64 / 1e7
        } else {
            0.0
        };
        let size = if f.is_directory { 0 } else { f.size };
        members.push((
            Member::new(name, f.is_directory, size, mtime),
            Source::SevenZ(i),
        ));
    }
    Ok(Listing {
        format: Box::new(SevenZFormat { slot, meta }),
        members,
    })
}

struct SevenZFormat {
    slot: Arc<HandleSlot>,
    meta: sevenz_rust2::Archive,
}

impl Format for SevenZFormat {
    fn open(&self, archive: &Archive, idx: usize) -> Result<Box<dyn ReadStream>> {
        archive.open_via_temp(idx)
    }

    fn decode_to(
        &self,
        archive: &Archive,
        idx: usize,
        out: &Path,
        mut on_chunk: OnChunk<'_>,
    ) -> Result<()> {
        let mut dst = std::fs::File::create(out)?;
        let Source::SevenZ(file_index) = *archive.source(idx) else {
            return Ok(());
        };
        let Some(block) = self
            .meta
            .stream_map
            .file_block_index
            .get(file_index)
            .copied()
            .flatten()
        else {
            return Ok(()); // an empty file
        };
        let target: *const sevenz_rust2::ArchiveEntry = &self.meta.files[file_index];
        let mut src = Window::whole(self.slot.get()?);
        let password = Password::empty();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u32;
        let mut failure: Option<VfsError> = None;
        let mut found = false;
        let mut buf = vec![0u8; oc_vfs::CHUNK];
        let result = BlockDecoder::new(threads, block, &self.meta, &password, &mut src)
            .for_each_entries(&mut |entry, reader: &mut dyn Read| {
                if !std::ptr::eq(entry, target) {
                    // Solid block: everything before the member must be decoded anyway.
                    io::copy(reader, &mut io::sink())?;
                    return Ok(true);
                }
                found = true;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    dst.write_all(&buf[..n])?;
                    if let Some(cb) = on_chunk.as_mut()
                        && let Err(e) = cb(n as u64)
                    {
                        failure = Some(e);
                        return Err(sevenz_rust2::Error::Other("aborted".into()));
                    }
                }
                Ok(false)
            });
        if let Some(e) = failure {
            return Err(e);
        }
        result.map_err(|e| {
            VfsError::Operation(format!(
                "Can't extract {}: {e}",
                archive.member_name(idx)
            ))
        })?;
        if !found {
            return Err(VfsError::Operation(format!(
                "Can't extract {}",
                archive.member_name(idx)
            )));
        }
        dst.flush()?;
        Ok(())
    }

    fn close(&self) {
        self.slot.close();
    }
}
