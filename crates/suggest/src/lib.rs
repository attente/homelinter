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
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::format::{FormatString, PlaceholderValues};
use homelinter_model::path;
use homelinter_model::suggestion::{DependentOpType, DependentOperation, Provenance, Suggestion};
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
        warnings: Vec::new(),
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
        let values = repo_values(&repo.root, meta.remote_host.clone());
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
                    format!(
                        "Moved repository {} to the {} destination.",
                        repo.root.display(),
                        "repository"
                    ),
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
            let dependents = delete_dependents(
                &f.path,
                &canonical,
                &keeper_path,
                &input.symlinks.entries,
                &by_canon,
            );
            let size = f.size;
            // The delete requires the recorded full-content hash at
            // execution time; carry it from analysis into the provenance.
            let mut prov = f.provenance.clone();
            prov.full_hash = f.full_hash;
            out.suggestions.push(Suggestion::new_delete(
                f.kind,
                &f.path,
                format!(
                    "Duplicate of {}; retaining the oldest copy and reclaiming {} bytes.",
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
        out.warnings.extend(g.metadata_warning.iter().cloned());
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

    out.suggestions
        .sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
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
    if let Some(dp) = &f.extracted_date {
        metadata.insert(
            "extracted_date".into(),
            serde_json::Value::String(format!("{:04}-{:02}-{:02}", dp.year, dp.month, dp.day)),
        );
        metadata.insert(
            "date_source".into(),
            serde_json::Value::String(f.date_source.into()),
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
    let reason = format!("Moved to the {} destination.", f.kind.name());
    Ok(Some(Suggestion::new_move(
        f.kind,
        &f.path,
        &new_path,
        reason,
        metadata,
        dependents,
        f.provenance.clone(),
    )))
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
    let title = f.embedded_title.clone().unwrap_or_else(|| stem.clone());
    PlaceholderValues {
        year,
        year2,
        month,
        day,
        hour,
        minute,
        second,
        title: Some(title),
        author: f.embedded_author.clone(),
        location: f.location.clone(),
        subject: f.subject.clone(),
        camera: f.camera_model.clone(),
        remote_host: None,
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

fn repo_values(root: &Path, remote_host: Option<String>) -> PlaceholderValues {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    PlaceholderValues {
        asset_kind: AssetKind::Repository,
        title: Some(name.clone()),
        file_name: Some(name.clone()),
        stem: Some(name),
        remote_host,
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

#[cfg(unix)]
fn repo_provenance(root: &Path) -> Provenance {
    use std::os::unix::fs::MetadataExt;
    let mut p = Provenance::default();
    if let Ok(m) = std::fs::metadata(root) {
        p.dev = m.dev();
        p.ino = m.ino();
        p.identity_available = m.dev() != 0 && m.ino() != 0;
        if let Ok(mt) = m.modified() {
            if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
                p.mtime_ns = d.as_nanos() as i128;
            }
        }
    }
    p
}

#[cfg(not(unix))]
fn repo_provenance(_root: &Path) -> Provenance {
    Provenance {
        identity_available: false,
        ..Default::default()
    }
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

fn date_from_time(
    t: std::time::SystemTime,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
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
    deleted: &Path,
    canonical_deleted: &Path,
    keeper: &Path,
    links: &[SymlinkEntry],
    by_canon: &HashMap<PathBuf, Vec<usize>>,
) -> Vec<DependentOperation> {
    let _ = deleted;
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
    let mut prov = Provenance::default();
    prov.link_resolved_target = link
        .resolved_target
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    prov.link_raw_relative = Some(link.target_relative);
    DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.link_path.to_string_lossy().into_owned(),
        recorded_target: link.raw_target.clone(),
        replacement_target: replacement,
        recorded_target_relative: link.target_relative,
        provenance: prov,
    }
}

/// Lexically relative path from `from` (a directory) to `to`.
pub fn relative_to(from: &Path, to: &Path) -> String {
    let from_comps: Vec<_> = from.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < from_comps.len()
        && common < to_comps.len()
        && from_comps[common] == to_comps[common]
    {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_comps.len() {
        parts.push("..".to_string());
    }
    for c in &to_comps[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b")), ".");
    }
}
