//! The persistent metadata cache: a single SQLite database in the platform
//! application-data directory, in WAL mode, with the schema and semantics
//! from "Cache Implementation" in DESIGN.md.
//!
//! The scaffold implements storage only. Provider lookups (LLM
//! descriptions, reverse geocoding, filename analysis) plug in later; the
//! key construction and read/write primitives are the real contract they
//! will use, and `DELETE /cache` operates on the real database.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{params, Connection, OpenFlags};

use homelinter_model::config::app_data_dir;

pub const SCHEMA_VERSION: &str = "1";

/// Cache kinds accepted by the schema. Scaffold code stores nothing in
/// `entries` until providers are wired; the table is created and the
/// `kind` column is constrained by convention.
pub const KIND_GEOCODE: &str = "geocode";
pub const KIND_DESCRIPTION: &str = "description";
pub const KIND_FILENAME_ANALYSIS: &str = "filename_analysis";

pub struct Cache {
    conn: Connection,
    path: PathBuf,
}

/// Canonical request tuple used to build a cache key:
/// `(kind, provider_id, provider_settings_hash, input_hash)`.
pub fn cache_key(
    kind: &str,
    provider_id: &str,
    provider_settings_hash: &[u8; 32],
    input_hash: &[u8; 32],
) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for field in [
        kind.as_bytes(),
        provider_id.as_bytes(),
        provider_settings_hash,
        input_hash,
    ] {
        h.update((field.len() as u64).to_le_bytes());
        h.update(field);
    }
    h.finalize().into()
}

impl Cache {
    /// Open (creating when needed) the cache database in `dir`. A missing,
    /// incompatible, or corrupt cache file is rebuilt atomically; opening
    /// never blocks a scan.
    pub fn open_dir(dir: &Path) -> Cache {
        std::fs::create_dir_all(dir).ok();
        let path = dir.join("cache.db");
        match Cache::open_path(&path) {
            Ok(c) => c,
            Err(_) => {
                // Rebuild path: fresh database written to a temp name in the
                // same directory, then renamed over the old file.
                let tmp = dir.join(format!(
                    "cache.db.rebuild-{}",
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                let _ = std::fs::remove_file(&tmp);
                if let Ok(c) = Cache::open_path(&tmp) {
                    if std::fs::rename(&tmp, &path).is_ok() {
                        return c;
                    }
                }
                let _ = std::fs::remove_file(&tmp);
                // Last resort: work on a memory database rather than fail the
                // service because the cache is unwritable.
                Cache {
                    conn: Connection::open_in_memory().expect("in-memory sqlite"),
                    path,
                }
            }
        }
    }

    /// The platform default cache location.
    pub fn default_path() -> Option<PathBuf> {
        app_data_dir().map(|d| d.join("cache.db"))
    }

    fn open_path(path: &Path) -> rusqlite::Result<Cache> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(&conn, path)?;
        Ok(Cache {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn init(conn: &Connection, path: &Path) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entries (
                cache_key BLOB PRIMARY KEY,
                kind TEXT NOT NULL,
                provider TEXT NOT NULL,
                input_hash BLOB NOT NULL,
                value TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS entries_kind_idx ON entries(kind);",
        )?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        match version.as_deref() {
            None | Some(SCHEMA_VERSION) => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(_) => {
                // Incompatible version: the caller's rebuild path replaces
                // the file; here we refuse and let open_dir rebuild.
                return Err(rusqlite::Error::InvalidQuery);
            }
        }
        let _ = path;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up a cached value by key. Returns the cached value when the
    /// provider identifier still matches the requesting provider.
    pub fn get(&self, key: &[u8; 32], provider_id: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM entries WHERE cache_key = ?1 AND provider = ?2")?;
        let mut rows = stmt.query(params![key.as_slice(), provider_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Insert a cache entry write-once: same key always carries the same
    /// value (providers are keyed by their settings hash), so a concurrent
    /// duplicate insert is safe in either outcome.
    pub fn put(
        &self,
        key: &[u8; 32],
        kind: &str,
        provider_id: &str,
        input_hash: &[u8; 32],
        value: &str,
    ) -> rusqlite::Result<()> {
        let created_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO entries (cache_key, kind, provider, input_hash, value, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key.as_slice(), kind, provider_id, input_hash.as_slice(), value, created_at],
        )?;
        Ok(())
    }

    /// Clear all cached results in a single transactional write. Never
    /// deletes the cache file.
    pub fn clear(&self) -> rusqlite::Result<usize> {
        let before = self.entry_count()?;
        self.conn.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM entries;
             DELETE FROM meta WHERE key != 'schema_version';
             COMMIT;",
        )?;
        Ok(before as usize)
    }

    pub fn entry_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hl-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn put_get_roundtrip_and_clear() {
        let dir = tmp();
        let c = Cache::open_dir(&dir);
        let key = cache_key(
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[7u8; 32],
            &[1u8; 32],
        );
        assert!(c.get(&key, "nominatim:openstreetmap").unwrap().is_none());
        c.put(
            &key,
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[1u8; 32],
            "Berlin",
        )
        .unwrap();
        assert_eq!(
            c.get(&key, "nominatim:openstreetmap").unwrap().as_deref(),
            Some("Berlin")
        );
        // Write-once: second insert is ignored, value unchanged.
        c.put(
            &key,
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[1u8; 32],
            "Paris",
        )
        .unwrap();
        assert_eq!(
            c.get(&key, "nominatim:openstreetmap").unwrap().as_deref(),
            Some("Berlin")
        );
        // Provider mismatch never serves the value.
        assert!(c.get(&key, "nominatim:other").unwrap().is_none());
        assert_eq!(c.clear().unwrap(), 1);
        assert!(c.get(&key, "nominatim:openstreetmap").unwrap().is_none());
        drop(c);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn key_changes_with_any_field() {
        let a = cache_key(KIND_GEOCODE, "p", &[0u8; 32], &[0u8; 32]);
        let b = cache_key(KIND_GEOCODE, "p", &[0u8; 32], &[1u8; 32]);
        let c = cache_key(KIND_DESCRIPTION, "p", &[0u8; 32], &[0u8; 32]);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
