//! Stream building blocks shared by the formats.
//!
//! Members are read through *independent* streams: each holds its own
//! position over a shared file handle (positional reads), so previews, copies
//! and searches can read the same archive from several threads without the
//! lock the Python version needed around one shared file object.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oc_vfs::ReadStream;

/// Read at an absolute offset without touching a shared cursor.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

/// Fill `buf` completely from `offset`; errors with UnexpectedEof if the file is shorter.
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The archive's file handle, opened lazily and droppable: the open-archive
/// cache closes evicted archives (they may still be shown in a panel), which
/// must not keep the file locked on Windows. Streams already reading keep
/// their own `Arc<File>`, so closing never breaks them.
#[derive(Debug)]
pub(crate) struct HandleSlot {
    path: PathBuf,
    file: Mutex<Option<Arc<File>>>,
}

impl HandleSlot {
    pub(crate) fn new(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(HandleSlot {
            path: path.to_path_buf(),
            file: Mutex::new(Some(Arc::new(file))),
        })
    }

    pub(crate) fn get(&self) -> io::Result<Arc<File>> {
        let mut slot = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = slot.as_ref() {
            return Ok(f.clone());
        }
        let f = Arc::new(File::open(&self.path)?);
        *slot = Some(f.clone());
        Ok(f)
    }

    pub(crate) fn close(&self) {
        self.file.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

/// A cheap-to-clone `Read + Seek` over the archive file, for libraries that
/// want to own their reader (the zip crate clones it per member).
#[derive(Clone, Debug)]
pub(crate) struct SharedFile {
    slot: Arc<HandleSlot>,
    pos: u64,
}

impl SharedFile {
    pub(crate) fn new(slot: Arc<HandleSlot>) -> Self {
        SharedFile { slot, pos: 0 }
    }
}

impl Read for SharedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = read_at(&*self.slot.get()?, buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.slot.get()?.metadata()?.len().checked_add_signed(d),
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

/// A byte range of a file: stored zip members, plain tar members. Truly seekable.
pub(crate) struct Window {
    file: Arc<File>,
    start: u64,
    len: u64,
    pos: u64,
}

impl Window {
    pub(crate) fn new(file: Arc<File>, start: u64, len: u64) -> Self {
        Window {
            file,
            start,
            len,
            pos: 0,
        }
    }

    /// The whole file from the start; for decoders that want a plain reader.
    pub(crate) fn whole(file: Arc<File>) -> Self {
        Window::new(file, 0, u64::MAX)
    }
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.len - self.pos) as usize;
        let n = read_at(
            &self.file,
            &mut buf[..want],
            self.start.saturating_add(self.pos),
        )?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Window {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let len = if self.len == u64::MAX {
                    self.file.metadata()?.len()
                } else {
                    self.len
                };
                len.checked_add_signed(d)
            }
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

impl ReadStream for Window {}

pub(crate) type Opener = Box<dyn Fn() -> io::Result<Box<dyn Read + Send>> + Send + Sync>;

/// A compressed stream that can still seek, by decoding again from the start
/// (backwards) or decoding and discarding (forwards) — what Python's gzip and
/// zipfile objects do. Seeking works but costs time proportional to the
/// offset, so `seekable()` is false: callers that need cheap random access
/// should materialize the member instead.
pub(crate) struct Decoded {
    open: Opener,
    inner: Option<Box<dyn Read + Send>>,
    pos: u64,
    len: Option<u64>,
}

impl Decoded {
    pub(crate) fn new(open: Opener, len: Option<u64>) -> Self {
        Decoded {
            open,
            inner: None,
            pos: 0,
            len,
        }
    }

    fn inner(&mut self) -> io::Result<&mut Box<dyn Read + Send>> {
        if self.inner.is_none() {
            self.inner = Some((self.open)()?);
            self.pos = 0;
        }
        Ok(self.inner.as_mut().expect("just set"))
    }

    /// Skip `n` decoded bytes; returns how many were actually there.
    fn skip(&mut self, n: u64) -> io::Result<u64> {
        let skipped = io::copy(&mut self.inner()?.take(n), &mut io::sink())?;
        self.pos += skipped;
        Ok(skipped)
    }
}

impl Read for Decoded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner()?.read(buf)?;
        self.pos += n as u64;
        if n == 0 && !buf.is_empty() && self.len.is_none() {
            self.len = Some(self.pos);
        }
        Ok(n)
    }
}

impl Seek for Decoded {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let len = match self.len {
                    Some(l) => l,
                    None => {
                        self.skip(u64::MAX)?;
                        self.len = Some(self.pos);
                        self.pos
                    }
                };
                len.checked_add_signed(d)
            }
        };
        let target = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        if target == self.pos && self.inner.is_some() {
            return Ok(target);
        }
        if target < self.pos {
            self.inner = None;
        }
        self.inner()?;
        let have = self.pos;
        self.skip(target - have)?;
        // Like Python's compressed streams, a seek past the end stops at the end.
        Ok(self.pos)
    }
}

impl ReadStream for Decoded {
    fn seekable(&self) -> bool {
        false
    }
}

/// A member decoded into a temp file (7z, rar, unusual zip methods). The temp
/// directory goes away with the stream; field order makes the file close first.
pub(crate) struct TempFileStream {
    file: File,
    _dir: Option<tempfile::TempDir>,
}

impl TempFileStream {
    pub(crate) fn new(mut file: File, dir: Option<tempfile::TempDir>) -> io::Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        Ok(TempFileStream { file, _dir: dir })
    }
}

impl Read for TempFileStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for TempFileStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl ReadStream for TempFileStream {}

/// An empty member (directory-like entries that Python reads as b"").
pub(crate) fn empty_stream() -> Box<dyn ReadStream> {
    Box::new(io::Cursor::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_with(data: &[u8]) -> (tempfile::NamedTempFile, Arc<File>) {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(data).unwrap();
        let f = Arc::new(File::open(t.path()).unwrap());
        (t, f)
    }

    #[test]
    fn window_reads_and_seeks_within_range() {
        let (_t, f) = temp_with(b"0123456789");
        let mut w = Window::new(f, 2, 5);
        let mut s = String::new();
        w.read_to_string(&mut s).unwrap();
        assert_eq!(s, "23456");
        w.seek(SeekFrom::End(-2)).unwrap();
        s.clear();
        w.read_to_string(&mut s).unwrap();
        assert_eq!(s, "56");
        assert!(w.seek(SeekFrom::Current(-100)).is_err());
    }

    #[test]
    fn decoded_seeks_by_redecoding() {
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let shared = Arc::new(data.clone());
        let opener: Opener = Box::new(move || {
            Ok(Box::new(io::Cursor::new(shared.as_ref().clone())) as Box<dyn Read + Send>)
        });
        let mut d = Decoded::new(opener, None);
        assert!(!d.seekable());
        let mut buf = [0u8; 4];
        d.seek(SeekFrom::Start(5000)).unwrap();
        d.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[5000..5004]);
        d.seek(SeekFrom::Start(10)).unwrap();
        d.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[10..14]);
        assert_eq!(d.seek(SeekFrom::End(-1)).unwrap(), 9999);
        assert_eq!(d.seek(SeekFrom::Start(20_000)).unwrap(), 10_000);
    }

    #[test]
    fn shared_file_survives_close() {
        let (t, _) = temp_with(b"abc");
        let slot = Arc::new(HandleSlot::new(t.path()).unwrap());
        let mut r = SharedFile::new(slot.clone());
        slot.close();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
    }
}
