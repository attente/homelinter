//! Plug-in seam for embedded metadata and content providers.
//!
//! Extractors are consulted in configured priority order per analyzed file;
//! the first extractor that supplies a field wins that field. Every extractor
//! failure degrades to missing metadata; analysis never fails because of one.
//!
//! The chain ships `exiftool` plus built-in EXIF, audio (`lofty`), and
//! document (`pdfinfo` plus zip/XML container) readers; LLM analysis and
//! reverse geocoding implement the same shape in later phases.

use std::path::Path;
use std::time::Duration;

use homelinter_model::asset_kind::AssetKind;
use homelinter_model::config::RawConfig;
use rayon::prelude::*;

use crate::dateparse::DateParts;

/// Limits handed to extractors that shell out to external tools, from the
/// `[limits]` configuration section. Tool execution shares the document
/// extraction discipline: a bounded timeout, a stdout cap, and a minimal
/// sanitized environment.
#[derive(Debug, Clone, Default)]
pub struct ExtractionContext {
    /// Enabled content providers configured for this scan.
    pub config: Option<RawConfig>,
    /// Maximum wall time for one external tool invocation.
    pub tool_timeout: Duration,
    /// Maximum captured stdout bytes for one invocation.
    pub tool_stdout_max_bytes: u64,
}

/// Embedded metadata one extractor can produce for one file. `None` means
/// "this extractor has no value"; the merge in `analyze` takes the first
/// `Some` across extractors per field.
#[derive(Debug, Clone, Default)]
pub struct EmbeddedMeta {
    /// Date taken (photos: `DateTimeOriginal`, fallback `DateTime`).
    pub date_taken: Option<DateParts>,
    /// Title (photos: XPTitle, fallback ImageDescription).
    pub title: Option<String>,
    /// Author / artist (photos: EXIF Artist).
    pub author: Option<String>,
    /// Camera model (photos: EXIF Model).
    pub camera: Option<String>,
    /// Raw GPS coordinates; south/west latitudes and longitudes are
    /// negative. Backing input for the `%l` reverse-geocoder.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Audio/video bitrate in kilobits per second when an extractor
    /// supplies one (informational; no placeholder consumes it).
    pub bitrate_kbps: Option<u32>,
    /// Audio/video duration in whole seconds when an extractor supplies
    /// one (informational; no placeholder consumes it).
    pub duration_secs: Option<u64>,
    /// Page count for printed documents when an extractor supplies one
    /// (informational; no placeholder consumes it).
    pub page_count: Option<u64>,
}

/// One file handed to a batch extraction: its path and classified kind.
#[derive(Debug, Clone, Copy)]
pub struct ExtractTarget<'a> {
    pub path: &'a Path,
    pub kind: AssetKind,
}

/// A metadata extractor for embedded content, consulted in priority order
/// before filename- and provider-derived metadata.
pub trait MetadataExtractor: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract(&self, _path: &Path, _kind: AssetKind, _ctx: &ExtractionContext) -> EmbeddedMeta {
        EmbeddedMeta::default()
    }

    /// Extract metadata for a batch of files, positionally aligned with
    /// `files` (the result at index `i` belongs to `files[i]`). The default
    /// implementation runs `extract` for every file in parallel, which is
    /// safe because extractors are `Sync` and each file is independent.
    /// Extractors that can amortize per-process startup (for example
    /// `exiftool`, whose process launch dominates its work on a single
    /// file) override this to process the batch in a few invocations.
    fn extract_batch(
        &self,
        files: &[ExtractTarget<'_>],
        ctx: &ExtractionContext,
    ) -> Vec<EmbeddedMeta> {
        files
            .par_iter()
            .map(|t| self.extract(t.path, t.kind, ctx))
            .collect()
    }
}

/// Merge one extractor's result into an accumulator: the first `Some` wins
/// per field, matching the "consulted in priority order" contract.
pub fn merge_meta(acc: &mut EmbeddedMeta, m: &EmbeddedMeta) {
    if acc.date_taken.is_none() {
        acc.date_taken = m.date_taken;
    }
    if acc.title.is_none() {
        acc.title = m.title.clone();
    }
    if acc.author.is_none() {
        acc.author = m.author.clone();
    }
    if acc.camera.is_none() {
        acc.camera = m.camera.clone();
    }
    if acc.latitude.is_none() {
        acc.latitude = m.latitude;
    }
    if acc.longitude.is_none() {
        acc.longitude = m.longitude;
    }
    if acc.bitrate_kbps.is_none() {
        acc.bitrate_kbps = m.bitrate_kbps;
    }
    if acc.duration_secs.is_none() {
        acc.duration_secs = m.duration_secs;
    }
    if acc.page_count.is_none() {
        acc.page_count = m.page_count;
    }
}
