//! Scanning, indexing and importing books.
//!
//! Watched folders are read-only: Omaread never renames or moves a file it does
//! not own (CONTEXT.md §4). Opening a book copies it into the managed library
//! under a canonical name; the original is left alone. Identity is the SHA-256
//! of the file, so the same book under two names is one row.

use crate::db::{BookRow, Db, file_hash, library_dir};
use rbook::Epub;
use std::path::{Path, PathBuf};

/// Folders scanned at startup, one path per line.
///
/// ponytail: a newline list, not TOML. It holds paths and nothing else; add a
/// real config format when there is a second thing to configure.
pub fn folders_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omaread/folders.txt")
}

/// What both writers put at the top of `folders.txt`.
const HEADER: &str = "# Folders Omaread scans. One path per line.\n\
                      # These are read-only: Omaread never renames or moves files here.";

/// Replace the watched folder list. One path per line, as it was read.
pub fn set_folders(dirs: &[PathBuf]) -> Result<(), String> {
    let file = folders_file();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = HEADER
        .lines()
        .map(|l| format!("{l}\n"))
        .chain(dirs.iter().map(|p| format!("{}\n", p.display())))
        .collect();
    std::fs::write(&file, body).map_err(|e| format!("write {}: {e}", file.display()))
}

/// Watched folders, seeded with the obvious ones on first run.
pub fn folders() -> Vec<PathBuf> {
    let file = folders_file();
    if let Ok(text) = std::fs::read_to_string(&file) {
        return text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(expand_home)
            .collect();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let seeded: Vec<PathBuf> = home
        .iter()
        .flat_map(|h| [h.join("Documents"), h.join("Downloads")])
        .filter(|p| p.is_dir())
        .collect();

    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = seeded
        .iter()
        .map(|p| format!("{}\n", p.display()))
        .collect();
    let _ = std::fs::write(&file, format!("{HEADER}\n{body}"));
    seeded
}

pub fn expand_home(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(s)),
        None => PathBuf::from(s),
    }
}

/// Every `.epub` under `dir`, recursively.
///
/// ponytail: `std::fs` recursion rather than a walkdir dependency. Symlinked
/// directories are not followed, which also stops a cycle wedging the scan.
pub fn epubs_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            epubs_under(&path, out);
        } else if is_epub(&path) {
            out.push(path);
        }
    }
}

pub fn is_epub(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
}

/// Widest a stored cover needs to be: twice the 225px card, so a HiDPI screen
/// still has pixels to spare.
///
/// Publishers ship 2100×3000 — 25MB of RGBA that blitz decodes to paint a
/// thumbnail. Shrinking on the way in took the library's covers from 97MB to a
/// tenth of that on disk, and the decoded page from ~240MB to ~35MB.
pub const COVER_MAX_W: u32 = 450;

/// Shrink a cover to `COVER_MAX_W`, or hand it back untouched.
///
/// ponytail: re-encoded as JPEG at quality 82, so a cover with transparency is
/// flattened onto white. Covers are photographs of paper; keep PNG for PNG if a
/// real one ever looks wrong.
pub fn shrink_cover(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(img) = image::load_from_memory(&bytes) else { return bytes };
    if img.width() <= COVER_MAX_W {
        return bytes;
    }
    let small = img.resize(COVER_MAX_W, u32::MAX, image::imageops::FilterType::Triangle);
    let mut out = std::io::Cursor::new(Vec::new());
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
    match enc.encode_image(&image::DynamicImage::ImageRgb8(small.to_rgb8())) {
        Ok(()) => out.into_inner(),
        Err(_) => bytes,
    }
}

/// Read title, author and cover out of a book.
pub fn describe(path: &Path, hash: String) -> Result<BookRow, String> {
    let epub = Epub::open(path).map_err(|e| format!("open: {e}"))?;

    let title = epub
        .metadata()
        .title()
        .map(|t| t.value().to_string())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into())
        });

    let author = epub
        .metadata()
        .creators()
        .next()
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    let cover = epub
        .manifest()
        .cover_image()
        .and_then(|entry| epub.read_resource_bytes(entry.href()).ok())
        .map(shrink_cover);

    Ok(BookRow {
        hash,
        path: path.to_string_lossy().into_owned(),
        title,
        author,
        has_cover: cover.is_some(),
        cover,
        managed: path.starts_with(library_dir()),
        missing: false,
        started: false,
        progress: 0.0,
        tags: Vec::new(),
    })
}

/// Index every book in the watched folders and the managed library.
///
/// Returns (seen, newly indexed). Books already known by hash are skipped
/// without re-parsing, which is what keeps a rescan cheap.
pub fn scan(db: &Db) -> (usize, usize) {
    let mut paths = Vec::new();
    for dir in folders() {
        epubs_under(&dir, &mut paths);
    }
    epubs_under(&library_dir(), &mut paths);

    let mut added = 0;
    for path in &paths {
        let Ok(hash) = file_hash(path) else { continue };
        if db.has(&hash) {
            continue;
        }
        match describe(path, hash) {
            Ok(row) => {
                if db.upsert_book(&row).is_ok() {
                    added += 1;
                }
            }
            Err(e) => eprintln!("omaread: skipping {}: {e}", path.display()),
        }
    }
    let _ = db.mark_missing();
    (paths.len(), added)
}

/// Copy a book into the managed library under a canonical name and index it.
///
/// Returns the path actually opened: the managed copy, or the original if it is
/// already known or the copy fails.
/// Extract and index the full text of one book. Idempotent.
///
/// The text is pulled from the raw XHTML, not from a laid-out document: a full
/// Stylo/Taffy/Parley pass per chapter would make indexing cost more than
/// reading the book (CONTEXT.md §4).
pub fn index_book(db: &Db, hash: &str, book: &crate::book::Book) -> Result<usize, String> {
    let chapters: Vec<(usize, String)> = (0..book.chapter_count())
        .filter_map(|i| book.chapter_html(i).ok().map(|h| (i, crate::search::text_of_html(&h))))
        .collect();
    let indexed = chapters.iter().filter(|(_, t)| !t.trim().is_empty()).count();
    db.index_book(hash, &chapters).map(|()| indexed)
}

/// Index every catalogued book that has no text yet. This is the backfill for
/// books that were scanned but never opened; opening a book indexes it anyway.
pub fn index_all(db: &Db) -> (usize, usize) {
    let rows = db.books("", crate::db::Sort::Title).unwrap_or_default();
    let mut done = 0;
    let mut failed = 0;
    for row in rows.iter().filter(|r| !r.missing && !db.is_indexed(&r.hash)) {
        match crate::book::Book::open(&row.path)
            .and_then(|b| index_book(db, &row.hash, &b))
        {
            Ok(n) => {
                done += 1;
                println!("indexed {n} chapters — {}", row.title);
            }
            Err(e) => {
                failed += 1;
                eprintln!("FAIL index {}: {e}", row.path);
            }
        }
    }
    (done, failed)
}

pub fn import(db: &Db, path: &Path) -> PathBuf {
    let Ok(hash) = file_hash(path) else { return path.to_path_buf() };

    if db.has(&hash) {
        return path.to_path_buf();
    }
    let Ok(row) = describe(path, hash) else { return path.to_path_buf() };

    let dir = library_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        let _ = db.upsert_book(&row);
        return path.to_path_buf();
    }

    let dest = dir.join(canonical_name(&row.author, &row.title));
    if dest != path && !dest.exists() && std::fs::copy(path, &dest).is_err() {
        let _ = db.upsert_book(&row);
        return path.to_path_buf();
    }

    let mut row = row;
    row.path = dest.to_string_lossy().into_owned();
    row.managed = true;
    let _ = db.upsert_book(&row);
    dest
}

/// `Author - Title.epub`, with anything a filesystem dislikes removed.
pub fn canonical_name(author: &str, title: &str) -> String {
    let author = sanitize(author);
    let title = sanitize(title);
    let stem = match (author.is_empty(), title.is_empty()) {
        (_, true) => "Untitled".to_string(),
        (true, false) => title,
        (false, false) => format!("{author} - {title}"),
    };
    // Leave room for the extension inside common filename limits.
    let stem: String = stem.chars().take(180).collect();
    format!("{}.epub", stem.trim_end_matches([' ', '.']))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_are_filesystem_safe() {
        assert_eq!(canonical_name("Postman, Neil", "Divertirse hasta morir"),
                   "Postman, Neil - Divertirse hasta morir.epub");
        assert_eq!(canonical_name("", "Solo"), "Solo.epub");
        assert_eq!(canonical_name("A", ""), "Untitled.epub");
        // Separators and control characters must never survive.
        let n = canonical_name("a/b\\c", "d:e*f?\"g<h>i|j");
        assert!(!n.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']), "{n}");
        // Runs of whitespace collapse rather than leaving a ragged name.
        assert_eq!(canonical_name("  A   B ", " T   U "), "A B - T U.epub");
    }

    #[test]
    fn long_names_stay_within_filesystem_limits() {
        let n = canonical_name(&"a".repeat(300), &"b".repeat(300));
        assert!(n.len() <= 190, "name too long: {}", n.len());
        assert!(n.ends_with(".epub"));
    }

    #[test]
    fn only_epubs_are_picked_up() {
        assert!(is_epub(Path::new("/x/a.epub")));
        assert!(is_epub(Path::new("/x/a.EPUB")));
        assert!(!is_epub(Path::new("/x/a.pdf")));
        assert!(!is_epub(Path::new("/x/epub")));
    }

    #[test]
    fn scan_finds_nested_books_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("omaread-scan-{}", std::process::id()));
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("one.epub"), b"x").unwrap();
        std::fs::write(nested.join("two.EPUB"), b"x").unwrap();
        std::fs::write(nested.join("skip.pdf"), b"x").unwrap();

        let mut found = Vec::new();
        epubs_under(&dir, &mut found);
        assert_eq!(found.len(), 2, "{found:?}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
