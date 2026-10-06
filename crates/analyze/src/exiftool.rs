//! ExifTool-backed metadata extractor (photos and videos).
//!
//! ExifTool is the primary extractor when available: it understands JPEG,
//! HEIC, RAW, TIFF, PNG, and video containers that the built-in reader
//! does not, and it reports coordinates in signed decimal form via `-n`.
//! Only the (tiny) requested tags are emitted; stdout is further bounded
//! by the shared `run_capture` discipline, so a malformed container cannot
//! balloon analysis.

use std::collections::HashMap;
use std::path::Path;

use homelinter_model::asset_kind::AssetKind;
use rayon::prelude::*;
use serde_json::Value;

use crate::dateparse::parse_exif_datetime;
use crate::extractor::{EmbeddedMeta, ExtractTarget, ExtractionContext, MetadataExtractor};
use crate::tool_runner::run_capture;

/// Number of paths handed to one exiftool invocation. Process startup
/// dominates the cost of reading a single file, so batching many files per
/// invocation is the single largest scan speed-up for media-heavy trees.
/// The chunk stays small enough that its `-json` output is far below the
/// configured stdout cap.
const BATCH_SIZE: usize = 128;

/// The exiftool shell-out only runs for containers exiftool understands;
/// text, archives, and other kinds are never handed to it.
fn applies(kind: AssetKind) -> bool {
    matches!(kind, AssetKind::Photo | AssetKind::Video | AssetKind::Audio)
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

/// Run exiftool over batches of files, collecting only the tags needed by
/// the suggestion engine. The JSON output is parsed leniently: every
/// failure degrades to "no metadata".
pub struct ExifToolExtractor {}

impl MetadataExtractor for ExifToolExtractor {
    fn name(&self) -> &'static str {
        "exiftool"
    }

    fn extract(&self, path: &Path, kind: AssetKind, ctx: &ExtractionContext) -> EmbeddedMeta {
        if !applies(kind) {
            return EmbeddedMeta::default();
        }
        let path_str = match path.to_str() {
            Some(s) => s,
            None => return EmbeddedMeta::default(),
        };
        run_batch(&[path_str], ctx)
            .and_then(|mut by_path| by_path.remove(path_str))
            .unwrap_or_default()
    }

    /// Extract a batch of files: applicable files are split into chunks of
    /// `BATCH_SIZE` and each chunk is read with one exiftool invocation, the
    /// chunks themselves running in parallel. Non-applicable kinds and files
    /// exiftool skipped keep the default (empty) metadata, which matches the
    /// per-file contract.
    fn extract_batch(
        &self,
        files: &[ExtractTarget<'_>],
        ctx: &ExtractionContext,
    ) -> Vec<EmbeddedMeta> {
        let applicable: Vec<(usize, &str)> = files
            .iter()
            .enumerate()
            .filter(|(_, t)| applies(t.kind))
            .filter_map(|(i, t)| t.path.to_str().map(|s| (i, s)))
            .collect();
        let mut out = vec![EmbeddedMeta::default(); files.len()];
        if applicable.is_empty() {
            return out;
        }
        let parsed: Vec<(usize, Option<EmbeddedMeta>)> = applicable
            .par_chunks(BATCH_SIZE)
            .flat_map_iter(|chunk| {
                let paths: Vec<&str> = chunk.iter().map(|(_, p)| *p).collect();
                let by_path = run_batch(&paths, ctx);
                chunk.iter().map(move |(i, p)| {
                    let meta = by_path
                        .as_ref()
                        .and_then(|m| m.get(*p))
                        .cloned();
                    (*i, meta)
                })
            })
            .collect();
        for (i, meta) in parsed {
            if let Some(meta) = meta {
                out[i] = meta;
            }
        }
        out
    }
}

/// The fixed, small exiftool tag list. `-n` requests raw numeric values and
/// `-json` requests a structured array keyed by the input path.
fn tag_args() -> Vec<&'static str> {
    vec![
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
    ]
}

/// Run exiftool once over `paths` and map each returned row back to the
/// input path string it names (`SourceFile`). Every failure degrades to
/// `None`, which callers treat as "no metadata".
fn run_batch(paths: &[&str], ctx: &ExtractionContext) -> Option<HashMap<String, EmbeddedMeta>> {
    let mut args = tag_args();
    args.extend_from_slice(paths);
    let bytes = run_capture(
        "exiftool",
        &args,
        ctx.tool_timeout,
        ctx.tool_stdout_max_bytes,
    )?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    let rows = root.as_array()?;
    let mut by_path = HashMap::with_capacity(rows.len());
    for row in rows {
        if let Some(obj) = row.as_object() {
            if let Some(source) = obj.get("SourceFile").and_then(|v| v.as_str()) {
                by_path.insert(source.to_string(), parse_row(obj));
            }
        }
    }
    Some(by_path)
}

/// Parse one exiftool JSON row into the fields homelinter consumes.
fn parse_row(row: &serde_json::Map<String, Value>) -> EmbeddedMeta {
    let mut meta = EmbeddedMeta::default();
    let s = |key: &str| row.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let f = |key: &str| {
        row.get(key).and_then(|v| v.as_f64()).or_else(|| {
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
    meta.title = s("XPTitle")
        .or_else(|| s("Title"))
        .or_else(|| s("ImageDescription"))
        .filter(|t| !t.trim().is_empty());
    meta.author = s("Artist");
    meta.camera = s("Model").or_else(|| s("Make"));
    meta.bitrate_kbps = f("Bitrate").map(|b| (b / 1000.0).round() as u32);
    meta.duration_secs = f("Duration").map(|d| d.max(0.0) as u64);
    meta.latitude = signed_coordinate(f("GPSLatitude"), s("GPSLatitudeRef").as_deref(), 'S', 'N');
    meta.longitude =
        signed_coordinate(f("GPSLongitude"), s("GPSLongitudeRef").as_deref(), 'W', 'E');
    meta
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
        assert!(has_sign(
            signed_coordinate(Some(-lat), none, 'S', 'N'),
            true
        ));
        assert!(has_sign(
            signed_coordinate(Some(lat), none, 'S', 'N'),
            false
        ));
        assert!(has_sign(
            signed_coordinate(Some(lon), Some(""), 'W', 'E'),
            true
        ));
    }
}
