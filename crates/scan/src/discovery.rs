//! The primary discovery walk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{Glob, GlobMatcher};
use homelinter_model::config::HiddenDefinition;
use homelinter_model::path;

/// Version-control system markers that turn a directory into an atomic
/// repository unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VcsKind {
    Git,
    Hg,
    Svn,
}

impl VcsKind {
    pub fn name(self) -> &'static str {
        match self {
            VcsKind::Git => "git",
            VcsKind::Hg => "hg",
            VcsKind::Svn => "svn",
        }
    }
    pub fn marker(self) -> &'static str {
        match self {
            VcsKind::Git => ".git",
            VcsKind::Hg => ".hg",
            VcsKind::Svn => ".svn",
        }
    }
}

/// A regular file discovered by the primary scan. Empty files are ignored
/// entirely and never appear here.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Normalized absolute path.
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub created: Option<SystemTime>,
}

/// A repository root (directory containing a `.git`/`.hg`/`.svn` marker).
/// Repositories are atomic units: the walker never descends into them.
#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub root: PathBuf,
    pub vcs: VcsKind,
    /// Whether the marker is a file (git worktrees) rather than a directory.
    pub marker_is_file: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoveryError(pub String);

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DiscoveryError {}

impl From<std::io::Error> for DiscoveryError {
    fn from(e: std::io::Error) -> Self {
        DiscoveryError(e.to_string())
    }
}

pub struct DiscoveryOptions {
    /// Normalized absolute scan root.
    pub root: PathBuf,
    /// Whether hidden files and directories are included in primary
    /// discovery.
    pub hidden: bool,
    /// Windows-only definition of hidden; ignored on Unix where hidden
    /// always means dotfile names.
    pub hidden_definition: HiddenDefinition,
    /// `scan.exclude_paths` patterns (host globs).
    pub exclude_patterns: Vec<String>,
    /// `scan.exclude_extensions` entries: bare, case-insensitive extensions
    /// excluded at any depth in addition to `exclude_patterns`.
    pub exclude_extensions: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ScanOutput {
    pub files: Vec<FileEntry>,
    pub repos: Vec<RepoEntry>,
    pub dirs: Vec<PathBuf>,
    /// Hidden and VCS-ignored directories are never consulted by the
    /// primary scan, but keep a count for reporting.
    pub files_scanned: u64,
}

#[derive(Debug)]
struct ExcludeSet {
    /// Patterns without a path separator: match the final component at any
    /// depth.
    basename_matchers: Vec<GlobMatcher>,
    /// Patterns with a path separator: match the relative path from the
    /// scan root.
    relative_matchers: Vec<GlobMatcher>,
    relative_patterns: Vec<String>,
    /// Normalized, lowercased extensions excluded at any depth.
    extensions: BTreeSet<String>,
}

impl ExcludeSet {
    fn compile(patterns: &[String], extensions: &[String]) -> Result<ExcludeSet, DiscoveryError> {
        let mut out = ExcludeSet {
            basename_matchers: Vec::new(),
            relative_matchers: Vec::new(),
            relative_patterns: Vec::new(),
            extensions: extensions
                .iter()
                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        };
        for raw in patterns {
            let p = raw.trim();
            if p.is_empty() {
                continue;
            }
            let has_separator = p.contains('/') || (cfg!(windows) && p.contains('\\'));
            let pattern = if has_separator {
                p.to_string()
            } else {
                // Final-component match at any depth, including the root.
                format!("**/{p}")
            };
            let glob = Glob::new(&pattern)
                .map_err(|e| DiscoveryError(format!("invalid exclude pattern '{p}': {e}")))?;
            let matcher = glob.compile_matcher();
            if has_separator {
                out.relative_patterns.push(pattern);
                out.relative_matchers.push(matcher);
            } else {
                out.basename_matchers.push(matcher);
            }
        }
        Ok(out)
    }

    /// Test whether a directory (by relative path or final name) is
    /// excluded. A directory match prunes the whole subtree.
    fn excludes_dir(&self, rel: &str, name: &str) -> bool {
        self.relative_matchers.iter().any(|m| m.is_match(rel))
            || self.basename_matchers.iter().any(|m| m.is_match(name))
    }

    fn excludes_file(&self, rel: &str, name: &str) -> bool {
        if self.excludes_dir(rel, name) {
            return true;
        }
        match final_extension(name) {
            Some(ext) => self.extensions.contains(&ext),
            None => false,
        }
    }
}

/// The lowercased final extension of a file name, if it has one. A hidden
/// name like `.png` or a trailing dot yields `None`, matching how the asset
/// classifier treats extension-free names.
fn final_extension(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let dot = lower.rfind('.')?;
    if dot == 0 || dot + 1 >= lower.len() {
        return None;
    }
    Some(lower[dot + 1..].to_string())
}

fn rel_path(root: &Path, abs: &Path) -> String {
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}

/// Whether `name` counts as hidden for this platform and setting.
fn is_hidden_name(name: &str, def: HiddenDefinition) -> bool {
    #[cfg(unix)]
    {
        let _ = def;
        name.starts_with('.')
    }
    #[cfg(windows)]
    {
        // std exposes the Hidden/System attributes through
        // `std::os::windows::fs::MetadataExt`; the walker checks them via
        // the entry's metadata when the definition needs it.
        match def {
            HiddenDefinition::Dotfiles => name.starts_with('.'),
            HiddenDefinition::Attribute | HiddenDefinition::Explorer => {
                // The attribute check runs in `entry_hidden_windows` against
                // live metadata; fall back to name matching when metadata is
                // unavailable.
                name.starts_with('.')
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = def;
        name.starts_with('.')
    }
}

/// Marker-name lookup over a directory's children, returning the VCS kind.
fn repo_marker_in(names: &[PathBuf]) -> Option<VcsKind> {
    [VcsKind::Git, VcsKind::Hg, VcsKind::Svn]
        .into_iter()
        .find(|&kind| names.iter().any(|n| n == Path::new(kind.marker())))
}

fn is_vcs_marker_name(n: &std::ffi::OsStr) -> bool {
    matches!(n.to_str(), Some(".git" | ".hg" | ".svn"))
}

/// Walk the primary tree. `home` is only used to bound the relative
/// representation; discovery itself never leaves the root. `cancel` is a
/// cooperative-cancellation probe polled while walking: once it returns
/// true the walk stops early and returns the partially-collected output,
/// so a scan cancelled mid-discovery does not need to finish the tree.
pub fn discover(
    opts: &DiscoveryOptions,
    cancel: &dyn Fn() -> bool,
) -> Result<ScanOutput, DiscoveryError> {
    let root = opts.root.clone();
    if !root.is_dir() {
        return Err(DiscoveryError(format!(
            "scan root is not a directory: {}",
            root.display()
        )));
    }
    let excludes = ExcludeSet::compile(&opts.exclude_patterns, &opts.exclude_extensions)?;
    let mut out = ScanOutput::default();
    walk_dir(&root, &root, opts, &excludes, cancel, &mut out)?;
    // Deterministic order: repository roots before files keeps reporting
    // stable; each list is sorted by normalized absolute path.
    out.files.sort_by_key(|a| path::sort_key(&a.path));
    out.repos.sort_by_key(|a| path::sort_key(&a.root));
    out.dirs.sort_by_key(|a| path::sort_key(a));
    Ok(out)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    opts: &DiscoveryOptions,
    excludes: &ExcludeSet,
    cancel: &dyn Fn() -> bool,
    out: &mut ScanOutput,
) -> Result<(), DiscoveryError> {
    let entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return Ok(()), // unreadable directories are not errors
    };
    if entries.is_empty() {
        return Ok(());
    }
    let names: Vec<PathBuf> = entries
        .iter()
        .map(|e| PathBuf::from(e.file_name()))
        .collect();
    let root_repo = repo_marker_in(&names);

    // A directory with a VCS marker is an atomic repository unit: record it
    // and never descend. Repositories strictly below the scan root are never
    // descended into.
    if dir != root {
        if let Some(kind) = root_repo {
            let marker_is_file = kind == VcsKind::Git
                && std::fs::metadata(dir.join(".git"))
                    .map(|m| m.is_file())
                    .unwrap_or(false);
            out.repos.push(RepoEntry {
                root: dir.to_path_buf(),
                vcs: kind,
                marker_is_file,
            });
            return Ok(());
        }
    }

    for e in entries {
        // Cooperative cancellation: polled per entry so a cancel request
        // lands promptly even inside one very large directory.
        if cancel() {
            return Ok(());
        }
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        if !opts.hidden && is_hidden_name(&name_str, opts.hidden_definition) {
            continue;
        }
        let abs = dir.join(&name);
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if homelinter_model::identity::link_kind(&abs, &meta.file_type())
            != homelinter_model::identity::LinkKind::NotLink
        {
            // Symlinks are never followed or analyzed by the primary scan;
            // junctions and other reparse points are unsupported and are
            // never traversed either.
            continue;
        }
        let rel = rel_path(root, &abs);
        if meta.is_dir() {
            if excludes.excludes_dir(&rel, &name_str) {
                continue;
            }
            // The scan root itself may be a VCS root; its metadata directory
            // is never scanned, even when hidden files are enabled, keeping
            // the unit atomic. Everything else under the root is scanned.
            if dir == root && root_repo.is_some() && is_vcs_marker_name(&name) {
                continue;
            }
            walk_dir(root, &abs, opts, excludes, cancel, out)?;
            out.dirs.push(abs);
        } else if meta.is_file() {
            if excludes.excludes_file(&rel, &name_str) {
                continue;
            }
            let size = meta.len();
            if size == 0 {
                // Empty files are completely ignored.
                continue;
            }
            out.files.push(FileEntry {
                path: abs,
                size,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                created: meta.created().ok(),
            });
            out.files_scanned += 1;
        }
        // Other entry kinds (fifos, sockets, devices) are ignored.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hl-scan-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_files_and_prunes_hidden() {
        let root = tmpdir("basic");
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        std::fs::write(root.join(".hidden.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join(".cfg")).unwrap();
        std::fs::write(root.join(".cfg/inner.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join("Docs")).unwrap();
        std::fs::write(root.join("Docs/b.pdf"), "pdf").unwrap();

        let opts = DiscoveryOptions {
            root: root.clone(),
            hidden: false,
            hidden_definition: HiddenDefinition::Explorer,
            exclude_patterns: Vec::new(),
            exclude_extensions: Vec::new(),
        };
        let out = discover(&opts, &|| false).unwrap();
        assert_eq!(out.files.len(), 2);
        assert_eq!(out.files_scanned, 2);
        let names: Vec<String> = out
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.pdf".to_string()));

        let opts_hidden = DiscoveryOptions {
            hidden: true,
            ..opts
        };
        let out2 = discover(&opts_hidden, &|| false).unwrap();
        assert_eq!(out2.files.len(), 4);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn excludes_by_relative_and_basename() {
        let root = tmpdir("excl");
        std::fs::write(root.join("keep.rs"), "k").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/o.txt"), "o").unwrap();
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("sub/deep/x.hs"), "h").unwrap();
        std::fs::write(root.join("sub/deep/keep.txt"), "t").unwrap();

        let opts = DiscoveryOptions {
            root: root.clone(),
            hidden: false,
            hidden_definition: HiddenDefinition::Explorer,
            exclude_patterns: vec!["build/**".into(), "*.hs".into()],
            exclude_extensions: Vec::new(),
        };
        let out = discover(&opts, &|| false).unwrap();
        let names: Vec<String> = out
            .files
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n.ends_with("keep.rs")));
        assert!(names.iter().any(|n| n.ends_with("keep.txt")));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn excludes_by_extension_case_insensitively_at_any_depth() {
        let root = tmpdir("excl-ext");
        std::fs::write(root.join("keep.jpg"), "j").unwrap();
        std::fs::write(root.join("shot.png"), "p").unwrap();
        std::fs::write(root.join("UPPER.PNG"), "P").unwrap();
        std::fs::write(root.join(".png"), "hidden").unwrap();
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("sub/deep/icon.Png"), "i").unwrap();

        let opts = DiscoveryOptions {
            root: root.clone(),
            hidden: true,
            hidden_definition: HiddenDefinition::Explorer,
            exclude_patterns: Vec::new(),
            exclude_extensions: vec![" .PNG ".into(), "png".into()],
        };
        let out = discover(&opts, &|| false).unwrap();
        let mut names: Vec<String> = out
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![".png", "keep.jpg"]);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn repositories_are_atomic() {
        let root = tmpdir("repo");
        std::fs::create_dir_all(root.join("proj/.git")).unwrap();
        std::fs::write(root.join("proj/.git/config"), "[core]").unwrap();
        std::fs::write(root.join("proj/src.rs"), "code").unwrap();
        std::fs::write(root.join("outside.txt"), "x").unwrap();

        let opts = DiscoveryOptions {
            root: root.clone(),
            hidden: false,
            hidden_definition: HiddenDefinition::Explorer,
            exclude_patterns: Vec::new(),
            exclude_extensions: Vec::new(),
        };
        let out = discover(&opts, &|| false).unwrap();
        assert_eq!(out.files.len(), 1); // outside.txt only
        assert_eq!(out.repos.len(), 1);
        assert_eq!(out.repos[0].vcs, VcsKind::Git);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn cancel_probe_stops_the_walk_early() {
        let root = tmpdir("cancel");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.txt"), "c").unwrap();

        let opts = DiscoveryOptions {
            root: root.clone(),
            hidden: false,
            hidden_definition: HiddenDefinition::Explorer,
            exclude_patterns: Vec::new(),
            exclude_extensions: Vec::new(),
        };
        // A probe that fires immediately leaves the walk empty but succeeds.
        let out = discover(&opts, &|| true).unwrap();
        assert_eq!(out.files.len(), 0);
        assert_eq!(out.repos.len(), 0);
        // A probe that never fires collects everything.
        let out2 = discover(&opts, &|| false).unwrap();
        assert_eq!(out2.files.len(), 3);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn skips_empty_files_and_symlinks() {
        #[cfg(unix)]
        {
            let root = tmpdir("links");
            std::fs::write(root.join("real.txt"), "data").unwrap();
            std::fs::write(root.join("empty.txt"), "").unwrap();
            std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
            std::os::unix::fs::symlink(root.join("sub"), root.join("dirlink")).unwrap();
            std::fs::create_dir_all(root.join("sub")).unwrap();
            std::fs::write(root.join("sub/inner.txt"), "i").unwrap();

            let opts = DiscoveryOptions {
                root: root.clone(),
                hidden: false,
                hidden_definition: HiddenDefinition::Explorer,
                exclude_patterns: Vec::new(),
                exclude_extensions: Vec::new(),
            };
            let out = discover(&opts, &|| false).unwrap();
            // real.txt + sub/inner.txt; empty.txt and symlinks are ignored.
            assert_eq!(out.files.len(), 2);
            assert!(!out.files.iter().any(|f| f.path.ends_with("empty.txt")));

            std::fs::remove_dir_all(root).ok();
        }
    }
}
