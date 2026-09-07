//! Analysis: classification, metadata collection, hashing, and duplicate
//! grouping for every unit discovered by the primary scan.
//!
//! Embedded-media extraction (EXIF, ID3, document titles) and provider
//! analysis (LLM descriptions, reverse geocoding) plug in behind
//! `MetadataExtractor`. The scaffold ships a `kamadak-exif` fallback;
//! extractors are consulted in priority order per file and the first value
//! wins per field.
//! Filename-derived dates are parsed here, conservatively, and never
//! override an embedded date-taken.

mod builtin_exif;
mod dateparse;
mod exiftool;
mod extractor;
mod repo;
mod tool_runner;

pub use builtin_exif::BuiltinExifExtractor;
pub use dateparse::{parse_exif_datetime, parse_filename_date, DateParts};
pub use exiftool::ExifToolExtractor;
pub use extractor::{merge_meta, EmbeddedMeta, ExtractionContext, MetadataExtractor};
pub use repo::{RepoAnalyzer, RepoMeta};

use std::collections::BTreeMap;
use std::path::PathBuf;

use homelinter_model::asset_kind::{classify_path, AssetKind};
use homelinter_model::hash;
use homelinter_model::suggestion::Provenance;
use homelinter_scan::{FileEntry, RepoEntry};

/// A file with its analysis results.
#[derive(Debug, Clone)]
pub struct AnalyzedFile {
    pub path: PathBuf,
    pub kind: AssetKind,
    pub size: u64,
    pub modified: std::time::SystemTime,
    pub created: Option<std::time::SystemTime>,
    /// Full-content SHA-256 when computed. Computed for every file below the
    /// large-file threshold and for every large-file fast-path collision
    /// that needed confirmation. `None` for large files that never collided.
    pub full_hash: Option<[u8; 32]>,
    pub provenance: Provenance,
    /// Date extracted from the filename (or embedded metadata), before the
    /// modification-time fallback.
    pub extracted_date: Option<DateParts>,
    /// Where the extracted date came from ("filename", or absent).
    pub date_source: &'static str,
    /// Embedded title metadata when an extractor provided one.
    pub embedded_title: Option<String>,
    /// Embedded author/artist when an extractor provided one (EXIF Artist),
    /// backing the `%a` placeholder.
    pub embedded_author: Option<String>,
    /// Camera model/make when an extractor provided one (EXIF Model or
    /// Make), backing the `%c` placeholder.
    pub camera_model: Option<String>,
    /// Raw GPS coordinates when an extractor found them (EXIF for photos
    /// and videos). Backing input for the `%l` reverse-geocoder.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Reverse-geocoded place name backing the `%l` placeholder. Resolved
    /// asynchronously by the provider worker after the scan; `None` until
    /// a provider (or the cache) supplies a value.
    pub location: Option<String>,
    /// Description/subject from a content provider, backing `%s`.
    /// Resolved asynchronously by the provider worker.
    pub subject: Option<String>,
}

/// One confirmed duplicate group: files with identical full-content
/// hashes. The keeper is the preferred file; the rest are deletion
/// candidates.
#[derive(Debug, Clone)]
pub struct DupGroup {
    /// Index into `Analysis.files`.
    pub keeper: usize,
    /// Indexes of the redundant copies.
    pub redundant: Vec<usize>,
    /// Emitted warning when a newer copy has better metadata.
    pub metadata_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Analysis {
    /// Deterministically ordered by normalized absolute path.
    pub files: Vec<AnalyzedFile>,
    pub repos: Vec<(RepoEntry, RepoMeta)>,
    pub groups: Vec<DupGroup>,
    /// True when any file used the degraded identity (no dev/ino).
    pub degraded_identity: bool,
}

pub struct AnalyzeOptions {
    pub large_file_threshold: u64,
    pub toolset: homelinter_scan::ToolSet,
    /// Plug-in extractors consulted for embedded metadata, in order.
    pub extractors: Vec<Box<dyn MetadataExtractor + Send + Sync>>,
    pub ctx: ExtractionContext,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            large_file_threshold: homelinter_model::config::DEFAULT_LARGE_FILE_THRESHOLD,
            toolset: homelinter_scan::ToolSet::detect(),
            extractors: Vec::new(),
            ctx: ExtractionContext {
                tool_timeout: std::time::Duration::from_secs(
                    homelinter_model::config::DEFAULT_TOOL_TIMEOUT_SECONDS,
                ),
                tool_stdout_max_bytes: homelinter_model::config::DEFAULT_TOOL_STDOUT_MAX_BYTES,
                config: None,
            },
        }
    }
}

pub fn analyze(files: &[FileEntry], repos: &[RepoEntry], opts: &AnalyzeOptions) -> Analysis {
    let mut analyzed: Vec<AnalyzedFile> = Vec::with_capacity(files.len());
    let mut degraded = false;
    for f in files {
        let kind = classify_path(&f.path);
        // Merge all configured extractors, first value wins per field.
        let mut merged = EmbeddedMeta::default();
        for ex in &opts.extractors {
            let m = ex.extract(&f.path, kind, &opts.ctx);
            merge_meta(&mut merged, &m);
        }
        let (date, source) = match merged.date_taken.clone() {
            Some(d) => (Some(d), "embedded"),
            None => filename_date(f),
        };
        let mut prov = Provenance::default();
        prov.size = f.size;
        if let Ok(m) = f.modified.duration_since(std::time::UNIX_EPOCH) {
            prov.mtime_ns = m.as_nanos() as i128;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(&f.path) {
                prov.dev = meta.dev();
                prov.ino = meta.ino();
                prov.identity_available = meta.dev() != 0 && meta.ino() != 0;
            }
            if !prov.identity_available {
                degraded = true;
            }
        }
        #[cfg(not(unix))]
        {
            prov.identity_available = false;
            degraded = true;
        }
        analyzed.push(AnalyzedFile {
            path: f.path.clone(),
            kind,
            size: f.size,
            modified: f.modified,
            created: f.created,
            full_hash: None,
            provenance: prov,
            extracted_date: date,
            date_source: source,
            embedded_title: merged.title,
            embedded_author: merged.author,
            camera_model: merged.camera,
            latitude: merged.latitude,
            longitude: merged.longitude,
            location: None,
            subject: None,
        });
    }

    let groups = dedup_groups(&mut analyzed, opts.large_file_threshold);

    let repo_analyzer = RepoAnalyzer::new(&opts.toolset);
    let repo_meta: Vec<(RepoEntry, RepoMeta)> = repos
        .iter()
        .map(|r| (r.clone(), repo_analyzer.analyze(r)))
        .collect();

    Analysis {
        files: analyzed,
        repos: repo_meta,
        groups,
        degraded_identity: degraded,
    }
}

/// Date parsed conservatively from the filename, used only when no embedded
/// or content-provider date is available (the modification-time fallback
/// lives downstream).
fn filename_date(f: &FileEntry) -> (Option<DateParts>, &'static str) {
    let name = f.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match parse_filename_date(name) {
        Some(d) => (Some(d), "filename"),
        None => (None, ""),
    }
}

/// Group files by verified full-content hash.
///
/// Files below the large-file threshold are hashed in full immediately.
/// Files at or above the threshold get the fast-path hash first; only files
/// whose fast-path hash collides are promoted to a full-content hash, and
/// only verified full-content equality forms a group. The fast path never
/// establishes a duplicate relationship by itself.
fn dedup_groups(files: &mut [AnalyzedFile], threshold: u64) -> Vec<DupGroup> {
    // Known full hashes, keyed to member indexes.
    let mut full: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    // Large files awaiting confirmation, keyed to member indexes.
    let mut fast_pending: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();

    for (i, f) in files.iter_mut().enumerate() {
        if f.size >= threshold && f.size > 0 {
            let size = f.size;
            if let Ok(h) = hash::hash_file_fast(&f.path, size) {
                fast_pending.entry(h).or_default().push(i);
            }
        } else {
            if let Ok(h) = hash::hash_file_full(&f.path) {
                f.full_hash = Some(h);
                full.entry(h).or_default().push(i);
            }
        }
    }

    // Confirm fast-path collisions with full-content hashes.
    for (_, members) in fast_pending {
        if members.len() < 2 {
            continue;
        }
        for i in members {
            if let Ok(h) = hash::hash_file_full(&files[i].path) {
                files[i].full_hash = Some(h);
                full.entry(h).or_default().push(i);
            }
        }
    }

    let mut groups = Vec::new();
    for (_, members) in full {
        if members.len() < 2 {
            continue;
        }
        let mut members = members;
        members.sort_by(|a, b| sort_oldest_first(&files[*a], &files[*b]));
        let keeper = members[0];
        let newest = *members.last().unwrap();
        // Metadata preservation: when the newest duplicate carries embedded
        // title metadata the keeper lacks, suppress the group's suggestion
        // and leave the files for manual review.
        let metadata_warning = if files[newest].embedded_title.is_some()
            && files[keeper].embedded_title.is_none()
        {
            Some(format!(
                "newer duplicate {} carries embedded metadata the keeper lacks; leaving untouched for manual review",
                files[newest].path.display()
            ))
        } else {
            None
        };
        if metadata_warning.is_some() {
            continue;
        }
        groups.push(DupGroup {
            keeper,
            redundant: members[1..].to_vec(),
            metadata_warning,
        });
    }
    groups.sort_by(|a, b| {
        files[a.keeper]
            .path
            .to_string_lossy()
            .cmp(&files[b.keeper].path.to_string_lossy())
    });
    groups
}

/// Oldest first: creation time when available, else modification time.
fn sort_oldest_first(a: &AnalyzedFile, b: &AnalyzedFile) -> std::cmp::Ordering {
    let a_time = a.created.unwrap_or(a.modified);
    let b_time = b.created.unwrap_or(b.modified);
    a_time
        .cmp(&b_time)
        .then_with(|| a.path.to_string_lossy().cmp(&b.path.to_string_lossy()))
}
