use rusqlite::{Connection, Transaction, params};
use std::path::Path;

pub struct Catalog {
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

    // does the entry's file size and the time-changed match the arguments provided?
    pub fn metadata_matches(&self, size: i64, mtime: i64) -> bool {
        self.size == size && self.mtime == mtime
    }
}

impl Catalog {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;",
        )?;

        let catalog = Catalog { conn };
        catalog.migrate()?;

        Ok(catalog)
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        let catalog = Catalog { conn };
        catalog.migrate()?;

        Ok(catalog)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS contents (
                 hash BLOB PRIMARY KEY,
                 size INTEGER NOT NULL
             );
            
            
            CREATE TABLE IF NOT EXISTS files (
                 path  TEXT PRIMARY KEY,
                 size  INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash  BLOB NOT NULL REFERENCES contents (hash)
             );
             
             CREATE INDEX IF NOT EXISTS files_hash ON files(hash);",
        )
    }

    pub fn transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    // Reads

    pub fn lookup(&self, path: &str) -> rusqlite::Result<Option<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT
                    path,
                    size,
                    mtime,
                    hash
                FROM files 
                WHERE path = ?1",
        )?;
        let mut rows = stmt.query(params![path])?;

        match rows.next()? {
            Some(row) => Ok(Some(Entry::from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn has_blob(&self, hash: &[u8]) -> rusqlite::Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT 1 
            FROM contents
            WHERE hash = ?1",
        )?;

        stmt.exists(params![hash])
    }

    pub fn all(&self) -> rusqlite::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT 
                    path, 
                    size, 
                    mtime, 
                    hash 
                FROM files 
                ORDER BY path",
        )?;
        let rows = stmt.query_map([], |row| Entry::from_row(row))?;
        rows.collect()
    }

    pub fn all_paths(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT
                path
            FROM files
            ORDER BY path",
        )?;

        let rows = stmt.query_map([], |row| row.get(0))?;

        rows.collect()
    }

    // Write

    pub fn insert_contents(&self, hash: &[u8], size: i64) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT OR IGNORE INTO contents (hash, size) VALUES (?1, ?2)")?;

        stmt.execute(params![hash, size])?;

        Ok(())
    }

    pub fn upsert_file(&self, entry: &Entry) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO files (path, size, mtime, hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                 size  = excluded.size,
                 mtime = excluded.mtime,
                 hash  = excluded.hash",
        )?;

        stmt.execute(params![entry.path, entry.size, entry.mtime, entry.hash])?;

        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("DELETE FROM files WHERE path = ?1")?;

        stmt.execute(params![path])?;

        Ok(())
    }

    pub fn unreferenced_blobs(&self) -> rusqlite::Result<Vec<Vec<u8>>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT b.hash
             FROM blobs b
             LEFT JOIN files f ON f.hash = b.hash
             WHERE f.path IS NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;

        rows.collect()
    }
}
