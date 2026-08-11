use rusqlite::{Connection, params};
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
}

impl Catalog {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA syncronous = NORMAL;",
        )?;

        let catalog = Catalog { conn };
        catalog.migrate()?;

        Ok(catalog)
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        let catalog = Catalog {
            conn: Connection::open_in_memory()?,
        };
        catalog.migrate()?;
        Ok(catalog)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                 path  TEXT PRIMARY KEY,
                 size  INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash  BLOB NOT NULL
             );",
        )
    }

    pub fn lookup(&self, path: &str) -> rusqlite::Result<Option<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT
                        path
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
}
