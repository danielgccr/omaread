//! Reading progress, keyed by file content.
//!
//! Books are identified by SHA-256 so progress survives a rename or a move
//! (CONTEXT.md §4). Schema stays minimal: metadata, covers, highlights, tags and
//! FTS5 arrive with the phases that need them, and nothing is owed compatibility
//! before 1.0.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Recent,
    Title,
    Author,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Recent => Sort::Title,
            Sort::Title => Sort::Author,
            Sort::Author => Sort::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "Recent",
            Sort::Title => "Title",
            Sort::Author => "Author",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BookRow {
    pub hash: String,
    pub path: String,
    pub title: String,
    pub author: String,
    pub cover: Option<Vec<u8>>,
    pub managed: bool,
    pub missing: bool,
    pub started: bool,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open the library database under `$XDG_DATA_HOME/omaread`.
    pub fn open() -> Result<Self, String> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        Self::open_at(dir.join("omaread.db"))
    }

    pub fn open_at(path: impl AsRef<Path>) -> Result<Self, String> {
        let conn = Connection::open(path.as_ref()).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS books (
                 file_hash   TEXT PRIMARY KEY,
                 file_path   TEXT NOT NULL,
                 title       TEXT NOT NULL DEFAULT '',
                 last_cfi    TEXT,
                 opened_at   INTEGER
             );",
        )
        .map_err(|e| e.to_string())?;

        // Nothing is owed compatibility before 1.0, so grow the table in place
        // and let the already-exists errors fall on the floor.
        for col in [
            "author TEXT NOT NULL DEFAULT ''",
            "cover BLOB",
            "managed INTEGER NOT NULL DEFAULT 0",
            "missing INTEGER NOT NULL DEFAULT 0",
            "added_at INTEGER",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE books ADD COLUMN {col}"), []);
        }

        Ok(Self { conn })
    }

    pub fn save_progress(
        &self,
        hash: &str,
        path: &str,
        title: &str,
        cfi: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO books (file_hash, file_path, title, last_cfi, opened_at)
                 VALUES (?1, ?2, ?3, ?4, unixepoch())
                 ON CONFLICT(file_hash) DO UPDATE SET
                     file_path = excluded.file_path,
                     title     = excluded.title,
                     last_cfi  = excluded.last_cfi,
                     opened_at = excluded.opened_at",
                params![hash, path, title, cfi],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Insert or refresh a book's catalogue entry. Reading progress is left
    /// alone: re-indexing a book must never lose your place.
    pub fn upsert_book(&self, b: &BookRow) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO books (file_hash, file_path, title, author, cover, managed, missing, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, unixepoch())
                 ON CONFLICT(file_hash) DO UPDATE SET
                     file_path = excluded.file_path,
                     title     = excluded.title,
                     author    = excluded.author,
                     cover     = coalesce(excluded.cover, books.cover),
                     managed   = max(books.managed, excluded.managed),
                     missing   = 0",
                params![b.hash, b.path, b.title, b.author, b.cover, b.managed as i64],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn has(&self, hash: &str) -> bool {
        self.conn
            .query_row("SELECT 1 FROM books WHERE file_hash = ?1", params![hash], |_| Ok(()))
            .optional()
            .map(|o| o.is_some())
            .unwrap_or(false)
    }

    /// Books, newest-read first unless `sort` says otherwise, filtered by a
    /// case-insensitive substring of title or author.
    pub fn books(&self, query: &str, sort: Sort) -> Result<Vec<BookRow>, String> {
        let order = match sort {
            Sort::Recent => "coalesce(opened_at, added_at) DESC",
            Sort::Title => "title COLLATE NOCASE ASC",
            Sort::Author => "author COLLATE NOCASE ASC, title COLLATE NOCASE ASC",
        };
        let like = format!("%{}%", query.trim());
        let sql = format!(
            "SELECT file_hash, file_path, title, author, cover, managed, missing,
                    last_cfi IS NOT NULL
             FROM books
             WHERE (?1 = '%%' OR title LIKE ?1 ESCAPE '\\' OR author LIKE ?1 ESCAPE '\\')
             ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![like], |r| {
                Ok(BookRow {
                    hash: r.get(0)?,
                    path: r.get(1)?,
                    title: r.get(2)?,
                    author: r.get(3)?,
                    cover: r.get(4)?,
                    managed: r.get::<_, i64>(5)? != 0,
                    missing: r.get::<_, i64>(6)? != 0,
                    started: r.get::<_, i64>(7)? != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    pub fn cover(&self, hash: &str) -> Option<Vec<u8>> {
        self.conn
            .query_row("SELECT cover FROM books WHERE file_hash = ?1", params![hash], |r| {
                r.get::<_, Option<Vec<u8>>>(0)
            })
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    /// Flag rows whose file is gone. Ghost rows keep their reading progress
    /// (CONTEXT.md §4) rather than being deleted.
    pub fn mark_missing(&self) -> Result<usize, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_hash, file_path FROM books WHERE missing = 0")
            .map_err(|e| e.to_string())?;
        let gone: Vec<String> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|(_, p)| !Path::new(p).exists())
            .map(|(h, _)| h)
            .collect();
        for h in &gone {
            let _ = self
                .conn
                .execute("UPDATE books SET missing = 1 WHERE file_hash = ?1", params![h]);
        }
        Ok(gone.len())
    }

    pub fn last_cfi(&self, hash: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT last_cfi FROM books WHERE file_hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(|e| e.to_string())
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omaread")
}

/// Where copied books live. Watched folders are never written to.
pub fn library_dir() -> PathBuf {
    data_dir().join("library")
}

/// SHA-256 of a file, streamed.
pub fn file_hash(path: impl AsRef<Path>) -> Result<String, String> {
    let mut f = std::fs::File::open(path.as_ref()).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_round_trips_and_upserts() {
        let dir = std::env::temp_dir().join(format!("omaread-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(dir.join("t.db")).unwrap();

        assert_eq!(db.last_cfi("h1").unwrap(), None);

        db.save_progress("h1", "/a.epub", "A", "epubcfi(/6/4!/4/2)").unwrap();
        assert_eq!(db.last_cfi("h1").unwrap().as_deref(), Some("epubcfi(/6/4!/4/2)"));

        // Same book, moved and further along.
        db.save_progress("h1", "/moved/a.epub", "A", "epubcfi(/6/8!/4/6)").unwrap();
        assert_eq!(db.last_cfi("h1").unwrap().as_deref(), Some("epubcfi(/6/8!/4/6)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_is_stable_and_content_keyed() {
        let dir = std::env::temp_dir().join(format!("omaread-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"hello").unwrap();
        assert_eq!(file_hash(&a).unwrap(), file_hash(&b).unwrap());
        std::fs::write(&b, b"world").unwrap();
        assert_ne!(file_hash(&a).unwrap(), file_hash(&b).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
