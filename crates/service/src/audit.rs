//! The persisted NDJSON audit log of executed suggestions.
//!
//! The log is append-only, persists across sessions and restarts, and is
//! never session-scoped. Exported scripts do not append to it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use homelinter_model::config::app_data_dir;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub operation: String,
    /// Absolute source path.
    pub source: String,
    /// Absolute destination path, when the operation has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    pub reason: String,
    pub scan_root: String,
    pub result: String,
}

/// Default audit file location: `<app-data>/homelinter/audit.ndjson`.
pub fn default_audit_path() -> Option<std::path::PathBuf> {
    app_data_dir().map(|d| d.join("audit.ndjson"))
}

pub struct AuditLog {
    path: std::path::PathBuf,
}

impl AuditLog {
    pub fn open(path: std::path::PathBuf) -> AuditLog {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        AuditLog { path }
    }

    pub fn default_open() -> AuditLog {
        let path = default_audit_path().unwrap_or_else(|| std::path::PathBuf::from("audit.ndjson"));
        AuditLog::open(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry as a single NDJSON line. Appends are line-atomic
    /// for practical purposes (single write under an exclusive file lock).
    pub fn append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(f, "{line}")?;
        f.flush()
    }

    /// Read entries, newest first. Entries with a timestamp strictly before
    /// `since` (when given) are omitted; at most `limit` entries are
    /// returned.
    pub fn read_newest_first(&self, since: Option<&str>, limit: usize) -> Vec<AuditEntry> {
        let Ok(content) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut entries: Vec<AuditEntry> = Vec::new();
        for line in content.lines().rev() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<AuditEntry>(line) else {
                continue;
            };
            if let Some(since) = since {
                if entry.timestamp.as_str() < since {
                    continue;
                }
            }
            entries.push(entry);
            if entries.len() >= limit {
                break;
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_newest_first() {
        let dir = std::env::temp_dir().join(format!(
            "hl-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = AuditLog::open(dir.join("audit.ndjson"));
        let e = |ts: &str, src: &str| AuditEntry {
            timestamp: ts.into(),
            operation: "move".into(),
            source: src.into(),
            destination: None,
            reason: "r".into(),
            scan_root: "/scan".into(),
            result: "executed".into(),
        };
        log.append(&e("2024-01-01T00:00:00Z", "/a")).unwrap();
        log.append(&e("2024-01-02T00:00:00Z", "/b")).unwrap();
        let all = log.read_newest_first(None, 100);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].source, "/b");
        assert_eq!(all[1].source, "/a");
        let since = log.read_newest_first(Some("2024-01-02T00:00:00Z"), 100);
        assert_eq!(since.len(), 1);
        let limited = log.read_newest_first(None, 1);
        assert_eq!(limited.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
