//! Open files: a seekable, read-ahead reader and a pipelined writer.
//!
//! Reads go through `smb2::FileReader` (positioned reads on one handle,
//! opened with read/write/delete sharing so a preview and a copy — or video
//! playback — can read the same file). Each read takes the connection lock,
//! like the Python version's `_Serialized` wrapper; sequential reads fetch
//! growing blocks and keep the next one in flight.
//!
//! Writes use our own CREATE so the file is opened with read sharing (as
//! Python's `share_access="r"`), then WRITEs of [`WRITE_CHUNK`] bytes with up
//! to [`WRITE_WINDOW`] in flight. The library's credit pool makes a WRITE wait
//! for credits instead of failing (smbprotocol's failure mode that forced the
//! Python version to serialize everything).

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::Duration;

use oc_vfs::{ReadStream, Result, VfsError, WriteStream};
use smb2::client::connection::Connection;
use smb2::msg::create::{CreateDisposition, CreateRequest, CreateResponse, ImpersonationLevel, ShareAccess};
use smb2::msg::write::{WriteRequest, WriteResponse};
use smb2::pack::{ReadCursor, Unpack};
use smb2::types::flags::{FileAccessMask, FileAttributes};
use smb2::types::status::NtStatus;
use smb2::types::{Command, CreditCharge, FileId, OplockLevel, TreeId};
use smb2::{FileReader, Tree};

use crate::error::{Failure, SmbResult};
use crate::fs::{CLOSE_TIMEOUT, OP_TIMEOUT, Shared, exec, rel};
use crate::runtime::{Pending, run};

const MIN_BLOCK: u64 = 64 * 1024;
const MAX_BLOCK: u64 = 4 * 1024 * 1024;
/// Bytes per WRITE: 4 credits, so even a server granting few credits (16 on
/// small NAS boxes) can fund several at once.
pub(crate) const WRITE_CHUNK: usize = 256 * 1024;
/// WRITEs in flight per open file.
pub(crate) const WRITE_WINDOW: usize = 8;

const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

fn to_io(e: VfsError) -> io::Error {
    match e {
        VfsError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

fn protocol(status: NtStatus, command: Command) -> Failure {
    Failure::Smb(smb2::Error::Protocol { status, command })
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

pub(crate) struct SmbReader {
    shared: Arc<Shared>,
    path: String,
    /// Shared with an in-flight prefetch; `None` once closed.
    reader: Option<Arc<FileReader>>,
    size: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
    block: u64,
    /// The next block, already requested: (offset, result).
    prefetch: Option<(u64, Pending<SmbResult<Vec<u8>>>)>,
}

impl SmbReader {
    pub(crate) fn open(shared: Arc<Shared>, path: &str) -> Result<Self> {
        let sh = shared.clone();
        let p = rel(path);
        let reader = exec(async move {
            let mut guard = sh.session().await?;
            let s = &mut *guard;
            Ok(s.client.open_file_reader(&s.tree, &p).await?)
        })
        .map_err(|f| shared.vfs_error(f, path))?;
        if reader.info().is_directory {
            let _ = run(CLOSE_TIMEOUT, reader.close());
            return Err(VfsError::Io(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("[Errno 21] Is a directory: '{}'", shared.unc(path)),
            )));
        }
        Ok(SmbReader {
            size: reader.size(),
            reader: Some(Arc::new(reader)),
            shared,
            path: path.to_string(),
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
            block: MIN_BLOCK,
            prefetch: None,
        })
    }

    fn fetch(&self, offset: u64, len: u64) -> SmbResult<Pending<SmbResult<Vec<u8>>>> {
        let reader = self.reader.clone().ok_or(Failure::Closed)?;
        let shared = self.shared.clone();
        Ok(Pending::spawn(async move {
            let _lock = shared.session().await?; // one request at a time per connection
            Ok(reader.read_at(offset, len).await?)
        }))
    }

    /// Make the buffer cover `self.pos` (which is < size).
    fn fill(&mut self, want: usize) -> SmbResult<()> {
        let pos = self.pos;
        let sequential = !self.buf.is_empty() && pos == self.buf_start + self.buf.len() as u64;
        let data = match self.prefetch.take() {
            Some((offset, pending)) if offset == pos => pending.wait(OP_TIMEOUT).unwrap_or(Err(Failure::TimedOut))?,
            stale => {
                if let Some((_, pending)) = stale {
                    // Jumped elsewhere: stop it rather than let it hold the
                    // connection for a block nobody reads.
                    pending.cancel();
                }
                self.block = if sequential { (self.block * 2).min(MAX_BLOCK) } else { MIN_BLOCK };
                let len = (want as u64).clamp(self.block, MAX_BLOCK.max(self.block));
                self.fetch(pos, len)?.wait(OP_TIMEOUT).unwrap_or(Err(Failure::TimedOut))?
            }
        };
        if data.is_empty() {
            // The file shrank since it was opened.
            self.size = pos;
        }
        let next = pos + data.len() as u64;
        self.buf = data;
        self.buf_start = pos;
        if sequential && next < self.size {
            self.block = (self.block * 2).min(MAX_BLOCK);
            self.prefetch = Some((next, self.fetch(next, self.block)?));
        }
        Ok(())
    }

    fn close(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        if let Some((_, pending)) = self.prefetch.take() {
            pending.cancel();
        }
        let _ = run(CLOSE_TIMEOUT, async move {
            // The cancelled prefetch lets go of the reader at its next poll.
            let mut reader = reader;
            for _ in 0..100 {
                match Arc::try_unwrap(reader) {
                    Ok(r) => {
                        let _ = r.close().await;
                        return;
                    }
                    Err(r) => reader = r,
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
}

impl Read for SmbReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let in_buf = self.pos >= self.buf_start && self.pos < self.buf_start + self.buf.len() as u64;
        if !in_buf {
            self.fill(out.len()).map_err(|f| to_io(self.shared.vfs_error(f, &self.path)))?;
            if self.pos >= self.size {
                return Ok(0);
            }
        }
        let start = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf.len() - start);
        out[..n].copy_from_slice(&self.buf[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SmbReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match target {
            Some(n) => {
                self.pos = n;
                Ok(n)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid seek to a negative position")),
        }
    }
}

impl ReadStream for SmbReader {}

impl Drop for SmbReader {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// The share-relative name a CREATE carries (DFS shares want the full path).
fn wire_name(tree: &Tree, rel: &str) -> String {
    dfs_name(tree.is_dfs, &tree.server, &tree.share_name, rel)
}

fn dfs_name(is_dfs: bool, server: &str, share: &str, rel: &str) -> String {
    let encoded = smb2::encode_path(rel);
    if !is_dfs {
        return encoded;
    }
    let host = match server.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((h, _)) => h,
        None => match server.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => h,
            _ => server,
        },
    };
    if encoded.is_empty() { format!("{host}\\{share}") } else { format!("{host}\\{share}\\{encoded}") }
}

pub(crate) struct SmbWriter {
    shared: Arc<Shared>,
    path: String,
    tree: Arc<Tree>,
    conn: Connection,
    file_id: FileId,
    chunk: usize,
    buf: Vec<u8>,
    offset: u64,
    inflight: VecDeque<Pending<SmbResult<()>>>,
    failed: Option<io::ErrorKind>,
    closed: bool,
}

impl SmbWriter {
    /// Create or truncate `path` for writing, readable by others meanwhile.
    pub(crate) fn create(shared: Arc<Shared>, path: &str) -> Result<Self> {
        let sh = shared.clone();
        let p = rel(path);
        let (tree, conn, file_id, max_write) = exec(async move {
            let guard = sh.session().await?;
            let conn = guard.client.connection().clone();
            let req = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(
                    FileAccessMask::FILE_WRITE_DATA
                        | FileAccessMask::FILE_WRITE_ATTRIBUTES
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                ),
                file_attributes: FileAttributes::NORMAL,
                share_access: ShareAccess(ShareAccess::FILE_SHARE_READ),
                create_disposition: CreateDisposition::FileOverwriteIf,
                create_options: FILE_NON_DIRECTORY_FILE,
                name: wire_name(&guard.tree, &p),
                create_contexts: vec![],
            };
            let frame = conn.execute(Command::Create, &req, Some(guard.tree.tree_id)).await?;
            if frame.header.status != NtStatus::SUCCESS {
                return Err(protocol(frame.header.status, Command::Create));
            }
            let resp = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))?;
            let max_write = conn.params().map_or(65536, |p| p.max_write_size);
            Ok((Arc::new(guard.tree.clone()), conn, resp.file_id, max_write))
        })
        .map_err(|f| shared.vfs_error(f, path))?;
        let chunk = WRITE_CHUNK.min(max_write.max(4096) as usize);
        Ok(SmbWriter {
            shared,
            path: path.to_string(),
            tree,
            conn,
            file_id,
            chunk,
            buf: Vec::with_capacity(chunk),
            offset: 0,
            inflight: VecDeque::new(),
            failed: None,
            closed: false,
        })
    }

    fn error(&self, f: Failure) -> io::Error {
        to_io(self.shared.vfs_error(f, &self.path))
    }

    fn check_failed(&self) -> io::Result<()> {
        match self.failed {
            Some(kind) => {
                Err(io::Error::new(kind, format!("{}: an earlier write failed", self.shared.unc(&self.path))))
            }
            None => Ok(()),
        }
    }

    /// Wait for the oldest WRITE.
    fn wait_oldest(&mut self) -> io::Result<()> {
        if let Some(p) = self.inflight.pop_front()
            && let Err(f) = p.wait(OP_TIMEOUT).unwrap_or(Err(Failure::TimedOut))
        {
            let e = self.error(f);
            self.failed = Some(e.kind());
            return Err(e);
        }
        Ok(())
    }

    fn wait_all(&mut self) -> io::Result<()> {
        let mut first = Ok(());
        while !self.inflight.is_empty() {
            if let Err(e) = self.wait_oldest()
                && first.is_ok()
            {
                first = Err(e);
            }
        }
        first
    }

    /// Bytes one WRITE may carry now: [`Self::chunk`], lowered to what half
    /// the credit window funds once the server has shown its ceiling (a WRITE
    /// charging more than the ceiling could never be sent).
    fn fundable(&self) -> usize {
        match self.conn.credit_ceiling() {
            Some(ceiling) => self.chunk.min(usize::from((ceiling / 2).max(1)) * 65536),
            None => self.chunk,
        }
    }

    fn send(&mut self, data: Vec<u8>) -> io::Result<()> {
        let fundable = self.fundable();
        if data.len() > fundable {
            for piece in data.chunks(fundable) {
                self.send_one(piece.to_vec())?;
            }
            return Ok(());
        }
        self.send_one(data)
    }

    fn send_one(&mut self, data: Vec<u8>) -> io::Result<()> {
        while self.inflight.len() >= WRITE_WINDOW {
            self.wait_oldest()?;
        }
        let (conn, tree_id, file_id, offset) = (self.conn.clone(), self.tree.tree_id, self.file_id, self.offset);
        self.offset += data.len() as u64;
        self.inflight.push_back(Pending::spawn(async move {
            match write_at(&conn, tree_id, file_id, offset, &data).await {
                // Sized before the server showed a lower credit ceiling: the
                // library refuses such a WRITE at once (it could never be
                // funded). Send it again in pieces the ceiling funds.
                Err(Failure::Smb(smb2::Error::CreditStarvation { waited, .. }))
                    if waited.is_zero() && data.len() > 65536 =>
                {
                    let ceiling = conn.credit_ceiling().unwrap_or(2);
                    let piece = usize::from((ceiling / 2).max(1)) * 65536;
                    let mut at = offset;
                    for part in data.chunks(piece) {
                        write_at(&conn, tree_id, file_id, at, part).await?;
                        at += part.len() as u64;
                    }
                    Ok(())
                }
                other => other,
            }
        }));
        Ok(())
    }

    fn send_buffered(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk));
        self.send(data)
    }

    /// Release the server's handle (bounded wait).
    fn close_handle(&mut self) -> SmbResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let (tree, mut conn, file_id) = (self.tree.clone(), self.conn.clone(), self.file_id);
        run(CLOSE_TIMEOUT, async move { Ok(tree.close_handle(&mut conn, file_id).await?) })
            .unwrap_or(Err(Failure::TimedOut))
    }
}

impl Write for SmbWriter {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        self.check_failed()?;
        let total = data.len();
        while !data.is_empty() {
            let room = self.chunk - self.buf.len();
            let n = room.min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.buf.len() == self.chunk {
                self.send_buffered()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_failed()?;
        self.send_buffered()?;
        self.wait_all()
    }
}

impl WriteStream for SmbWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let written = self.flush();
        let closed = self.close_handle();
        written?;
        closed.map_err(|f| self.shared.vfs_error(f, &self.path))
    }
}

impl Drop for SmbWriter {
    fn drop(&mut self) {
        if !self.closed {
            // Not finished (cancelled or failed): drop what is still on the
            // wire and release the handle right away, so the caller can
            // delete the partial file.
            for pending in self.inflight.drain(..) {
                pending.cancel();
            }
            let _ = self.close_handle();
        }
    }
}

/// One WRITE of `data` at `offset`, charged by its size.
async fn write_at(conn: &Connection, tree_id: TreeId, file_id: FileId, offset: u64, data: &[u8]) -> SmbResult<()> {
    let len = data.len() as u32;
    let req = WriteRequest {
        data_offset: 0x70,
        offset,
        file_id,
        channel: 0,
        remaining_bytes: 0,
        write_channel_info_offset: 0,
        write_channel_info_length: 0,
        flags: 0,
        data: data.to_vec(),
    };
    let charge = CreditCharge(u64::from(len).div_ceil(65536).clamp(1, u64::from(u16::MAX)) as u16);
    let frame = conn.execute_with_credits(Command::Write, &req, Some(tree_id), charge).await?;
    if frame.header.status != NtStatus::SUCCESS {
        return Err(protocol(frame.header.status, Command::Write));
    }
    let resp = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))?;
    if resp.count != len {
        return Err(Failure::Smb(smb2::Error::invalid_data(format!("the server wrote {} of {len} bytes", resp.count))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dfs_names() {
        assert_eq!(dfs_name(false, "nas:445", "Share", "a/b"), r"a\b");
        assert_eq!(dfs_name(true, "nas:445", "Share", "a/b"), r"nas\Share\a\b");
        assert_eq!(dfs_name(true, "[fe80::1]:445", "Share", ""), r"fe80::1\Share");
    }
}
