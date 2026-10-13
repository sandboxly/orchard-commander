//! Opening media for FFmpeg: local files directly, everything else through a
//! custom AVIOContext over a `Read + Seek` stream (`ui/media_stream.py` in
//! 1.x). Reads are served from a read-ahead block so FFmpeg's small requests
//! don't each cost a server round trip; a block that arrives slowly is cut
//! short ([`FILL_PATIENCE`]) so playback goes on. Unseekable streams get no seek
//! callback: they play from the start, without seeking.
//!
//! Every open input has an abort flag, checked by FFmpeg's interrupt callback
//! and before each stream read, so a player being dropped stops after the
//! read in progress instead of finishing a slow download.

use std::ffi::{CString, c_int, c_void};
use std::io::{self, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::ff;
use super::ff::ffi;
use super::ff::util::error::{EINVAL, EIO, ENOSYS, ESPIPE};
use crate::{MediaError, MediaSource};

/// Bytes fetched per stream request (players ask for small pieces).
pub(crate) const READ_AHEAD: usize = 1024 * 1024;
/// FFmpeg's own I/O buffer.
const IO_BUFFER: usize = 64 * 1024;
/// A read-ahead block that takes longer than this to fill is cut short at
/// what has arrived: on a slow server, waiting for a whole 1 MiB block would
/// outlast the audio buffered ahead and playback would stutter (a 320 kbit/s
/// MP3 over 80 KB/s would stop for ~10 s every block).
const FILL_PATIENCE: Duration = Duration::from_millis(150);

struct IoState<R> {
    reader: R,
    /// FFmpeg's read position.
    pos: u64,
    size: Option<u64>,
    seekable: bool,
    block: Vec<u8>,
    block_start: u64,
    /// Where the reader is, if known.
    reader_pos: Option<u64>,
    abort: Arc<AtomicBool>,
    read_ahead: usize,
}

/// How a stream is read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadOptions {
    /// Bytes fetched per request.
    pub(crate) read_ahead: usize,
    /// Cap on what FFmpeg reads to find the format and streams (its own
    /// defaults: 1 MiB and 5 MB).
    pub(crate) probesize: Option<i64>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions { read_ahead: READ_AHEAD, probesize: None }
    }
}

const fn averror(e: c_int) -> c_int {
    ffi::AVERROR(e)
}

impl<R: Read + Seek> IoState<R> {
    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, c_int> {
        if self.aborted() {
            return Err(ffi::AVERROR_EXIT);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self.size.is_some_and(|size| self.pos >= size) {
            return Err(ffi::AVERROR_EOF);
        }
        let in_block = self.pos.checked_sub(self.block_start).filter(|&o| o < self.block.len() as u64);
        let offset = match in_block {
            Some(o) => o as usize,
            None => {
                self.fill(buf.len())?;
                0
            }
        };
        let n = buf.len().min(self.block.len() - offset);
        if n == 0 {
            return Err(ffi::AVERROR_EOF);
        }
        buf[..n].copy_from_slice(&self.block[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }

    /// Read a new block at `pos`.
    fn fill(&mut self, want: usize) -> Result<(), c_int> {
        if self.reader_pos != Some(self.pos) {
            if self.seekable {
                self.reader.seek(SeekFrom::Start(self.pos)).map_err(|_| averror(EIO))?;
            } else {
                // Only forward, by reading and discarding.
                let at = self.reader_pos.unwrap_or(0);
                let skip = self.pos.checked_sub(at).ok_or(averror(ESPIPE))?;
                let copied = io::copy(&mut (&mut self.reader).take(skip), &mut io::sink()).map_err(|_| averror(EIO))?;
                if copied < skip {
                    return Err(ffi::AVERROR_EOF);
                }
            }
            self.reader_pos = Some(self.pos);
        }
        let target = self.read_ahead.max(want);
        self.block.resize(target, 0);
        let mut filled = 0;
        let started = Instant::now();
        while filled < target {
            if self.aborted() {
                return Err(ffi::AVERROR_EXIT);
            }
            if filled > 0 && started.elapsed() >= FILL_PATIENCE {
                break; // a slow source: hand on what's there
            }
            match self.reader.read(&mut self.block[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::debug!("media stream read: {e}");
                    if filled == 0 {
                        self.block.clear();
                        self.reader_pos = None;
                        return Err(averror(EIO));
                    }
                    break;
                }
            }
        }
        self.block.truncate(filled);
        self.block_start = self.pos;
        self.reader_pos = Some(self.pos + filled as u64);
        Ok(())
    }

    fn seek(&mut self, offset: i64, whence: c_int) -> i64 {
        if whence & ffi::AVSEEK_SIZE != 0 {
            if self.size.is_none() && self.seekable {
                self.size = self.reader.seek(SeekFrom::End(0)).ok();
                self.reader_pos = self.size;
            }
            return self.size.map_or(averror(ENOSYS) as i64, |s| s as i64);
        }
        let base = match whence & !ffi::AVSEEK_FORCE {
            0 => 0,                // SEEK_SET
            1 => self.pos as i64,  // SEEK_CUR
            2 => match self.size { // SEEK_END
                Some(s) => s as i64,
                None => return averror(ENOSYS) as i64,
            },
            _ => return averror(EINVAL) as i64,
        };
        match base.checked_add(offset) {
            Some(new) if new >= 0 => {
                self.pos = new as u64;
                new
            }
            _ => averror(EINVAL) as i64,
        }
    }
}

unsafe extern "C" fn read_packet<R: Read + Seek>(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    if opaque.is_null() || buf.is_null() || size <= 0 {
        return if size == 0 { 0 } else { averror(EINVAL) };
    }
    // SAFETY: `opaque` is the IoState<R> this context was created with, and
    // FFmpeg calls back from one thread at a time; `buf` holds `size` bytes.
    let state = unsafe { &mut *(opaque as *mut IoState<R>) };
    let out = unsafe { std::slice::from_raw_parts_mut(buf, size as usize) };
    match catch_unwind(AssertUnwindSafe(|| state.read(out))) {
        Ok(Ok(n)) => n as c_int,
        Ok(Err(e)) => e,
        Err(_) => averror(EIO),
    }
}

unsafe extern "C" fn seek<R: Read + Seek>(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return averror(EINVAL) as i64;
    }
    // SAFETY: as in read_packet.
    let state = unsafe { &mut *(opaque as *mut IoState<R>) };
    catch_unwind(AssertUnwindSafe(|| state.seek(offset, whence))).unwrap_or(averror(EIO) as i64)
}

unsafe extern "C" fn interrupted(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    // SAFETY: the Arc<AtomicBool> boxed in IoResources, alive as long as the context.
    let flag = unsafe { &*(opaque as *const Arc<AtomicBool>) };
    flag.load(Ordering::Acquire) as c_int
}

unsafe fn free_state<R>(p: *mut c_void) {
    // SAFETY: created by Box::into_raw in open_reader.
    drop(unsafe { Box::from_raw(p as *mut IoState<R>) });
}

/// What an input needs besides its format context, freed after it.
struct IoResources {
    avio: *mut ffi::AVIOContext,
    io: *mut c_void,
    free_io: Option<unsafe fn(*mut c_void)>,
    interrupt: *mut Arc<AtomicBool>,
}

impl Drop for IoResources {
    fn drop(&mut self) {
        // SAFETY: each pointer is owned here and freed once.
        unsafe {
            if !self.avio.is_null() {
                // FFmpeg may have replaced the buffer; free whichever it has now.
                ffi::av_freep(ptr::addr_of_mut!((*self.avio).buffer) as *mut c_void);
                ffi::avio_context_free(&mut self.avio);
            }
            if let Some(free) = self.free_io
                && !self.io.is_null()
            {
                free(self.io);
            }
            if !self.interrupt.is_null() {
                drop(Box::from_raw(self.interrupt));
            }
        }
    }
}

/// An open, probed input (streams found). Derefs to FFmpeg's input context.
pub(crate) struct Input<'a> {
    ctx: ManuallyDrop<ff::format::context::Input>,
    _res: IoResources,
    pub(crate) seekable: bool,
    _reader: PhantomData<&'a mut ()>,
}

// SAFETY: the context and the reader (Send) are used from one thread at a time.
unsafe impl Send for Input<'_> {}

impl Deref for Input<'_> {
    type Target = ff::format::context::Input;
    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl DerefMut for Input<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

impl Drop for Input<'_> {
    fn drop(&mut self) {
        // SAFETY: dropped once, before the I/O resources it uses.
        unsafe { ManuallyDrop::drop(&mut self.ctx) };
    }
}

fn path_cstring(path: &Path) -> Result<CString, MediaError> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_str().ok_or_else(|| MediaError::Decode("unsupported file name".into()))?.as_bytes().to_vec();
    CString::new(bytes).map_err(|_| MediaError::Decode("unsupported file name".into()))
}

unsafe fn open_with<'a>(url: &CString, res: IoResources, seekable: bool, probesize: Option<i64>) -> Result<Input<'a>, MediaError> {
    unsafe {
        let mut ctx = ffi::avformat_alloc_context();
        if ctx.is_null() {
            return Err(MediaError::Decode("out of memory".into()));
        }
        if let Some(limit) = probesize {
            (*ctx).probesize = limit;
            (*ctx).format_probesize = limit.min(1 << 20) as c_int;
        }
        (*ctx).interrupt_callback = ffi::AVIOInterruptCB { callback: Some(interrupted), opaque: res.interrupt as *mut c_void };
        if !res.avio.is_null() {
            (*ctx).pb = res.avio;
            (*ctx).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;
        }
        let r = ffi::avformat_open_input(&mut ctx, url.as_ptr(), ptr::null(), ptr::null_mut());
        if r < 0 {
            // FFmpeg freed the context (not our custom I/O; `res` does that).
            return Err(MediaError::Decode(super::describe(&ff::Error::from(r))));
        }
        let r = ffi::avformat_find_stream_info(ctx, ptr::null_mut());
        if r < 0 {
            ffi::avformat_close_input(&mut ctx);
            return Err(MediaError::Decode(super::describe(&ff::Error::from(r))));
        }
        Ok(Input {
            ctx: ManuallyDrop::new(ff::format::context::Input::wrap(ctx)),
            _res: res,
            seekable,
            _reader: PhantomData,
        })
    }
}

/// Open a local file.
pub(crate) fn open_file(path: &Path, abort: Arc<AtomicBool>) -> Result<Input<'static>, MediaError> {
    super::init();
    let url = path_cstring(path)?;
    let res = IoResources {
        avio: ptr::null_mut(),
        io: ptr::null_mut(),
        free_io: None,
        interrupt: Box::into_raw(Box::new(abort)),
    };
    // SAFETY: `res` holds valid (or null) pointers that outlive the context.
    unsafe { open_with(&url, res, true, None) }
}

/// Open a stream; `name` hints at the format.
pub(crate) fn open_reader<'a, R: Read + Seek + Send + 'a>(
    reader: R,
    seekable: bool,
    size: Option<u64>,
    name: &str,
    abort: Arc<AtomicBool>,
    options: ReadOptions,
) -> Result<Input<'a>, MediaError> {
    super::init();
    let url = CString::new(name.replace('\0', "")).unwrap_or_default();
    let state = Box::new(IoState {
        reader,
        pos: 0,
        size,
        seekable,
        block: Vec::new(),
        block_start: 0,
        // Unseekable streams are where they were opened: at the start.
        reader_pos: if seekable { None } else { Some(0) },
        abort: abort.clone(),
        read_ahead: options.read_ahead.max(4096),
    });
    let mut res = IoResources {
        avio: ptr::null_mut(),
        io: Box::into_raw(state) as *mut c_void,
        free_io: Some(free_state::<R>),
        interrupt: Box::into_raw(Box::new(abort)),
    };
    // SAFETY: the buffer is handed to the AVIOContext (freed with it); the
    // opaque pointer lives in `res` until after the context is closed.
    unsafe {
        let buffer = ffi::av_malloc(IO_BUFFER) as *mut u8;
        if buffer.is_null() {
            return Err(MediaError::Decode("out of memory".into()));
        }
        let seek_cb: Option<unsafe extern "C" fn(*mut c_void, i64, c_int) -> i64> =
            if seekable { Some(seek::<R>) } else { None };
        res.avio = ffi::avio_alloc_context(buffer, IO_BUFFER as c_int, 0, res.io, Some(read_packet::<R>), None, seek_cb);
        if res.avio.is_null() {
            ffi::av_free(buffer as *mut c_void);
            return Err(MediaError::Decode("out of memory".into()));
        }
        (*res.avio).seekable = if seekable { ffi::AVIO_SEEKABLE_NORMAL } else { 0 };
        open_with(&url, res, seekable, options.probesize)
    }
}

/// Open a [`MediaSource`] (taking ownership of a stream).
pub(crate) fn open_source(source: MediaSource, abort: Arc<AtomicBool>) -> Result<Input<'static>, MediaError> {
    match source {
        MediaSource::Path(p) => open_file(&p, abort),
        MediaSource::Stream { stream, size, name } => {
            let seekable = stream.seekable();
            open_reader(stream, seekable, size, &name, abort, ReadOptions::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn state(data: Vec<u8>, seekable: bool) -> IoState<Cursor<Vec<u8>>> {
        let size = Some(data.len() as u64);
        IoState {
            reader: Cursor::new(data),
            pos: 0,
            size,
            seekable,
            block: Vec::new(),
            block_start: 0,
            reader_pos: if seekable { None } else { Some(0) },
            abort: Arc::new(AtomicBool::new(false)),
            read_ahead: READ_AHEAD,
        }
    }

    #[test]
    fn reads_seeks_and_reports_the_size() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut s = state(data.clone(), true);
        let mut buf = [0u8; 100];
        assert_eq!(s.read(&mut buf), Ok(100));
        assert_eq!(&buf[..], &data[..100]);
        assert_eq!(s.seek(0, ffi::AVSEEK_SIZE), 3_000_000);
        assert_eq!(s.seek(-10, 2), 2_999_990);
        assert_eq!(s.read(&mut buf), Ok(10)); // only what's left
        assert_eq!(&buf[..10], &data[2_999_990..]);
        assert_eq!(s.read(&mut buf), Err(ffi::AVERROR_EOF));
        assert_eq!(s.seek(1_500_000, 0), 1_500_000);
        assert_eq!(s.read(&mut buf), Ok(100));
        assert_eq!(&buf[..], &data[1_500_000..1_500_100]);
        assert!(s.seek(-1, 0) < 0);
    }

    #[test]
    fn unseekable_streams_only_go_forward() {
        let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 7) as u8).collect();
        let mut s = state(data.clone(), false);
        let mut buf = [0u8; 10];
        s.pos = 1_500_000; // FFmpeg skipping ahead
        assert_eq!(s.read(&mut buf), Ok(10));
        assert_eq!(&buf[..], &data[1_500_000..1_500_010]);
        s.pos = 5; // back, before the block
        assert!(s.read(&mut buf).is_err());
    }

    /// A source that delivers 16 KiB per read, slowly.
    struct Trickle(Cursor<Vec<u8>>);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(40));
            let n = buf.len().min(16 * 1024);
            self.0.read(&mut buf[..n])
        }
    }

    impl Seek for Trickle {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }

    #[test]
    fn slow_sources_are_not_waited_for_a_whole_block() {
        let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 13) as u8).collect();
        let mut s = IoState {
            reader: Trickle(Cursor::new(data.clone())),
            pos: 0,
            size: Some(data.len() as u64),
            seekable: true,
            block: Vec::new(),
            block_start: 0,
            reader_pos: None,
            abort: Arc::new(AtomicBool::new(false)),
            read_ahead: READ_AHEAD,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let t = Instant::now();
        let n = s.read(&mut buf).unwrap();
        // 1 MiB at 16 KiB per 40 ms would take 2.6 s.
        assert!(t.elapsed() < Duration::from_millis(600), "{:?}", t.elapsed());
        assert!(n > 0 && buf[..n] == data[..n]);
        // Reading on continues seamlessly from the short block.
        let mut all = buf[..n].to_vec();
        while all.len() < 300_000 {
            let n = s.read(&mut buf).unwrap();
            all.extend_from_slice(&buf[..n]);
        }
        assert_eq!(all[..], data[..all.len()]);
    }

    #[test]
    fn abort_stops_reads() {
        let mut s = state(vec![1; 100], true);
        s.abort.store(true, Ordering::Release);
        assert_eq!(s.read(&mut [0u8; 10]), Err(ffi::AVERROR_EXIT));
    }
}
