//! Preview content helpers, independent of any previewer: what kind of file
//! something is, text detection and decoding, hex dumps, listings, file facts.

use std::io::Read;
use std::path::Path;

use oc_vfs::{Entry, ListOptions, VPath, human_size};

/// How much of a file is read to tell text from binary.
pub const TEXT_SNIFF_BYTES: usize = 8192;
/// Text previews show at most this much of a file.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// Hex previews dump this much of the beginning.
pub const HEXDUMP_BYTES: usize = 4096;
/// Listings show at most this many entries.
pub const MAX_LISTING_ENTRIES: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Directory,
    Archive,
    Text,
    Markdown,
    Image,
    Video,
    Audio,
    Binary,
    Empty,
    Unreadable,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Directory => "directory",
            Kind::Archive => "archive",
            Kind::Text => "text",
            Kind::Markdown => "markdown",
            Kind::Image => "image",
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Binary => "binary",
            Kind::Empty => "empty",
            Kind::Unreadable => "unreadable",
        }
    }
}

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg"];
pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "3gp", "ts", "mts", "m2ts", "ogv",
];
pub const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif"];
pub const MARKDOWN_EXTS: &[&str] = &["md", "markdown", "mdown"];

/// Archive suffixes, longest first so "x.tar.gz" is a tarball, not a gzip file.
pub const ARCHIVE_SUFFIXES: &[(&str, &str)] = &[
    (".tar.gz", "tar"),
    (".tar.bz2", "tar"),
    (".tar.xz", "tar"),
    (".tgz", "tar"),
    (".tbz", "tar"),
    (".tbz2", "tar"),
    (".txz", "tar"),
    (".tar", "tar"),
    (".zip", "zip"),
    (".7z", "7z"),
    (".rar", "rar"),
    (".gz", "gz"),
    (".bz2", "bz2"),
    (".xz", "xz"),
    (".lzma", "xz"),
];

/// "tar", "zip", … for archive names; a bare suffix (".zip") is not an archive.
pub fn archive_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    ARCHIVE_SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.len() > suffix.len() && lower.ends_with(suffix))
        .map(|&(_, kind)| kind)
}

pub fn is_archive(name: &str) -> bool {
    archive_type(name).is_some()
}

/// Heuristic: no NUL bytes and decodes as UTF-8 (allowing a cut-off final
/// character), else "mostly printable" for legacy 8-bit encodings.
pub fn looks_like_text(data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    if data.contains(&0) {
        return false;
    }
    match std::str::from_utf8(data) {
        Ok(_) => return true,
        // Python's rule: an error in the last 3 bytes is a multi-byte char split by the sniff limit.
        Err(e) if e.valid_up_to() + 3 >= data.len() => return true,
        Err(_) => {}
    }
    let printable = data.iter().filter(|&&b| (32..127).contains(&b) || matches!(b, 9 | 10 | 13) || b >= 160).count();
    printable as f64 / data.len() as f64 > 0.95
}

/// Lower-case extension after the last dot ("" if none). Unlike
/// [`crate::ext_for_name`], ".png" counts as a PNG here, as in Python.
fn name_ext(name: &str) -> String {
    name.rfind('.').map(|i| name[i + 1..].to_lowercase()).unwrap_or_default()
}

/// The kind implied by the file name alone (archives and media), if any.
pub fn kind_by_name(name: &str) -> Option<Kind> {
    if is_archive(name) {
        return Some(Kind::Archive);
    }
    let ext = name_ext(name);
    let ext = ext.as_str();
    if IMAGE_EXTS.contains(&ext) {
        Some(Kind::Image)
    } else if VIDEO_EXTS.contains(&ext) {
        Some(Kind::Video)
    } else if AUDIO_EXTS.contains(&ext) {
        Some(Kind::Audio)
    } else {
        None
    }
}

/// Classify a file from its name and first bytes (no local copy needed).
pub fn kind_from_head(name: &str, head: &[u8], size: u64) -> Kind {
    if let Some(kind) = kind_by_name(name) {
        return kind;
    }
    if size == 0 {
        return Kind::Empty;
    }
    if looks_like_text(head) {
        if MARKDOWN_EXTS.contains(&name_ext(name).as_str()) { Kind::Markdown } else { Kind::Text }
    } else {
        Kind::Binary
    }
}

/// Classify a file on any file system, reading at most its first 8 KB.
pub fn detect(target: &VPath) -> Kind {
    let Ok(entry) = target.stat() else { return Kind::Unreadable };
    if entry.is_dir {
        return Kind::Directory;
    }
    let name = target.name();
    if let Some(kind) = kind_by_name(&name) {
        return kind;
    }
    let mut head = Vec::with_capacity(TEXT_SNIFF_BYTES.min(entry.size as usize));
    let read = target.open_read().and_then(|f| Ok(f.take(TEXT_SNIFF_BYTES as u64).read_to_end(&mut head)?));
    if read.is_err() {
        return Kind::Unreadable;
    }
    kind_from_head(&name, &head, entry.size)
}

/// Decoded text and whether it was cut at the size limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextContent {
    pub text: String,
    pub truncated: bool,
}

/// UTF-8 if possible; else, for a truncated file, UTF-8 dropping bad bytes
/// (the cut may have split a character), otherwise Latin-1. CRLF becomes LF.
pub fn decode_text(data: &[u8], truncated: bool) -> TextContent {
    let text = match std::str::from_utf8(data) {
        Ok(s) => s.to_owned(),
        Err(_) if truncated => utf8_ignoring_errors(data),
        Err(_) => latin1(data),
    };
    let text = if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text };
    TextContent { text, truncated }
}

/// Like Python's `decode("utf-8", errors="ignore")`.
pub(crate) fn utf8_ignoring_errors(mut data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    loop {
        match std::str::from_utf8(data) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (good, rest) = data.split_at(e.valid_up_to());
                // SAFETY-free: `good` was just validated.
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                match e.error_len() {
                    Some(n) => data = &rest[n..],
                    None => return out, // incomplete sequence at the end
                }
            }
        }
    }
}

pub(crate) fn latin1(data: &[u8]) -> String {
    data.iter().map(|&b| b as char).collect()
}

/// The first `limit` bytes of a local file as text.
pub fn read_text(path: &Path, limit: usize) -> std::io::Result<TextContent> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut data)?;
    let truncated = data.len() > limit;
    data.truncate(limit);
    Ok(decode_text(&data, truncated))
}

/// Classic 16-bytes-per-line dump: offset, hex in two groups of 8, ASCII.
pub fn hexdump(data: &[u8], offset: u64) -> String {
    use std::fmt::Write;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // 8 offset + 2 + 48 hex + 2 + 16 ASCII + newline
    let mut out = String::with_capacity(data.len().div_ceil(16) * 77);
    for (i, chunk) in data.chunks(16).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{:08x}  ", offset.wrapping_add(i as u64 * 16));
        let start = out.len();
        for (j, &b) in chunk.iter().enumerate() {
            if j > 0 {
                out.push(' ');
            }
            if j == 8 {
                out.push(' ');
            }
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
        let width = out.len() - start; // ASCII only: bytes == chars
        out.extend(std::iter::repeat_n(' ', 48usize.saturating_sub(width) + 2));
        out.extend(chunk.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }));
    }
    out
}

/// A dump of the first `limit` bytes of a local file.
pub fn read_hexdump(path: &Path, limit: usize) -> std::io::Result<String> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.take(limit as u64).read_to_end(&mut data)?;
    Ok(hexdump(&data, 0))
}

/// "1,234,567"
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn size_fact(size: u64) -> String {
    format!("{} ({} bytes)", human_size(size), thousands(size))
}

/// "2024-05-01 13:45:00" in local time.
pub fn format_mtime(mtime: f64) -> String {
    use chrono::{Local, TimeZone};
    let secs = mtime.floor();
    let nanos = ((mtime - secs) * 1e9) as u32;
    match Local.timestamp_opt(secs as i64, nanos) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        chrono::LocalResult::None => String::new(),
    }
}

/// Facts about a file on any file system, from its listing entry.
pub fn describe_entry(entry: &Entry, location: &str) -> Vec<(String, String)> {
    let mut facts = vec![("Name".to_string(), entry.name.clone()), ("Location".to_string(), location.to_string())];
    if !entry.is_dir {
        facts.push(("Size".into(), size_fact(entry.size)));
    }
    if entry.mtime != 0.0 {
        facts.push(("Modified".into(), format_mtime(entry.mtime)));
    }
    facts
}

/// Facts about a local file, including permissions and a link's target.
pub fn describe(path: &Path) -> std::io::Result<Vec<(String, String)>> {
    let meta = std::fs::symlink_metadata(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let location = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let mut facts = vec![("Name".to_string(), name), ("Location".to_string(), location)];
    if !meta.is_dir() {
        facts.push(("Size".into(), size_fact(meta.len())));
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    facts.push(("Modified".into(), format_mtime(mtime)));
    facts.push(("Permissions".into(), filemode(&meta)));
    if meta.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(path) {
            facts.push(("Link to".into(), target.display().to_string()));
        }
    Ok(facts)
}

/// "-rw-r--r--" like `ls -l` (Python's `stat.filemode`).
#[cfg(unix)]
pub fn filemode(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let ft = meta.file_type();
    let mode = meta.permissions().mode();
    let kind = if ft.is_symlink() {
        'l'
    } else if ft.is_dir() {
        'd'
    } else if ft.is_char_device() {
        'c'
    } else if ft.is_block_device() {
        'b'
    } else if ft.is_fifo() {
        'p'
    } else if ft.is_socket() {
        's'
    } else {
        '-'
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    let special = |exec: u32, special: u32, on: char, off: char| match (mode & exec != 0, mode & special != 0) {
        (true, true) => on,
        (false, true) => off,
        (true, false) => 'x',
        (false, false) => '-',
    };
    s.push(bit(0o400, 'r'));
    s.push(bit(0o200, 'w'));
    s.push(special(0o100, 0o4000, 's', 'S'));
    s.push(bit(0o040, 'r'));
    s.push(bit(0o020, 'w'));
    s.push(special(0o010, 0o2000, 's', 'S'));
    s.push(bit(0o004, 'r'));
    s.push(bit(0o002, 'w'));
    s.push(special(0o001, 0o1000, 't', 'T'));
    s
}

#[cfg(not(unix))]
pub fn filemode(meta: &std::fs::Metadata) -> String {
    let kind = if meta.is_dir() { 'd' } else { '-' };
    let w = if meta.permissions().readonly() { '-' } else { 'w' };
    format!("{kind}r{w}-r{w}-r{w}-")
}

/// "1 directory, 2 files (12 B)"
pub fn listing_summary(dirs: usize, files: usize, total: u64) -> String {
    format!(
        "{dirs} director{}, {files} file{} ({})",
        if dirs == 1 { "y" } else { "ies" },
        if files == 1 { "" } else { "s" },
        human_size(total)
    )
}

/// One listing line: "name/" for folders, name and right-aligned size for files.
pub fn listing_line(e: &Entry) -> String {
    if e.is_dir {
        format!("{}{}", e.name, std::path::MAIN_SEPARATOR)
    } else {
        format!("{:<40} {:>9}", e.name, human_size(e.size))
    }
}

/// A folder-style summary: counts and total size, then one line per entry.
pub fn format_listing(entries: &[Entry], max_entries: usize) -> String {
    let dirs = entries.iter().filter(|e| e.is_dir).count();
    let total = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let mut lines = vec![listing_summary(dirs, entries.len() - dirs, total), String::new()];
    lines.extend(entries.iter().take(max_entries).map(listing_line));
    if entries.len() > max_entries {
        lines.push(format!("… and {} more", entries.len() - max_entries));
    }
    lines.join("\n")
}

/// [`format_listing`] of a folder on any file system.
pub fn directory_summary(target: &VPath, show_hidden: bool) -> oc_vfs::Result<String> {
    let entries = target.list_dir(ListOptions { show_hidden, ..ListOptions::default() })?;
    Ok(format_listing(&entries, MAX_LISTING_ENTRIES))
}
