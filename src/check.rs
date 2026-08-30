//! Headless conformance run.
//!
//! No public releases before 1.0 means no stranger's broken EPUB will surface a
//! bug for a long time (CONTEXT.md §13). This is the substitute: open every
//! chapter of every book given, lay it out, paginate it, and assert the
//! invariants that matter. Intended for CI and for sweeping the local library.

use crate::book::Book;
use crate::chapter;
use crate::paginate::Atom;
use crate::style::{PAGE_MARGIN_EM, ReadingStyle};
use blitz_dom::net::Resource;
use blitz_traits::net::{NetCallback, SharedCallback};
use std::sync::Arc;

/// A page width and height that a real window would plausibly have.
const W: u32 = 760;
const H: u32 = 1000;

struct Discard;
impl NetCallback<Resource> for Discard {
    fn call(&self, _doc_id: usize, _result: Result<Resource, Option<String>>) {}
}

#[derive(Default)]
struct Tally {
    books: usize,
    chapters: usize,
    pages: usize,
    empty: usize,
    panicked: usize,
    cutting: usize,
    stalled: usize,
    failed_books: Vec<String>,
}

pub fn run(paths: &[String]) -> i32 {
    if paths.is_empty() {
        eprintln!("usage: omaread --check <book.epub>...");
        return 2;
    }

    let style = ReadingStyle::default();
    let margin = PAGE_MARGIN_EM * style.font_px();
    let page_h = (H as f32 - 2.0 * margin).max(1.0);
    let make_viewport = || chapter::viewport(W, H, 1.0, false);

    let mut t = Tally::default();

    for path in paths {
        let book = match Book::open(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("FAIL open {path}: {e}");
                t.failed_books.push(path.clone());
                continue;
            }
        };
        t.books += 1;

        for i in 0..book.chapter_count() {
            let cb: SharedCallback<Resource> = Arc::new(Discard);
            let ch = match chapter::load(&book, i, &style, make_viewport(), page_h, cb) {
                Ok(c) => c,
                Err(_) => {
                    t.panicked += 1;
                    eprintln!("PANIC {path} chapter {}", i + 1);
                    continue;
                }
            };
            t.chapters += 1;
            t.pages += ch.pages.count();

            if ch.text_len() == 0 && ch.content_height() < 1.0 {
                t.empty += 1;
            }

            let atoms = chapter::collect_atoms(ch.dom());
            for (n, &top) in ch.pages.tops.iter().enumerate() {
                if let Some(a) = cuts(&atoms, top) {
                    t.cutting += 1;
                    eprintln!(
                        "CUT  {path} ch{} page {n} at {top:.1} through {:?} {:.1}..{:.1}",
                        i + 1,
                        a.kind,
                        a.top,
                        a.bottom
                    );
                    // What else covers this offset? A break is impossible inside
                    // an atom taller than a page, so name the culprit.
                    for o in atoms.iter().filter(|o| o.splits(top)) {
                        eprintln!(
                            "       covered by {:?} {:.1}..{:.1} (height {:.1}, page {:.1})",
                            o.kind, o.top, o.bottom, o.bottom - o.top, page_h
                        );
                    }
                }
            }
            for w in ch.pages.tops.windows(2) {
                if w[1] <= w[0] {
                    t.stalled += 1;
                    eprintln!("STALL {path} ch{} pages did not advance", i + 1);
                    break;
                }
            }
        }
    }

    println!(
        "checked {} books, {} chapters, {} pages | empty {} | engine panics {} | breaks cutting an atom {} | stalls {}",
        t.books, t.chapters, t.pages, t.empty, t.panicked, t.cutting, t.stalled
    );

    let bad = t.cutting + t.stalled + t.failed_books.len();
    if bad > 0 { 1 } else { 0 }
}

fn cuts(atoms: &[Atom], y: f32) -> Option<&Atom> {
    // Same predicate the paginator uses, so the check and the invariant agree.
    atoms.iter().find(|a| a.splits(y))
}
