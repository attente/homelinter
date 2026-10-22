//! Built-in audio tag reader using the pure-Rust `lofty` crate.
//!
//! `exiftool` reads ID3 and related tags natively and is preferred when
//! available (see `exiftool.rs`); this extractor is the in-process fallback
//! for audio files and for video files when tag metadata is absent from
//! the photo EXIF chain. `lofty` reads MP3, FLAC, Vorbis Comments,
//! MP4/M4A, WAV, AIFF, and related containers without an external
//! binary. Tag reads are structural rather than linear: payloads are
//! skipped by header offsets, so a large audio file costs a handful of
//! seeks, not a whole-file read.

use std::fs::File;
use std::path::Path;

use homelinter_model::asset_kind::AssetKind;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use crate::dateparse::parse_tag_date_parts;
use crate::extractor::{EmbeddedMeta, ExtractionContext, FieldSet, MetadataExtractor};

/// Reads container tag metadata (title, artist, date) and audio
/// properties (bitrate, duration).
pub struct BuiltinAudioExtractor;

impl MetadataExtractor for BuiltinAudioExtractor {
    fn name(&self) -> &'static str {
        "lofty"
    }

    fn provides(&self, kind: AssetKind) -> FieldSet {
        if matches!(kind, AssetKind::Audio | AssetKind::Video) {
            FieldSet::MEDIA_STREAM
        } else {
            FieldSet::EMPTY
        }
    }

    fn extract(&self, path: &Path, kind: AssetKind, _ctx: &ExtractionContext) -> EmbeddedMeta {
        let mut meta = EmbeddedMeta::default();
        if !matches!(kind, AssetKind::Audio | AssetKind::Video) {
            return meta;
        }
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return meta,
        };
        let tagged = match lofty::read_from(&mut file) {
            Ok(t) => t,
            Err(_) => return meta,
        };
        // The container parsed, so every tag this reader looks for is
        // answered, present or not.
        meta.settled = FieldSet::MEDIA_STREAM;

        // A container may hold several tags (ID3v2 plus ID3v1, for
        // example); collect the first non-empty value for each field.
        let mut title: Option<String> = None;
        let mut author: Option<String> = None;
        let mut date: Option<String> = None;
        for tag in tagged.tags() {
            if title.is_none() {
                title = tag.title().map(|c| c.into_owned());
            }
            if author.is_none() {
                author = tag.artist().map(|c| c.into_owned());
            }
            if date.is_none() {
                date = tag
                    .get_string(&lofty::tag::ItemKey::RecordingDate)
                    .map(|c| c.to_string());
            }
            if title.is_some() && author.is_some() && date.is_some() {
                break;
            }
        }
        meta.title = title.filter(|t| !t.trim().is_empty());
        meta.author = author.filter(|t| !t.trim().is_empty());
        meta.date_taken = date.as_deref().and_then(parse_tag_date_parts);

        let props = tagged.properties();
        if !props.duration().is_zero() {
            meta.duration_secs = Some(props.duration().as_secs());
        }
        meta.bitrate_kbps = props.audio_bitrate().or(props.overall_bitrate());
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ID3v2.4 text frame: 4-byte ID, 4-byte syncsafe size, 2 flag
    /// bytes, then a one-byte text encoding (0 = ISO-8859-1) and the
    /// payload. Every fixture body is under 128 bytes, so syncsafe sizes
    /// equal their plain big-endian value.
    fn frame(id: &str, text: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id.as_bytes());
        v.extend_from_slice(&[0, 0, 0, text.len() as u8 + 1]);
        v.extend_from_slice(&[0, 0]);
        v.push(0);
        v.extend_from_slice(text.as_bytes());
        v
    }

    /// ID3v2.4 header plus frames. The 10-byte header is not counted in
    /// the syncsafe size field; every fixture body is under 128 bytes, so
    /// the field is simply `[0, 0, 0, len]`.
    fn id3v2(frames: Vec<Vec<u8>>) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[4, 0, 0]);
        v.extend_from_slice(&[0, 0, 0, body.len() as u8]);
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn reads_id3_title_artist_date_and_properties() {
        let dir = std::env::temp_dir().join(format!(
            "hl-audio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("track.mp3");

        let mut data = id3v2(vec![
            frame("TIT2", "Sunset"),
            frame("TPE1", "Jane Doe"),
            frame("TDRC", "2024-03-15"),
        ]);
        // A run of MPEG1 Layer3 128 kbps 44.1 kHz stereo frames (each 4
        // header bytes plus 413 payload bytes) so lofty can derive the
        // bitrate and duration. Repeating frames keep the stream valid
        // all the way to EOF; trailing zero bytes would abort the scan.
        for _ in 0..20 {
            data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            data.resize(data.len() + 413, 0);
        }
        std::fs::write(&path, &data).unwrap();

        let meta =
            BuiltinAudioExtractor.extract(&path, AssetKind::Audio, &ExtractionContext::default());
        assert_eq!(meta.title.as_deref(), Some("Sunset"));
        assert_eq!(meta.author.as_deref(), Some("Jane Doe"));
        let dp = meta.date_taken.expect("tag date");
        assert_eq!((dp.year, dp.month, dp.day), (2024, 3, 15));
        assert_eq!(meta.bitrate_kbps, Some(128));
        std::fs::remove_dir_all(&dir).ok();
    }
}
