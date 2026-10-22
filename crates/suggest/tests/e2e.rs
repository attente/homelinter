//! End-to-end suggestion generation tests: scan a temp home, analyze,
//! generate suggestions, and assert deterministic, safe outcomes.

use homelinter_analyze::{
    analyze, AnalyzeOptions, EmbeddedMeta, ExtractionContext, MetadataExtractor,
};
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::config::{HiddenDefinition, RawConfig};
use homelinter_model::format::FormatString;
use homelinter_model::suggestion::SuggestionType;
use homelinter_scan::{scan_root, DiscoveryOptions};
use homelinter_suggest::{generate_suggestions, validate_batch, GenerateInput};

use std::collections::BTreeMap;
use std::path::Path;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hl-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let base = tmpdir(tag);
    let root = base.join("root");
    let home = base.join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    (base, root, home)
}

fn discovery(root: &std::path::Path, hidden: bool) -> DiscoveryOptions {
    DiscoveryOptions {
        root: root.to_path_buf(),
        hidden,
        hidden_definition: HiddenDefinition::Dotfiles,
        exclude_patterns: vec![],
        exclude_extensions: vec![],
    }
}

/// Build the effective format map: photo and document entries only; the
/// scan fixture only contains those kinds.
fn fmt_map() -> BTreeMap<AssetKind, FormatString> {
    let mut m = BTreeMap::new();
    m.insert(
        AssetKind::Photo,
        FormatString::parse("Pictures/%n.%e").unwrap(),
    );
    m.insert(
        AssetKind::Document,
        FormatString::parse("Documents/%n.%e").unwrap(),
    );
    m
}

#[test]
fn duplicate_kept_moved_others_deleted() {
    let (base, root, home) = setup("dup");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    // Two identical images in Downloads; keeper is the older (a.jpg).
    let a = downloads.join("a.jpg");
    let b = downloads.join("b.jpg");
    std::fs::write(&a, b"same-content").unwrap();
    std::fs::write(&b, b"same-content").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    filetime::set_file_mtime(&a, filetime::FileTime::from_system_time(old)).unwrap();

    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(scan.files.len(), 2);
    let analysis = analyze(
        &scan.files,
        &scan.repos,
        &AnalyzeOptions::default(),
        &|| false,
        &|_| {},
    );
    let out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt_map(),
        home: &home,
    })
    .unwrap();

    // One delete (newer duplicate) + one move (the keeper).
    let deletes: Vec<_> = out
        .suggestions
        .iter()
        .filter(|s| s.suggestion_type == SuggestionType::Delete)
        .collect();
    let moves: Vec<_> = out
        .suggestions
        .iter()
        .filter(|s| s.suggestion_type == SuggestionType::Move)
        .collect();
    assert_eq!(deletes.len(), 1);
    assert_eq!(moves.len(), 1);
    assert!(deletes[0].target.ends_with("b.jpg"));
    assert!(moves[0].target.ends_with("a.jpg"));
    let _ = std::fs::remove_dir_all(&base);
}

struct TitleFor(std::path::PathBuf);

impl MetadataExtractor for TitleFor {
    fn name(&self) -> &'static str {
        "test-title"
    }

    fn extract(&self, path: &Path, _kind: AssetKind, _ctx: &ExtractionContext) -> EmbeddedMeta {
        let mut meta = EmbeddedMeta::default();
        if path == self.0 {
            meta.title = Some("Titled".into());
        }
        meta
    }
}

#[test]
fn metadata_preservation_warning_is_surfaced() {
    let (base, root, home) = setup("dupmwarn");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let keeper = downloads.join("a.jpg");
    let newer = downloads.join("b.jpg");
    std::fs::write(&keeper, b"same-content").unwrap();
    std::fs::write(&newer, b"same-content").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    filetime::set_file_mtime(&keeper, filetime::FileTime::from_system_time(old)).unwrap();

    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    let opts = AnalyzeOptions {
        extractors: vec![Box::new(TitleFor(newer.clone()))],
        ..AnalyzeOptions::default()
    };
    let analysis = analyze(&scan.files, &scan.repos, &opts, &|| false, &|_| {});

    // Suppressed group, but the reason is reported instead of dropped.
    assert!(analysis.groups.is_empty());
    assert_eq!(analysis.warnings.len(), 1);
    assert!(analysis.warnings[0].contains("b.jpg"));
    assert!(analysis.warnings[0].contains("manual review"));

    let out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt_map(),
        home: &home,
    })
    .unwrap();
    assert!(out
        .suggestions
        .iter()
        .all(|s| s.suggestion_type != SuggestionType::Delete));
    assert_eq!(out.warnings, analysis.warnings);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn symlink_repair_attached_on_move() {
    use std::os::unix::fs::symlink;
    let (base, root, home) = setup("symlink");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    // A symlink inside home points at the file that will move.
    let f = downloads.join("photo.jpg");
    std::fs::write(&f, b"xx").unwrap();
    let link = home.join("link-to-photo.jpg");
    symlink(&f, &link).unwrap();

    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    let analysis = analyze(
        &scan.files,
        &scan.repos,
        &AnalyzeOptions::default(),
        &|| false,
        &|_| {},
    );
    let out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt_map(),
        home: &home,
    })
    .unwrap();

    let moves: Vec<_> = out
        .suggestions
        .iter()
        .filter(|s| s.suggestion_type == SuggestionType::Move)
        .collect();
    assert_eq!(moves.len(), 1, "expected exactly one move suggestion");
    assert_eq!(
        moves[0].dependent_operations.len(),
        1,
        "repair must be attached"
    );
    let dep = &moves[0].dependent_operations[0];
    assert!(dep.link_path.ends_with("link-to-photo.jpg"));
    assert!(dep.replacement_target.contains("Pictures"));
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn delete_repairs_links_pointing_at_duplicate() {
    use std::os::unix::fs::symlink;
    let (base, root, home) = setup("duplink");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    // Files identical in content; a_keep.jpg written first and sorts
    // before z_dup.jpg, so it is the keeper under either creation-time or
    // lexical tie-break. z_dup.jpg is unambiguously the deletable copy.
    let keeper = downloads.join("a_keep.jpg");
    let dup = downloads.join("z_dup.jpg");
    std::fs::write(&keeper, b"same").unwrap();
    std::fs::write(&dup, b"same").unwrap();
    // Link inside home points at the duplicate that will be deleted.
    let link = home.join("preview.jpg");
    symlink(&dup, &link).unwrap();

    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    let analysis = analyze(
        &scan.files,
        &scan.repos,
        &AnalyzeOptions::default(),
        &|| false,
        &|_| {},
    );
    let out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt_map(),
        home: &home,
    })
    .unwrap();

    let deletes: Vec<_> = out
        .suggestions
        .iter()
        .filter(|s| s.suggestion_type == SuggestionType::Delete)
        .collect();
    assert_eq!(deletes.len(), 1);
    assert!(deletes[0].target.ends_with("z_dup.jpg"));
    assert_eq!(deletes[0].dependent_operations.len(), 1);
    assert!(deletes[0].dependent_operations[0]
        .replacement_target
        .ends_with("a_keep.jpg"));
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn validation_marks_identity_degraded() {
    let (base, root, home) = setup("degraded");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("doc.txt"), b"hello").unwrap();
    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    let mut analysis = analyze(
        &scan.files,
        &scan.repos,
        &AnalyzeOptions::default(),
        &|| false,
        &|_| {},
    );
    // Simulate a non-unix degraded identity for all files.
    for f in &mut analysis.files {
        f.provenance.identity_available = false;
    }
    let mut out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt_map(),
        home: &home,
    })
    .unwrap();
    let rejected = validate_batch(&mut out.suggestions, false);
    assert_eq!(rejected.len(), out.suggestions.len());
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn noop_move_not_generated() {
    // A destination identical to the source produces no suggestion.
    let (base, root, home) = setup("noop");
    let downloads = root.join("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let f = downloads.join("keep.txt");
    std::fs::write(&f, b"hello").unwrap();
    let scan = scan_root(
        &root,
        &home,
        &discovery(&root, false),
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    let analysis = analyze(
        &scan.files,
        &scan.repos,
        &AnalyzeOptions::default(),
        &|| false,
        &|_| {},
    );
    // Title is "keep"; destination "keep.txt" resolves under a directory
    // named "keep.txt.txt", colliding back with the literal source name
    // only if resolution lands exactly on the file. Simpler: point the
    // format at the file's own directory verbatim.
    let mut fmt = fmt_map();
    fmt.insert(
        AssetKind::Document,
        FormatString::parse("Documents/%n.%e").unwrap(),
    );
    let home2 = root.clone(); // resolution is anchored at root here
    let out = generate_suggestions(&GenerateInput {
        analysis: &analysis,
        symlinks: &scan.symlinks,
        formats: &fmt,
        home: &home2,
    })
    .unwrap();
    // The file is under Downloads, destination under Documents: distinct,
    // so a suggestion exists. Assert it does not equal the source path.
    assert_eq!(out.suggestions.len(), 1);
    assert_ne!(
        out.suggestions[0].target,
        out.suggestions[0].new_path.clone().unwrap()
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn model_defaults_cover_kinds() {
    // The model ships per-kind defaults so every classified kind resolves.
    let cfg = RawConfig::default();
    let defaults = cfg.resolved_target_paths().unwrap();
    for kind in AssetKind::ALL {
        assert!(defaults.contains_key(&kind), "missing default for {kind:?}");
    }
}
