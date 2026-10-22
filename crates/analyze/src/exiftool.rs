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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use homelinter_model::asset_kind::AssetKind;
use rayon::prelude::*;
use serde_json::Value;

use crate::dateparse::parse_exif_datetime;
use crate::extractor::{
    EmbeddedMeta, ExtractTarget, ExtractionContext, FieldSet, MetadataExtractor,
};
use crate::tool_runner::run_capture_lenient;

/// Number of paths handed to one exiftool invocation. Process startup
/// dominates the cost of reading a single file, so batching many files per
/// invocation is the single largest scan speed-up for media-heavy trees.
/// The chunk stays small enough that its `-json` output is far below the
/// configured stdout cap.
const BATCH_SIZE: usize = 128;

/// Additional wall-clock budget granted per file beyond the first. The
/// configured `tool_timeout` is the budget for a single-file invocation
/// (startup plus one file); a 128-file batch is one invocation doing 128
/// files of work, so it gets a proportional allowance on top. The total
/// stays bounded because `BATCH_SIZE` is fixed.
const PER_FILE_TIMEOUT: Duration = Duration::from_millis(250);

/// Smallest batch a failed chunk is retried at. A chunk that fails is
/// halved and retried down to this size so one unreadable file (or a
/// transient timeout) cannot discard metadata for the whole 128-file chunk.
const MIN_RETRY_BATCH: usize = 8;

/// The exiftool shell-out only runs for containers exiftool understands;
/// text, archives, and other kinds are never handed to it.
fn applies(kind: AssetKind) -> bool {
    matches!(kind, AssetKind::Photo | AssetKind::Video | AssetKind::Audio)
}

/// The fields exiftool has answered for a file once it returns a row for it:
/// for photos that is the whole photo field set plus the stream fields, which
/// makes its coverage a superset of the built-in EXIF reader (`DateTime` is
/// requested for exactly that reason), so the built-in reader only has work
/// left on photos exiftool could not read. Videos and audio leave the date
/// unsettled: their dates live in container tags (`CreationTime`, ID3 `TDRC`,
/// Vorbis `DATE`) that this small tag list does not request, so the built-in
/// reader still gets to look for one.
#[cfg(test)]
fn provides_set(kind: AssetKind) -> FieldSet {
    ExifToolExtractor {}.provides(kind)
}

fn settled(kind: AssetKind) -> FieldSet {
    let fields = FieldSet::TITLE
        .union(FieldSet::AUTHOR)
        .union(FieldSet::BITRATE)
        .union(FieldSet::DURATION)
        .union(FieldSet::OFFSET);
    match kind {
        AssetKind::Photo => fields
            .union(FieldSet::DATE)
            .union(FieldSet::CAMERA)
            .union(FieldSet::LATITUDE)
            .union(FieldSet::LONGITUDE),
        AssetKind::Video => fields
            .union(FieldSet::CAMERA)
            .union(FieldSet::LATITUDE)
            .union(FieldSet::LONGITUDE),
        _ => fields,
    }
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

    /// Photos and videos carry the full photo field set; audio carries the
    /// stream fields only.
    fn provides(&self, kind: AssetKind) -> FieldSet {
        if !applies(kind) {
            return FieldSet::EMPTY;
        }
        let fields = FieldSet::MEDIA_STREAM.union(FieldSet::OFFSET);
        if matches!(kind, AssetKind::Photo | AssetKind::Video) {
            fields
                .union(FieldSet::CAMERA)
                .union(FieldSet::LATITUDE)
                .union(FieldSet::LONGITUDE)
        } else {
            fields
        }
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
            .map(|(mut meta, clean)| {
                if clean {
                    meta.settled = settled(kind);
                }
                meta
            })
            .unwrap_or_default()
    }

    /// Extract a batch of files: applicable files are split into chunks of
    /// `BATCH_SIZE` and each chunk is read with one exiftool invocation, the
    /// chunks themselves running in parallel. Non-applicable kinds and files
    /// exiftool skipped keep the default (empty) metadata, which matches the
    /// per-file contract. Progress is reported per finished chunk, the
    /// finest granularity one exiftool invocation allows.
    fn extract_batch(
        &self,
        files: &[ExtractTarget<'_>],
        ctx: &ExtractionContext,
        progress: &(dyn Fn(u64) + Sync),
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
        let done = AtomicU64::new(0);
        let parsed: Vec<(usize, Option<Row>)> = applicable
            .par_chunks(BATCH_SIZE)
            .flat_map_iter(|chunk| {
                let paths: Vec<&str> = chunk.iter().map(|(_, p)| *p).collect();
                let by_path = extract_chunk(&paths, ctx);
                progress(
                    done.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64,
                );
                chunk
                    .iter()
                    .map(move |(i, p)| ((*i), by_path.get(*p).cloned()))
            })
            .collect();
        for (i, row) in parsed {
            if let Some((mut meta, clean)) = row {
                // A clean row means exiftool opened and read this container, so
                // it has answered for every tag it asks for. A file exiftool
                // failed on (no row, or a row carrying an `Error`) stays
                // unsettled and keeps its turn for the built-in readers.
                if clean {
                    meta.settled = settled(files[i].kind);
                }
                out[i] = meta;
            }
        }
        out
    }
}

/// The fixed, small exiftool tag list. `-n` requests raw numeric values,
/// `-json` requests a structured array keyed by the input path, and `-m`
/// ignores minor per-file errors so a single odd container does not make the
/// whole batch exit non-zero. The catalog tags (XMP `Description`/`Creator`,
/// IPTC `Caption-Abstract`/`By-line`) and the timezone-aware QuickTime
/// `CreationDate` are fallbacks consulted only when the tags they shadow are
/// absent or unparseable, so they widen coverage without displacing any value
/// the primary chain already produced.
fn tag_args() -> Vec<&'static str> {
    vec![
        "-n",
        "-json",
        "-m",
        "-DateTimeOriginal",
        "-ModifyDate",
        "-CreateDate",
        "-TrackCreateDate",
        "-MediaCreateDate",
        "-CreationDate",
        "-OffsetTimeOriginal",
        "-OffsetTimeDigitized",
        "-OffsetTime",
        "-GPSLatitude",
        "-GPSLatitudeRef",
        "-GPSLongitude",
        "-GPSLongitudeRef",
        "-Model",
        "-Make",
        "-Artist",
        "-Creator",
        "-By-line",
        "-Title",
        "-XPTitle",
        "-ImageDescription",
        "-Description",
        "-Caption-Abstract",
        "-Bitrate",
        "-Duration",
    ]
}

/// Wall-clock budget for one batched invocation over `files` paths: the
/// configured `tool_timeout` plus a per-file allowance for every file beyond
/// the first.
fn batch_timeout(ctx: &ExtractionContext, files: usize) -> Duration {
    let extra = PER_FILE_TIMEOUT.saturating_mul(files.saturating_sub(1) as u32);
    ctx.tool_timeout.saturating_add(extra)
}

/// Extract metadata for one chunk, retrying a failed chunk at half size
/// down to `MIN_RETRY_BATCH` so a single unreadable file or a transient
/// timeout cannot discard metadata for every sibling in the chunk.
fn extract_chunk(paths: &[&str], ctx: &ExtractionContext) -> HashMap<String, Row> {
    if let Some(by_path) = run_batch(paths, ctx) {
        return by_path;
    }
    if paths.len() <= MIN_RETRY_BATCH {
        return HashMap::new();
    }
    let mid = paths.len() / 2;
    let mut merged = extract_chunk(&paths[..mid], ctx);
    merged.extend(extract_chunk(&paths[mid..], ctx));
    merged
}

/// One parsed exiftool row: the fields it carries and whether exiftool read
/// the file cleanly. A row that reports an `Error` for its own file ("Unknown
/// file type", a truncated payload) still contributes whatever tags it did
/// yield, but it does not settle the file, so the built-in readers still get a
/// chance at it.
type Row = (EmbeddedMeta, bool);

/// Run exiftool once over `paths` and map each returned row back to the
/// input path string it names (`SourceFile`). A nonzero exit still yields
/// whatever rows exiftool emitted; only a timeout, output overflow, or a
/// truncated/unparseable payload degrades to `None`, which callers treat as
/// "retry smaller".
fn run_batch(paths: &[&str], ctx: &ExtractionContext) -> Option<HashMap<String, Row>> {
    let mut args = tag_args();
    args.extend_from_slice(paths);
    let bytes = run_capture_lenient(
        "exiftool",
        &args,
        batch_timeout(ctx, paths.len()),
        ctx.tool_stdout_max_bytes,
    )?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    let rows = root.as_array()?;
    let mut by_path = HashMap::with_capacity(rows.len());
    for row in rows {
        if let Some(obj) = row.as_object() {
            if let Some(source) = obj.get("SourceFile").and_then(|v| v.as_str()) {
                let clean = row_is_clean(obj);
                by_path.insert(source.to_string(), (parse_row(obj), clean));
            }
        }
    }
    Some(by_path)
}

/// Whether exiftool read this file without reporting a per-file error.
fn row_is_clean(row: &serde_json::Map<String, Value>) -> bool {
    !row.contains_key("Error")
}

/// The trailing UTC offset of a timezone-aware tag value such as the QuickTime
/// `CreationDate` ("2024:03:15 14:30:00+02:00"), returned verbatim as
/// `+02:00` or `-05:00`. A value with no offset, a malformed offset, or one
/// outside the plausible range yields `None`.
fn trailing_utc_offset(value: &str) -> Option<String> {
    // Slicing by byte offset can land inside a multi-byte character when a
    // value is unexpectedly exotic; `get` refuses instead of panicking.
    let start = value.len().checked_sub(6)?;
    let off = value.get(start..)?;
    let b = off.as_bytes();
    if b[0] != b'+' && b[0] != b'-' {
        return None;
    }
    if b[3] != b':' || ![b[1], b[2], b[4], b[5]].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours = (b[1] - b'0') * 10 + (b[2] - b'0');
    let minutes = (b[4] - b'0') * 10 + (b[5] - b'0');
    (hours < 24 && minutes < 60).then(|| off.to_string())
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
    let parseable = |raw: &Option<String>| {
        raw.as_deref()
            .is_some_and(|raw| parse_exif_datetime(raw).is_some())
    };
    let primary = s("DateTimeOriginal")
        .or_else(|| s("CreateDate"))
        .or_else(|| s("TrackCreateDate"))
        .or_else(|| s("MediaCreateDate"))
        // IFD0 `ModifyDate` (EXIF tag `DateTime`), the fallback the built-in
        // reader also uses: asking for it keeps exiftool's photo coverage a
        // superset of that reader, so a settled photo never loses a date that
        // only IFD0 carries. The unqualified `-DateTime` is deliberately not
        // requested: exiftool resolves it to the file's modification time.
        .or_else(|| s("ModifyDate"));
    let creation = s("CreationDate");
    // The timezone-aware QuickTime/`Keys` `CreationDate` is a final fallback
    // consulted only when every primary tag is absent or unparseable (a
    // zeroed container such as a stripped WhatsApp download, for example). It
    // never displaces a date the primary chain already produced, so existing
    // values cannot change; a file can only gain a date it lacked.
    let from_creation = !parseable(&primary) && parseable(&creation);
    let date = if from_creation {
        creation.clone()
    } else {
        primary.clone()
    };
    meta.date_taken = date.as_deref().and_then(parse_exif_datetime);
    meta.date_offset = s("OffsetTimeOriginal")
        .or_else(|| s("OffsetTimeDigitized"))
        .or_else(|| s("OffsetTime"))
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .or_else(|| {
            // A `CreationDate` fallback carries the capture offset inside
            // the value itself ("2024:03:15 14:30:00+02:00"). Keep it for
            // inspection and ordering like the EXIF offset tags; it never
            // rewrites the rendered filename. Taken only when the date
            // actually came from `CreationDate`, so the offset always
            // describes the date it accompanies.
            if from_creation {
                creation.as_deref().and_then(trailing_utc_offset)
            } else {
                None
            }
        });
    meta.title = s("XPTitle")
        .or_else(|| s("Title"))
        .or_else(|| s("ImageDescription"))
        // Cataloging fallbacks: XMP `dc:description` (Lightroom and friends)
        // and the IPTC caption. Consulted only when every photo-EXIF
        // title/description tag is absent or blank, so they widen coverage
        // without displacing an existing title.
        .or_else(|| s("Description"))
        .or_else(|| s("Caption-Abstract"))
        .filter(|t| !t.trim().is_empty());
    meta.author = s("Artist")
        // XMP `dc:creator` and the IPTC by-line: where photo cataloging
        // tools record the author when no EXIF `Artist` tag exists.
        .or_else(|| s("Creator"))
        .or_else(|| s("By-line"));
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
    fn a_photo_row_settles_what_the_builtin_exif_reader_looks_for() {
        // exiftool's photo coverage must be a superset of `kamadak-exif`, or
        // skipping the built-in reader for a settled photo would lose fields.
        let photo = settled(AssetKind::Photo);
        assert!(photo.contains(FieldSet::PHOTO));
        // Only what it claims to fill is ever settled.
        for kind in [AssetKind::Photo, AssetKind::Video, AssetKind::Audio] {
            assert!(provides_set(kind).contains(settled(kind)));
        }
    }

    #[test]
    fn media_dates_stay_open_for_the_builtin_reader() {
        // ID3 `TDRC` and Vorbis `DATE` are not in the tag list, so a returned
        // row does not answer for the date of audio or video containers.
        assert!(!settled(AssetKind::Audio).contains(FieldSet::DATE));
        assert!(!settled(AssetKind::Video).contains(FieldSet::DATE));
        assert!(settled(AssetKind::Audio).contains(FieldSet::TITLE));
    }

    #[test]
    fn the_tag_list_requests_the_ifd0_modify_date_fallback() {
        // The built-in reader falls back to IFD0 `DateTime`; requesting it here
        // keeps the "exiftool is a superset" claim true. `-DateTime` would be
        // the file's modification time, so it must not appear.
        assert!(tag_args().contains(&"-ModifyDate"));
        assert!(!tag_args().contains(&"-DateTime"));
    }

    #[test]
    fn the_tag_list_requests_catalog_and_creation_date_fallbacks() {
        // The additive catalog tags (XMP/IPTC) and the timezone-aware
        // QuickTime `CreationDate`: they widen coverage without displacing
        // values the primary chain already produced.
        for arg in [
            "-Description",
            "-Caption-Abstract",
            "-Creator",
            "-By-line",
            "-CreationDate",
        ] {
            assert!(tag_args().contains(&arg), "missing {arg}");
        }
    }

    #[test]
    fn catalog_tags_only_fill_a_missing_title_or_author() {
        // Every photo-EXIF tag outranks the catalog fallbacks; the XMP and
        // IPTC values are consulted only when those are absent or blank.
        let full: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "ImageDescription": "Exif description",
                "Description": "Xmp description",
                "Caption-Abstract": "Iptc caption",
                "Artist": "Exif Artist",
                "Creator": "Xmp Creator",
                "By-line": "Iptc Byline"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&full);
        assert_eq!(meta.title.as_deref(), Some("Exif description"));
        assert_eq!(meta.author.as_deref(), Some("Exif Artist"));

        // XMP `dc:description` and `dc:creator` outrank their IPTC twins.
        let xmp: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "Description": "Xmp description",
                "Caption-Abstract": "Iptc caption",
                "Creator": "Xmp Creator",
                "By-line": "Iptc Byline"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&xmp);
        assert_eq!(meta.title.as_deref(), Some("Xmp description"));
        assert_eq!(meta.author.as_deref(), Some("Xmp Creator"));

        // IPTC only: the case cataloging software produces for photos
        // stripped of EXIF.
        let iptc: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "Caption-Abstract": "Iptc caption",
                "By-line": "Iptc Byline"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&iptc);
        assert_eq!(meta.title.as_deref(), Some("Iptc caption"));
        assert_eq!(meta.author.as_deref(), Some("Iptc Byline"));
    }

    #[test]
    fn creation_date_is_a_final_date_fallback() {
        // A container whose primary tags are zeroed (a stripped WhatsApp
        // download) gains a date from `CreationDate`.
        let zeroed: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreateDate": "0000:00:00 00:00:00",
                "CreationDate": "2024:03:15 14:30:00+02:00"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&zeroed);
        assert_eq!(
            meta.date_taken.map(|d| (d.year, d.month, d.day, d.hour)),
            Some((2024, 3, 15, 14))
        );

        // A valid primary date is never displaced, even when `CreationDate`
        // carries a timezone.
        let both: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreateDate": "2023:11:05 19:11:38",
                "CreationDate": "2024:03:15 14:30:00+02:00"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&both);
        assert_eq!(
            meta.date_taken.map(|d| d.year),
            Some(2023),
            "CreationDate must not displace a valid primary date"
        );
    }

    #[test]
    fn creation_date_offset_follows_the_date_it_supplies() {
        // When `CreationDate` supplies the date, its embedded offset becomes
        // `date_offset`, verbatim.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreationDate": "2024:03:15 14:30:00+02:00"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&row);
        assert_eq!(meta.date_taken.map(|d| d.year), Some(2024));
        assert_eq!(meta.date_offset.as_deref(), Some("+02:00"));

        // Negative offsets are kept verbatim too.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreateDate": "0000:00:00 00:00:00",
                "CreationDate": "2024:03:15 09:30:00-05:00"
            }"#,
        )
        .unwrap();
        assert_eq!(parse_row(&row).date_offset.as_deref(), Some("-05:00"));

        // An explicit offset tag outranks the embedded one.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreationDate": "2024:03:15 14:30:00+02:00",
                "OffsetTimeOriginal": "+09:00"
            }"#,
        )
        .unwrap();
        assert_eq!(parse_row(&row).date_offset.as_deref(), Some("+09:00"));

        // When a primary tag supplies the date, the `CreationDate` offset is
        // not paired with it: the offset would describe a different value.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreateDate": "2023:11:05 19:11:38",
                "CreationDate": "2024:03:15 14:30:00+02:00"
            }"#,
        )
        .unwrap();
        assert_eq!(parse_row(&row).date_offset, None);

        // A `CreationDate` without an offset leaves `date_offset` empty.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "CreationDate": "2024:03:15 14:30:00"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&row);
        assert_eq!(meta.date_taken.map(|d| d.year), Some(2024));
        assert_eq!(meta.date_offset, None);
    }

    #[test]
    fn trailing_offset_accepts_only_wellformed_values() {
        assert_eq!(
            trailing_utc_offset("2024:03:15 14:30:00+02:00").as_deref(),
            Some("+02:00")
        );
        assert_eq!(
            trailing_utc_offset("14:30:00-05:00").as_deref(),
            Some("-05:00")
        );
        // No offset, a malformed one, an out-of-range one, and a value whose
        // tail is not even ASCII.
        assert_eq!(trailing_utc_offset("2024:03:15 14:30:00"), None);
        assert_eq!(trailing_utc_offset("2024:03:15 14:30:00+2a:00"), None);
        assert_eq!(trailing_utc_offset("2024:03:15 14:30:00+26:00"), None);
        assert_eq!(trailing_utc_offset("2024:03:15 14:30:00+02:99"), None);
        assert_eq!(trailing_utc_offset("14:30:\u{e9}02:00"), None);
        assert_eq!(trailing_utc_offset("short"), None);
    }

    #[test]
    fn a_row_reporting_an_error_does_not_settle_the_file() {
        let ok: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{ "SourceFile": "a.jpg", "Model": "X" }"#).unwrap();
        let bad: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{ "SourceFile": "a.jpg", "Error": "Unknown file type" }"#)
                .unwrap();
        assert!(row_is_clean(&ok));
        assert!(!row_is_clean(&bad));
    }

    #[test]
    fn ifd0_modify_date_is_a_date_fallback() {
        // The tag the built-in reader also falls back to (EXIF `DateTime`), so
        // a photo settled by exiftool cannot lose a date only IFD0 carries.
        let row: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{ "ModifyDate": "2024:03:15 10:30:45" }"#).unwrap();
        let meta = parse_row(&row);
        assert_eq!(meta.date_taken.map(|d| d.year), Some(2024));
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

    #[test]
    fn parse_row_reads_capture_offset() {
        // `OffsetTimeOriginal` is the capture-time offset and wins over the
        // generic `OffsetTime` when both are present.
        let row: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "DateTimeOriginal": "2024:03:15 15:30:45",
                "OffsetTimeOriginal": "+09:00",
                "OffsetTime": "+08:00"
            }"#,
        )
        .unwrap();
        let meta = parse_row(&row);
        assert_eq!(
            meta.date_taken.map(|d| (d.year, d.month, d.day, d.hour)),
            Some((2024, 3, 15, 15))
        );
        assert_eq!(meta.date_offset.as_deref(), Some("+09:00"));

        // A blank offset is no offset.
        let row: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{ "OffsetTime": "  " }"#).unwrap();
        assert_eq!(parse_row(&row).date_offset, None);
    }

    #[test]
    fn batch_timeout_scales_with_file_count() {
        let ctx = ExtractionContext {
            tool_timeout: Duration::from_secs(30),
            ..Default::default()
        };
        assert_eq!(batch_timeout(&ctx, 0), Duration::from_secs(30));
        assert_eq!(batch_timeout(&ctx, 1), Duration::from_secs(30));
        // One extra 250 ms allowance per file beyond the first.
        assert_eq!(batch_timeout(&ctx, 5), Duration::from_millis(31_000));
        // Bounded by BATCH_SIZE, never an unbounded multiplication.
        assert_eq!(
            batch_timeout(&ctx, BATCH_SIZE),
            Duration::from_millis(30_000 + 250 * (BATCH_SIZE as u64 - 1))
        );
    }
}
