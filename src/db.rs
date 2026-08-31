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

/// One full-text hit: which spine item, and enough words around the match to
/// recognise it in a list.
#[derive(Debug, Clone)]
pub struct Hit {
    pub spine: usize,
    pub snippet: String,
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
            "indexed INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE books ADD COLUMN {col}"), []);
        }

        // One index answers both in-book and whole-library search (§4).
        //
        // ponytail: the text is stored as well as indexed, which is what makes
        // `snippet()` possible and what makes the whole library cost ~400MB for
        // ~360 books. A contentless table (`content=''`) halves that and loses
        // snippets; do it only if the size ever actually bites.
        //
        // `remove_diacritics 2` is not optional for this library: it is mostly
        // Spanish and Catalan, and without it "cancion" does not find "canción"
        // — which is how people type when they are searching.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chapters USING fts5(
                 text,
                 file_hash UNINDEXED,
                 spine UNINDEXED,
                 tokenize = \"unicode61 remove_diacritics 2\"
             );",
        )
        .map_err(|e| format!("create full-text index: {e}"))?;

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
        // Typing in the library searches the shelf *and* the pages: the same
        // index that answers in-book search answers this for free (§4). The
        // clause is only added when there is something to match, because MATCH
        // against nothing is an error rather than an empty result.
        let fts = crate::search::fts_query(query);
        let full_text = match fts {
            Some(_) => " OR file_hash IN (SELECT file_hash FROM chapters WHERE chapters MATCH ?2)",
            None => "",
        };
        let mut args = vec![like];
        args.extend(fts);
        let sql = format!(
            "SELECT file_hash, file_path, title, author, cover, managed, missing,
                    last_cfi IS NOT NULL
             FROM books
             WHERE (?1 = '%%' OR title LIKE ?1 ESCAPE '\\' OR author LIKE ?1 ESCAPE '\\'{full_text})
             ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
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

    /// Replace a book's indexed text. Idempotent, so re-indexing is safe.
    pub fn index_book(&self, hash: &str, chapters: &[(usize, String)]) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM chapters WHERE file_hash = ?1", params![hash])
            .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO chapters (text, file_hash, spine) VALUES (?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for (spine, text) in chapters {
                if text.trim().is_empty() {
                    continue;
                }
                stmt.execute(params![text, hash, *spine as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.execute("UPDATE books SET indexed = 1 WHERE file_hash = ?1", params![hash])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn is_indexed(&self, hash: &str) -> bool {
        self.conn
            .query_row(
                "SELECT indexed FROM books WHERE file_hash = ?1",
                params![hash],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .is_some_and(|n| n != 0)
    }

    /// Hits inside one book, best first. `fts` must already be an FTS5
    /// expression — see `search::fts_query`.
    pub fn search_in_book(&self, hash: &str, fts: &str, limit: usize) -> Vec<Hit> {
        let mut stmt = match self.conn.prepare(
            "SELECT spine, snippet(chapters, 0, '', '', '…', 14)
             FROM chapters
             WHERE chapters MATCH ?1 AND file_hash = ?2
             ORDER BY rank
             LIMIT ?3",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("omaread: search: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![fts, hash, limit as i64], |r| {
            Ok(Hit { spine: r.get::<_, i64>(0)? as usize, snippet: r.get(1)? })
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                eprintln!("omaread: search: {e}");
                Vec::new()
            }
        }
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
    /// FTS5 has to actually be compiled into the bundled SQLite, the index has
    /// to survive re-indexing, and diacritics have to fold — this library is
    /// mostly Spanish.
    #[test]
    fn full_text_search_finds_words_and_ignores_accents() {
        let dir = std::env::temp_dir().join(format!("omaread-fts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = super::Db::open_at(dir.join("t.db")).expect("fts5 must be available");

        db.upsert_book(&super::BookRow {
            hash: "h1".into(),
            path: "/a.epub".into(),
            title: "Un libro".into(),
            ..Default::default()
        })
        .unwrap();

        assert!(!db.is_indexed("h1"));
        db.index_book(
            "h1",
            &[
                (0, "La canción de la resonancia".to_string()),
                (3, "Nada que ver".to_string()),
                (4, "   ".to_string()),
            ],
        )
        .unwrap();
        assert!(db.is_indexed("h1"));

        let q = crate::search::fts_query("cancion").unwrap();
        let hits = db.search_in_book("h1", &q, 10);
        assert_eq!(hits.len(), 1, "accent-insensitive match: {hits:?}");
        assert_eq!(hits[0].spine, 0);
        assert!(hits[0].snippet.contains("canción"), "{:?}", hits[0].snippet);

        // Typing in the library reaches the text, not just the shelf.
        let found = db.books("resonancia", super::Sort::Recent).unwrap();
        assert_eq!(found.len(), 1, "library search should match chapter text");
        // A word in no book matches nothing, and is not a syntax error.
        assert!(db.books("zzzznotaword", super::Sort::Recent).unwrap().is_empty());
        // Punctuation-only input has no tokens to match, so it must skip the
        // MATCH entirely rather than hand FTS5 an empty expression and error.
        assert!(db.books("!!!", super::Sort::Recent).is_ok());

        // Re-indexing replaces rather than duplicates.
        db.index_book("h1", &[(0, "La canción de la resonancia".to_string())]).unwrap();
        assert_eq!(db.search_in_book("h1", &q, 10).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

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
