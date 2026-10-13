//! Audio tags and properties via lofty (MP3, FLAC, MP4/M4A, Ogg, Opus, WAV,
//! AIFF, APE, WavPack, Musepack, AAC); WMA (ASF) and DSF, which lofty
//! doesn't read, with the small parsers in [`super::asf`] and [`super::dsf`].

use std::io::{BufReader, Cursor, SeekFrom};

use lofty::config::ParseOptions;
use lofty::file::{FileType, TaggedFile};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{Tag, TagType};

use super::bytes::read_at;
use super::{
    AUDIO, Field, Metadata, MetadataProvider, ProviderError, ReadSeek, Source, VIDEO, asf, clean, dsf, duration,
    has_suffix, is_digits, number, year,
};

/// Audio tags, also the tags of MP4/M4V/MOV videos.
pub struct AudioTags;

const FIELDS: &[Field] = &[
    Field::new("artist", "Artist"),
    Field::new("albumartist", "Album artist"),
    Field::new("album", "Album"),
    Field::new("title", "Title"),
    Field::new("track", "Track number, 2 digits (03)"),
    Field::new("tracktotal", "Number of tracks"),
    Field::new("disc", "Disc number"),
    Field::new("year", "Year"),
    Field::new("genre", "Genre"),
    Field::new("composer", "Composer"),
    Field::new("duration", "Length (3m25s)"),
    Field::new("bitrate", "Bit rate in kbit/s"),
];

fn probe(file: &mut dyn ReadSeek, ext: &str, read_properties: bool) -> Result<Option<TaggedFile>, ProviderError> {
    file.seek(SeekFrom::Start(0))?;
    let options = ParseOptions::new().read_properties(read_properties).read_cover_art(false);
    let mut probe = Probe::new(BufReader::new(file)).options(options).guess_file_type()?;
    if probe.file_type().is_none() {
        // No magic bytes (e.g. raw AAC/MP3 without a tag): trust the extension.
        match FileType::from_ext(ext.trim_start_matches('.')) {
            Some(t) => probe = probe.set_file_type(t),
            None => return Ok(None),
        }
    }
    Ok(Some(probe.read()?))
}

/// The first of `keys` the tag has (even if empty, as mutagen's lookup did).
fn tag_value<'t>(tag: Option<&'t Tag>, keys: &[ItemKey]) -> &'t str {
    let Some(tag) = tag else { return "" };
    keys.iter().find_map(|k| tag.get_string(*k)).unwrap_or("")
}

impl MetadataProvider for AudioTags {
    fn id(&self) -> &str {
        "audio"
    }

    fn label(&self) -> &str {
        "Audio tags"
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn handles(&self, name: &str) -> bool {
        has_suffix(name, AUDIO) || has_suffix(name, &[".mp4", ".m4v", ".mov"]) // MP4 tags: also on videos
    }

    fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError> {
        let ext = src.suffix();
        let is_video = VIDEO.contains(&ext.as_str());
        let magic = read_at(src.file, 0, 16)?;
        let other = if asf::is_asf(&magic) {
            Some(asf::read(&mut BufReader::new(&mut *src.file))?.audio_fields())
        } else if dsf::is_dsf(&magic) {
            Some(dsf_fields(src.file)?)
        } else {
            None
        };
        if let Some(mut out) = other {
            if is_video {
                out.remove("bitrate"); // as below: no audio properties for videos
            }
            return Ok(out);
        }
        let tagged = match probe(src.file, &ext, true) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(Metadata::new()),
            // Videos often have no audio stream lofty can describe; their tags still count.
            Err(_) if is_video => match probe(src.file, &ext, false)? {
                Some(t) => t,
                None => return Ok(Metadata::new()),
            },
            Err(e) => return Err(e),
        };
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
        let mut out = tag_fields(tag);
        let props = tagged.properties();
        let length = props.duration().as_secs_f64();
        if length > 0.0 {
            out.insert("duration".into(), duration(length));
        }
        // MP4 video: only the tags and length, not the audio properties.
        if let Some(kbps) = props.audio_bitrate().or(props.overall_bitrate()).filter(|&b| b > 0 && !is_video) {
            out.insert("bitrate".into(), kbps.to_string());
        }
        Ok(out)
    }
}

/// The tag fields, as the Python version got them from mutagen's "easy" tags.
fn tag_fields(tag: Option<&Tag>) -> Metadata {
    let get = |keys: &[ItemKey]| clean(tag_value(tag, keys));
    let mut out = Metadata::new();
    let mut put = |name: &str, value: String| {
        if !value.is_empty() {
            out.insert(name.to_string(), value);
        }
    };
    put("artist", get(&[ItemKey::TrackArtist]));
    put("albumartist", get(&[ItemKey::AlbumArtist]));
    put("album", get(&[ItemKey::AlbumTitle]));
    put("title", get(&[ItemKey::TrackTitle]));
    put("genre", get(&[ItemKey::Genre]));
    put("composer", get(&[ItemKey::Composer]));

    // lofty splits "3/12" into number and total already; a total only
    // written as "3/12" comes back normalised like Python's int() did.
    let (track, from_number) = number(tag_value(tag, &[ItemKey::TrackNumber]).trim());
    let pair_tag = tag.is_some_and(|t| matches!(t.tag_type(), TagType::Id3v2 | TagType::Mp4Ilst));
    let total_raw = tag_value(tag, &[ItemKey::TrackTotal]);
    let total = if pair_tag && is_digits(total_raw) {
        number(total_raw).0
    } else if total_raw.is_empty() {
        from_number
    } else {
        clean(total_raw)
    };
    if is_digits(&track) {
        let width = if is_digits(&total) && total.parse::<u64>().map_or(true, |n| n >= 100) { 3 } else { 2 };
        put("track", format!("{track:0>width$}"));
    }
    put("tracktotal", total);
    put("disc", number(tag_value(tag, &[ItemKey::DiscNumber]).trim()).0);
    put("year", year(&clean(tag_value(tag, &[ItemKey::RecordingDate, ItemKey::OriginalReleaseDate, ItemKey::Year]))));
    out
}

/// DSF: mutagen's length and bit rate from the format chunk; the ID3v2 tag
/// read as for MP3s (mutagen gave it raw frame names the Python version
/// never matched, so it found no tags at all).
fn dsf_fields(file: &mut dyn ReadSeek) -> Result<Metadata, ProviderError> {
    let len = file.seek(SeekFrom::End(0))?;
    let info = dsf::read(file, len)?;
    let tagged = info.id3.as_deref().and_then(|tag| {
        let options = ParseOptions::new().read_properties(false).read_cover_art(false);
        let wav = Cursor::new(dsf::id3_in_wav(tag));
        Probe::new(wav).options(options).set_file_type(FileType::Wav).read().ok()
    });
    let mut out = tag_fields(tagged.as_ref().and_then(|t| t.tag(TagType::Id3v2)));
    let length = info.length();
    if length > 0.0 {
        out.insert("duration".into(), duration(length));
    }
    if info.bitrate() > 0 {
        let kbps = crate::mask::round_half_even(info.bitrate() as f64 / 1000.0);
        out.insert("bitrate".into(), format!("{kbps}"));
    }
    Ok(out)
}
