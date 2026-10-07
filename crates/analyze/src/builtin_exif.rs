//! Built-in EXIF reader using the pure-Rust kamadak-exif crate.
//!
//! ExifTool is preferred when available. This extractor is the in-process
//! fallback for photo files and supports the image containers handled by the
//! crate, including JPEG, TIFF, PNG, WebP, and HEIF where applicable.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use exif::{In, Reader, Tag, Value};
use homelinter_model::asset_kind::AssetKind;

use crate::dateparse::parse_exif_datetime;
use crate::extractor::{EmbeddedMeta, ExtractionContext, MetadataExtractor};

const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

pub struct BuiltinExifExtractor;

impl MetadataExtractor for BuiltinExifExtractor {
    fn name(&self) -> &'static str {
        "kamadak-exif"
    }

    fn extract(&self, path: &Path, kind: AssetKind, ctx: &ExtractionContext) -> EmbeddedMeta {
        if kind != AssetKind::Photo {
            return EmbeddedMeta::default();
        }

        let cap = ctx.tool_stdout_max_bytes.min(MAX_INPUT_BYTES as u64) as usize;
        let Ok(file) = File::open(path) else {
            return EmbeddedMeta::default();
        };
        let mut bytes = Vec::new();
        if BufReader::new(file)
            .take(cap as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return EmbeddedMeta::default();
        }

        let mut reader = BufReader::new(std::io::Cursor::new(bytes));
        let Ok(exif) = Reader::new().read_from_container(&mut reader) else {
            return EmbeddedMeta::default();
        };

        let field = |tag| exif.get_field(tag, In::PRIMARY);
        let mut meta = EmbeddedMeta::default();

        let date = field(Tag::DateTimeOriginal).or_else(|| field(Tag::DateTime));
        meta.date_taken = date.and_then(|f| parse_exif_datetime(&value_text(&f.value)));
        meta.date_offset = field(Tag::OffsetTimeOriginal)
            .or_else(|| field(Tag::OffsetTimeDigitized))
            .or_else(|| field(Tag::OffsetTime))
            .and_then(|f| ascii_text(&f.value));

        meta.title = field(Tag::ImageDescription).and_then(|f| ascii_text(&f.value));
        meta.author = field(Tag::Artist).and_then(|f| ascii_text(&f.value));
        meta.camera = field(Tag::Model)
            .and_then(|f| ascii_text(&f.value))
            .or_else(|| field(Tag::Make).and_then(|f| ascii_text(&f.value)));

        // `kamadak-exif` parses the Exif and GPS sub-IFDs with the parent's
        // `ifd_num`, so their fields are reported under `In::PRIMARY` (not a
        // sub-IFD number). Reading GPS with `In(4)` never matched and silently
        // dropped every coordinate.
        let lat = field(Tag::GPSLatitude).and_then(|f| rational_dms(&f.value));
        let lon = field(Tag::GPSLongitude).and_then(|f| rational_dms(&f.value));
        let lat_ref = field(Tag::GPSLatitudeRef).and_then(|f| ascii_text(&f.value));
        let lon_ref = field(Tag::GPSLongitudeRef).and_then(|f| ascii_text(&f.value));
        meta.latitude = signed_coordinate(lat, lat_ref.as_deref(), 'S', 'N');
        meta.longitude = signed_coordinate(lon, lon_ref.as_deref(), 'W', 'E');
        meta
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Ascii(values) => values
            .first()
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .unwrap_or_default(),
        _ => value.display_as(Tag::DateTimeOriginal).to_string(),
    }
}

fn ascii_text(value: &Value) -> Option<String> {
    let Value::Ascii(values) = value else {
        return None;
    };
    let text = values
        .first()
        .map(|v| String::from_utf8_lossy(v).trim().to_string())?;
    (!text.is_empty()).then_some(text)
}

fn rational_dms(value: &Value) -> Option<f64> {
    let Value::Rational(values) = value else {
        return None;
    };
    if values.len() < 3 {
        return None;
    }
    let n = |i: usize| (values[i].num as f64) / (values[i].denom.max(1) as f64);
    Some(n(0) + n(1) / 60.0 + n(2) / 3600.0)
}

fn signed_coordinate(
    value: Option<f64>,
    reference: Option<&str>,
    negative: char,
    positive: char,
) -> Option<f64> {
    let value = value?;
    let first = reference
        .and_then(|s| s.trim().chars().next())
        .unwrap_or('\0')
        .to_ascii_uppercase();
    let is_negative = if first == negative {
        true
    } else if first == positive {
        false
    } else {
        value < 0.0
    };
    Some(if is_negative {
        -value.abs()
    } else {
        value.abs()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_reference_wins() {
        assert_eq!(
            signed_coordinate(Some(48.0), Some("S"), 'S', 'N'),
            Some(-48.0)
        );
        assert_eq!(
            signed_coordinate(Some(-2.0), Some("E"), 'W', 'E'),
            Some(2.0)
        );
        assert_eq!(signed_coordinate(Some(-2.0), None, 'W', 'E'), Some(-2.0));
    }
}
