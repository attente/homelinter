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
pub const KIND_AUTHOR: &str = "author";
pub const KIND_FILENAME_ANALYSIS: &str = "filename_analysis";

pub struct Cache {
    conn: Connection,
    path: PathBuf,
}

/// One cached result as shown by a cache viewer. The key and input hash are
/// hex-encoded for display; the value is the stored provider result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheEntry {
    pub cache_key: String,
    pub kind: String,
    pub provider: String,
    pub input_hash: String,
    pub value: String,
    pub created_at: String,
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

    /// The number of entries of one kind, or of every kind when `kind` is
    /// `None`.
    pub fn count_kind(&self, kind: Option<&str>) -> rusqlite::Result<u64> {
        match kind {
            Some(k) => self.conn.query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = ?1",
                params![k],
                |r| r.get(0),
            ),
            None => self.entry_count(),
        }
    }

    /// Per-kind entry counts, sorted by kind name, for viewer summaries.
    pub fn counts_by_kind(&self) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM entries GROUP BY kind ORDER BY kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// List entries newest first, optionally filtered by kind. `offset` and
    /// `limit` page through large caches; the viewer reads only what it
    /// displays.
    pub fn list(
        &self,
        kind: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> rusqlite::Result<Vec<CacheEntry>> {
        let row_to_entry = |r: &rusqlite::Row| -> rusqlite::Result<CacheEntry> {
            let key: Vec<u8> = r.get(0)?;
            let input: Vec<u8> = r.get(3)?;
            Ok(CacheEntry {
                cache_key: hex::encode(key),
                kind: r.get(1)?,
                provider: r.get(2)?,
                input_hash: hex::encode(input),
                value: r.get(4)?,
                created_at: r.get(5)?,
            })
        };
        let sql = match kind {
            Some(_) => {
                "SELECT cache_key, kind, provider, input_hash, value, created_at
                 FROM entries WHERE kind = ?1
                 ORDER BY created_at DESC, cache_key LIMIT ?2 OFFSET ?3"
            }
            None => {
                "SELECT cache_key, kind, provider, input_hash, value, created_at
                 FROM entries
                 ORDER BY created_at DESC, cache_key LIMIT ?1 OFFSET ?2"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match kind {
            Some(k) => stmt.query_map(
                params![k, limit as i64, offset as i64],
                row_to_entry,
            )?,
            None => stmt.query_map(params![limit as i64, offset as i64], row_to_entry)?,
        };
        rows.collect()
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
    fn list_filters_pages_and_reports_counts() {
        let dir = tmp();
        let c = Cache::open_dir(&dir);
        let geo = cache_key(KIND_GEOCODE, "nominatim:openstreetmap", &[1u8; 32], &[1u8; 32]);
        let desc = cache_key(KIND_DESCRIPTION, "description:ollama:mistral", &[2u8; 32], &[2u8; 32]);
        c.put(&geo, KIND_GEOCODE, "nominatim:openstreetmap", &[1u8; 32], "Berlin")
            .unwrap();
        c.put(
            &desc,
            KIND_DESCRIPTION,
            "description:ollama:mistral",
            &[2u8; 32],
            "a coastline",
        )
        .unwrap();

        assert_eq!(c.entry_count().unwrap(), 2);
        assert_eq!(c.count_kind(Some(KIND_GEOCODE)).unwrap(), 1);
        assert_eq!(c.count_kind(Some(KIND_AUTHOR)).unwrap(), 0);

        let counts = c.counts_by_kind().unwrap();
        assert_eq!(
            counts,
            vec![
                (KIND_DESCRIPTION.to_string(), 1),
                (KIND_GEOCODE.to_string(), 1),
            ]
        );

        let all = c.list(None, 0, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].cache_key.len(), 64);

        let only_geo = c.list(Some(KIND_GEOCODE), 0, 10).unwrap();
        assert_eq!(only_geo.len(), 1);
        assert_eq!(only_geo[0].kind, KIND_GEOCODE);
        assert_eq!(only_geo[0].value, "Berlin");

        // Paging beyond the end yields an empty slice, not an error.
        assert!(c.list(None, 5, 10).unwrap().is_empty());
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
