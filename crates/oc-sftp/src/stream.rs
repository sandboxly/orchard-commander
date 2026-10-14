//! Streams over SFTP file handles.
//!
//! Reads are pipelined: while the caller consumes one reply, further read
//! requests are already on their way (read-ahead). The window starts small
//! after opening or seeking, so random access (previews, archive indexes)
//! doesn't pull megabytes it won't use, and doubles with every sequential
//! reply up to 8 MiB in flight, which keeps fast links busy. A stream waits at
//! most `STALL_TIMEOUT` for any reply, however slow the link otherwise is.
//! Writes are pipelined the same way: requests are sent without waiting for
//! each acknowledgement; errors surface on a later write or on `finish`.
//!
//! What a stream keeps in flight is also bounded by time ([`Rate`]): about
//! a second's worth at the measured speed. More doesn't make a transfer
//! faster, but everything else on the connection (listings, other streams)
//! waits behind it: on a slow link, 8 MiB would hold a listing back for
//! a minute.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_vfs::{ReadStream, Result, VfsError, WriteStream};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::task::JoinHandle;

use crate::runtime::{block_on_within, join_within, runtime, spawn};
use crate::session::{Conn, STALL_TIMEOUT, Sftp, is_eof};

/// First request size after opening or seeking.
const START_LEN: u32 = 32 * 1024;
/// Bytes requested but not yet consumed, at most.
const MAX_IN_FLIGHT: u64 = 8 * 1024 * 1024;
/// Bytes in flight allowed whatever the measured speed.
const MIN_IN_FLIGHT: u64 = 256 * 1024;
/// At most this much transfer time in flight.
const IN_FLIGHT_SECS: f64 = 1.0;
const MAX_REQUESTS: u64 = 256;
/// How long dropping an unfinished writer waits for its file to be closed.
const DROP_CLOSE_WAIT: Duration = Duration::from_secs(15);

/// A stream's speed since it started (or last seeked), which bounds what
/// it keeps in flight.
struct Rate {
    since: Instant,
    bytes: u64,
}

impl Rate {
    fn new() -> Self {
        Rate { since: Instant::now(), bytes: 0 }
    }

    fn add(&mut self, n: u64) {
        self.bytes = self.bytes.saturating_add(n);
    }

    /// Bytes to keep in flight: [`IN_FLIGHT_SECS`] at the measured speed.
    fn in_flight(&self) -> u64 {
        let secs = self.since.elapsed().as_secs_f64().max(0.001);
        ((self.bytes as f64 / secs * IN_FLIGHT_SECS) as u64).clamp(MIN_IN_FLIGHT, MAX_IN_FLIGHT)
    }
}

pub(crate) fn to_io(err: VfsError) -> io::Error {
    match err {
        VfsError::Io(e) => e,
        VfsError::NotFound(p) => io::Error::new(io::ErrorKind::NotFound, format!("{p} not found")),
        other => io::Error::other(other.to_string()),
    }
}

struct Pending {
    off: u64,
    len: u32,
    task: JoinHandle<std::result::Result<Vec<u8>, SftpError>>,
}

pub(crate) struct SftpReader {
    conn: Arc<Conn>,
    sftp: Arc<Sftp>,
    handle: String,
    display: String,
    /// From stat at open; None (or 0, e.g. /proc files) means "read until EOF".
    size: Option<u64>,
    /// File offset of the next byte `read` returns.
    pos: u64,
    buf: Vec<u8>,
    buf_pos: usize,
    /// Bytes to drop from the front of the next reply (after a short forward seek).
    discard: u64,
    pending: VecDeque<Pending>,
    /// Offset of the next request to send.
    next_off: u64,
    /// Sequential replies since opening/seeking: grows request size and window.
    level: u32,
    rate: Rate,
    eof: bool,
}

impl SftpReader {
    pub fn open(conn: Arc<Conn>, path: &str, display: String) -> Result<Self> {
        let sftp = conn.checkout()?;
        let (s, p) = (sftp.clone(), path.to_string());
        let opened = block_on_within(STALL_TIMEOUT, async move {
            // Open and stat in one round trip.
            let (handle, attrs) =
                tokio::join!(s.raw.open(p.clone(), OpenFlags::READ, FileAttributes::default()), s.raw.stat(p));
            Ok::<_, SftpError>((handle?.handle, attrs.ok().and_then(|a| a.attrs.size)))
        })?
        .unwrap_or(Err(SftpError::Timeout));
        let (handle, size) = match opened {
            Ok(v) => v,
            Err(e) => {
                sftp.note(&e);
                conn.checkin(sftp);
                return Err(conn.error(e, &display));
            }
        };
        Ok(SftpReader {
            conn,
            sftp,
            handle,
            display,
            size: size.filter(|s| *s > 0),
            pos: 0,
            buf: Vec::new(),
            buf_pos: 0,
            discard: 0,
            pending: VecDeque::new(),
            next_off: 0,
            level: 0,
            rate: Rate::new(),
            eof: false,
        })
    }

    fn request_len(&self) -> u32 {
        START_LEN.checked_shl(self.level).unwrap_or(u32::MAX).min(self.sftp.read_chunk).max(1)
    }

    fn window(&self) -> usize {
        let max = (self.rate.in_flight() / u64::from(self.request_len())).clamp(1, MAX_REQUESTS);
        1u64.checked_shl(self.level).unwrap_or(u64::MAX).min(max) as usize
    }

    fn issue(&self, off: u64, len: u32) -> Pending {
        let (s, h) = (self.sftp.clone(), self.handle.clone());
        let task = runtime().spawn(async move { s.raw.read(h, off, len).await.map(|d| d.data) });
        Pending { off, len, task }
    }

    /// Keep the read-ahead window full.
    fn fill(&mut self) {
        let (window, len) = (self.window(), self.request_len());
        while self.pending.len() < window {
            let len = match self.size {
                Some(size) if self.next_off >= size => break,
                Some(size) => u64::from(len).min(size - self.next_off) as u32,
                None => len,
            };
            let p = self.issue(self.next_off, len);
            self.pending.push_back(p);
            self.next_off = self.next_off.saturating_add(u64::from(len));
        }
    }

    fn cancel(&mut self) {
        for p in self.pending.drain(..) {
            p.task.abort();
        }
    }

    fn stalled(&mut self) -> io::Error {
        self.sftp.mark_dead();
        self.fail(SftpError::Timeout)
    }

    fn fail(&mut self, err: SftpError) -> io::Error {
        self.sftp.note(&err);
        self.cancel();
        to_io(self.conn.error(err, &self.display))
    }

    /// Wait for the next reply into `buf`; false at the end of the file.
    fn next_reply(&mut self) -> io::Result<bool> {
        loop {
            if self.eof {
                return Ok(false);
            }
            self.fill();
            let Some(p) = self.pending.pop_front() else {
                self.eof = true;
                return Ok(false);
            };
            let mut data = match join_within(p.task, STALL_TIMEOUT) {
                Ok(None) => return Err(self.stalled()),
                Ok(Some(Ok(data))) => data,
                Ok(Some(Err(e))) if is_eof(&e) => {
                    self.eof = true;
                    self.cancel();
                    return Ok(false);
                }
                Ok(Some(Err(e))) => return Err(self.fail(e)),
                Err(e) => {
                    self.cancel();
                    return Err(e);
                }
            };
            data.truncate(p.len as usize); // never trust a server to send only what was asked
            let got = data.len() as u64;
            self.rate.add(got);
            if got == 0 {
                self.eof = true;
                self.cancel();
                return Ok(false);
            }
            if got < u64::from(p.len) {
                let end = p.off.saturating_add(got);
                if self.size.is_some_and(|size| end >= size) {
                    self.cancel(); // the file shrank: nothing after this
                    self.next_off = end;
                    self.size = Some(end);
                } else {
                    // A short read (server-side limit, or the file shrank:
                    // then the rest is answered with EOF): ask for the rest first.
                    let rest = self.issue(end, p.len - got as u32);
                    self.pending.push_front(rest);
                }
            } else if self.level < 16 {
                self.level += 1;
            }
            if self.discard > 0 {
                let skip = self.discard.min(got) as usize;
                self.discard -= skip as u64;
                data.drain(..skip);
                if data.is_empty() {
                    continue;
                }
            }
            self.buf = data;
            self.buf_pos = 0;
            return Ok(true);
        }
    }
}

impl Read for SftpReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.buf_pos >= self.buf.len() && !self.next_reply()? {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - self.buf_pos);
        out[..n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
        self.buf_pos += n;
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl Seek for SftpReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
            SeekFrom::End(d) => {
                let size = match self.size {
                    Some(s) => s,
                    None => {
                        let (s, h) = (self.sftp.clone(), self.handle.clone());
                        match block_on_within(STALL_TIMEOUT, async move { s.raw.fstat(h).await })? {
                            Some(Ok(a)) => a.attrs.size.unwrap_or(0),
                            Some(Err(e)) => return Err(self.fail(e)),
                            None => return Err(self.stalled()),
                        }
                    }
                };
                i128::from(size) + i128::from(d)
            }
        };
        let target = u64::try_from(target)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek to a negative position"))?;
        if target == self.pos {
            return Ok(target);
        }
        let buffered = (self.buf.len() - self.buf_pos) as u64;
        let buf_start = self.pos - self.buf_pos as u64;
        if !self.buf.is_empty() && target >= buf_start && target <= self.pos.saturating_add(buffered) {
            self.buf_pos = (target - buf_start) as usize;
            self.pos = target;
            return Ok(target);
        }
        let ahead_start = self.pos.saturating_add(buffered);
        if target > ahead_start && target < self.next_off && target - ahead_start <= MAX_IN_FLIGHT {
            // Forward within what's already requested: drop replies wholly before
            // the target and skip into the first one that holds it.
            while let Some(p) = self.pending.front() {
                if p.off + u64::from(p.len) > target {
                    break;
                }
                if let Some(p) = self.pending.pop_front() {
                    p.task.abort();
                }
            }
            if let Some(first) = self.pending.front() {
                self.discard = target.saturating_sub(first.off);
                self.buf.clear();
                self.buf_pos = 0;
                self.pos = target;
                return Ok(target);
            }
        }
        self.cancel();
        self.buf.clear();
        self.buf_pos = 0;
        self.discard = 0;
        self.pos = target;
        self.next_off = target;
        self.level = 0;
        self.rate = Rate::new();
        self.eof = false;
        Ok(target)
    }
}

impl ReadStream for SftpReader {}

impl Drop for SftpReader {
    fn drop(&mut self) {
        self.cancel();
        let (conn, sftp, handle) = (self.conn.clone(), self.sftp.clone(), std::mem::take(&mut self.handle));
        spawn(async move {
            let _ = sftp.raw.close(handle).await;
            conn.checkin(sftp);
        });
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

pub(crate) struct SftpWriter {
    conn: Arc<Conn>,
    sftp: Arc<Sftp>,
    handle: String,
    display: String,
    off: u64,
    buf: Vec<u8>,
    /// Writes sent and not yet acknowledged, with their lengths.
    inflight: VecDeque<(u64, JoinHandle<std::result::Result<(), SftpError>>)>,
    rate: Rate,
    failed: Option<(io::ErrorKind, String)>,
    done: bool,
}

impl SftpWriter {
    pub fn open(conn: Arc<Conn>, path: &str, display: String, flags: OpenFlags) -> Result<Self> {
        let sftp = conn.checkout()?;
        let (s, p) = (sftp.clone(), path.to_string());
        let opened =
            block_on_within(STALL_TIMEOUT, async move { s.raw.open(p, flags, FileAttributes::default()).await })?
                .unwrap_or(Err(SftpError::Timeout));
        let handle = match opened {
            Ok(h) => h.handle,
            Err(e) => {
                sftp.note(&e);
                conn.checkin(sftp);
                return Err(conn.error(e, &display));
            }
        };
        let chunk = sftp.write_chunk as usize;
        Ok(SftpWriter {
            conn,
            sftp,
            handle,
            display,
            off: 0,
            buf: Vec::with_capacity(chunk),
            inflight: VecDeque::new(),
            rate: Rate::new(),
            failed: None,
            done: false,
        })
    }

    fn chunk(&self) -> usize {
        self.sftp.write_chunk as usize
    }

    fn max_inflight(&self) -> usize {
        (self.rate.in_flight() / u64::from(self.sftp.write_chunk.max(1))).clamp(1, MAX_REQUESTS) as usize
    }

    fn check(&self) -> io::Result<()> {
        match &self.failed {
            Some((kind, msg)) => Err(io::Error::new(*kind, msg.clone())),
            None => Ok(()),
        }
    }

    fn record(&mut self, err: io::Error) -> io::Error {
        self.failed = Some((err.kind(), err.to_string()));
        err
    }

    fn wait_one(&mut self) -> io::Result<()> {
        let Some((len, task)) = self.inflight.pop_front() else { return Ok(()) };
        self.rate.add(len);
        match join_within(task, STALL_TIMEOUT) {
            Ok(None) => {
                self.sftp.mark_dead();
                let err = to_io(self.conn.error(SftpError::Timeout, &self.display));
                Err(self.record(err))
            }
            Ok(Some(Ok(()))) => Ok(()),
            Ok(Some(Err(e))) => {
                self.sftp.note(&e);
                let err = to_io(self.conn.error(e, &self.display));
                Err(self.record(err))
            }
            Err(e) => Err(self.record(e)),
        }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = self.chunk();
        let data = std::mem::replace(&mut self.buf, Vec::with_capacity(chunk));
        let (s, h, off) = (self.sftp.clone(), self.handle.clone(), self.off);
        let len = data.len() as u64;
        self.off += len;
        self.inflight.push_back((len, runtime().spawn(async move { s.raw.write(h, off, data).await.map(|_| ()) })));
        while self.inflight.len() > self.max_inflight() {
            self.wait_one()?;
        }
        Ok(())
    }
}

impl Write for SftpWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.check()?;
        let n = data.len().min(self.chunk() - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() >= self.chunk() {
            self.send()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.send()?;
        while !self.inflight.is_empty() {
            self.wait_one()?;
        }
        Ok(())
    }
}

impl WriteStream for SftpWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.flush()?;
        self.done = true;
        let (s, h) = (self.sftp.clone(), std::mem::take(&mut self.handle));
        match block_on_within(STALL_TIMEOUT, async move { s.raw.close(h).await })?.unwrap_or(Err(SftpError::Timeout)) {
            Ok(_) => {
                self.conn.checkin(self.sftp.clone());
                Ok(())
            }
            Err(e) => {
                self.sftp.note(&e);
                self.conn.checkin(self.sftp.clone());
                Err(self.conn.error(e, &self.display))
            }
        }
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Not finished (an error or a cancelled copy): let what's in flight
        // land and close the handle before returning, as Python's `close()`
        // did. The caller then removes the partial file, which servers on
        // Windows refuse while it's open, and no late write can reach a file
        // opened again under the same name.
        let inflight: Vec<_> = self.inflight.drain(..).map(|(_, task)| task).collect();
        let (conn, sftp, handle) = (self.conn.clone(), self.sftp.clone(), std::mem::take(&mut self.handle));
        let cleanup = async move {
            for t in inflight {
                let _ = t.await;
            }
            let closed = sftp.raw.close(handle).await;
            if closed.is_err() {
                sftp.mark_dead(); // the handle may still be open: don't reuse the channel
            }
            conn.checkin(sftp);
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn(cleanup); // never block inside an async runtime
            return;
        }
        let task = runtime().spawn(cleanup);
        if !matches!(join_within(task, DROP_CLOSE_WAIT), Ok(Some(()))) {
            // Gave up waiting (the task is aborted): close the channel, and with it the file.
            self.sftp.mark_dead();
            self.conn.checkin(self.sftp.clone());
        }
    }
}
