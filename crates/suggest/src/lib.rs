//! Suggestion generation and batch validation.
//!
//! One top-level suggestion per discovered unit: moves for files and
//! repository roots whose resolved destination differs from the current
//! path, deletes for confirmed redundant duplicates, and no suggestions for
//! ordinary directories. Dependent symlink repairs are attached to their
//! parent suggestion and are never surfaced independently.

mod validate;

pub use validate::validate_batch;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use homelinter_analyze::{Analysis, AnalyzedFile};
use homelinter_model::asset_kind::{strip_trailing_document_extension, AssetKind};
use homelinter_model::format::{verbatim_segment, FormatString, PlaceholderValues};
use homelinter_model::path;
use homelinter_model::suggestion::{
    DependentOpType, DependentOperation, MetadataField, Provenance, Suggestion,
};
use homelinter_scan::SymlinkEntry;
use homelinter_scan::SymlinkIndex;

#[derive(Debug, Clone)]
pub struct SuggestError(pub String);

impl std::fmt::Display for SuggestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SuggestError {}

pub struct GenerateInput<'a> {
    pub analysis: &'a Analysis,
    pub symlinks: &'a SymlinkIndex,
    /// Effective per-kind destination format strings (overrides, config,
    /// defaults already resolved).
    pub formats: &'a BTreeMap<AssetKind, FormatString>,
    pub home: &'a Path,
}

pub struct GenerateOutput {
    pub suggestions: Vec<Suggestion>,
    pub warnings: Vec<String>,
}

pub fn generate_suggestions(input: &GenerateInput) -> Result<GenerateOutput, SuggestError> {
    let analysis = input.analysis;
    let mut out = GenerateOutput {
        suggestions: Vec::new(),
        warnings: analysis.warnings.clone(),
    };

    // Index symlinks by their canonical resolved target.
    let mut by_canon: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, link) in input.symlinks.entries.iter().enumerate() {
        if let Some(t) = &link.resolved_target {
            by_canon.entry(t.clone()).or_default().push(i);
        }
    }

    // Files blocked from their own move because they are redundant copies.
    let mut redundant: BTreeSet<usize> = BTreeSet::new();
    for g in &analysis.groups {
        redundant.extend(g.redundant.iter().copied());
    }

    // Move candidates: every file index (except redundant copies) plus the
    // duplicate keepers (a keeper is a move candidate like any file).
    let mut move_candidates: Vec<usize> = (0..analysis.files.len())
        .filter(|i| !redundant.contains(i))
        .collect();

    // Repositories are units and get their own kind-based destination.
    for (repo, meta) in &analysis.repos {
        let canonical = match std::fs::canonicalize(&repo.root) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let values = repo_values(
            &repo.root,
            meta.remote_host.clone(),
            meta.remote_owner.clone(),
            meta.remote_name.clone(),
        );
        let fmt = input
            .formats
            .get(&AssetKind::Repository)
            .ok_or_else(|| SuggestError("missing repository destination format".into()))?;
        if let Some(new_path) = fmt
            .resolve(&values, input.home)
            .map_err(|e| SuggestError(format!("cannot resolve repository format: {e}")))?
        {
            if new_path != repo.root {
                let dependents = collect_dependents(
                    &repo.root,
                    &canonical,
                    &new_path,
                    &input.symlinks.entries,
                    &by_canon,
                );
                let meta_json = repo_meta_json(meta);
                out.suggestions.push(Suggestion::new_move(
                    AssetKind::Repository,
                    &repo.root,
                    &new_path,
                    // Move suggestions carry no human-readable reason: the
                    // from/to paths already explain the operation, so the
                    // generic "moved to the <kind> destination" text was
                    // redundant.
                    String::new(),
                    meta_json,
                    dependents,
                    repo_provenance(&repo.root),
                ));
            }
        }
    }

    // Redundant copies: exactly one delete suggestion each, with dependent
    // repairs when deleting would leave a confirmed symlink broken.
    for g in &analysis.groups {
        let keeper_path = analysis.files[g.keeper].path.clone();
        for &ridx in &g.redundant {
            let f = &analysis.files[ridx];
            let canonical = match std::fs::canonicalize(&f.path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // Only links confirmed at scan time are repaired on delete: the
            // link must resolve to the deleted file's identity.
            let dependents =
                delete_dependents(&canonical, &keeper_path, &input.symlinks.entries, &by_canon);
            let size = f.size;
            // The delete requires the recorded full-content hash at
            // execution time; carry it from analysis into the provenance.
            let mut prov = f.provenance.clone();
            prov.full_hash = f.full_hash;
            out.suggestions.push(Suggestion::new_delete(
                f.kind,
                &f.path,
                format!(
                    "Duplicate of {}; retaining the oldest copy and freeing {} bytes.",
                    keeper_path.display(),
                    size
                ),
                serde_json::Map::from_iter([
                    (
                        "duplicate_of".to_string(),
                        serde_json::Value::String(keeper_path.to_string_lossy().into_owned()),
                    ),
                    ("size".to_string(), serde_json::Value::from(size)),
                ])
                .into_iter()
                .collect(),
                dependents,
                prov,
            ));
        }
    }

    // Move candidates, in deterministic order.
    move_candidates.sort_by_key(|i| path::sort_key(&analysis.files[*i].path));
    for i in move_candidates {
        let f = &analysis.files[i];
        if let Some(s) = build_move_suggestion(
            f,
            input.formats.get(&f.kind),
            input.home,
            &input.symlinks.entries,
            &by_canon,
        )? {
            out.suggestions.push(s);
        }
    }

    out.suggestions.sort_by_key(|a| a.sort_key());
    Ok(out)
}

/// Rebuild a single move suggestion after one file's provider metadata
/// changed (location or subject arrived asynchronously). The moved file
/// stays the same unit, so the suggestion id is stable; only the resolved
/// `new_path` (and dependents derived from it) may change.
pub fn rebuild_move_suggestion(
    f: &AnalyzedFile,
    fmt: Option<&FormatString>,
    home: &Path,
    symlinks: &SymlinkIndex,
) -> Result<Option<Suggestion>, SuggestError> {
    let mut by_canon: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, link) in symlinks.entries.iter().enumerate() {
        if let Some(t) = &link.resolved_target {
            by_canon.entry(t.clone()).or_default().push(i);
        }
    }
    build_move_suggestion(f, fmt, home, &symlinks.entries, &by_canon)
}

fn build_move_suggestion(
    f: &AnalyzedFile,
    fmt: Option<&FormatString>,
    home: &Path,
    links: &[SymlinkEntry],
    by_canon: &HashMap<PathBuf, Vec<usize>>,
) -> Result<Option<Suggestion>, SuggestError> {
    let Some(fmt) = fmt else {
        return Err(SuggestError(format!(
            "missing destination format for {}",
            f.kind.name()
        )));
    };
    let values = file_values(f);
    let new_path = match fmt
        .resolve(&values, home)
        .map_err(|e| SuggestError(format!("cannot resolve destination: {e}")))?
    {
        Some(p) => p,
        None => return Ok(None),
    };
    if new_path == f.path {
        return Ok(None);
    }
    let canonical = match std::fs::canonicalize(&f.path) {
        Ok(c) => c,
        Err(_) => PathBuf::new(),
    };
    let dependents = if canonical.as_os_str().is_empty() {
        Vec::new()
    } else {
        collect_dependents(&f.path, &canonical, &new_path, links, by_canon)
    };
    let mut metadata = BTreeMap::new();
    // Always report where the rendered date comes from, including the
    // filesystem modification-time fallback, so the metadata tab can answer
    // "what does this time mean?" for every managed file. EXIF dates are a
    // zone-less wall clock; `date_offset` records the zone when the file
    // carries one, but the filename always keeps the wall clock verbatim.
    let date_source = if f.date_source.is_empty() {
        "filesystem"
    } else {
        f.date_source
    };
    metadata.insert(
        "date_source".into(),
        serde_json::Value::String(date_source.into()),
    );
    if let Some(dp) = &f.extracted_date {
        metadata.insert(
            "extracted_date".into(),
            serde_json::Value::String(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dp.year, dp.month, dp.day, dp.hour, dp.minute, dp.second
            )),
        );
    }
    if let Some(offset) = &f.date_offset {
        metadata.insert(
            "date_offset".into(),
            serde_json::Value::String(offset.clone()),
        );
    }
    if f.size > 0 {
        metadata.insert("size".into(), serde_json::Value::from(f.size));
    }
    if let Some(b) = f.bitrate_kbps {
        metadata.insert("bitrate_kbps".into(), serde_json::Value::from(b));
    }
    if let Some(d) = f.duration_secs {
        metadata.insert("duration_secs".into(), serde_json::Value::from(d));
    }
    if let Some(p) = f.page_count {
        metadata.insert("page_count".into(), serde_json::Value::from(p));
    }
    // Location / EXIF facts surfaced for the SPA detail pane (preview,
    // location-with-map, and metadata tabs). Only inserted when present. A
    // manual override replaces the provider or embedded value, so the pane
    // shows the value that actually shaped the destination.
    let effective_location = f.overrides.location.as_ref().or(f.location.as_ref());
    if let Some(loc) = effective_location {
        metadata.insert("location".into(), serde_json::Value::String(loc.clone()));
    }
    if let Some(lat) = f.latitude {
        metadata.insert("latitude".into(), serde_json::Value::from(lat));
    }
    if let Some(lon) = f.longitude {
        metadata.insert("longitude".into(), serde_json::Value::from(lon));
    }
    if let Some(cam) = &f.camera_model {
        metadata.insert("camera".into(), serde_json::Value::String(cam.clone()));
    }
    // The effective title/subject (override, else embedded title, else
    // provider subject) is shown as `title`, so the edit form can prefill
    // it and a provider description is visible even without embedded
    // metadata.
    let effective_title = f
        .overrides
        .subject
        .as_ref()
        .or(f.embedded_title.as_ref())
        .or(f.subject.as_ref());
    if let Some(t) = effective_title {
        metadata.insert("title".into(), serde_json::Value::String(t.clone()));
    }
    let effective_author = f.overrides.author.as_ref().or(f.embedded_author.as_ref());
    if let Some(a) = effective_author {
        metadata.insert("author".into(), serde_json::Value::String(a.clone()));
    }
    // A manual override replaces the value the destination would otherwise
    // use. Report that replaced value under `original_*` so the metadata tab
    // can show what the edit hid. The location and author originals come from
    // a cleared-override view, so their precedence (provider or GPS fallback;
    // embedded author) cannot drift from what clearing the override would
    // render. The subject is different: it is the content description (`%s`),
    // so its original is the provider description the edit replaced, with an
    // embedded title as the fallback when the provider returned nothing. A
    // filename stem is never reported - it names the file rather than
    // describing it. Only an overridden field gets a row, and only when there
    // is a value to report.
    if !f.overrides.is_empty() {
        let mut cleared = f.clone();
        cleared.overrides = homelinter_analyze::MetadataOverrides::default();
        let originals = file_values(&cleared);
        let mut insert = |key: &str, value: Option<&String>| {
            if let Some(value) = value {
                metadata.insert(key.to_string(), serde_json::Value::String(value.clone()));
            }
        };
        if f.overrides.location.is_some() {
            insert("original_location", originals.location.as_ref());
        }
        if f.overrides.subject.is_some() {
            let description = f
                .subject
                .as_deref()
                .and_then(meaningful)
                .map(strip_trailing_document_extension)
                .map(str::to_owned)
                .or_else(|| f.authoritative_title().map(str::to_owned));
            insert("original_description", description.as_ref());
        }
        if f.overrides.author.is_some() {
            insert("original_author", originals.author.as_ref());
        }
    }
    // A move re-verifies content when analysis recorded a full-content hash:
    // every file below the fast-path threshold and every confirmed group
    // member has one, so carry it instead of dropping it. The sampling hash
    // rides along for the files that only have that one: it never verifies
    // content, it only tells a touched mtime from an edited file.
    let mut provenance = f.provenance.clone();
    provenance.full_hash = f.full_hash;
    provenance.fast_hash = f.fast_hash;
    // Move suggestions carry no human-readable reason: the from/to paths
    // already explain the operation, so the generic "moved to the <kind>
    // destination" text was redundant (see `Suggestion.reason`).
    let mut suggestion = Suggestion::new_move(
        f.kind,
        &f.path,
        &new_path,
        String::new(),
        metadata,
        dependents,
        provenance,
    );
    let mut manual = Vec::new();
    if f.overrides.location.is_some() {
        manual.push(MetadataField::Location);
    }
    if f.overrides.subject.is_some() {
        manual.push(MetadataField::Subject);
    }
    if f.overrides.author.is_some() {
        manual.push(MetadataField::Author);
    }
    suggestion.manual_overrides = manual;
    Ok(Some(suggestion))
}

/// Format embedded GPS coordinates as a stable, filesystem-safe decimal
/// degree fallback when no reverse-geocoded location is available.
/// Six fractional digits gives roughly meter-scale precision while avoiding
/// noisy path names from provider-specific coordinate formatting.
fn coordinate_fallback(latitude: Option<f64>, longitude: Option<f64>) -> Option<String> {
    let (Some(latitude), Some(longitude)) = (latitude, longitude) else {
        return None;
    };
    if !latitude.is_finite() || !longitude.is_finite() {
        return None;
    }
    Some(format!("{latitude:.6},{longitude:.6}"))
}

/// Treat a provider answer of `unknown` (or blank) as no answer, so the
/// placeholder's normal fallback applies instead of a literal `unknown` in a
/// filename.
fn meaningful(value: &str) -> Option<&str> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(v)
    }
}

fn file_values(f: &AnalyzedFile) -> PlaceholderValues {
    let (stem, ext) = split_name(&f.path);
    let (year, year2, month, day, hour, minute, second) = match f.extracted_date {
        Some(d) => (
            Some(format!("{:04}", d.year)),
            Some(format!("{:02}", d.year % 100)),
            Some(format!("{:02}", d.month)),
            Some(format!("{:02}", d.day)),
            Some(format!("{:02}", d.hour)),
            Some(format!("{:02}", d.minute)),
            Some(format!("{:02}", d.second)),
        ),
        None => date_from_time(f.modified),
    };
    // `%t` precedence: embedded title metadata, then a content-derived
    // title or description (the `%s` provider value, which for documents is
    // the recovered original title), then the filename stem. `%s` itself
    // prefers the authoritative title first (see below). A recovered title
    // or stem can carry a duplicated trailing document extension (the stem
    // of `Name.docx.pdf` is `Name.docx`), which is stripped before the real
    // extension is re-appended by `%e`; an authoritative embedded title is
    // used verbatim.
    let clean = |s: &str| strip_trailing_document_extension(s).to_owned();
    // A manual override wins over both the provider value and any embedded
    // title (it is the user's explicit choice), so it backs `%s` and `%t`
    // alike. Absent an override, an authoritative embedded title still wins
    // over a provider description.
    let manual_subject = f
        .overrides
        .subject
        .as_deref()
        .and_then(meaningful)
        .map(clean);
    let provider_subject = f.subject.as_deref().and_then(meaningful).map(clean);
    let subject = manual_subject.clone().or(provider_subject);
    let authoritative_subject =
        manual_subject.or_else(|| f.authoritative_title().map(str::to_owned));
    let title = authoritative_subject
        .clone()
        .or_else(|| subject.clone())
        .unwrap_or_else(|| clean(&stem));
    PlaceholderValues {
        year,
        year2,
        month,
        day,
        hour,
        minute,
        second,
        title: Some(title),
        author: f
            .overrides
            .author
            .clone()
            .or_else(|| f.embedded_author.clone()),
        location: f
            .overrides
            .location
            .clone()
            .or_else(|| f.location.clone())
            .or_else(|| coordinate_fallback(f.latitude, f.longitude)),
        subject: authoritative_subject
            .or(subject)
            .or_else(|| Some(clean(&stem))),
        camera: f.camera_model.clone(),
        remote_host: None,
        remote_owner: None,
        file_name: Some(
            f.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        stem: Some(stem),
        extension: if ext.is_empty() { None } else { Some(ext) },
        asset_kind: f.kind,
    }
}

fn repo_values(
    root: &Path,
    remote_host: Option<String>,
    remote_owner: Option<String>,
    remote_name: Option<String>,
) -> PlaceholderValues {
    let local_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `%n` names a repository from the final segment of its remote URL when
    // one is available, preserving the remote's spelling; the local
    // directory name is the fallback (and stays the value `%f`/`%t` report).
    // A remote name is not a host filename, so it is used verbatim only when
    // it is a safe single path segment; otherwise the local name keeps the
    // destination well formed.
    let stem = remote_name
        .as_deref()
        .and_then(verbatim_segment)
        .map(str::to_owned)
        .unwrap_or_else(|| local_name.clone());
    PlaceholderValues {
        asset_kind: AssetKind::Repository,
        title: Some(local_name.clone()),
        file_name: Some(local_name),
        stem: Some(stem),
        remote_host,
        remote_owner,
        ..Default::default()
    }
}

fn repo_meta_json(meta: &homelinter_analyze::RepoMeta) -> BTreeMap<String, serde_json::Value> {
    let mut m = BTreeMap::new();
    if let Some(url) = &meta.remote_url {
        m.insert("remote_url".into(), serde_json::Value::String(url.clone()));
    }
    if let Some(b) = &meta.default_branch {
        m.insert(
            "default_branch".into(),
            serde_json::Value::String(b.clone()),
        );
    }
    if let Some(n) = meta.commit_count {
        m.insert("commit_count".into(), serde_json::Value::from(n));
    }
    m
}

fn repo_provenance(root: &Path) -> Provenance {
    let mut p = Provenance::default();
    if let Ok(id) = homelinter_model::identity::file_identity(root) {
        p.dev = id.dev;
        p.ino = id.ino;
        p.identity_available = id.available;
    }
    if let Ok(mt) = std::fs::metadata(root).and_then(|m| m.modified()) {
        if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
            p.mtime_ns = d.as_nanos() as i128;
        }
    }
    p
}

fn split_name(p: &Path) -> (String, String) {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            (name[..i].to_string(), name[i + 1..].to_string())
        }
        _ => (name, String::new()),
    }
}

/// `(year, year2, month, day, hour, minute, second)` placeholders derived
/// from an instant, in the order `file_values` unpacks them.
type DateFields = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn date_from_time(t: std::time::SystemTime) -> DateFields {
    use chrono::{DateTime, Datelike, Local, Timelike};
    let dt: DateTime<Local> = DateTime::from(t);
    (
        Some(format!("{:04}", dt.year())),
        Some(format!("{:02}", dt.year() % 100)),
        Some(format!("{:02}", dt.month())),
        Some(format!("{:02}", dt.day())),
        Some(format!("{:02}", dt.hour())),
        Some(format!("{:02}", dt.minute())),
        Some(format!("{:02}", dt.second())),
    )
}

/// Links whose canonical resolved target is the moved object or a
/// descendant of a moved directory are repaired to point at the new
/// location, preserving the relative/absolute form of the original target.
fn collect_dependents(
    target: &Path,
    canonical_target: &Path,
    new_abs: &Path,
    links: &[SymlinkEntry],
    by_canon: &HashMap<PathBuf, Vec<usize>>,
) -> Vec<DependentOperation> {
    let mut out = Vec::new();
    let target_is_dir = std::fs::metadata(target)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if target_is_dir {
        for link in links.iter() {
            let Some(rt) = &link.resolved_target else {
                continue;
            };
            let Ok(suffix) = rt.strip_prefix(canonical_target) else {
                continue;
            };
            let replacement_abs = if suffix.as_os_str().is_empty() {
                new_abs.to_path_buf()
            } else {
                new_abs.join(suffix)
            };
            out.push(make_repair(link, &replacement_abs));
        }
        return out;
    }
    // Exact file match via canonical identity.
    if let Some(idxs) = by_canon.get(canonical_target) {
        for &i in idxs {
            out.push(make_repair(&links[i], new_abs));
        }
    }
    out
}

fn delete_dependents(
    canonical_deleted: &Path,
    keeper: &Path,
    links: &[SymlinkEntry],
    by_canon: &HashMap<PathBuf, Vec<usize>>,
) -> Vec<DependentOperation> {
    let mut out = Vec::new();
    if let Some(idxs) = by_canon.get(canonical_deleted) {
        for &i in idxs {
            out.push(make_repair(&links[i], keeper));
        }
    }
    out
}

fn make_repair(link: &SymlinkEntry, replacement_abs: &Path) -> DependentOperation {
    let replacement = if link.target_relative {
        // Recalculate relative from the symlink's parent directory.
        relative_to(
            link.link_path.parent().unwrap_or(Path::new("/")),
            replacement_abs,
        )
    } else {
        replacement_abs.to_string_lossy().into_owned()
    };
    let prov = Provenance {
        link_resolved_target: link
            .resolved_target
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        link_raw_relative: Some(link.target_relative),
        ..Default::default()
    };
    DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.link_path.to_string_lossy().into_owned(),
        recorded_target: link.raw_target.clone(),
        replacement_target: replacement,
        recorded_target_relative: link.target_relative,
        provenance: prov,
    }
}

pub use homelinter_model::path::relative_to;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_fallback_uses_decimal_degrees() {
        assert_eq!(
            coordinate_fallback(Some(48.8566), Some(2.3522)),
            Some("48.856600,2.352200".into())
        );
        assert_eq!(
            coordinate_fallback(Some(-0.5), Some(-73.25)),
            Some("-0.500000,-73.250000".into())
        );
        assert_eq!(coordinate_fallback(Some(f64::NAN), Some(2.0)), None);
        assert_eq!(coordinate_fallback(Some(1.0), None), None);
    }

    #[test]
    fn repo_n_uses_remote_name_verbatim() {
        let v = repo_values(
            Path::new("/home/alice/checkout"),
            Some("github.com".into()),
            Some("William".into()),
            Some("HomeLinter".into()),
        );
        assert_eq!(v.stem.as_deref(), Some("HomeLinter"));
        assert_eq!(v.remote_owner.as_deref(), Some("William"));
        // The local directory name still backs `%f`/`%t`.
        assert_eq!(v.file_name.as_deref(), Some("checkout"));
        assert_eq!(v.title.as_deref(), Some("checkout"));

        // No remote name: fall back to the local directory name.
        let local = repo_values(Path::new("/home/alice/checkout"), None, None, None);
        assert_eq!(local.stem.as_deref(), Some("checkout"));

        // An unsafe remote name is not emitted as a path segment.
        let unsafe_name = repo_values(
            Path::new("/home/alice/checkout"),
            None,
            None,
            Some("../escape".into()),
        );
        assert_eq!(unsafe_name.stem.as_deref(), Some("checkout"));
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b")), ".");
    }

    fn analyzed(path: &str) -> AnalyzedFile {
        AnalyzedFile {
            path: PathBuf::from(path),
            kind: AssetKind::Photo,
            size: 1,
            modified: std::time::SystemTime::now(),
            created: None,
            full_hash: None,
            fast_hash: None,
            provenance: Provenance::default(),
            extracted_date: None,
            date_source: "",
            date_offset: None,
            embedded_title: None,
            embedded_author: None,
            camera_model: None,
            latitude: None,
            longitude: None,
            location: None,
            subject: None,
            overrides: Default::default(),
            bitrate_kbps: None,
            duration_secs: None,
            page_count: None,
        }
    }

    #[test]
    fn subject_prefers_authoritative_title_over_provider_and_stem() {
        let mut f = analyzed("/tmp/IMG_0001.jpg");
        f.subject = Some("provider guess".into());
        // No title: the provider description wins over the stem fallback.
        assert_eq!(file_values(&f).subject.as_deref(), Some("provider guess"));
        // An embedded title is authoritative and wins over both.
        f.embedded_title = Some("  Sunset over the River \n".into());
        assert_eq!(
            file_values(&f).subject.as_deref(),
            Some("Sunset over the River")
        );
    }

    #[test]
    fn manual_override_beats_embedded_and_provider_values() {
        let mut f = analyzed("/tmp/scan0001.jpg");
        f.subject = Some("provider subject".into());
        f.embedded_title = Some("Embedded Title".into());
        f.location = Some("embedded place".into());
        f.embedded_author = Some("Embedded Author".into());
        f.overrides = homelinter_analyze::MetadataOverrides {
            location: Some("Manual Place".into()),
            subject: Some("Manual Subject".into()),
            author: Some("Manual Author".into()),
        };
        let v = file_values(&f);
        assert_eq!(v.location.as_deref(), Some("Manual Place"));
        assert_eq!(v.subject.as_deref(), Some("Manual Subject"));
        assert_eq!(v.title.as_deref(), Some("Manual Subject"));
        assert_eq!(v.author.as_deref(), Some("Manual Author"));
        // Clearing the subject override falls back to the authoritative
        // embedded title, not the provider value.
        f.overrides.subject = None;
        assert_eq!(file_values(&f).subject.as_deref(), Some("Embedded Title"));
        // Clearing location falls back to embedded GPS/location.
        f.overrides.location = None;
        assert_eq!(file_values(&f).location.as_deref(), Some("embedded place"));
    }

    #[test]
    fn suggestions_report_the_fields_backed_by_manual_overrides() {
        let fmt = FormatString::parse("Pictures/%l/%s/%f").unwrap();
        let home = Path::new("/home/user");
        let symlinks = SymlinkIndex::default();
        let mut f = analyzed("/tmp/IMG_0001.jpg");
        f.location = Some("Provider Place".into());
        f.subject = Some("Provider Subject".into());
        f.embedded_author = Some("Embedded Author".into());

        let plain = rebuild_move_suggestion(&f, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert!(plain.manual_overrides.is_empty());

        f.overrides = homelinter_analyze::MetadataOverrides {
            location: None,
            subject: Some("Manual Subject".into()),
            author: None,
        };
        let edited = rebuild_move_suggestion(&f, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(edited.manual_overrides, vec![MetadataField::Subject]);
    }

    #[test]
    fn move_suggestions_carry_the_analyzed_full_content_hash() {
        // Analysis computes a full-content hash for every file it can, and a
        // move must carry it so the executor can re-verify content when the
        // modification time drifted.
        let fmt = FormatString::parse("Pictures/%f").unwrap();
        let home = Path::new("/home/user");
        let symlinks = SymlinkIndex::default();
        let mut f = analyzed("/tmp/IMG_0001.jpg");
        f.full_hash = Some([0x5a; 32]);
        let s = rebuild_move_suggestion(&f, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(s.provenance.full_hash, Some([0x5a; 32]));

        // A large file records only the sampling hash, and the move carries
        // it so the executor can tell a touched mtime from an edit.
        f.full_hash = None;
        f.fast_hash = Some([0x11; 32]);
        let s = rebuild_move_suggestion(&f, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(s.provenance.full_hash, None);
        assert_eq!(s.provenance.fast_hash, Some([0x11; 32]));
    }

    #[test]
    fn title_falls_back_to_the_provider_value_then_stem() {
        let mut f = analyzed("/tmp/Untitled.pdf");
        // No provider value and no embedded title: the stem is the title.
        assert_eq!(file_values(&f).title.as_deref(), Some("Untitled"));
        // A provider value (the recovered original title for documents)
        // backs `%t` when no embedded title exists.
        f.subject = Some("Quarterly Report".into());
        assert_eq!(file_values(&f).title.as_deref(), Some("Quarterly Report"));
        // Embedded metadata still wins over the provider value.
        f.embedded_title = Some("  The Real Title ".into());
        assert_eq!(file_values(&f).title.as_deref(), Some("The Real Title"));
    }

    #[test]
    fn strips_duplicated_document_extension_from_titles() {
        // The stem of `Name.docx.pdf` is `Name.docx`.
        let mut f = analyzed("/tmp/William Hua - Offboarding.docx.pdf");
        assert_eq!(
            file_values(&f).title.as_deref(),
            Some("William Hua - Offboarding")
        );
        // A provider that echoes the filename is cleaned for both `%t` and `%s`.
        f.subject = Some("Quarterly Report.pdf".into());
        assert_eq!(file_values(&f).title.as_deref(), Some("Quarterly Report"));
        assert_eq!(file_values(&f).subject.as_deref(), Some("Quarterly Report"));
        // An authoritative embedded title is used verbatim.
        f.embedded_title = Some("Signed Contract.pdf".into());
        assert_eq!(
            file_values(&f).title.as_deref(),
            Some("Signed Contract.pdf")
        );
    }

    #[test]
    fn unknown_provider_answer_falls_back_to_the_stem() {
        let mut f = analyzed("/tmp/Quarterly Report.pdf");
        f.subject = Some("unknown".into());
        // `unknown` is no answer, so `%t` and `%s` use the stem.
        assert_eq!(file_values(&f).title.as_deref(), Some("Quarterly Report"));
        assert_eq!(file_values(&f).subject.as_deref(), Some("Quarterly Report"));
    }

    #[test]
    fn metadata_reports_the_value_a_manual_override_replaced() {
        let fmt = FormatString::parse("Archives/%f").unwrap();
        let home = Path::new("/home/user");
        let symlinks = SymlinkIndex::default();

        // No override: nothing was replaced, so no `original_*` rows appear.
        let plain = analyzed("/tmp/Untitled.jpg");
        let s = rebuild_move_suggestion(&plain, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert!(!s.metadata.contains_key("original_location"));
        assert!(!s.metadata.contains_key("original_description"));
        assert!(!s.metadata.contains_key("original_author"));

        // An override for every field reports the provider or embedded value
        // the destination would have used without it. `title` carries the
        // effective value the editor prefills `subject` from.
        let mut f = analyzed("/tmp/Untitled.jpg");
        f.location = Some("Berlin".into());
        f.subject = Some("Provider Description".into());
        f.embedded_title = Some("Embedded Title".into());
        f.embedded_author = Some("Real Author".into());
        f.overrides.location = Some("Manual Place".into());
        f.overrides.subject = Some("Manual Subject".into());
        f.overrides.author = Some("Manual Author".into());
        let s = rebuild_move_suggestion(&f, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.metadata.get("title").and_then(|v| v.as_str()),
            Some("Manual Subject")
        );
        assert_eq!(
            s.metadata.get("original_location").and_then(|v| v.as_str()),
            Some("Berlin")
        );
        // The subject backs `%s`/`%t` but is the content description, so its
        // original is the provider description, not the embedded title that
        // would win `%t` without the override.
        assert_eq!(
            s.metadata
                .get("original_description")
                .and_then(|v| v.as_str()),
            Some("Provider Description")
        );
        assert_eq!(
            s.metadata.get("original_author").and_then(|v| v.as_str()),
            Some("Real Author")
        );

        // An embedded title is the description only when the provider did not
        // supply one, and a bare filename stem is never reported.
        let mut titled = analyzed("/tmp/Untitled.jpg");
        titled.embedded_title = Some("Embedded Title".into());
        titled.overrides.subject = Some("Manual Subject".into());
        let s = rebuild_move_suggestion(&titled, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.metadata
                .get("original_description")
                .and_then(|v| v.as_str()),
            Some("Embedded Title")
        );
        let mut stem_only = analyzed("/tmp/Untitled.jpg");
        stem_only.overrides.subject = Some("Manual Subject".into());
        let s = rebuild_move_suggestion(&stem_only, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert!(!s.metadata.contains_key("original_description"));

        // Without a provider or embedded value, the location's original is
        // the GPS fallback; with neither, there is no original row at all.
        let mut gps = analyzed("/tmp/Untitled.jpg");
        gps.latitude = Some(48.8566);
        gps.longitude = Some(2.3522);
        gps.overrides.location = Some("Manual Place".into());
        let s = rebuild_move_suggestion(&gps, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.metadata.get("original_location").and_then(|v| v.as_str()),
            Some("48.856600,2.352200")
        );
        let mut none = analyzed("/tmp/Untitled.jpg");
        none.overrides.location = Some("Manual Place".into());
        let s = rebuild_move_suggestion(&none, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert!(!s.metadata.contains_key("original_location"));
    }

    #[test]
    fn metadata_reports_date_and_zone_provenance() {
        let fmt = FormatString::parse("Archives/%Y/%f").unwrap();
        let home = Path::new("/home/user");
        let symlinks = SymlinkIndex::default();

        // No embedded or filename date: the suggestion falls back to the
        // filesystem modification time, and the metadata says so rather
        // than leaving the rendered date unexplained.
        let filesystem = analyzed("/tmp/archive.zip");
        let s = rebuild_move_suggestion(&filesystem, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.metadata.get("date_source").and_then(|v| v.as_str()),
            Some("filesystem")
        );
        assert!(!s.metadata.contains_key("extracted_date"));

        // An embedded date keeps the camera's zone-less wall clock verbatim
        // and reports the recorded offset alongside it.
        let mut embedded = analyzed("/tmp/IMG_0001.jpg");
        embedded.extracted_date = Some(homelinter_analyze::DateParts {
            year: 2024,
            month: 3,
            day: 15,
            hour: 15,
            minute: 30,
            second: 45,
        });
        embedded.date_source = "embedded";
        embedded.date_offset = Some("+09:00".into());
        let s = rebuild_move_suggestion(&embedded, Some(&fmt), home, &symlinks)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.metadata.get("date_source").and_then(|v| v.as_str()),
            Some("embedded")
        );
        assert_eq!(
            s.metadata.get("extracted_date").and_then(|v| v.as_str()),
            Some("2024-03-15 15:30:45")
        );
        assert_eq!(
            s.metadata.get("date_offset").and_then(|v| v.as_str()),
            Some("+09:00")
        );
    }
}
