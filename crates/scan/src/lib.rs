//! Primary discovery and the home-wide symlink pass.
//!
//! Discovery is implemented with platform-neutral Rust APIs (never spawned
//! `find`/`fd`), honoring the hidden-file rule, scan exclusions, and the
//! repository-atomicity rule. The separate home-wide symlink pass indexes
//! symlinks across the entire home directory regardless of the scan root,
//! hidden settings, or exclusions, collecting only the metadata needed for
//! repair.

mod discovery;
mod symlinks;
mod tooling;

pub use discovery::{
    discover, DiscoveryError, DiscoveryOptions, FileEntry, RepoEntry, ScanOutput, VcsKind,
};
pub use symlinks::{SymlinkEntry, SymlinkIndex};
pub use tooling::ToolSet;

use std::path::{Path, PathBuf};

/// All discovery results for one scan: primary units and the home-wide
/// symlink index.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub files: Vec<FileEntry>,
    pub repos: Vec<RepoEntry>,
    pub symlinks: SymlinkIndex,
    /// Directories seen by the primary scan (excluding repositories), for
    /// structure inference.
    pub dirs: Vec<PathBuf>,
    /// Primary regular files plus repository units, for reporting.
    pub primary_units: u64,
}

/// Run the primary discovery walk and the home-wide symlink pass for one
/// scan request.
pub fn scan_root(
    _root: &Path,
    home: &Path,
    opts: &DiscoveryOptions,
) -> Result<ScanResult, DiscoveryError> {
    let primary = discover(opts)?;
    let symlinks = SymlinkIndex::scan(home);
    let primary_units = primary.files_scanned + primary.repos.len() as u64;
    Ok(ScanResult {
        files: primary.files,
        repos: primary.repos,
        symlinks,
        dirs: primary.dirs,
        primary_units,
    })
}
