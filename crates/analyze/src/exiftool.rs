//! ExifTool-backed metadata extractor (photos and videos).
//!
//! ExifTool is the primary extractor when available: it understands JPEG,
//! HEIC, RAW, TIFF, PNG, and video containers that the built-in reader
//! does not, and it reports coordinates in signed decimal form via `-n`.
//! Only the (tiny) requested tags are emitted; stdout is further bounded
//! by the shared `run_capture` discipline, so a malformed container cannot
//! balloon analysis.

use std::path::Path;

use homelinter_model::asset_kind::AssetKind;
use serde_json::Value;

use crate::dateparse::parse_exif_datetime;
use crate::extractor::{EmbeddedMeta, ExtractionContext, MetadataExtractor};
use crate::tool_runner::run_capture;

/// The exiftool shell-out only runs for containers exiftool understands;
/// text, archives, and other kinds are never handed to it.
fn applies(kind: AssetKind) -> bool {
    matches!(
        kind,
        AssetKind::Photo | AssetKind::Video | AssetKind::Audio
    )
}

/// Derive a signed coordinate from a numeric value and an optional hemisphere
/// reference letter. `neg_letter`/`pos_letter` name the axis: latitude is
/// `('S', 'N')`, longitude `('W', 'E')`. The reference letter wins when
/// present; a missing/unknown ref falls back to the numeric sign (some
/// writers pre-sign the value).
fn signed_coordinate(
    v: Option<f64>,
    r: Option<&str>,
    neg_letter: char,
    pos_letter: char,
) -> Option<f64> {
    let v = v?;
    let r = r.map(str::trim).unwrap_or("");
    let first = r.chars().next().unwrap_or('\0').to_ascii_uppercase();
    let ref_neg = if first == neg_letter {
        true
    } else if first == pos_letter {
        false
    } else {
        v < 0.0
    };
    Some(if ref_neg { -v.abs() } else { v.abs() })
}

/// Run exiftool once per file, collecting only the tags needed by the
/// suggestion engine. The JSON output is parsed leniently: every failure
/// degrades to "no metadata".
pub struct ExifToolExtractor {}

impl MetadataExtractor for ExifToolExtractor {
    fn name(&self) -> &'static str {
        "exiftool"
    }

    fn extract(
        &self,
        path: &Path,
        kind: AssetKind,
        ctx: &ExtractionContext,
    ) -> EmbeddedMeta {
        let mut meta = EmbeddedMeta::default();
        if !applies(kind) {
            return meta;
        }
        let path_str = match path.to_str() {
            Some(s) => s,
            None => return meta,
        };
        let mut args: Vec<&str> = vec![
            "-n",
            "-json",
            "-DateTimeOriginal",
            "-CreateDate",
            "-TrackCreateDate",
            "-MediaCreateDate",
            "-GPSLatitude",
            "-GPSLatitudeRef",
            "-GPSLongitude",
            "-GPSLongitudeRef",
            "-Model",
            "-Make",
            "-Artist",
            "-Title",
            "-XPTitle",
            "-ImageDescription",
            "-Bitrate",
            "-Duration",
        ];
        args.push(path_str);
        let bytes = match run_capture(
            "exiftool",
            &args,
            ctx.tool_timeout,
            ctx.tool_stdout_max_bytes,
        ) {
            Some(b) => b,
            None => return meta,
        };
        // `-json` emits a JSON array with one object per input file; we
        // pass a single path, so take the first element.
        let Some(root) = serde_json::from_slice::<Value>(&bytes).ok() else {
            return meta;
        };
        let Some(row) = root
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_object()) else {
            return meta;
        };
        let s = |key: &str| row.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        let f = |key: &str| {
            row.get(key)
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    row.get(key)
                        .and_then(|v| v.as_str())
                        .and_then(|raw| raw.trim().parse::<f64>().ok())
                })
        };
        let date = s("DateTimeOriginal")
            .or_else(|| s("CreateDate"))
            .or_else(|| s("TrackCreateDate"))
            .or_else(|| s("MediaCreateDate"));
        meta.date_taken = date.as_deref().and_then(parse_exif_datetime);
        let title = s("XPTitle")
            .or_else(|| s("Title"))
            .or_else(|| s("ImageDescription"))
            .filter(|t| !t.trim().is_empty());
        meta.title = title;
        meta.author = s("Artist");
        meta.camera = s("Model").or_else(|| s("Make"));
        meta.bitrate_kbps = f("Bitrate").map(|b| (b / 1000.0).round() as u32);
        meta.duration_secs = f("Duration").map(|d| d.max(0.0) as u64);
        meta.latitude = signed_coordinate(
            f("GPSLatitude"),
            s("GPSLatitudeRef").as_deref(),
            'S',
            'N',
        );
        meta.longitude = signed_coordinate(
            f("GPSLongitude"),
            s("GPSLongitudeRef").as_deref(),
            'W',
            'E',
        );
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_sign(v: Option<f64>, negative: bool) -> bool {
        v.map(|x| x < 0.0) == Some(negative)
    }

    #[test]
    fn coordinate_refs_win_over_sign() {
        let lat: f64 = 48.85;
        let lon: f64 = -2.35;
        let sf = Some("S");
        let nf = Some("N");
        let ef = Some("E");
        let wf = Some("W");
        let none: Option<&str> = None;
        assert!(has_sign(signed_coordinate(Some(lat), sf, 'S', 'N'), true));
        assert!(has_sign(signed_coordinate(Some(lat), nf, 'S', 'N'), false));
        assert!(has_sign(signed_coordinate(Some(lon), ef, 'W', 'E'), false));
        assert!(has_sign(signed_coordinate(Some(lon), wf, 'W', 'E'), true));
        assert!(has_sign(signed_coordinate(Some(-lat), none, 'S', 'N'), true));
        assert!(has_sign(signed_coordinate(Some(lat), none, 'S', 'N'), false));
        assert!(has_sign(signed_coordinate(Some(lon), Some(""), 'W', 'E'), true));
    }
}
