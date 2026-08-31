//! The hermetic resource provider.
//!
//! Omaread never makes a network request. This provider resolves resources
//! *only* from inside the open book's archive. `blitz-net` is deliberately not a
//! dependency, so there is no HTTP client in the binary to regress into.
//!
//! See CONTEXT.md §5.

use crate::book::Book;
use crate::db::Db;
use crate::grid::COVER_ORIGIN;
use blitz_traits::net::{BoxedHandler, Bytes, NetProvider, Request, SharedCallback};
use std::sync::{Arc, Mutex};

/// Synthetic origin for in-archive resources. Chosen so that relative hrefs in a
/// chapter resolve against it via normal URL rules, and so that anything with a
/// real scheme (http, https, data with remote refs) is visibly not ours.
pub const ORIGIN: &str = "omaread-book://book/";

pub struct BookNetProvider<D> {
    book: Book,
    callback: SharedCallback<D>,
}

impl<D> BookNetProvider<D> {
    pub fn new(book: Book, callback: SharedCallback<D>) -> Self {
        Self { book, callback }
    }
}

impl<D: Send + Sync + 'static> NetProvider<D> for BookNetProvider<D> {
    fn fetch(&self, doc_id: usize, request: Request, handler: BoxedHandler<D>) {
        let url = request.url;

        // Anything not on our synthetic origin is off-book: a tracking pixel, a
        // remote stylesheet, a CDN font. Drop it silently and render the
        // placeholder. This is the whole point of the type.
        if url.scheme() != "omaread-book" {
            log_blocked(&url);
            return;
        }

        let Some(href) = in_archive_path(url.path()) else {
            log_blocked(&url);
            return;
        };

        match self.book.read_bytes(&href) {
            Ok(bytes) => handler.bytes(doc_id, Bytes::from(bytes), self.callback.clone()),
            Err(e) => eprintln!("omaread: missing resource {href}: {e}"),
        }
    }
}

/// Serves cover images to the library view straight out of SQLite. Same
/// hermetic rule as the book provider: nothing but our own origin resolves.
// ponytail: covers are served at whatever size the publisher shipped, so blitz
// decodes a full-size JPEG per card and a full grid rebuild costs 1.8s for 361
// books — about 1.5s of it decoding. Downscale at import (and cache the decoded
// RGBA, §4's in-memory LRU) when that stops being only a rebuild cost.
pub struct CoverProvider<D> {
    db: Arc<Mutex<Db>>,
    callback: SharedCallback<D>,
}

impl<D> CoverProvider<D> {
    pub fn new(db: Arc<Mutex<Db>>, callback: SharedCallback<D>) -> Self {
        Self { db, callback }
    }
}

impl<D: Send + Sync + 'static> NetProvider<D> for CoverProvider<D> {
    fn fetch(&self, doc_id: usize, request: Request, handler: BoxedHandler<D>) {
        let url = request.url;
        if url.scheme() != "omaread-cover" {
            log_blocked(&url);
            return;
        }
        let hash = url.path().trim_start_matches('/');
        let bytes = self.db.lock().ok().and_then(|d| d.cover(hash));
        if let Some(bytes) = bytes {
            handler.bytes(doc_id, Bytes::from(bytes), self.callback.clone());
        }
    }
}

const _: () = {
    // Keep the origin constant and the scheme check above in step.
    assert!(COVER_ORIGIN.as_bytes()[0] == b'o');
};

fn log_blocked(url: &blitz_traits::net::Url) {
    eprintln!("omaread: blocked off-book resource {url}");
}

/// Normalise a URL path into an absolute in-archive path, rejecting traversal.
///
/// EPUBs are zip archives from untrusted sources; `../` in an href must never
/// escape the archive root.
///
/// The result keeps its leading slash: rbook treats a slashless path as relative
/// to the OPF directory, which silently doubles the prefix (see `book.rs`).
fn in_archive_path(path: &str) -> Option<String> {
    let decoded = percent_decode(path);
    let mut out: Vec<&str> = Vec::new();

    for segment in decoded.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                // Refuse to climb above the root rather than silently clamping:
                // an href that tries is malformed or hostile, and we want neither.
                out.pop()?;
            }
            s if s.contains('\\') || s.contains('\0') => return None,
            s => out.push(s),
        }
    }

    if out.is_empty() {
        return None;
    }
    Some(format!("/{}", out.join("/")))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::in_archive_path;

    #[test]
    fn resolves_normal_paths() {
        assert_eq!(in_archive_path("/OEBPS/ch01.html").as_deref(), Some("/OEBPS/ch01.html"));
        assert_eq!(in_archive_path("OEBPS/./assets/x.png").as_deref(), Some("/OEBPS/assets/x.png"));
        assert_eq!(in_archive_path("/OEBPS/a/../b.png").as_deref(), Some("/OEBPS/b.png"));
    }

    #[test]
    fn percent_decodes() {
        assert_eq!(in_archive_path("/OEBPS/a%20b.png").as_deref(), Some("/OEBPS/a b.png"));
    }

    #[test]
    fn rejects_traversal_and_junk() {
        assert_eq!(in_archive_path("/../etc/passwd"), None);
        assert_eq!(in_archive_path("/OEBPS/../../etc/passwd"), None);
        assert_eq!(in_archive_path("/%2e%2e/%2e%2e/etc/passwd"), None);
        assert_eq!(in_archive_path("/"), None);
        assert_eq!(in_archive_path("/OEBPS/a\\b.png"), None);
    }
}
