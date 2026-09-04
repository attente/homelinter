//! The home-wide symlink index.
//!
//! For every scan, homelinter also performs a separate symlink pass over the
//! entire user's home directory, regardless of the scan root, hidden
//! settings, or scan exclusions. The pass never follows symlinks and never
//! reads their targets' contents: it records only what repair needs - the
//! absolute link path, the raw target, the target's relative/absolute form,
//! and the resolved target identity when resolvable.

use std::path::{Path, PathBuf};

/// One indexed symlink. `raw_target` preserves the link's written form so
/// repair can preserve relative vs absolute semantics.
#[derive(Debug, Clone)]
pub struct SymlinkEntry {
    pub link_path: PathBuf,
    /// The raw target exactly as stored in the link.
    pub raw_target: String,
    pub target_relative: bool,
    /// Resolved absolute target (symlinks followed for identity only, never
    /// for content). `None` when unresolvable (broken link, permissions).
    pub resolved_target: Option<PathBuf>,
    /// Whether the resolved target is a directory.
    pub resolved_is_dir: bool,
}

/// The index produced by the home-wide pass, with the home path captured at
/// scan start.
#[derive(Debug, Clone, Default)]
pub struct SymlinkIndex {
    pub home: Option<PathBuf>,
    pub entries: Vec<SymlinkEntry>,
}

impl SymlinkIndex {
    pub fn scan(home: &Path) -> SymlinkIndex {
        let mut index = SymlinkIndex {
            home: Some(home.to_path_buf()),
            entries: Vec::new(),
        };
        walk(home, &mut index.entries);
        index.entries.sort_by(|a, b| {
            a.link_path
                .to_string_lossy()
                .cmp(&b.link_path.to_string_lossy())
        });
        index
    }
}

fn walk(dir: &Path, out: &mut Vec<SymlinkEntry>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            if let Some(e) = index_one(&path) {
                out.push(e);
            }
            continue; // never follow links
        }
        if ft.is_dir() {
            walk(&path, out);
        }
    }
}

fn index_one(link_path: &Path) -> Option<SymlinkEntry> {
    let raw = std::fs::read_link(link_path).ok()?;
    let raw_str = raw.to_string_lossy().into_owned();
    let target_relative = raw.is_relative();
    // Resolve for identity. On Unix this resolves the full chain; broken
    // links yield None and stay out of repair eligibility.
    let resolved = std::fs::canonicalize(if target_relative {
        link_path
            .parent()
            .map(|p| p.join(&raw))
            .unwrap_or(raw.clone())
    } else {
        raw.clone()
    })
    .ok();
    let resolved_is_dir = resolved.as_deref().map(|p| p.is_dir()).unwrap_or(false);
    Some(SymlinkEntry {
        link_path: link_path.to_path_buf(),
        raw_target: raw_str,
        target_relative,
        resolved_target: resolved,
        resolved_is_dir,
    })
}
