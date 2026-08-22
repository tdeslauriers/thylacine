use rusqlite::{Connection, Transaction, params};
use std::path::{Path, PathBuf};

/// Two tables, both disposable:
///
/// * `sources` — what each source path looked like last run, so unchanged
///   files can be skipped without reading them.
/// * `archived` — what is currently in the archive, so a file that already
///   exists there (under any name, in any directory) is not copied again.
///
/// Neither holds anything irreplaceable. Delete this database and the next
/// run rebuilds it by re-hashing; the archive itself is untouched. That is
/// why it lives under `$XDG_CACHE_HOME` and not in the archive.
pub struct Cache {
    conn: Connection,
}

pub struct Entry {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub hash: Vec<u8>,
}

impl Entry {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Entry {
            path: row.get("path")?,
            size: row.get("size")?,
            mtime: row.get("mtime")?,
            hash: row.get("hash")?,
        })
    }

    /// Heuristic: matching size and mtime means we may skip re-hashing.
    /// Deliberate archive extraction or `touch` can defeat this, which is what
    /// a future `verify --deep` is for.
    pub fn metadata_matches(&self, size: i64, mtime: i64) -> bool {
        self.size == size && self.mtime == mtime
    }
}

impl Cache {
    /// Open (or create) the cache for one archive.
    ///
    /// Keyed on the archive's stable id rather than its path, so an external
    /// drive mounted at a different point keeps its cache.
    pub fn open(archive_id: &str) -> rusqlite::Result<Self> {
        let dir = cache_root().join(archive_id);
        std::fs::create_dir_all(&dir).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;
        Self::at_path(&dir.join("index.db"), archive_id)
    }

    pub fn at_path(path: &Path, archive_id: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;

        let cache = Cache { conn };
        cache.migrate()?;
        cache.bind_to_archive(archive_id)?;
        Ok(cache)
    }

    pub fn in_memory(archive_id: &str) -> rusqlite::Result<Self> {
        let cache = Cache {
            conn: Connection::open_in_memory()?,
        };
        cache.migrate()?;
        cache.bind_to_archive(archive_id)?;
        Ok(cache)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );

             -- One row per source file, from the last time we looked at it.
             CREATE TABLE IF NOT EXISTS sources (
                 path  TEXT PRIMARY KEY,
                 size  INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash  BLOB NOT NULL
             );

             -- One row per file living in the archive, path relative to its
             -- root. The hash index is what answers 'do I already have this
             -- content somewhere?' regardless of where it was filed.
             CREATE TABLE IF NOT EXISTS archived (
                 path  TEXT PRIMARY KEY,
                 size  INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash  BLOB NOT NULL
             );

             CREATE INDEX IF NOT EXISTS sources_hash  ON sources(hash);
             CREATE INDEX IF NOT EXISTS archived_hash ON archived(hash);",
        )
    }

    /// Record which archive this cache describes, and discard it wholesale if
    /// it turns out to describe a different one. Being wrong here would mean
    /// skipping files the archive does not actually hold.
    fn bind_to_archive(&self, archive_id: &str) -> rusqlite::Result<()> {
        let existing: Option<String> = self
            .conn
            .prepare_cached("SELECT value FROM meta WHERE key = 'repo_id'")?
            .query_row([], |row| row.get(0))
            .ok();

        match existing {
            Some(id) if id == archive_id => Ok(()),
            Some(_) => {
                self.conn.execute_batch("DELETE FROM sources; DELETE FROM archived;")?;
                self.set_archive_id(archive_id)
            }
            None => self.set_archive_id(archive_id),
        }
    }

    fn set_archive_id(&self, archive_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('repo_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![archive_id],
        )?;
        Ok(())
    }

    pub fn transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    pub fn lookup_source(&self, path: &str) -> rusqlite::Result<Option<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, size, mtime, hash FROM sources WHERE path = ?1",
        )?;
        let mut rows = stmt.query(params![path])?;

        match rows.next()? {
            Some(row) => Ok(Some(Entry::from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn upsert_source(&self, entry: &Entry) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO sources (path, size, mtime, hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                 size  = excluded.size,
                 mtime = excluded.mtime,
                 hash  = excluded.hash",
        )?;
        stmt.execute(params![entry.path, entry.size, entry.mtime, entry.hash])?;
        Ok(())
    }

    pub fn remove_source(&self, path: &str) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM sources WHERE path = ?1")?
            .execute(params![path])?;
        Ok(())
    }

    pub fn source_paths(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM sources ORDER BY path")?;
        let paths = stmt.query_map([], |row| row.get(0))?.collect();
        paths
    }

    pub fn archive_id(&self) -> rusqlite::Result<String> {
        self.conn
            .prepare_cached("SELECT value FROM meta WHERE key = 'repo_id'")?
            .query_row([], |row| row.get(0))
    }
    // --- the archive index ---

    /// Where in the archive this content already lives, if anywhere.
    ///
    /// Looked up by hash rather than by path, which is what makes manual
    /// reorganisation safe: rename `pics/2023` to `pics/2023-hawaii` by hand
    /// and the next run still recognises every file in it.
    pub fn find_by_hash(&self, hash: &[u8]) -> rusqlite::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM archived WHERE hash = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![hash])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn record_archived(&self, entry: &Entry) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO archived (path, size, mtime, hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                 size  = excluded.size,
                 mtime = excluded.mtime,
                 hash  = excluded.hash",
        )?;
        stmt.execute(params![entry.path, entry.size, entry.mtime, entry.hash])?;
        Ok(())
    }

    /// Full row for one archived path, for verification and tests.
    pub fn archived_entry(&self, path: &str) -> rusqlite::Result<Option<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, size, mtime, hash FROM archived WHERE path = ?1",
        )?;
        let mut rows = stmt.query(params![path])?;
        match rows.next()? {
            Some(row) => Ok(Some(Entry::from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn archived_count(&self) -> rusqlite::Result<i64> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT count(*) FROM archived")?;
        stmt.query_row([], |row| row.get(0))
    }

    pub fn archived_paths(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM archived ORDER BY path")?;
        let paths = stmt.query_map([], |row| row.get(0))?.collect();
        paths
    }

    /// Drop every archived row, for a full reindex.
    pub fn clear_archived(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM archived;")
    }
}

/// `$XDG_CACHE_HOME/thylacine`, falling back to `$HOME/.cache/thylacine`.
fn cache_root() -> PathBuf {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cache"),
            None => PathBuf::from("/tmp"),
        },
    };
    base.join("thylacine")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCHIVE_A: &str = "aaaa1111";
    const ARCHIVE_B: &str = "bbbb2222";

    fn entry(path: &str, hash: &[u8]) -> Entry {
        Entry {
            path: path.to_string(),
            size: 10,
            mtime: 100,
            hash: hash.to_vec(),
        }
    }

    #[test]
    fn roundtrips_an_entry() {
        let cache = Cache::in_memory(ARCHIVE_A).unwrap();
        cache.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();

        let found = cache.lookup_source("/a.txt").unwrap().unwrap();
        assert_eq!(found.hash, b"deadbeef");
        assert!(found.metadata_matches(10, 100));
        assert!(!found.metadata_matches(10, 101));
    }

    #[test]
    fn finds_archived_content_by_hash_not_path() {
        let cache = Cache::in_memory(ARCHIVE_A).unwrap();
        cache
            .record_archived(&Entry {
                path: "pics/2023/sunset.jpg".into(),
                size: 10,
                mtime: 100,
                hash: b"deadbeef".to_vec(),
            })
            .unwrap();

        // Same content arriving from a different machine under a different
        // name must still be recognised.
        assert_eq!(
            cache.find_by_hash(b"deadbeef").unwrap(),
            Some("pics/2023/sunset.jpg".to_string())
        );
        assert_eq!(cache.find_by_hash(b"unseen").unwrap(), None);
        assert_eq!(cache.archived_count().unwrap(), 1);
    }

    #[test]
    fn reindex_clears_archived_rows() {
        let cache = Cache::in_memory(ARCHIVE_A).unwrap();
        cache.record_archived(&entry("pics/a.jpg", b"deadbeef")).unwrap();
        cache.clear_archived().unwrap();
        assert_eq!(cache.archived_count().unwrap(), 0);
    }

    #[test]
    fn switching_archives_clears_both_tables() {
        let dir = std::env::temp_dir().join(format!("thyl-both-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        let _ = std::fs::remove_file(&path);

        {
            let cache = Cache::at_path(&path, ARCHIVE_A).unwrap();
            cache.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();
            cache.record_archived(&entry("pics/a.jpg", b"deadbeef")).unwrap();
        }

        let cache = Cache::at_path(&path, ARCHIVE_B).unwrap();
        assert!(cache.source_paths().unwrap().is_empty());
        assert_eq!(cache.archived_count().unwrap(), 0);
    }

    #[test]
    fn upsert_replaces_on_conflict() {
        let cache = Cache::in_memory(ARCHIVE_A).unwrap();
        cache.upsert_source(&entry("/a.txt", b"old")).unwrap();

        let mut changed = entry("/a.txt", b"new");
        changed.mtime = 200;
        cache.upsert_source(&changed).unwrap();

        assert_eq!(cache.source_paths().unwrap().len(), 1);
        let found = cache.lookup_source("/a.txt").unwrap().unwrap();
        assert_eq!(found.hash, b"new");
        assert_eq!(found.mtime, 200);
    }

    #[test]
    fn cache_survives_reopen_for_the_same_repository() {
        let dir = std::env::temp_dir().join(format!("thyl-cache-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        let _ = std::fs::remove_file(&path);

        {
            let cache = Cache::at_path(&path, ARCHIVE_A).unwrap();
            cache.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();
        }

        let cache = Cache::at_path(&path, ARCHIVE_A).unwrap();
        assert_eq!(cache.source_paths().unwrap(), vec!["/a.txt"]);
    }

    #[test]
    fn cache_is_discarded_for_a_different_repository() {
        let dir = std::env::temp_dir().join(format!("thyl-cache-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        let _ = std::fs::remove_file(&path);

        {
            let cache = Cache::at_path(&path, ARCHIVE_A).unwrap();
            cache.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();
        }

        // Same file, different repository: trusting these rows would mean
        // skipping files repo B has never seen.
        let cache = Cache::at_path(&path, ARCHIVE_B).unwrap();
        assert!(cache.source_paths().unwrap().is_empty());
        assert_eq!(cache.archive_id().unwrap(), ARCHIVE_B);
    }

    #[test]
    fn two_caches_are_independent() {
        let a = Cache::in_memory(ARCHIVE_A).unwrap();
        let b = Cache::in_memory(ARCHIVE_B).unwrap();

        a.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();

        assert_eq!(a.source_paths().unwrap().len(), 1);
        assert!(b.lookup_source("/a.txt").unwrap().is_none());
    }

    #[test]
    fn removing_a_path_forgets_it() {
        let cache = Cache::in_memory(ARCHIVE_A).unwrap();
        cache.upsert_source(&entry("/a.txt", b"deadbeef")).unwrap();
        cache.remove_source("/a.txt").unwrap();
        assert!(cache.source_paths().unwrap().is_empty());
    }
}