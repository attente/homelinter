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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Date taken (photos: `DateTimeOriginal`, fallback `DateTime`). exiftool
    /// additionally falls back to the timezone-aware QuickTime
    /// `CreationDate` when every primary date tag is absent or unparseable.
    pub date_taken: Option<DateParts>,
    /// UTC offset of the capture time when the file records one (EXIF
    /// `OffsetTimeOriginal`, fallback `OffsetTimeDigitized`/`OffsetTime`;
    /// exiftool also takes it from the offset embedded in a `CreationDate`
    /// fallback when that fallback supplies the date), e.g. `+09:00`. The
    /// EXIF date itself is a zone-less wall clock, so an offset is the only
    /// way to recover the absolute instant; it is kept for inspection and
    /// internal ordering and never rewrites the filename.
    pub date_offset: Option<String>,
    /// Title (photos: XPTitle, fallback ImageDescription; exiftool adds XMP
    /// `Description` and the IPTC caption as cataloging fallbacks).
    pub title: Option<String>,
    /// Author / artist (photos: EXIF Artist; exiftool adds XMP `Creator`
    /// and the IPTC by-line as cataloging fallbacks).
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
    /// Fields this extractor examined for the file, whether or not they were
    /// present. An extractor that read a container and reported on it settles
    /// the fields it can see, so a later extractor limited to those fields
    /// cannot add a value the first one already looked for. Left empty by
    /// extractors that only answer for the values they return, and by any file
    /// the extractor could not read (so a fallback still gets a chance at it).
    pub settled: FieldSet,
}

impl EmbeddedMeta {
    /// Everything an extractor has answered for this file: the fields it
    /// settled plus the fields it carries a value for.
    pub fn known(&self) -> FieldSet {
        self.settled.union(self.fields())
    }

    /// The fields this value actually carries.
    pub fn fields(&self) -> FieldSet {
        let mut set = FieldSet::EMPTY;
        if self.date_taken.is_some() {
            set = set.union(FieldSet::DATE);
        }
        if self.date_offset.is_some() {
            set = set.union(FieldSet::OFFSET);
        }
        if self.title.is_some() {
            set = set.union(FieldSet::TITLE);
        }
        if self.author.is_some() {
            set = set.union(FieldSet::AUTHOR);
        }
        if self.camera.is_some() {
            set = set.union(FieldSet::CAMERA);
        }
        if self.latitude.is_some() {
            set = set.union(FieldSet::LATITUDE);
        }
        if self.longitude.is_some() {
            set = set.union(FieldSet::LONGITUDE);
        }
        if self.bitrate_kbps.is_some() {
            set = set.union(FieldSet::BITRATE);
        }
        if self.duration_secs.is_some() {
            set = set.union(FieldSet::DURATION);
        }
        if self.page_count.is_some() {
            set = set.union(FieldSet::PAGE_COUNT);
        }
        set
    }

    /// Whether an extractor limited to `need` could still change this file's
    /// metadata: true when some field in `need` has not been answered for.
    pub fn needs(&self, need: FieldSet) -> bool {
        !need.is_empty() && !self.known().contains(need)
    }
}

/// A set of `EmbeddedMeta` fields. Extractors report the fields they are able
/// to fill (`MetadataExtractor::provides`) and the fields they have already
/// examined for a read file (`EmbeddedMeta::settled`), so the analysis hands
/// each pass only the files that are still unanswered: because the merge keeps
/// the first `Some` per field, running an extractor over a file that already
/// answered every field it can see would change no result, and its read is
/// pure cost (the built-in EXIF reader, for example, re-reads up to 16 MiB of
/// every photo `exiftool` already described).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FieldSet(u16);

impl FieldSet {
    pub const EMPTY: Self = Self(0);
    pub const DATE: Self = Self(1 << 0);
    pub const OFFSET: Self = Self(1 << 1);
    pub const TITLE: Self = Self(1 << 2);
    pub const AUTHOR: Self = Self(1 << 3);
    pub const CAMERA: Self = Self(1 << 4);
    pub const LATITUDE: Self = Self(1 << 5);
    pub const LONGITUDE: Self = Self(1 << 6);
    pub const BITRATE: Self = Self(1 << 7);
    pub const DURATION: Self = Self(1 << 8);
    pub const PAGE_COUNT: Self = Self(1 << 9);
    /// Every field any extractor can report. An extractor that does not
    /// override `MetadataExtractor::provides` is never skipped.
    pub const ALL: Self = Self(0x03ff);
    /// The fields a photo container can carry (everything but the stream and
    /// document fields).
    pub const PHOTO: Self = Self::DATE
        .union(Self::OFFSET)
        .union(Self::TITLE)
        .union(Self::AUTHOR)
        .union(Self::CAMERA)
        .union(Self::LATITUDE)
        .union(Self::LONGITUDE);
    /// The fields an audio/video container can carry.
    pub const MEDIA_STREAM: Self = Self::DATE
        .union(Self::TITLE)
        .union(Self::AUTHOR)
        .union(Self::BITRATE)
        .union(Self::DURATION);
    /// The fields a document container can carry.
    pub const DOCUMENT: Self = Self::DATE.union(Self::TITLE).union(Self::AUTHOR);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every field of `other` is in this set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
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

    /// The fields this extractor can fill for a file of `kind`; empty when it
    /// does not handle that kind. The analysis passes an extractor only the
    /// files that are still missing at least one of these fields, so a file an
    /// earlier extractor already answered is never read again. The default is
    /// `FieldSet::ALL`, which keeps any extractor that does not declare its
    /// fields in every pass.
    fn provides(&self, _kind: AssetKind) -> FieldSet {
        FieldSet::ALL
    }
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
    ///
    /// `progress` is handed the number of files of this batch processed so
    /// far (monotonic, called from worker threads, so calls may arrive out
    /// of order for equal values). It exists so a long extraction pass shows
    /// movement in clients; extractors that cannot count finer than one
    /// invocation report after each invocation.
    fn extract_batch(
        &self,
        files: &[ExtractTarget<'_>],
        ctx: &ExtractionContext,
        progress: &(dyn Fn(u64) + Sync),
    ) -> Vec<EmbeddedMeta> {
        let done = AtomicU64::new(0);
        files
            .par_iter()
            .map(|t| {
                let meta = self.extract(t.path, t.kind, ctx);
                progress(done.fetch_add(1, Ordering::Relaxed) + 1);
                meta
            })
            .collect()
    }
}

/// Merge one extractor's result into an accumulator: the first `Some` wins
/// per field, matching the "consulted in priority order" contract.
pub fn merge_meta(acc: &mut EmbeddedMeta, m: &EmbeddedMeta) {
    if acc.date_taken.is_none() {
        acc.date_taken = m.date_taken;
    }
    if acc.date_offset.is_none() {
        acc.date_offset = m.date_offset.clone();
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
    acc.settled = acc.settled.union(m.settled);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with_all() -> EmbeddedMeta {
        EmbeddedMeta {
            date_taken: Some(DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 10,
                minute: 30,
                second: 45,
            }),
            date_offset: Some("+02:00".to_string()),
            title: Some("T".to_string()),
            author: Some("A".to_string()),
            camera: Some("C".to_string()),
            latitude: Some(48.0),
            longitude: Some(2.0),
            bitrate_kbps: Some(320),
            duration_secs: Some(9),
            page_count: Some(3),
            settled: FieldSet::EMPTY,
        }
    }

    #[test]
    fn fields_lists_every_present_field() {
        assert_eq!(EmbeddedMeta::default().fields(), FieldSet::EMPTY);
        assert_eq!(meta_with_all().fields(), FieldSet::ALL);
    }

    #[test]
    fn needs_stops_once_every_needed_field_is_answered() {
        let meta = EmbeddedMeta {
            date_taken: Some(DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 0,
                minute: 0,
                second: 0,
            }),
            ..Default::default()
        };
        assert!(!meta.needs(FieldSet::DATE));
        assert!(meta.needs(FieldSet::DATE.union(FieldSet::CAMERA)));
        // Settling without a value also answers the field: the extractor looked
        // and the tag was absent, so no later pass can add it.
        let settled = EmbeddedMeta {
            settled: FieldSet::PHOTO,
            ..Default::default()
        };
        assert!(!settled.needs(FieldSet::PHOTO));
        assert!(settled.needs(FieldSet::PHOTO.union(FieldSet::PAGE_COUNT)));
        // An extractor that can fill nothing is skipped by the caller.
        assert!(!EmbeddedMeta::default().needs(FieldSet::EMPTY));
    }

    #[test]
    fn known_merges_settled_fields_with_present_values() {
        let meta = EmbeddedMeta {
            settled: FieldSet::CAMERA,
            title: Some("T".to_string()),
            ..Default::default()
        };
        assert!(meta
            .known()
            .contains(FieldSet::CAMERA.union(FieldSet::TITLE)));
        assert!(!meta.known().contains(FieldSet::DATE));
        assert_eq!(meta.fields(), FieldSet::TITLE);
    }

    #[test]
    fn photo_field_set_excludes_stream_and_document_fields() {
        assert!(FieldSet::PHOTO.contains(FieldSet::CAMERA));
        assert!(!FieldSet::PHOTO.contains(FieldSet::BITRATE));
        assert!(!FieldSet::PHOTO.contains(FieldSet::PAGE_COUNT));
        assert!(FieldSet::ALL.contains(FieldSet::PHOTO));
        assert!(FieldSet::ALL.contains(FieldSet::MEDIA_STREAM));
        assert!(FieldSet::ALL.contains(FieldSet::DOCUMENT));
    }
}
