//! RAR archives through the vendored unrar library (RAR4 and RAR5, solid or
//! not), so no external tool is needed. The library only extracts whole
//! members to a path, so streams go through a temp file. Every read opens
//! its own library handle; nothing is shared between threads.

use std::path::Path;

use oc_vfs::{ReadStream, Result, VfsError};
use unrar::error::{Code, UnrarError};

use crate::archive::{Archive, Format, ListError, Listing, Member, OnChunk, Source, missing_link_target};
use crate::{dirname, dos_time_to_epoch, normalize, normpath};

const ENCRYPTED: &str = "Password-protected RAR archives aren't supported yet";
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn is_password_error(e: &UnrarError) -> bool {
    matches!(e.code, Code::MissingPassword | Code::BadPassword)
}

fn list_error(e: UnrarError) -> ListError {
    if is_password_error(&e) {
        ListError::User(ENCRYPTED.into())
    } else {
        ListError::Other(e.to_string())
    }
}

pub(crate) fn list(path: &Path) -> std::result::Result<Listing, ListError> {
    let listing = unrar::Archive::new(path)
        .open_for_listing()
        .map_err(list_error)?;
    if listing.has_encrypted_headers() {
        return Err(ListError::User(ENCRYPTED.into()));
    }
    let mut members = Vec::new();
    for header in listing {
        let h = header.map_err(list_error)?;
        if h.is_encrypted() {
            return Err(ListError::User(ENCRYPTED.into()));
        }
        let Some(name) = normalize(&h.filename.to_string_lossy()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let is_dir = h.is_directory();
        // Unix-made archives keep the mode in the attributes.
        let is_link = !is_dir && h.file_attr & S_IFMT == S_IFLNK;
        let size = if is_dir { 0 } else { h.unpacked_size };
        let member = Member {
            is_link,
            ..Member::new(name, is_dir, size, dos_time_to_epoch(h.file_time))
        };
        members.push((member, Source::Rar));
    }
    Ok(Listing {
        format: Box::new(RarFormat),
        members,
    })
}

struct RarFormat;

fn extract_error(name: &str, e: UnrarError) -> VfsError {
    if is_password_error(&e) {
        VfsError::Operation(ENCRYPTED.into())
    } else {
        VfsError::Operation(format!("Can't extract {name}: {e}"))
    }
}

impl Format for RarFormat {
    fn open(&self, archive: &Archive, idx: usize) -> Result<Box<dyn ReadStream>> {
        archive.open_via_temp(idx)
    }

    fn decode_to(
        &self,
        archive: &Archive,
        idx: usize,
        out: &Path,
        on_chunk: OnChunk<'_>,
    ) -> Result<()> {
        decode_member(archive, idx, out, 0)?;
        if let Some(cb) = on_chunk {
            cb(std::fs::metadata(out)?.len())?;
        }
        Ok(())
    }
}

/// Follow at most this many links (cycles).
const MAX_LINK_DEPTH: usize = 16;

fn decode_member(archive: &Archive, idx: usize, out: &Path, depth: usize) -> Result<()> {
    let name = archive.member_name(idx);
    let mut cursor = unrar::Archive::new(archive.path())
        .open_for_processing()
        .map_err(|e| extract_error(name, e))?;
    loop {
        let Some(header) = cursor.read_header().map_err(|e| extract_error(name, e))? else {
            return Err(VfsError::NotFound(name.to_owned()));
        };
        let entry = header.entry();
        if entry.is_file() && normalize(&entry.filename.to_string_lossy()).as_deref() == Some(name)
        {
            header.extract_to(out).map_err(|e| extract_error(name, e))?;
            break;
        }
        // In solid archives, skipping still decodes (the library handles that).
        cursor = header.skip().map_err(|e| extract_error(name, e))?;
    }
    // The library recreates Unix symlinks at `out`. Such a link would point
    // relative to the temp location (or outside it), and writing to or
    // reading through it would touch files that aren't in the archive. Read
    // links through their target inside the archive instead, like tar.
    match std::fs::symlink_metadata(out) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let link = std::fs::read_link(out)?;
            std::fs::remove_file(out)?;
            let target = normpath(&format!("{}/{}", dirname(name), link.to_string_lossy()));
            let target_idx = target
                .as_deref()
                .and_then(|t| archive.index_of(t))
                .filter(|_| depth < MAX_LINK_DEPTH)
                .ok_or_else(|| {
                    missing_link_target(name)
                })?;
            if archive.member_at(target_idx).is_dir {
                std::fs::File::create(out)?;
                return Ok(());
            }
            decode_member(archive, target_idx, out, depth + 1)
        }
        Ok(_) => Ok(()),
        // The library skips links it considers unsafe without an error; a
        // regular file that didn't appear is a failure, not empty data.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !archive.member_at(idx).is_link {
                return Err(VfsError::Operation(format!("Can't extract {name}")));
            }
            Err(missing_link_target(name))
        }
        Err(e) => Err(e.into()),
    }
}
