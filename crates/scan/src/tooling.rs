//! Runtime detection of optional external tools.
//!
//! External tools (fd, exiftool, git, pandoc, ...) are optional and
//! platform-specific. Core discovery, hashing, path handling, and execution
//! safety are implemented with platform-neutral Rust APIs; tools are only
//! consulted for optional metadata enrichment when present on the PATH.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct ToolSet {
    /// Tool names found on the PATH, normalized to lowercase on Windows.
    pub available: BTreeSet<String>,
}

impl ToolSet {
    pub fn detect() -> ToolSet {
        let mut t = ToolSet::default();
        for name in [
            "fd",
            "find",
            "exiftool",
            "git",
            "hg",
            "svn",
            "pdftotext",
            "pandoc",
            "mutool",
        ] {
            if which(name) {
                t.available.insert(name.to_string());
            }
        }
        t
    }

    pub fn has(&self, name: &str) -> bool {
        self.available.contains(name)
    }
}

fn which(name: &str) -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        if cand.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&cand) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return true;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                return true;
            }
        }
    }
    false
}
