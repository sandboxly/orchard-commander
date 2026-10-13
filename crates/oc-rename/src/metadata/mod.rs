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

mod asf;
mod audio;
mod bytes;
mod dsf;
mod exif;
mod flv;
mod media;
mod mpeg;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

use oc_vfs::VPath;

pub use audio::AudioTags;
pub use exif::PhotoExif;
pub use media::MediaProperties;

/// Field name -> value; values are never empty.
pub type Metadata = HashMap<String, String>;

/// Whatever a provider fails with; it only means "no metadata from this provider".
pub type ProviderError = Box<dyn std::error::Error + Send + Sync>;

pub const AUDIO: &[&str] = &[
    ".mp3", ".flac", ".m4a", ".m4b", ".aac", ".ogg", ".oga", ".opus", ".wma", ".wav", ".aiff", ".aif", ".ape", ".wv",
    ".mpc", ".dsf",
];
/// Python's list plus the other MPEG stream names (DVD, AVCHD / Blu-ray).
pub const VIDEO: &[&str] = &[
    ".mp4", ".m4v", ".mov", ".mkv", ".webm", ".avi", ".wmv", ".asf", ".flv", ".mpg", ".mpeg", ".3gp", ".ts", ".vob",
    ".m2ts", ".mts",
];
pub const IMAGES: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tif", ".tiff", ".webp", ".heic", ".heif", ".dng", ".cr2", ".nef", ".arw",
    ".orf", ".rw2",
];

/// The file's metadata can't be read (e.g. a cloud-only file); the message says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataUnavailable(pub String);

impl fmt::Display for MetadataUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MetadataUnavailable {}

/// A field a provider offers, for validation and the dialog's Insert menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
}

impl Field {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Field { name: Cow::Borrowed(name), description: Cow::Borrowed(description) }
    }
}

/// Seekable bytes; providers get the file through this.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// The file a provider reads: positioned at its start.
pub struct Source<'a> {
    /// The file's name (for decisions by extension).
    pub name: &'a str,
    /// Set for files on a local disk, for providers that need a real path
    /// (e.g. to run a tool); files elsewhere are only available as `file`.
    pub local_path: Option<&'a Path>,
    pub file: &'a mut dyn ReadSeek,
}

impl Source<'_> {
    /// Lower-case extension with the dot ("" if none), like Python's `Path.suffix`.
    pub fn suffix(&self) -> String {
        suffix(self.name)
    }
}

/// A source of `[=field]` values for some kinds of files.
pub trait MetadataProvider: Send + Sync {
    fn id(&self) -> &str;
    /// For the Insert menu.
    fn label(&self) -> &str;
    /// Field name -> description, in menu order.
    fn fields(&self) -> &[Field];
    /// Whether this provider reads files with this name (by extension, cheap).
    fn handles(&self, name: &str) -> bool;
    /// All fields it finds (blocking; runs on a worker thread). Errors and
    /// panics only mean "nothing from this provider".
    fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError>;
}

/// Lower-case extension with the dot: ".mp3"; "" for ".bashrc" or "name.".
pub fn suffix(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

pub(crate) fn has_suffix(name: &str, set: &[&str]) -> bool {
    let s = suffix(name);
    set.contains(&s.as_str())
}

// ---------------------------------------------------------------------------
// Value formatting
// ---------------------------------------------------------------------------

/// A metadata value made safe for a file name: one line, no path separators.
pub fn clean(value: &str) -> String {
    let joined = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = joined.replace(": ", " - ").replace(['/', '\\', ':'], "-").replace('\0', "");
    if cfg!(windows) {
        let text = text.replace(['<', '>', '"', '|', '?', '*'], "_");
        text.trim_matches([' ', '.']).to_string()
    } else {
        text.trim().to_string()
    }
}

/// '3m25s', '1h02m03s', '45s' (Python rounds halves to even).
pub fn duration(seconds: f64) -> String {
    let total = crate::mask::round_half_even(seconds).max(0.0) as u64;
    let (h, rest) = (total / 3600, total % 3600);
    let (m, s) = (rest / 60, rest % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Python's `f"{x:g}"`: 6 significant digits, no trailing zeros.
pub fn format_g(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    if !x.is_finite() {
        return if x.is_nan() { "nan".into() } else if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{x:.5e}"); // rounds to 6 significant digits
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let strip = |s: &str| -> String {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() }
    };
    if (-4..6).contains(&exp) {
        strip(&format!("{x:.*}", (5 - exp) as usize))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mantissa), exp.abs())
    }
}

/// "width", "height" and "resolution" for a size.
pub(crate) fn dimensions(out: &mut Metadata, width: u64, height: u64) {
    out.insert("width".into(), width.to_string());
    out.insert("height".into(), height.to_string());
    out.insert("resolution".into(), format!("{width}x{height}"));
}

/// The first four-digit number standing on its own ("1992-10-27" -> "1992").
pub(crate) fn year(text: &str) -> String {
    static YEAR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\b(\d{4})\b").expect("valid"));
    YEAR.captures(text).map(|c| c[1].to_string()).unwrap_or_default()
}

/// Python's `str(int(s))` for ASCII digit strings: no leading zeros.
fn strip_zeros(s: &str) -> String {
    let t = s.trim_start_matches('0');
    if t.is_empty() { "0".into() } else { t.into() }
}

pub(crate) fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// '3/12' -> ('3', '12'); '03' -> ('3', '').
pub(crate) fn number(text: &str) -> (String, String) {
    let (number, total) = text.split_once('/').unwrap_or((text, ""));
    let (number, total) = (number.trim(), total.trim());
    (
        if is_digits(number) { strip_zeros(number) } else { number.to_string() },
        if is_digits(total) { strip_zeros(total) } else { String::new() },
    )
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// An ordered set of providers; the first with a value for a field wins.
#[derive(Clone)]
pub struct Registry {
    providers: Vec<Arc<dyn MetadataProvider>>,
    fields: Vec<Field>,
    names: HashSet<String>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.providers.iter().map(|p| p.id())).finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::builtin()
    }
}

impl Registry {
    pub fn empty() -> Self {
        Registry { providers: Vec::new(), fields: Vec::new(), names: HashSet::new() }
    }

    /// Audio tags, then video/image properties, then EXIF.
    pub fn builtin() -> Self {
        let mut r = Registry::empty();
        r.providers = vec![Arc::new(AudioTags), Arc::new(MediaProperties), Arc::new(PhotoExif)];
        r.reindex();
        r
    }

    /// Add `provider` ahead of the others, replacing one with the same id.
    pub fn register(&mut self, provider: Arc<dyn MetadataProvider>) {
        self.providers.retain(|p| p.id() != provider.id());
        self.providers.insert(0, provider);
        self.reindex();
    }

    /// Builder form of [`Registry::register`].
    pub fn with(mut self, provider: Arc<dyn MetadataProvider>) -> Self {
        self.register(provider);
        self
    }

    fn reindex(&mut self) {
        self.fields.clear();
        self.names.clear();
        for provider in &self.providers {
            for field in provider.fields() {
                if self.names.insert(field.name.to_string()) {
                    self.fields.push(field.clone());
                }
            }
        }
    }

    pub fn providers(&self) -> &[Arc<dyn MetadataProvider>] {
        &self.providers
    }

    /// Every field any provider offers, with the first provider's description.
    pub fn all_fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn has_field(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Whether any provider reads files with this name.
    pub fn handled(&self, name: &str) -> bool {
        self.providers.iter().any(|p| p.handles(name))
    }

    /// All metadata fields for a file (blocking).
    ///
    /// Files elsewhere than on a local disk are streamed (and buffered if
    /// they can't seek); cloud-only placeholders are not read — that would
    /// download them.
    pub fn read(&self, location: &VPath, is_dir: bool, online_only: bool) -> Result<Metadata, MetadataUnavailable> {
        let name = location.name();
        let candidates: Vec<&Arc<dyn MetadataProvider>> = self.providers.iter().filter(|p| p.handles(&name)).collect();
        if candidates.is_empty() || is_dir {
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
        let mut out = Metadata::new();
        for provider in candidates {
            if file.seek(SeekFrom::Start(0)).is_err() {
                break;
            }
            let mut src = Source { name: &name, local_path: local.as_deref(), file: &mut *file };
            // Corrupt files: no metadata from this provider (a panicking parser included).
            let Ok(Ok(values)) = catch_unwind(AssertUnwindSafe(|| provider.read(&mut src))) else { continue };
            for (field, value) in values {
                if !value.is_empty() {
                    out.entry(field).or_insert(value);
                }
            }
        }
        Ok(out)
    }
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

static GLOBAL: LazyLock<RwLock<Arc<Registry>>> = LazyLock::new(|| RwLock::new(Arc::new(Registry::builtin())));

/// The application's registry (built-ins plus everything [`register`]ed).
pub fn registry() -> Arc<Registry> {
    GLOBAL.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Add a provider to the application's registry, ahead of the built-ins.
pub fn register(provider: Arc<dyn MetadataProvider>) {
    let mut global = GLOBAL.write().unwrap_or_else(|e| e.into_inner());
    let mut next = (**global).clone();
    next.register(provider);
    *global = Arc::new(next);
}

/// Every field name any provider offers -> its description (first provider's).
pub fn all_fields() -> Vec<Field> {
    registry().all_fields().to_vec()
}

pub fn handled(name: &str) -> bool {
    registry().handled(name)
}

/// All metadata fields for a file from the application's registry (blocking).
pub fn read(location: &VPath, is_dir: bool, online_only: bool) -> Result<Metadata, MetadataUnavailable> {
    registry().read(location, is_dir, online_only)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(clean("AC/DC"), "AC-DC");
        assert_eq!(clean("Live: Volume 1"), "Live - Volume 1");
        assert_eq!(clean("  a\n\tb  "), "a b");
        assert_eq!(clean("a:b\\c\0"), "a-b-c");
        assert_eq!(clean("x \0"), "x");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(2.063), "2s");
        assert_eq!(duration(205.0), "3m25s");
        assert_eq!(duration(3723.0), "1h02m03s");
        assert_eq!(duration(2.5), "2s");
        assert_eq!(duration(0.4), "0s");
        assert_eq!(duration(60.0), "1m00s");
    }

    #[test]
    fn g_format() {
        assert_eq!(format_g(50.0), "50");
        assert_eq!(format_g(17.5), "17.5");
        assert_eq!(format_g(29.97002997), "29.97");
        assert_eq!(format_g(23.976023976), "23.976");
        assert_eq!(format_g(1234567.0), "1.23457e+06");
        assert_eq!(format_g(0.0001), "0.0001");
        assert_eq!(format_g(0.00001), "1e-05");
        assert_eq!(format_g(100000.0), "100000");
        assert_eq!(format_g(999999.5), "1e+06");
    }

    #[test]
    fn years_and_numbers() {
        assert_eq!(year("1992-10-27"), "1992");
        assert_eq!(year("c1992"), "");
        assert_eq!(year("12345"), "");
        assert_eq!(year("Recorded 2001"), "2001");
        assert_eq!(number("3/12"), ("3".into(), "12".into()));
        assert_eq!(number("03"), ("3".into(), "".into()));
        assert_eq!(number(" 7 / x"), ("7".into(), "".into()));
        assert_eq!(number("A1"), ("A1".into(), "".into()));
        assert_eq!(number(""), ("".into(), "".into()));
    }

    #[test]
    fn suffixes() {
        assert_eq!(suffix("a.MP3"), ".mp3");
        assert_eq!(suffix(".bashrc"), "");
        assert_eq!(suffix("name."), "");
        assert_eq!(suffix("a.tar.gz"), ".gz");
    }

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

    #[test]
    fn registry_order_and_fields() {
        let r = Registry::builtin();
        let ids: Vec<&str> = r.providers().iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["audio", "media", "exif"]);
        assert!(r.has_field("artist") && r.has_field("quality") && r.has_field("taken"));
        assert!(!r.has_field("nonsense"));
        // Shared fields keep the first provider's description.
        let title = r.all_fields().iter().find(|f| f.name == "title").unwrap();
        assert_eq!(title.description, "Title");
        assert!(r.handled("x.flac") && r.handled("x.MKV") && !r.handled("x.txt"));
    }
}
