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
    /// The image itself. Only filled on the way *in* — a listing carries
    /// `has_cover` instead, because 358 JPEGs is 97MB of RAM to decide which
    /// cards get an `<img>`.
    pub cover: Option<Vec<u8>>,
    pub has_cover: bool,
    pub managed: bool,
    pub missing: bool,
    pub started: bool,
    pub tags: Vec<String>,
}

/// A bookmark or a highlight. Both anchor to a CFI, so both live in one table:
/// a bookmark is a highlight with no span.
#[derive(Debug, Clone, Default)]
pub struct Mark {
    pub id: i64,
    /// CFI of the anchor element, with a character offset for a highlight.
    pub cfi: String,
    /// Characters covered. 0 means a bookmark.
    pub length: usize,
    /// The highlighted words, kept so the list reads without opening the book.
    pub text: String,
    pub note: String,
}

impl Mark {
    pub fn is_bookmark(&self) -> bool {
        self.length == 0
    }
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

        // Measured page counts per chapter, per layout. Pagination depends on
        // font size and column width, so the layout is part of the key.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pagination (
                 file_hash TEXT NOT NULL,
                 layout    TEXT NOT NULL,
                 spine     INTEGER NOT NULL,
                 pages     INTEGER NOT NULL,
                 UNIQUE(file_hash, layout, spine)
             );",
        )
        .map_err(|e| format!("create pagination: {e}"))?;

        // A collection and a tag are the same thing — a named group of books —
        // so there is one table and one filter, not two of each (§7 lists both).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tags (
                 file_hash TEXT NOT NULL,
                 tag       TEXT NOT NULL,
                 UNIQUE(file_hash, tag)
             );
             CREATE INDEX IF NOT EXISTS tags_by_tag ON tags (tag);",
        )
        .map_err(|e| format!("create tags: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS marks (
                 id        INTEGER PRIMARY KEY,
                 file_hash TEXT NOT NULL,
                 cfi       TEXT NOT NULL,
                 length    INTEGER NOT NULL DEFAULT 0,
                 text      TEXT NOT NULL DEFAULT '',
                 note      TEXT NOT NULL DEFAULT '',
                 made_at   INTEGER,
                 UNIQUE(file_hash, cfi, length)
             );
             CREATE INDEX IF NOT EXISTS marks_by_book ON marks (file_hash);",
        )
        .map_err(|e| format!("create marks: {e}"))?;

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
        // `#tag` filters by tag rather than searching. One prefix, no new view.
        if let Some(tag) = query.trim().strip_prefix('#') {
            return self.books_tagged(&normalise_tag(tag), order);
        }
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
            "SELECT file_hash, file_path, title, author, cover IS NOT NULL, managed, missing,
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
                    cover: None,
                    has_cover: r.get::<_, i64>(4)? != 0,
                    managed: r.get::<_, i64>(5)? != 0,
                    missing: r.get::<_, i64>(6)? != 0,
                    started: r.get::<_, i64>(7)? != 0,
                    tags: Vec::new(),
                })
            })
            .map_err(|e| e.to_string())?;
        let mut books = rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        self.attach_tags(&mut books);
        Ok(books)
    }

    fn books_tagged(&self, prefix: &str, order: &str) -> Result<Vec<BookRow>, String> {
        let sql = format!(
            "SELECT file_hash, file_path, title, author, cover IS NOT NULL, managed, missing,
                    last_cfi IS NOT NULL
             FROM books
             WHERE file_hash IN (SELECT file_hash FROM tags WHERE tag LIKE ?1)
             ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![format!("{prefix}%")], |r| {
                Ok(BookRow {
                    hash: r.get(0)?,
                    path: r.get(1)?,
                    title: r.get(2)?,
                    author: r.get(3)?,
                    cover: None,
                    has_cover: r.get::<_, i64>(4)? != 0,
                    managed: r.get::<_, i64>(5)? != 0,
                    missing: r.get::<_, i64>(6)? != 0,
                    started: r.get::<_, i64>(7)? != 0,
                    tags: Vec::new(),
                })
            })
            .map_err(|e| e.to_string())?;
        let mut books = rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        self.attach_tags(&mut books);
        Ok(books)
    }

    /// Fill in every row's tags with one query, not one per book.
    fn attach_tags(&self, books: &mut [BookRow]) {
        if books.is_empty() {
            return;
        }
        let Ok(mut stmt) = self.conn.prepare("SELECT file_hash, tag FROM tags ORDER BY tag")
        else {
            return;
        };
        let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return;
        };
        let mut by_hash: std::collections::HashMap<String, Vec<String>> = Default::default();
        for (hash, tag) in rows.filter_map(Result::ok) {
            by_hash.entry(hash).or_default().push(tag);
        }
        for b in books {
            if let Some(tags) = by_hash.remove(&b.hash) {
                b.tags = tags;
            }
        }
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

    /// Add a bookmark or highlight. Re-marking the same span is a no-op rather
    /// than a duplicate, which is what makes `b` a toggle worth having.
    pub fn add_mark(&self, hash: &str, m: &Mark) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO marks (file_hash, cfi, length, text, note, made_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
                 ON CONFLICT(file_hash, cfi, length) DO NOTHING",
                params![hash, m.cfi, m.length as i64, m.text, m.note],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Marks for one book, in reading order. CFIs sort correctly as text only by
    /// accident, so order by the numbers that matter: spine, then offset.
    pub fn marks(&self, hash: &str) -> Vec<Mark> {
        let mut stmt = match self
            .conn
            .prepare("SELECT id, cfi, length, text, note FROM marks WHERE file_hash = ?1")
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("omaread: marks: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![hash], |r| {
            Ok(Mark {
                id: r.get(0)?,
                cfi: r.get(1)?,
                length: r.get::<_, i64>(2)? as usize,
                text: r.get(3)?,
                note: r.get(4)?,
            })
        });
        let mut out: Vec<Mark> = match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                eprintln!("omaread: marks: {e}");
                return Vec::new();
            }
        };
        out.sort_by_key(|m| {
            crate::cfi::Cfi::parse(&m.cfi)
                .map(|c| (c.spine, c.steps.clone(), c.offset.unwrap_or(0)))
                .unwrap_or((usize::MAX, Vec::new(), 0))
        });
        out
    }

    /// Marks inside one spine item, for painting the page.
    pub fn marks_in(&self, hash: &str, spine: usize) -> Vec<Mark> {
        self.marks(hash)
            .into_iter()
            .filter(|m| {
                crate::cfi::Cfi::parse(&m.cfi).is_some_and(|c| c.spine == spine && m.length > 0)
            })
            .collect()
    }

    pub fn remove_mark(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM marks WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Find a bookmark already at this exact anchor, so `b` can toggle it.
    pub fn bookmark_at(&self, hash: &str, cfi: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT id FROM marks WHERE file_hash = ?1 AND cfi = ?2 AND length = 0",
                params![hash, cfi],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_note(&self, id: i64, note: &str) -> Result<(), String> {
        self.conn
            .execute("UPDATE marks SET note = ?2 WHERE id = ?1", params![id, note])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Page counts for every chapter of a book at one layout, in spine order.
    /// `None` when the book has not been measured at this layout.
    pub fn pagination(&self, hash: &str, layout: &str, chapters: usize) -> Option<Vec<usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spine, pages FROM pagination WHERE file_hash = ?1 AND layout = ?2")
            .ok()?;
        let rows = stmt
            .query_map(params![hash, layout], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })
            .ok()?;

        let mut out = vec![0usize; chapters];
        let mut seen = 0;
        for (spine, pages) in rows.filter_map(Result::ok) {
            if let Some(slot) = out.get_mut(spine) {
                *slot = pages;
                seen += 1;
            }
        }
        // A partial measurement is not a page count; recompute rather than lie.
        (seen == chapters && chapters > 0).then_some(out)
    }

    pub fn save_pagination(&self, hash: &str, layout: &str, pages: &[usize]) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO pagination (file_hash, layout, spine, pages)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(file_hash, layout, spine) DO UPDATE SET pages = excluded.pages",
                )
                .map_err(|e| e.to_string())?;
            for (spine, n) in pages.iter().enumerate() {
                stmt.execute(params![hash, layout, spine as i64, *n as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Add the tag if the book lacks it, remove it if it has it. Returns
    /// whether it is now on the book.
    ///
    /// One gesture for both directions: a separate "remove" would need its own
    /// key and its own way to name the tag, and toggling reads the same way to a
    /// person.
    pub fn toggle_tag(&self, hash: &str, tag: &str) -> Result<bool, String> {
        let tag = normalise_tag(tag);
        if tag.is_empty() {
            return Ok(false);
        }
        let removed = self
            .conn
            .execute("DELETE FROM tags WHERE file_hash = ?1 AND tag = ?2", params![hash, tag])
            .map_err(|e| e.to_string())?;
        if removed > 0 {
            return Ok(false);
        }
        self.conn
            .execute("INSERT INTO tags (file_hash, tag) VALUES (?1, ?2)", params![hash, tag])
            .map(|_| true)
            .map_err(|e| e.to_string())
    }

    /// Every tag in use with the prefix, and how many books carry it.
    pub fn all_tags(&self, prefix: &str) -> Vec<(String, usize)> {
        let like = format!("{}%", normalise_tag(prefix));
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT tag, count(*) FROM tags WHERE tag LIKE ?1
             GROUP BY tag ORDER BY count(*) DESC, tag ASC LIMIT 12",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![like], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        });
        rows.map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    /// Completions for what has been typed so far, as `(text, hint)`.
    ///
    /// Books, not words. The box searches the shelf, so what it should offer is
    /// something you can land on: a title, hinted with its author, then the
    /// authors themselves. It used to offer the index's term list — "albarino,
    /// 8 chapters" — which answers a question nobody asked of a library view.
    ///
    /// Matched raw, not folded: `books()` filters with the same `LIKE`, so a
    /// suggestion is picked precisely because clicking it finds that book.
    pub fn suggestions(&self, prefix: &str, limit: usize) -> Vec<(String, String)> {
        let prefix = prefix.trim();
        if prefix.chars().count() < 2 {
            return Vec::new();
        }
        let like = format!("%{prefix}%");
        let mut out: Vec<(String, String)> = Vec::new();

        // One row per title, the most recently touched copy of it first.
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT title, author, max(coalesce(opened_at, added_at)) FROM books
             WHERE missing = 0 AND title LIKE ?1 ESCAPE '\\'
             GROUP BY title COLLATE NOCASE
             ORDER BY 3 DESC
             LIMIT ?2",
        ) {
            if let Ok(rows) = stmt.query_map(params![like, limit as i64], |r| {
                let title: String = r.get(0)?;
                let author: String = r.get(1)?;
                let hint = match author.is_empty() {
                    true => "book".to_string(),
                    false => author,
                };
                Ok((title, hint))
            }) {
                out.extend(rows.filter_map(Result::ok));
            }
        }

        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT DISTINCT author FROM books
             WHERE author <> '' AND missing = 0 AND author LIKE ?1 ESCAPE '\\'
             ORDER BY author COLLATE NOCASE
             LIMIT ?2",
        ) {
            if let Ok(rows) = stmt.query_map(params![like, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, "author".to_string()))
            }) {
                out.extend(rows.filter_map(Result::ok));
            }
        }

        out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
        out.truncate(limit);
        out
    }

    /// Replace a stored cover — used once per book, when the full-size image the
    /// publisher shipped is shrunk to what a card can show.
    pub fn set_cover(&self, hash: &str, cover: &[u8]) -> Result<(), String> {
        self.conn
            .execute("UPDATE books SET cover = ?2 WHERE file_hash = ?1", params![hash, cover])
            .map(|_| ())
            .map_err(|e| e.to_string())
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

/// Tags are stored folded and hyphenated: someone typing "Scifi" and "scifi"
/// means one group, an accent should not split one either, and a tag with a
/// space in it would be unsearchable behind a `#` prefix.
fn normalise_tag(tag: &str) -> String {
    let folded = crate::search::fold(tag.trim().trim_start_matches('#'));
    folded.split_whitespace().collect::<Vec<_>>().join("-")
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
    /// The box searches the shelf, so the completions have to be things on the
    /// shelf: a title once however many copies of it there are, then authors.
    #[test]
    fn suggestions_complete_titles_and_authors() {
        let dir = std::env::temp_dir().join(format!("omaread-sug-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = super::Db::open_at(dir.join("s.db")).unwrap();

        for (hash, title, author) in [
            ("h1", "Bodas de sangre", "Federico García Lorca"),
            ("h2", "Bodas de sangre", "Federico García Lorca"), // the same book twice
            ("h3", "Ríete de las bodas", "Megan Maxwell"),
            ("h4", "Otra cosa", "Alba Cardalda"),
        ] {
            db.upsert_book(&super::BookRow {
                hash: hash.into(),
                path: format!("/{hash}.epub"),
                title: title.into(),
                author: author.into(),
                ..Default::default()
            })
            .unwrap();
        }

        let sugs = db.suggestions("bod", 6);
        // Recency orders them, and these were all added in the same second.
        let mut titles: Vec<&str> = sugs.iter().map(|(t, _)| t.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["Bodas de sangre", "Ríete de las bodas"], "{sugs:?}");
        let bodas = sugs.iter().find(|(t, _)| t == "Bodas de sangre").unwrap();
        assert_eq!(bodas.1, "Federico García Lorca", "a title is hinted with its author");

        // An author is a completion too, and hinted as one.
        let sugs = db.suggestions("alba", 6);
        assert_eq!(sugs, vec![("Alba Cardalda".to_string(), "author".to_string())]);

        // Whatever comes back must actually find something when it is searched.
        for (text, _) in db.suggestions("bod", 6) {
            assert!(!db.books(&text, super::Sort::Title).unwrap().is_empty(), "{text} finds nothing");
        }

        assert!(db.suggestions("b", 6).is_empty(), "one letter suggests everything");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tag is a collection: one table, toggled by name, folded so that
    /// "Sci Fi", "sci-fi" and "SCI-FI" are one group rather than three.
    #[test]
    fn tags_toggle_normalise_and_filter() {
        let dir = std::env::temp_dir().join(format!("omaread-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = super::Db::open_at(dir.join("t.db")).unwrap();

        for (hash, title) in [("h1", "Un libro"), ("h2", "Otro")] {
            db.upsert_book(&super::BookRow {
                hash: hash.into(),
                path: format!("/{hash}.epub"),
                title: title.into(),
                ..Default::default()
            })
            .unwrap();
        }

        assert!(db.toggle_tag("h1", "Sci Fi").unwrap(), "added");
        assert!(db.toggle_tag("h2", "#sci-fi").unwrap(), "same tag, other spelling");
        assert!(db.toggle_tag("h1", "Ciencia-Ficción").unwrap());

        // Both spellings landed on one tag, so both books carry it.
        let tagged = db.books("#SCI-FI", super::Sort::Title).unwrap();
        assert_eq!(tagged.len(), 2, "{:?}", tagged.iter().map(|b| &b.title).collect::<Vec<_>>());

        // Accents fold, so the tag is findable as typed without them.
        assert_eq!(db.books("#ciencia-ficcion", super::Sort::Title).unwrap().len(), 1);

        // Rows carry their tags, in one query rather than one per book.
        let rows = db.books("", super::Sort::Title).unwrap();
        let h1 = rows.iter().find(|b| b.hash == "h1").unwrap();
        assert_eq!(h1.tags, vec!["ciencia-ficcion", "sci-fi"]);

        let all = db.all_tags("");
        assert_eq!(all[0], ("sci-fi".to_string(), 2), "commonest first: {all:?}");
        assert_eq!(db.all_tags("sci").len(), 1);

        // Toggling again removes it.
        assert!(!db.toggle_tag("h1", "sci-fi").unwrap(), "removed");
        assert_eq!(db.books("#sci-fi", super::Sort::Title).unwrap().len(), 1);
        // An empty tag is not a tag.
        assert!(!db.toggle_tag("h1", "  #  ").unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bookmarks toggle, highlights carry a span, notes attach, and the list
    /// comes back in reading order rather than the order things were made.
    #[test]
    fn marks_toggle_sort_and_carry_notes() {
        let dir = std::env::temp_dir().join(format!("omaread-marks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = super::Db::open_at(dir.join("m.db")).unwrap();

        let later = "epubcfi(/6/8!/4/2/6)";      // spine 3
        let earlier = "epubcfi(/6/4!/4/2/2:120)"; // spine 1
        db.add_mark("h", &super::Mark { cfi: later.into(), ..Default::default() }).unwrap();
        db.add_mark(
            "h",
            &super::Mark {
                cfi: earlier.into(),
                length: 9,
                text: "resonancia".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let marks = db.marks("h");
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].cfi, earlier, "reading order, not insertion order");
        assert!(marks[0].length > 0 && !marks[0].is_bookmark());
        assert!(marks[1].is_bookmark());

        // Marking the same anchor twice must not duplicate — that is the toggle.
        db.add_mark("h", &super::Mark { cfi: later.into(), ..Default::default() }).unwrap();
        assert_eq!(db.marks("h").len(), 2);
        assert_eq!(db.bookmark_at("h", later), Some(marks[1].id));
        // A highlight is not a bookmark, so it must not answer the toggle.
        assert_eq!(db.bookmark_at("h", earlier), None);

        db.set_note(marks[0].id, "the whole argument").unwrap();
        assert_eq!(db.marks("h")[0].note, "the whole argument");

        // Only highlights in the asked-for chapter get painted.
        assert_eq!(db.marks_in("h", 1).len(), 1);
        assert!(db.marks_in("h", 3).is_empty(), "a bookmark has nothing to paint");

        db.remove_mark(marks[0].id).unwrap();
        assert_eq!(db.marks("h").len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

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
