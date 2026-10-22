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

mod builtin_audio;
mod builtin_exif;
mod builtin_text;
mod dateparse;
mod doc_container;
mod doc_pdfinfo;
mod exiftool;
mod extractor;
mod repo;
mod tool_runner;

pub use builtin_audio::BuiltinAudioExtractor;
pub use builtin_exif::BuiltinExifExtractor;
pub use builtin_text::extract_builtin_text;
pub use dateparse::{parse_exif_datetime, parse_filename_date, DateParts};
pub use doc_container::DocumentContainerExtractor;
pub use doc_pdfinfo::PdfInfoExtractor;
pub use exiftool::ExifToolExtractor;
pub use extractor::{
    merge_meta, EmbeddedMeta, ExtractTarget, ExtractionContext, FieldSet, MetadataExtractor,
};
pub use repo::{RepoAnalyzer, RepoMeta};
pub use tool_runner::run_capture;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use homelinter_model::asset_kind::{classify_path, AssetKind};
use homelinter_model::hash;
use homelinter_model::suggestion::Provenance;
use homelinter_scan::{FileEntry, RepoEntry};
use rayon::prelude::*;

/// Persisted manual metadata edits for a file, keyed by its content
/// identity in the cache (see "Cache Implementation" in DESIGN.md). When a
/// field is set it wins over any embedded or provider-derived value for the
/// matching placeholder, so a user edit is stable across scans and re-requests.
#[derive(Debug, Clone, Default)]
pub struct MetadataOverrides {
    /// Backing `%l`, replacing the reverse-geocoded locality.
    pub location: Option<String>,
    /// Backing `%s`/`%t`, replacing a provider description and any embedded
    /// title.
    pub subject: Option<String>,
    /// Backing `%a`, replacing an embedded or inferred author.
    pub author: Option<String>,
}

impl MetadataOverrides {
    /// Whether no field is overridden. Equivalent to the default value.
    pub fn is_empty(&self) -> bool {
        self.location.is_none() && self.subject.is_none() && self.author.is_none()
    }
}

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
    /// Large-file fast-path SHA-256 (first 1 MiB + last 1 MiB + size) when
    /// the file was at or above the large-file threshold. A content
    /// fingerprint used as the provider cache identity when no full hash
    /// was needed for deduplication; it never establishes a duplicate group.
    pub fast_hash: Option<[u8; 32]>,
    pub provenance: Provenance,
    /// Date extracted from the filename (or embedded metadata), before the
    /// modification-time fallback.
    pub extracted_date: Option<DateParts>,
    /// Where the extracted date came from: "embedded" (photo/video
    /// metadata), "filename", or empty when neither exists and the
    /// suggestion falls back to the filesystem modification time.
    pub date_source: &'static str,
    /// UTC offset of the embedded date when the file records one (EXIF
    /// `OffsetTimeOriginal` and friends, e.g. `+09:00`). Kept for
    /// inspection and internal ordering; the filename keeps the zone-less
    /// wall clock exactly as the camera wrote it.
    pub date_offset: Option<String>,
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
    /// Persisted manual metadata edits, keyed by content identity; each set
    /// field wins over the embedded or provider-derived value.
    pub overrides: MetadataOverrides,
    /// Informational embedded metadata surfaced in suggestion `metadata`.
    pub bitrate_kbps: Option<u32>,
    pub duration_secs: Option<u64>,
    pub page_count: Option<u64>,
}

impl AnalyzedFile {
    /// The authoritative title for this file when one already exists in
    /// embedded metadata: a document/container title, an ID3 title, or an
    /// EXIF title. Occupies the `%s` subject in preference to a generated
    /// description, so a titled file never needs a description provider
    /// call. `None` when no extractor produced a non-empty title.
    pub fn authoritative_title(&self) -> Option<&str> {
        self.embedded_title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
    }

    /// Content identity for provider cache keys: the full-content hash when
    /// deduplication computed one, otherwise the large-file fast-path
    /// fingerprint. `None` only when the file could not be read at all.
    pub fn content_hash(&self) -> Option<[u8; 32]> {
        self.full_hash.or(self.fast_hash)
    }
}

/// One confirmed duplicate group: files with identical full-content
/// hashes. The keeper is the preferred file; the rest are deletion
/// candidates. Groups suppressed for manual review never reach here; their
/// reasons are reported in `Analysis.warnings`.
#[derive(Debug, Clone)]
pub struct DupGroup {
    /// Index into `Analysis.files`.
    pub keeper: usize,
    /// Indexes of the redundant copies.
    pub redundant: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Analysis {
    /// Deterministically ordered by normalized absolute path.
    pub files: Vec<AnalyzedFile>,
    pub repos: Vec<(RepoEntry, RepoMeta)>,
    pub groups: Vec<DupGroup>,
    /// Human-readable reasons for findings analysis left untouched, such as
    /// a duplicate group suppressed because the newest copy carries embedded
    /// metadata the keeper lacks. Surfaced to clients as the scan's
    /// warnings.
    pub warnings: Vec<String>,
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

/// One `scan.progress` report from an analysis pass: the phase label, the
/// completed work items in that phase, the phase's total when it is known up
/// front, and an optional detail naming the worker currently running (for
/// example the extractor behind a `metadata` pass). Phase-boundary reports
/// carry the phase's own count for `scanned` and `total` so clients can show
/// a finished phase as `n/n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseEvent {
    pub phase: &'static str,
    pub scanned: u64,
    pub total: Option<u64>,
    pub detail: Option<&'static str>,
}

/// Analyze every discovered unit. `cancel` is a cooperative-cancellation
/// probe polled between files (and between hashes and repositories): when
/// it returns true, analysis stops early and returns the partial `Analysis`
/// computed so far, which the caller discards once it observes the probe.
/// `progress` reports completed work items so callers can publish live
/// `scan.progress` events during a long analysis phase: metadata extraction
/// reports each extractor's own pass (named in `detail`, counting managed
/// files), hashing reports hashed files, and repositories report analyzed
/// repositories.
pub fn analyze(
    files: &[FileEntry],
    repos: &[RepoEntry],
    opts: &AnalyzeOptions,
    cancel: &(dyn Fn() -> bool + Sync),
    progress: &(dyn Fn(PhaseEvent) + Sync),
) -> Analysis {
    // Classify once, then hand every managed file to each extractor as one
    // batch. Built-in extractors parallelize internally and `exiftool`
    // overrides `extract_batch` to amortize process startup across many
    // files, so the per-extractor pass is far cheaper than the old per-file
    // loop while producing the identical first-value-wins merge.
    //
    // Files that classify to no kind (unmanaged) are filtered out here: they
    // are never extracted, hashed, or suggested, so homelinter cannot move
    // or deduplicate data, source, or unknown files that live where their
    // software expects them.
    let classified: Vec<(usize, AssetKind)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| classify_path(&f.path).map(|k| (i, k)))
        .collect();
    let mut merged = vec![EmbeddedMeta::default(); classified.len()];
    for ex in &opts.extractors {
        if cancel() {
            break;
        }
        // Only the files this extractor still has something to say about are
        // handed to it: an earlier extractor that read the file answered for
        // every field it can see (`settled`), and a returned value answers for
        // its field too, so re-reading such a file (up to a bounded prefix of
        // every photo, for the built-in EXIF reader) is pure cost. Extractors
        // that do not declare `provides` keep the full set.
        let pass: Vec<usize> = classified
            .iter()
            .enumerate()
            .filter(|(ci, &(_, kind))| merged[*ci].needs(ex.provides(kind)))
            .map(|(ci, _)| ci)
            .collect();
        if pass.is_empty() {
            continue;
        }
        let batch_targets: Vec<ExtractTarget<'_>> = pass
            .iter()
            .map(|&ci| {
                let (fi, kind) = classified[ci];
                ExtractTarget {
                    path: &files[fi].path,
                    kind,
                }
            })
            .collect();
        // Each extractor re-reads a subset of the managed files, so the count
        // is this pass's own work set (named in `detail`), not a running
        // total across passes: the numbers always read as "files of this pass
        // out of the files it has to read".
        let total = batch_targets.len() as u64;
        let batch = ex.extract_batch(&batch_targets, &opts.ctx, &|done| {
            progress(PhaseEvent {
                phase: "metadata",
                scanned: done.min(total),
                total: Some(total),
                detail: Some(ex.name()),
            });
        });
        for (&ci, m) in pass.iter().zip(batch.iter()) {
            merge_meta(&mut merged[ci], m);
        }
    }

    let mut degraded = false;
    let analyzed: Vec<AnalyzedFile> = classified
        .iter()
        .enumerate()
        .map(|(ci, &(fi, kind))| {
            let f = &files[fi];
            let meta = &merged[ci];
            let (date, source) = match meta.date_taken {
                Some(d) => (Some(d), "embedded"),
                None => filename_date(f),
            };
            let mut prov = Provenance {
                size: f.size,
                ..Default::default()
            };
            if let Ok(m) = f.modified.duration_since(std::time::UNIX_EPOCH) {
                prov.mtime_ns = m.as_nanos() as i128;
            }
            if let Ok(id) = homelinter_model::identity::file_identity(&f.path) {
                prov.dev = id.dev;
                prov.ino = id.ino;
                prov.identity_available = id.available;
            }
            if !prov.identity_available {
                degraded = true;
            }
            AnalyzedFile {
                path: f.path.clone(),
                kind,
                size: f.size,
                modified: f.modified,
                created: f.created,
                full_hash: None,
                fast_hash: None,
                provenance: prov,
                extracted_date: date,
                date_source: source,
                date_offset: meta.date_offset.clone(),
                embedded_title: meta.title.clone(),
                embedded_author: meta.author.clone(),
                camera_model: meta.camera.clone(),
                latitude: meta.latitude,
                longitude: meta.longitude,
                location: None,
                subject: None,
                overrides: MetadataOverrides::default(),
                bitrate_kbps: meta.bitrate_kbps,
                duration_secs: meta.duration_secs,
                page_count: meta.page_count,
            }
        })
        .collect();
    let mut analyzed = analyzed;
    // Boundary: extraction is complete for every managed file, so the phase
    // line closes at `n/n` with no extractor name attached.
    progress(PhaseEvent {
        phase: "metadata",
        scanned: analyzed.len() as u64,
        total: Some(analyzed.len() as u64),
        detail: None,
    });

    let (groups, warnings) =
        dedup_groups(&mut analyzed, opts.large_file_threshold, cancel, progress);

    // Repositories are independent; the per-VCS adapters shell out to the
    // VCS CLI, so running them in parallel hides their process latency.
    let repo_analyzer = RepoAnalyzer::new(&opts.toolset);
    let repos_progress = AtomicU64::new(0);
    let repo_meta: Vec<(RepoEntry, RepoMeta)> = repos
        .par_iter()
        .filter_map(|r| {
            if cancel() {
                return None;
            }
            let meta = repo_analyzer.analyze(r);
            progress(PhaseEvent {
                phase: "repos",
                scanned: repos_progress.fetch_add(1, Ordering::Relaxed) + 1,
                total: Some(repos.len() as u64),
                detail: None,
            });
            Some((r.clone(), meta))
        })
        .collect();

    Analysis {
        files: analyzed,
        repos: repo_meta,
        groups,
        warnings,
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
fn dedup_groups(
    files: &mut [AnalyzedFile],
    threshold: u64,
    cancel: &(dyn Fn() -> bool + Sync),
    progress: &(dyn Fn(PhaseEvent) + Sync),
) -> (Vec<DupGroup>, Vec<String>) {
    let hashed = AtomicU64::new(0);
    let files_total = files.len() as u64;
    // Pass 1: full-hash every file below the threshold and fast-hash the
    // rest. Files are independent, so the pass runs in parallel; hashing
    // large media trees in full is the second-largest scan cost after
    // metadata extraction.
    let pre: Vec<Option<([u8; 32], bool)>> = files
        .par_iter_mut()
        .map(|f| {
            if cancel() {
                return None;
            }
            let result = if f.size >= threshold && f.size > 0 {
                match hash::hash_file_fast(&f.path, f.size) {
                    Ok(h) => {
                        f.fast_hash = Some(h);
                        Some((h, true))
                    }
                    Err(_) => None,
                }
            } else {
                match hash::hash_file_full(&f.path) {
                    Ok(h) => {
                        f.full_hash = Some(h);
                        Some((h, false))
                    }
                    Err(_) => None,
                }
            };
            progress(PhaseEvent {
                phase: "hashing",
                scanned: hashed.fetch_add(1, Ordering::Relaxed) + 1,
                total: Some(files_total),
                detail: None,
            });
            result
        })
        .collect();

    // Known full hashes, keyed to member indexes.
    let mut full: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    // Large files awaiting confirmation, keyed to member indexes.
    let mut fast_pending: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    for (i, item) in pre.into_iter().enumerate() {
        match item {
            Some((h, true)) => fast_pending.entry(h).or_default().push(i),
            Some((h, false)) => full.entry(h).or_default().push(i),
            None => {}
        }
    }

    // Confirm fast-path collisions with full-content hashes, in parallel.
    let pending: Vec<([u8; 32], Vec<usize>)> = fast_pending.into_iter().collect();
    let files_ref: &[AnalyzedFile] = files;
    let confirmed: Vec<(usize, [u8; 32])> = pending
        .par_iter()
        .flat_map_iter(|(_, members)| {
            members.iter().filter_map(|&i| {
                if cancel() {
                    return None;
                }
                let h = hash::hash_file_full(&files_ref[i].path).ok();
                // The confirmation pass re-hashes candidates already counted
                // in the first pass, so the running count can exceed the file
                // total; it therefore carries no total and stays monotonic.
                progress(PhaseEvent {
                    phase: "hashing",
                    scanned: hashed.fetch_add(1, Ordering::Relaxed) + 1,
                    total: None,
                    detail: None,
                });
                h.map(|h| (i, h))
            })
        })
        .collect();
    for (i, h) in confirmed {
        files[i].full_hash = Some(h);
        full.entry(h).or_default().push(i);
    }

    let mut groups = Vec::new();
    let mut warnings = Vec::new();
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
        // and leave the files for manual review, reporting why.
        if files[newest].embedded_title.is_some() && files[keeper].embedded_title.is_none() {
            warnings.push(format!(
                "newer duplicate {} carries embedded metadata the keeper lacks; leaving untouched for manual review",
                files[newest].path.display()
            ));
            continue;
        }
        groups.push(DupGroup {
            keeper,
            redundant: members[1..].to_vec(),
        });
    }
    groups.sort_by(|a, b| {
        files[a.keeper]
            .path
            .to_string_lossy()
            .cmp(&files[b.keeper].path.to_string_lossy())
    });
    (groups, warnings)
}

/// Oldest first: creation time when available, else modification time.
fn sort_oldest_first(a: &AnalyzedFile, b: &AnalyzedFile) -> std::cmp::Ordering {
    let a_time = a.created.unwrap_or(a.modified);
    let b_time = b.created.unwrap_or(b.modified);
    a_time
        .cmp(&b_time)
        .then_with(|| a.path.to_string_lossy().cmp(&b.path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// An extractor that declares the fields it can fill and records every
    /// file it is actually asked to read.
    struct Recording {
        label: &'static str,
        fields: FieldSet,
        /// Fields this extractor settles for every file it reads, present or
        /// not (the way a real reader answers for the tags it looks at).
        settle: FieldSet,
        /// Fields this extractor fills, keyed by file name.
        values: fn(&std::path::Path) -> EmbeddedMeta,
        seen: Mutex<Vec<PathBuf>>,
    }

    impl Recording {
        fn new(
            label: &'static str,
            fields: FieldSet,
            settle: FieldSet,
            values: fn(&std::path::Path) -> EmbeddedMeta,
        ) -> Arc<Self> {
            Arc::new(Recording {
                label,
                fields,
                settle,
                values,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<String> {
            let mut names: Vec<String> = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl MetadataExtractor for Arc<Recording> {
        fn name(&self) -> &'static str {
            self.label
        }

        fn provides(&self, _kind: AssetKind) -> FieldSet {
            self.fields
        }

        fn extract(
            &self,
            path: &std::path::Path,
            _kind: AssetKind,
            _ctx: &ExtractionContext,
        ) -> EmbeddedMeta {
            self.seen.lock().unwrap().push(path.to_path_buf());
            let mut meta = (self.values)(path);
            meta.settled = meta.settled.union(self.settle);
            meta
        }
    }

    fn date_parts() -> DateParts {
        DateParts {
            year: 2024,
            month: 3,
            day: 15,
            hour: 10,
            minute: 30,
            second: 45,
        }
    }

    /// Every field a photo container can carry.
    fn full_photo_meta() -> EmbeddedMeta {
        EmbeddedMeta {
            date_taken: Some(date_parts()),
            date_offset: Some("+02:00".to_string()),
            title: Some("Title".to_string()),
            author: Some("Author".to_string()),
            camera: Some("Camera".to_string()),
            latitude: Some(48.0),
            longitude: Some(2.0),
            ..Default::default()
        }
    }

    /// A photo-named file whose bytes are irrelevant to these tests.
    fn write_photo(dir: &std::path::Path, name: &str) -> FileEntry {
        let path = dir.join(name);
        std::fs::write(&path, b"\xff\xd8\xff\xe0not-a-real-jpeg").unwrap();
        FileEntry {
            path,
            size: 4,
            modified: std::time::SystemTime::UNIX_EPOCH,
            created: None,
        }
    }

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hl-analyze-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run `analyze` over `files` with `extractors`, returning the analysis
    /// and the progress events it published.
    fn run(
        files: &[FileEntry],
        extractors: Vec<Box<dyn MetadataExtractor + Send + Sync>>,
    ) -> (Analysis, Vec<PhaseEvent>) {
        let opts = AnalyzeOptions {
            extractors,
            ..Default::default()
        };
        let events: Mutex<Vec<PhaseEvent>> = Mutex::new(Vec::new());
        let analysis = analyze(files, &[], &opts, &|| false, &|e| {
            events.lock().unwrap().push(e);
        });
        (analysis, events.into_inner().unwrap())
    }

    #[test]
    fn an_extractor_is_skipped_for_files_earlier_extractors_completed() {
        let dir = test_dir("skip");
        let files = vec![write_photo(&dir, "a.jpg"), write_photo(&dir, "b.jpg")];

        // Answers for every photo field, but only for `a.jpg` (it declines the
        // file it could not read, the way a reader does on a parse failure).
        let first = Recording::new("first", FieldSet::PHOTO, FieldSet::EMPTY, |path| {
            if path.file_name().unwrap() == "a.jpg" {
                EmbeddedMeta {
                    settled: FieldSet::PHOTO,
                    ..full_photo_meta()
                }
            } else {
                EmbeddedMeta::default()
            }
        });
        // Same fields, so it only has work left for `b.jpg`.
        let second = Recording::new("second", FieldSet::PHOTO, FieldSet::PHOTO, |_| {
            EmbeddedMeta {
                camera: Some("SecondCamera".to_string()),
                ..Default::default()
            }
        });
        // Only the camera, which the second pass already answered for, so this
        // whole pass is skipped.
        let third = Recording::new("third", FieldSet::CAMERA, FieldSet::CAMERA, |_| {
            EmbeddedMeta {
                camera: Some("ThirdCamera".to_string()),
                ..Default::default()
            }
        });

        let (analysis, events) = run(
            &files,
            vec![
                Box::new(first.clone()),
                Box::new(second.clone()),
                Box::new(third.clone()),
            ],
        );

        let by_name = |name: &str| {
            analysis
                .files
                .iter()
                .find(|f| f.path.file_name().unwrap() == name)
                .unwrap()
                .clone()
        };
        // `a.jpg` was answered by the first extractor, so the second pass
        // never read it; `b.jpg` was open and got the second's camera.
        assert_eq!(first.seen(), vec!["a.jpg", "b.jpg"]);
        assert_eq!(second.seen(), vec!["b.jpg"]);
        assert_eq!(by_name("a.jpg").camera_model.as_deref(), Some("Camera"));
        assert_eq!(
            by_name("b.jpg").camera_model.as_deref(),
            Some("SecondCamera")
        );
        // Nothing was left for the third extractor, so it read no file and
        // published no report.
        assert!(third.seen().is_empty());
        assert!(!events.iter().any(|e| e.detail == Some("third")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_settled_field_is_not_hunted_for_by_later_extractors() {
        // The distinction the fallback rests on: a reader that opened the
        // container has answered for the tags it looks at, so a missing title
        // means the file has none, not that the next reader should look too.
        let dir = test_dir("settled");
        let files = vec![write_photo(&dir, "a.jpg")];

        let first = Recording::new("first", FieldSet::PHOTO, FieldSet::PHOTO, |_| {
            EmbeddedMeta {
                date_taken: Some(date_parts()),
                ..Default::default()
            }
        });
        let second = Recording::new("second", FieldSet::PHOTO, FieldSet::PHOTO, |_| {
            EmbeddedMeta {
                title: Some("Late title".to_string()),
                ..Default::default()
            }
        });

        let (analysis, events) = run(
            &files,
            vec![Box::new(first.clone()), Box::new(second.clone())],
        );

        assert_eq!(first.seen(), vec!["a.jpg"]);
        assert!(second.seen().is_empty());
        assert!(!events.iter().any(|e| e.detail == Some("second")));
        assert_eq!(analysis.files[0].embedded_title, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn passes_report_their_own_work_set() {
        let dir = test_dir("progress");
        let files = vec![write_photo(&dir, "a.jpg"), write_photo(&dir, "b.jpg")];

        let first = Recording::new("first", FieldSet::DATE, FieldSet::EMPTY, |path| {
            if path.file_name().unwrap() == "a.jpg" {
                EmbeddedMeta {
                    date_taken: Some(date_parts()),
                    ..Default::default()
                }
            } else {
                EmbeddedMeta::default()
            }
        });
        let second = Recording::new("second", FieldSet::DATE, FieldSet::DATE, |_| EmbeddedMeta {
            date_taken: Some(date_parts()),
            ..Default::default()
        });

        let (_analysis, events) = run(&files, vec![Box::new(first), Box::new(second)]);

        // The second pass only had one file left to read, so its counts are
        // `1/1` rather than `1/2` or `2/2`.
        let second_pass: Vec<&PhaseEvent> = events
            .iter()
            .filter(|e| e.detail == Some("second"))
            .collect();
        assert!(!second_pass.is_empty());
        assert!(second_pass.iter().all(|e| e.total == Some(1)));
        assert_eq!(second_pass.last().unwrap().scanned, 1);
        // The phase boundary closes at the full managed count with no detail.
        let boundary = events
            .iter()
            .filter(|e| e.phase == "metadata")
            .next_back()
            .unwrap();
        assert_eq!(boundary.detail, None);
        assert_eq!(boundary.scanned, 2);
        assert_eq!(boundary.total, Some(2));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
