//! The persistent metadata cache: a single SQLite database in the platform
//! application-data directory, in WAL mode, with the schema and semantics
//! from "Cache Implementation" in DESIGN.md.
//!
//! The scaffold implements storage only. Provider lookups (LLM
//! descriptions, reverse geocoding, filename analysis) plug in later; the
//! key construction and read/write primitives are the real contract they
//! will use, and `DELETE /cache` operates on the real database.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{params, Connection, OpenFlags};

use homelinter_analyze::MetadataOverrides;
use homelinter_model::config::app_data_dir;

/// Bumped whenever the key construction or value contract changes in a way
/// that makes existing entries unreachable or unsafe to serve, so the cache
/// is rebuilt rather than left with orphaned rows.
pub const SCHEMA_VERSION: &str = "1";

/// Cache kinds accepted by the schema. Scaffold code stores nothing in
/// `entries` until providers are wired; the table is created and the
/// `kind` column is constrained by convention.
pub const KIND_GEOCODE: &str = "geocode";
pub const KIND_DESCRIPTION: &str = "description";
pub const KIND_AUTHOR: &str = "author";
pub const KIND_FILENAME_ANALYSIS: &str = "filename_analysis";

/// Manual-metadata override fields, keyed by content identity in the
/// `metadata_overrides` table. They mirror `MetadataOverrides`.
pub const FIELD_LOCATION: &str = "location";
pub const FIELD_SUBJECT: &str = "subject";
pub const FIELD_AUTHOR: &str = "author";

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
            CREATE INDEX IF NOT EXISTS entries_kind_idx ON entries(kind);
            CREATE TABLE IF NOT EXISTS metadata_overrides (
                content_key BLOB NOT NULL,
                field TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (content_key, field)
            );",
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

    /// Overwrite a cache entry. Ordinary inserts are write-once, but an
    /// explicit per-suggestion re-request must replace the value the user
    /// rejected even if a stale in-flight request re-inserted it first.
    pub fn replace(
        &self,
        key: &[u8; 32],
        kind: &str,
        provider_id: &str,
        input_hash: &[u8; 32],
        value: &str,
    ) -> rusqlite::Result<()> {
        let created_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO entries (cache_key, kind, provider, input_hash, value, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(cache_key) DO UPDATE SET
                 value = excluded.value,
                 created_at = excluded.created_at",
            params![
                key.as_slice(),
                kind,
                provider_id,
                input_hash.as_slice(),
                value,
                created_at
            ],
        )?;
        Ok(())
    }

    /// Delete one entry by key; a no-op when the key is absent. Used to
    /// invalidate exactly the results of a per-suggestion re-request.
    pub fn remove(&self, key: &[u8; 32]) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE cache_key = ?1",
            params![key.as_slice()],
        )?;
        Ok(())
    }

    /// Persist one manual metadata field for a content identity, replacing
    /// any existing value for the same `(content_key, field)`. Other fields
    /// are untouched. Overrides are user data, not cache results, so
    /// `clear` and the entry counters leave them alone.
    pub fn override_set(
        &self,
        content_key: &[u8; 32],
        field: &str,
        value: &str,
    ) -> rusqlite::Result<()> {
        let updated_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO metadata_overrides (content_key, field, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(content_key, field) DO UPDATE SET
                 value = excluded.value,
                 updated_at = excluded.updated_at",
            params![content_key.as_slice(), field, value, updated_at],
        )?;
        Ok(())
    }

    /// Remove one manual metadata field for a content identity; a no-op when
    /// the row is absent, so clearing an already-clear field is safe.
    pub fn override_clear(&self, content_key: &[u8; 32], field: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM metadata_overrides WHERE content_key = ?1 AND field = ?2",
            params![content_key.as_slice(), field],
        )?;
        Ok(())
    }

    /// Every persisted manual metadata override, keyed by content identity.
    /// Loaded once per scan so a rescan applies user edits without a lookup
    /// per file; the override set is small (one row per edited field).
    pub fn all_overrides(&self) -> rusqlite::Result<BTreeMap<[u8; 32], MetadataOverrides>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_key, field, value FROM metadata_overrides")?;
        let mut rows = stmt.query([])?;
        let mut out: BTreeMap<[u8; 32], MetadataOverrides> = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let key: Vec<u8> = row.get(0)?;
            let Ok(key): Result<[u8; 32], _> = key.try_into() else {
                continue;
            };
            let field: String = row.get(1)?;
            let value: String = row.get(2)?;
            let entry = out.entry(key).or_default();
            match field.as_str() {
                FIELD_LOCATION => entry.location = Some(value),
                FIELD_SUBJECT => entry.subject = Some(value),
                FIELD_AUTHOR => entry.author = Some(value),
                _ => {}
            }
        }
        Ok(out)
    }

    /// Number of persisted manual metadata override rows.
    pub fn override_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM metadata_overrides", [], |r| r.get(0))
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
            Some(k) => stmt.query_map(params![k, limit as i64, offset as i64], row_to_entry)?,
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
        let geo = cache_key(
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[1u8; 32],
            &[1u8; 32],
        );
        let desc = cache_key(
            KIND_DESCRIPTION,
            "description:ollama:mistral",
            &[2u8; 32],
            &[2u8; 32],
        );
        c.put(
            &geo,
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[1u8; 32],
            "Berlin",
        )
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
    fn replace_and_remove_override_write_once() {
        let dir = tmp();
        let c = Cache::open_dir(&dir);
        let key = cache_key(
            KIND_DESCRIPTION,
            "description:ollama:m",
            &[4u8; 32],
            &[5u8; 32],
        );
        c.put(
            &key,
            KIND_DESCRIPTION,
            "description:ollama:m",
            &[5u8; 32],
            "old",
        )
        .unwrap();
        // `replace` overwrites the write-once value, as a re-request needs.
        c.replace(
            &key,
            KIND_DESCRIPTION,
            "description:ollama:m",
            &[5u8; 32],
            "new",
        )
        .unwrap();
        assert_eq!(
            c.get(&key, "description:ollama:m").unwrap().as_deref(),
            Some("new")
        );
        // Removing the entry clears it entirely.
        c.remove(&key).unwrap();
        assert!(c.get(&key, "description:ollama:m").unwrap().is_none());
        // Removing an absent key is a no-op, not an error.
        c.remove(&key).unwrap();
        drop(c);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata_overrides_roundtrip_and_survive_clear() {
        let dir = tmp();
        let c = Cache::open_dir(&dir);
        let key = [7u8; 32];
        assert!(c.all_overrides().unwrap().is_empty());
        c.override_set(&key, FIELD_LOCATION, "Kyoto, Japan")
            .unwrap();
        c.override_set(&key, FIELD_SUBJECT, "Sunset").unwrap();
        c.override_set(&key, FIELD_AUTHOR, "Ada").unwrap();
        let all = c.all_overrides().unwrap();
        let o = all.get(&key).unwrap();
        assert_eq!(o.location.as_deref(), Some("Kyoto, Japan"));
        assert_eq!(o.subject.as_deref(), Some("Sunset"));
        assert_eq!(o.author.as_deref(), Some("Ada"));
        assert_eq!(c.override_count().unwrap(), 3);
        // A second set replaces the value: user data is not write-once.
        c.override_set(&key, FIELD_LOCATION, "Osaka, Japan")
            .unwrap();
        assert_eq!(
            c.all_overrides()
                .unwrap()
                .get(&key)
                .unwrap()
                .location
                .as_deref(),
            Some("Osaka, Japan")
        );
        assert_eq!(c.override_count().unwrap(), 3);
        // Clearing removes just the field; clearing an absent field is a
        // no-op, not an error.
        c.override_clear(&key, FIELD_AUTHOR).unwrap();
        c.override_clear(&key, FIELD_AUTHOR).unwrap();
        assert!(c
            .all_overrides()
            .unwrap()
            .get(&key)
            .unwrap()
            .author
            .is_none());
        // Clearing the provider cache must not touch user overrides.
        let ck = cache_key(KIND_DESCRIPTION, "p", &[1u8; 32], &[2u8; 32]);
        c.put(&ck, KIND_DESCRIPTION, "p", &[2u8; 32], "v").unwrap();
        c.clear().unwrap();
        assert_eq!(c.override_count().unwrap(), 2);
        assert_eq!(
            c.all_overrides()
                .unwrap()
                .get(&key)
                .unwrap()
                .subject
                .as_deref(),
            Some("Sunset")
        );
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
