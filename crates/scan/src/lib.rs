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

/// Which walk of a scan a live unit count belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStage {
    /// Primary discovery walk of the scan root.
    Discovery,
    /// Home-wide symlink index walk.
    Symlinks,
}

impl ScanStage {
    /// Phase name used in `scan.progress` events.
    pub fn phase(self) -> &'static str {
        match self {
            ScanStage::Discovery => "discovery",
            ScanStage::Symlinks => "symlinks",
        }
    }
}
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
/// scan request. `cancel` is a cooperative-cancellation probe polled inside
/// both walks (per entry), so a cancel request stops a scan promptly
/// instead of waiting for the current blocking phase to finish; the partial
/// result is discarded by the caller when it observes the probe.
/// `progress` receives a running count of entries visited in each walk, so
/// clients can show the walks advancing while no total is known yet.
pub fn scan_root(
    _root: &Path,
    home: &Path,
    opts: &DiscoveryOptions,
    cancel: &dyn Fn() -> bool,
    progress: &(dyn Fn(ScanStage, u64) + Sync),
) -> Result<ScanResult, DiscoveryError> {
    let primary = discover(opts, cancel, &|n| progress(ScanStage::Discovery, n))?;
    let symlinks = SymlinkIndex::scan(home, cancel, &|n| progress(ScanStage::Symlinks, n));
    let primary_units = primary.files_scanned + primary.repos.len() as u64;
    Ok(ScanResult {
        files: primary.files,
        repos: primary.repos,
        symlinks,
        dirs: primary.dirs,
        primary_units,
    })
}
