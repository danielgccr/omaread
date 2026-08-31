//! Omaread — a proper EPUB reader for Omarchy.
//!
//! Phase 1: one chapter on screen. See CONTEXT.md for the design record.

mod book;
mod chapter;
mod net;
mod cfi;
mod check;
mod db;
mod grid;
mod hud;
mod hyphen;
mod library;
mod paginate;
mod paint;
mod search;
mod shot;
mod style;
mod toc;

use anyrender::WindowRenderer;
use anyrender_vello::VelloWindowRenderer;
use peniko::Color;
use blitz_dom::net::Resource;
use blitz_traits::net::{NetCallback, SharedCallback};
use book::Book;
use db::{BookRow, Db, Sort};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use chapter::Chapter;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use style::{Chrome, GUTTER_EM, MEASURE_EM, PAGE_MARGIN_EM, ReadingStyle, Theme};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// How long the reading HUD lingers after the pointer stops moving.
const HUD_LINGER: Duration = Duration::from_millis(2200);

/// Below this window width, a second column would squeeze the measure into
/// something unreadable, so two-column mode silently falls back to one.
const TWO_COLUMN_MIN_EM: f32 = 2.0 * (MEASURE_EM + 2.0 * GUTTER_EM);

/// `#rrggbb` to a colour. The chrome palette is authored as CSS strings so the
/// stylesheet and the window ground cannot drift apart.
fn parse_hex(s: &str) -> Color {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0);
    Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--check") {
        std::process::exit(check::run(&args[1..]));
    }

    if args.first().map(String::as_str) == Some("--index") {
        let code = match db::Db::open() {
            Ok(db) => {
                let (done, failed) = library::index_all(&db);
                println!("indexed {done} books, {failed} failed");
                (failed > 0) as i32
            }
            Err(e) => {
                eprintln!("omaread: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    if args.first().map(String::as_str) == Some("--shot") {
        std::process::exit(shot::run(&args[1..]));
    }

    if matches!(args.first().map(String::as_str), Some("-h" | "--help")) {
        println!(
            "omaread [book.epub] [chapter]   open the library, or a book\n\
             omaread --check <book.epub>...  headless conformance run\n\
             omaread --shot <book.epub> <chapter> <page> <out.ppm> [hud]\n\
             omaread --index                 index every library book for search\n\n\
             Library:  type to search · Enter open · Tab sort · F5 rescan · Esc clear/quit\n\
             Reading:  ←/→ page · ↑/↓ chapter · Tab contents · / search · t theme · +/- size · l library · q quit\n\
                       move the mouse for the title and how far through you are\n\
             Contents: ↑/↓ select · Enter go · Tab or Esc back"
        );
        return;
    }

    // Dev convenience: jump straight to a spine item.
    let start = args
        .get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.saturating_sub(1))
        .unwrap_or(0);

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(args.first().cloned(), start);
    event_loop.run_app(&mut app).expect("run event loop");
}

/// Resources arrive from the provider on whatever thread fetched them; funnel
/// them back to the event loop rather than touching the document directly.
struct ChannelCallback(Sender<Resource>);

impl NetCallback<Resource> for ChannelCallback {
    fn call(&self, _doc_id: usize, result: Result<Resource, Option<String>>) {
        if let Ok(resource) = result {
            let _ = self.0.send(resource);
        }
    }
}

/// Which surface is on screen. Every arm is a laid-out document — the library
/// and the contents are HTML/CSS through the same pipeline as a book
/// (CONTEXT.md §2); only where the markup comes from differs.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Library,
    Reading,
    Toc,
}

struct App {
    view: View,
    rows: Vec<BookRow>,
    query: String,
    sort: Sort,
    selected: usize,
    /// Rebuilt whenever the query, sort or selection changes.
    lib_doc: Option<Chapter>,
    /// The contents list. Rebuilt on every open and on every selection move.
    toc_doc: Option<Chapter>,
    /// Title and progress, painted over the page while the pointer is active.
    hud_doc: Option<Chapter>,
    /// What `hud_doc` was built from; it is rebuilt only when this changes,
    /// because pointer motion arrives far faster than the text does.
    hud_key: String,
    hud_shown: bool,
    /// In-book search: the query, and whether the contents view is showing hits
    /// instead of navigation. Both lists are "places in this book", so they are
    /// the same view.
    find: String,
    finding: bool,
    /// When to put the HUD away. Drives `ControlFlow::WaitUntil`.
    hud_until: Option<Instant>,
    toc_sel: usize,
    /// The reading page to come back to when the contents close. All three
    /// views share `page`, so it has to be put somewhere.
    resume_page: usize,
    book: Option<Book>,
    db: Option<Arc<Mutex<Db>>>,
    hash: String,
    path: String,
    /// Restored on startup, consumed once the chapter it points at is open.
    pending: Option<cfi::Cfi>,
    /// Patterns for the book's `dc:language`; None means no hyphenation.
    hyphenator: Option<hyphen::Hyphenator_>,
    style: ReadingStyle,
    /// Palette Omarchy rendered for the app chrome. `None` elsewhere, and the
    /// chrome then follows the reading theme (CONTEXT.md §11).
    chrome: Option<Chrome>,
    chapter: Option<Chapter>,
    index: usize,
    page: usize,
    /// 1 or 2. Two-column falls back to one below `TWO_COLUMN_MIN_EM`.
    columns: usize,
    window: Option<Arc<Window>>,
    renderer: VelloWindowRenderer,
    net_tx: Sender<Resource>,
    net_rx: Receiver<Resource>,
    size: (u32, u32),
    scale: f32,
    cursor: (f32, f32),
}

impl App {
    fn new(open: Option<String>, start: usize) -> Self {
        let (net_tx, net_rx) = channel();

        // Progress is best-effort: a broken database must never stop you reading.
        let db = Db::open()
            .map_err(|e| eprintln!("omaread: no progress database ({e})"))
            .ok()
            .map(|d| Arc::new(Mutex::new(d)));

        let mut app = Self {
            view: View::Library,
            rows: Vec::new(),
            query: String::new(),
            sort: Sort::Recent,
            selected: 0,
            lib_doc: None,
            toc_doc: None,
            hud_doc: None,
            hud_key: String::new(),
            hud_shown: false,
            find: String::new(),
            finding: false,
            hud_until: None,
            toc_sel: 0,
            resume_page: 0,
            book: None,
            db,
            hash: String::new(),
            path: String::new(),
            pending: None,
            hyphenator: None,
            style: ReadingStyle::default(),
            chrome: style::omarchy_chrome(),
            chapter: None,
            index: start,
            page: 0,
            columns: 1,
            window: None,
            renderer: VelloWindowRenderer::new(),
            net_tx,
            net_rx,
            size: (900, 1000),
            scale: 1.0,
            cursor: (0.0, 0.0),
        };

        println!(
            "omaread: chrome — {}",
            match &app.chrome {
                Some((bg, ..)) => format!("Omarchy palette, ground {bg}"),
                None => "built-in palette (no omaread.css from Omarchy)".into(),
            }
        );

        if let Some(db) = app.db.clone() {
            if let Ok(d) = db.lock() {
                let (seen, added) = library::scan(&d);
                println!("omaread: library — {seen} files, {added} newly indexed");
            }
        }
        app.reload_rows();

        if let Some(p) = open {
            app.open_path(&PathBuf::from(p), start);
        }
        app
    }

    fn reload_rows(&mut self) {
        self.rows = self
            .db
            .as_ref()
            .and_then(|d| d.lock().ok()?.books(&self.query, self.sort).ok())
            .unwrap_or_default();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.lib_doc = None;
    }

    /// Import if needed, then open for reading.
    fn open_path(&mut self, path: &Path, start: usize) {
        let path = match self.db.as_ref().and_then(|d| d.lock().ok()) {
            Some(db) => library::import(&db, path),
            None => path.to_path_buf(),
        };
        let book = match Book::open(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("omaread: {e}");
                return;
            }
        };

        self.hash = db::file_hash(&path).unwrap_or_default();
        self.pending = self
            .db
            .as_ref()
            .and_then(|d| d.lock().ok()?.last_cfi(&self.hash).ok().flatten())
            .and_then(|s| cfi::Cfi::parse(&s));
        self.path = path.to_string_lossy().into_owned();

        let start = match &self.pending {
            Some(c) if c.spine < book.chapter_count() => c.spine,
            _ => start,
        };
        self.index_book(&book);
        self.hyphenator = hyphen::Hyphenator_::for_language(&book.language);
        println!(
            "omaread: {} ({} chapters, {})",
            book.title,
            book.chapter_count(),
            match (&book.language, self.hyphenator.is_some()) {
                (l, true) if !l.is_empty() => format!("hyphenating {l}"),
                (l, false) if !l.is_empty() => format!("no patterns for {l}"),
                _ => "no language declared".into(),
            }
        );

        if let Some(w) = &self.window {
            w.set_title(&format!("{} — Omaread", book.title));
        }
        self.book = Some(book);
        self.index = start;
        self.view = View::Reading;
        self.load_chapter(start);
        self.request_redraw();
    }

    fn open_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else { return };
        if row.missing {
            eprintln!("omaread: {} is missing from disk", row.title);
            return;
        }
        self.open_path(&PathBuf::from(&row.path), 0);
    }

    /// Lay out the library grid. Rebuilt on any change to query, sort or
    /// selection — the whole document is cheap next to a chapter, and an
    /// incremental DOM diff is complexity nobody has asked for.
    fn build_library(&mut self) {
        let (bg, fg, subtle, panel) = self.chrome();
        let html = grid::html(&self.rows, &self.query, self.sort, self.selected);
        let ua = grid::stylesheet(&bg, &fg, &subtle, &panel);

        let provider = self.db.clone().map(|db| {
            Arc::new(net::CoverProvider::new(db, self.callback()))
                as Arc<dyn blitz_traits::net::NetProvider<Resource>>
        });

        self.lib_doc =
            chapter::layout_document(html, ua, provider, self.viewport(), self.page_height());
        self.page = self
            .page
            .min(self.lib_doc.as_ref().map_or(0, |c| c.pages.count().saturating_sub(1)));
    }

    /// Lay out the HUD, unless the one in hand already says the right thing.
    fn build_hud(&mut self) {
        let Some(book) = self.book.clone() else { return };
        let percent = (self.progress() * 100.0).round() as u8;
        let height = self.size.1 as f32 / self.scale;
        let key = format!("{percent}|{height}|{:?}|{}", self.style.theme, book.title);
        if key == self.hud_key && self.hud_doc.is_some() {
            return;
        }

        let (_, fg, subtle, panel) = self.chrome();
        let html = hud::html(&book.title, percent, height);
        let ua = hud::stylesheet(&fg, &subtle, &panel);
        self.hud_doc =
            chapter::layout_document(html, ua, None, self.viewport(), self.page_height());
        self.hud_key = key;
    }

    /// Index the book's text the first time it is opened. Search over a book
    /// you have never opened is what `--index` is for.
    fn index_book(&self, book: &Book) {
        let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) else { return };
        if db.is_indexed(&self.hash) {
            return;
        }
        match library::index_book(&db, &self.hash, book) {
            Ok(n) => println!("omaread: indexed {n} chapters for search"),
            // Search is a convenience; a failure here must not stop you reading.
            Err(e) => eprintln!("omaread: could not index: {e}"),
        }
    }

    /// How far through the book the current page is, 0.0..=1.0.
    fn progress(&self) -> f32 {
        let Some(book) = &self.book else { return 0.0 };
        let within = match &self.chapter {
            Some(ch) if ch.pages.content_height > 0.0 => {
                ch.pages.top_of(self.page) / ch.pages.content_height
            }
            _ => 0.0,
        };
        book.progress(self.index, within)
    }

    /// Wake the HUD and start its clock. Pointer motion is the gesture.
    fn poke_hud(&mut self) {
        if self.view != View::Reading {
            return;
        }
        self.hud_until = Some(Instant::now() + HUD_LINGER);
        if !self.hud_shown {
            self.hud_shown = true;
            self.request_redraw();
        }
    }

    fn to_library(&mut self) {
        self.save_position();
        if let Some(w) = &self.window {
            w.set_title("Omaread");
        }
        self.reload_rows();
        self.view = View::Library;
        self.request_redraw();
    }

    /// The app chrome palette: Omarchy's when it rendered one, the reading
    /// theme's otherwise. Every chrome surface goes through here.
    fn chrome(&self) -> Chrome {
        self.chrome.clone().unwrap_or_else(|| {
            let (bg, fg, subtle, panel) = self.style.theme.chrome_colors();
            (bg.into(), fg.into(), subtle.into(), panel.into())
        })
    }

    /// The document the current view paints. Every view is one.
    fn doc(&self) -> Option<&Chapter> {
        match self.view {
            View::Library => self.lib_doc.as_ref(),
            View::Toc => self.toc_doc.as_ref(),
            View::Reading => self.chapter.as_ref(),
        }
    }

    fn doc_mut(&mut self) -> Option<&mut Chapter> {
        match self.view {
            View::Library => self.lib_doc.as_mut(),
            View::Toc => self.toc_doc.as_mut(),
            View::Reading => self.chapter.as_mut(),
        }
    }

    /// Lay out the contents. Cheap next to a chapter, so it is rebuilt on every
    /// open and every selection move rather than diffed — the same bargain the
    /// library grid makes.
    fn build_toc(&mut self) {
        let Some(book) = self.book.clone() else { return };
        let entries = self.toc_entries();
        let (heading, subtitle) = match self.finding {
            true => (
                "Search",
                format!(
                    "“{}” — {} hit{} in this book",
                    self.find,
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" }
                ),
            ),
            false => ("Contents", book.title.clone()),
        };
        let (bg, fg, subtle, panel) = self.chrome();
        let html = toc::html(&entries, heading, &subtitle, self.index, self.toc_sel);
        let ua = toc::stylesheet(&bg, &fg, &subtle, &panel);
        self.toc_doc =
            chapter::layout_document(html, ua, None, self.viewport(), self.page_height());
    }

    /// What the contents view lists: the book's navigation, or search hits.
    fn toc_entries(&self) -> Vec<book::TocEntry> {
        let Some(book) = &self.book else { return Vec::new() };
        if !self.finding {
            return (*book.toc).clone();
        }
        let Some(fts) = search::fts_query(&self.find) else { return Vec::new() };
        let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) else { return Vec::new() };

        db.search_in_book(&self.hash, &fts, 200)
            .into_iter()
            .map(|hit| {
                // Name the chapter from the book's own navigation when it has
                // one: "Chapter 4 · …the words…" beats a bare snippet.
                let where_ = book
                    .toc
                    .iter()
                    .filter(|e| e.spine <= hit.spine)
                    .next_back()
                    .map(|e| e.label.clone())
                    .unwrap_or_else(|| format!("Chapter {}", hit.spine + 1));
                book::TocEntry {
                    label: format!("{where_} · {}", hit.snippet),
                    depth: 0,
                    spine: hit.spine,
                    fragment: None,
                    find: Some(self.find.clone()),
                }
            })
            .collect()
    }

    fn open_toc(&mut self) {
        let Some(toc) = self.book.as_ref().map(|b| b.toc.clone()) else { return };
        self.resume_page = self.page;
        // Open on the entry covering where you are, not at the top: in a long
        // book the useful part of the contents is the part you are in. A search
        // starts at the best hit, which is the first one.
        self.toc_sel = match self.finding {
            true => 0,
            false => toc.iter().rposition(|e| e.spine <= self.index).unwrap_or(0),
        };
        self.view = View::Toc;
        self.page = 0;
        self.build_toc();
        self.page = self.toc_doc.as_ref().and_then(|d| page_of_index(d, self.toc_sel)).unwrap_or(0);
        self.request_redraw();
    }

    fn close_toc(&mut self) {
        self.finding = false;
        self.view = View::Reading;
        self.page = self.resume_page.min(self.page_count().saturating_sub(1));
        self.request_redraw();
    }

    /// Navigate to a contents entry, landing on its fragment's page when it has
    /// one — books that keep several chapters in one spine file would otherwise
    /// send every entry to page 1.
    fn open_toc_entry(&mut self, i: usize) {
        let Some(entry) = self.book.as_ref().and_then(|b| b.toc.get(i).cloned()) else { return };
        self.view = View::Reading;
        if entry.spine == self.index && self.chapter.is_some() {
            self.page = 0;
        } else {
            self.load_chapter(entry.spine);
        }
        // A hit knows its words, not its element: locate the text in the
        // chapter that is now laid out.
        if let Some(needle) = &entry.find {
            match self
                .chapter
                .as_ref()
                .and_then(|ch| chapter::node_containing_text(ch.dom(), needle))
                .and_then(|node| {
                    let ch = self.chapter.as_ref()?;
                    Some(ch.pages.page_containing(chapter::node_top(ch.dom(), node)?))
                }) {
                Some(page) => self.page = page,
                // Folding is close but not identical to FTS5's, so a miss is
                // possible; the chapter is still the right place to be.
                None => eprintln!("omaread: “{needle}” did not resolve to a page in this chapter"),
            }
        }
        if let Some(frag) = &entry.fragment {
            match self
                .chapter
                .as_ref()
                .and_then(|ch| find_by_attr(ch.dom(), 0, "id", frag))
                .and_then(|node| {
                    let ch = self.chapter.as_ref()?;
                    Some(ch.pages.page_containing(chapter::node_top(ch.dom(), node)?))
                }) {
                Some(page) => self.page = page,
                // A dangling fragment is the book's problem, not the reader's:
                // the chapter is still the right place to be.
                None => eprintln!("omaread: contents point at #{frag}, which is not in the chapter"),
            }
        }
        self.resume_page = self.page;
        self.save_position();
        self.request_redraw();
    }

    fn move_toc_selection(&mut self, by: isize) {
        let last = match self.book.as_ref().map(|b| b.toc.len()) {
            Some(n) if n > 0 => n as isize - 1,
            _ => return,
        };
        self.toc_sel = (self.toc_sel as isize + by).clamp(0, last) as usize;
        // Follow the selection onto its page.
        if let Some(page) = self.toc_doc.as_ref().and_then(|d| page_of_index(d, self.toc_sel)) {
            self.page = page;
        }
    }

    fn chapter_count(&self) -> usize {
        self.book.as_ref().map_or(0, |b| b.chapter_count())
    }

    fn callback(&self) -> SharedCallback<Resource> {
        Arc::new(ChannelCallback(self.net_tx.clone()))
    }

    /// How many columns actually paint.
    ///
    /// Two-column is specified (CONTEXT.md §3) and the paginator supports it —
    /// pages `2n`/`2n+1` of one flow — but it cannot be *painted* yet:
    /// `blitz_paint::paint_scene` resets the scene on entry, so a second call
    /// erases the first, and `VelloScenePainter.inner` is `pub(crate)` so
    /// per-column sub-scenes cannot be composed either. Needs an upstream
    /// non-resetting paint entry point or access to the inner scene.
    fn effective_columns(&self) -> usize {
        let css_w = self.size.0 as f32 / self.scale;
        let _wide_enough = self.columns == 2
            && css_w >= TWO_COLUMN_MIN_EM * self.style.font_px();
        1
    }

    /// Width of one column in physical pixels.
    fn column_width(&self) -> u32 {
        (self.size.0 / self.effective_columns() as u32).max(1)
    }

    /// The document is laid out at *column* width, not window width: in
    /// two-column mode both columns are views onto the same flow.
    fn viewport(&self) -> blitz_traits::shell::Viewport {
        chapter::viewport(
            self.column_width(),
            self.size.1,
            self.scale,
            self.style.theme == Theme::Night,
        )
    }

    /// Vertical margin above and below the page, in CSS pixels.
    fn page_margin(&self) -> f32 {
        PAGE_MARGIN_EM * self.style.font_px()
    }

    /// Usable height of one page, in CSS pixels — the window less its margins.
    fn page_height(&self) -> f32 {
        (self.size.1 as f32 / self.scale - 2.0 * self.page_margin()).max(1.0)
    }

    /// Jump to the saved position, if it points into the chapter just loaded.
    fn restore_position(&mut self) {
        let Some(c) = self.pending.take() else { return };
        if c.spine != self.index {
            self.pending = Some(c);
            return;
        }
        let Some(ch) = &self.chapter else { return };
        let Some(node) = cfi::resolve(ch.dom(), &c) else {
            eprintln!("omaread: saved position no longer resolves; starting at the top");
            return;
        };
        if let Some(y) = chapter::node_top(ch.dom(), node) {
            self.page = ch.pages.page_containing(y);
            println!("omaread: resumed at page {} of {}", self.page + 1, ch.pages.count());
        }
    }

    /// Anchor the current page to a CFI and store it.
    fn save_position(&self) {
        let (Some(db), Some(ch)) = (&self.db, &self.chapter) else { return };
        let Ok(db) = db.lock() else { return };
        let top = ch.pages.top_of(self.page);
        let Some(node) = chapter::node_at(ch.dom(), top) else { return };
        let Some(c) = cfi::of_node(ch.dom(), node, self.index) else { return };
        let title = self.book.as_ref().map(|b| b.title.as_str()).unwrap_or("");
        if let Err(e) = db.save_progress(&self.hash, &self.path, title, &c.to_string())
        {
            eprintln!("omaread: could not save position: {e}");
        }
    }

    fn last_page_of_loaded(&self) -> usize {
        self.chapter.as_ref().map_or(0, |c| c.pages.count().saturating_sub(1))
    }

    fn page_count(&self) -> usize {
        self.doc().map_or(1, |c| c.pages.count())
    }

    /// Page a chrome document — library or contents. `turn` is the reading
    /// version: it crosses chapter boundaries and saves a position, neither of
    /// which means anything here.
    fn turn_view(&mut self, forward: bool) {
        let last = self.page_count().saturating_sub(1);
        self.page = if forward {
            (self.page + 1).min(last)
        } else {
            self.page.saturating_sub(1)
        };
        self.request_redraw();
    }

    /// Load a chapter, skipping forward past any the engine chokes on.
    fn load_chapter(&mut self, index: usize) {
        self.load_chapter_at(index, false);
    }

    /// `backwards` opens the chapter on its last page, for turning back across a
    /// chapter boundary.
    fn load_chapter_at(&mut self, mut index: usize, backwards: bool) {
        let Some(book) = self.book.clone() else { return };
        let count = book.chapter_count();
        while index < count {
            let vp = self.viewport();
            let ph = self.page_height();
            let cb = self.callback();
            match chapter::load(&book, index, &self.style, vp, ph, self.hyphenator.as_ref(), cb) {
                Ok(ch) => {
                    if std::env::var_os("OMAREAD_DEBUG_PAGES").is_some() {
                        let atoms = chapter::collect_atoms(ch.dom());
                        let mut bad = 0;
                        for (n, &t) in ch.pages.tops.iter().enumerate() {
                            if let Some(a) =
                                atoms.iter().find(|a| t > a.top && t < a.bottom)
                            {
                                bad += 1;
                                eprintln!(
                                    "  BAD page {n} top {t:.1} cuts {:?} {:.1}..{:.1}",
                                    a.kind, a.top, a.bottom
                                );
                            }
                        }
                        eprintln!(
                            "  PAGES {} tops={:?} atoms={} cutting={bad}",
                            ch.pages.count(),
                            ch.pages.tops.iter().map(|t| t.round()).collect::<Vec<_>>(),
                            atoms.len(),
                        );
                    }
                    if std::env::var_os("OMAREAD_DEBUG_LAYOUT").is_some() {
                        let vw = self.size.0 as f32 / self.scale;
                        for tag in ["html", "body", "div", "p"] {
                            if let Some((x, y, w, h)) = chapter::element_rect(ch.dom(), tag) {
                                eprintln!(
                                    "  LAYOUT <{tag}> x={x:.0} y={y:.0} w={w:.0} h={h:.0}                                      (window {vw:.0}px, left gap {x:.0}, right gap {:.0})",
                                    vw - (x + w)
                                );
                            }
                        }
                    }
                    println!(
                        "omaread: chapter {}/{} — {} pages, {} chars, {} lines",
                        index + 1,
                        count,
                        ch.pages.count(),
                        ch.text_len(),
                        ch.line_count(),
                    );
                    self.chapter = Some(ch);
                    self.index = index;
                    self.page = if backwards { self.last_page_of_loaded() } else { 0 };
                    self.restore_position();
                    return;
                }
                Err(e) => {
                    eprintln!("omaread: {e}");
                    index += 1;
                }
            }
        }
        eprintln!("omaread: no renderable chapter at or after {index}");
    }

    /// Turn one page, crossing chapter boundaries in either direction.
    fn turn(&mut self, forward: bool) {
        let step = self.effective_columns();
        if forward {
            let last = self.page_count().saturating_sub(1);
            if self.page + step <= last {
                self.page += step;
            } else if self.index + 1 < self.chapter_count() {
                self.load_chapter(self.index + 1);
            } else {
                return;
            }
        } else if self.page >= step {
            self.page -= step;
        } else if self.index > 0 {
            self.load_chapter_at(self.index - 1, true);
        } else {
            return;
        }
        // ponytail: a write per page turn. WAL makes it cheap; batch only if it
        // ever shows up in a profile.
        self.save_position();
        self.request_redraw();
    }

    fn turn_chapter(&mut self, forward: bool) {
        let next = if forward {
            self.index + 1
        } else {
            match self.index.checked_sub(1) {
                Some(i) => i,
                None => return,
            }
        };
        if next < self.chapter_count() {
            self.load_chapter(next);
            self.request_redraw();
        }
    }

    /// The stylesheet is baked in at construction, so a theme or size change
    /// means rebuilding the document.
    fn restyle(&mut self) {
        let Some(index) = self.chapter.as_ref().map(|c| c.index) else {
            return;
        };
        // Keep the reader's place across a re-flow by carrying the flow offset,
        // not the page number: a smaller font means more text per page.
        let anchor = self.chapter.as_ref().map_or(0.0, |c| c.pages.top_of(self.page));
        self.load_chapter(index);
        if let Some(ch) = &self.chapter {
            self.page = ch.pages.page_containing(anchor);
        }
        self.request_redraw();
    }

    fn relayout(&mut self) {
        if self.view != View::Reading {
            let vp = self.viewport();
            let ph = self.page_height();
            if let Some(doc) = self.doc_mut() {
                chapter::relayout(doc, vp, ph);
                let last = doc.pages.count().saturating_sub(1);
                self.page = self.page.min(last);
            }
            return;
        }
        let vp = self.viewport();
        let ph = self.page_height();
        let anchor = self.chapter.as_ref().map_or(0.0, |c| c.pages.top_of(self.page));
        let panicked = match &mut self.chapter {
            Some(ch) => !chapter::relayout(ch, vp, ph),
            None => return,
        };
        if panicked {
            eprintln!("omaread: engine panicked during relayout; reloading chapter");
            let i = self.index;
            self.load_chapter(i);
            return;
        }
        if let Some(ch) = &self.chapter {
            self.page = ch.pages.page_containing(anchor);
        }
    }

    fn drain_resources(&mut self) {
        let pending: Vec<Resource> = self.net_rx.try_iter().collect();
        if pending.is_empty() {
            return;
        }
        // Resources belong to whichever document asked for them: cover images
        // for the library, figures for a chapter.
        let loaded = match self.doc_mut() {
            Some(doc) => catch_unwind(AssertUnwindSafe(|| {
                for resource in pending {
                    doc.doc.load_resource(resource);
                }
            }))
            .is_ok(),
            None => return,
        };

        if loaded {
            self.relayout();
            self.request_redraw();
        } else {
            eprintln!("omaread: engine panicked loading a resource; ignoring it");
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn redraw(&mut self) {
        let (w, h) = self.size;
        let scale = self.scale as f64;
        let page = self.page;
        let page_h = self.page_height();
        let margin = self.page_margin();
        let ground = match self.view {
            View::Reading => {
                let [r, g, b] = self.style.theme.background_rgb();
                Color::from_rgb8(r, g, b)
            }
            _ => parse_hex(&self.chrome().0),
        };

        match self.view {
            View::Library if self.lib_doc.is_none() => self.build_library(),
            View::Toc if self.toc_doc.is_none() => self.build_toc(),
            View::Reading if self.hud_shown => self.build_hud(),
            _ => {}
        }
        let showing_hud = self.hud_shown && self.view == View::Reading;

        // Disjoint field borrows: `render` takes the renderer mutably, the
        // closure needs the document mutably to set the page offset. That rules
        // out `doc_mut`, which borrows all of `self`.
        let App { renderer, chapter, lib_doc, toc_doc, hud_doc, view, .. } = self;
        let hud = if showing_hud { hud_doc.as_mut() } else { None };
        let Some(ch) = (match view {
            View::Library => lib_doc.as_mut(),
            View::Toc => toc_doc.as_mut(),
            View::Reading => chapter.as_mut(),
        }) else {
            return;
        };
        let top = ch.pages.top_of(page);
        let extent = ch.pages.extent_of(page);
        if std::env::var_os("OMAREAD_DEBUG_PAINT").is_some() {
            eprintln!(
                "PAINT page {}/{} top={top:.0} page_h={page_h:.0} margin={margin:.0} win={w}x{h}",
                page + 1,
                ch.pages.count()
            );
        }
        let doc = &mut ch.doc;
        let frame = paint::Frame {
            width: w,
            height: h,
            scale,
            margin,
            page_height: page_h,
        };

        renderer.render(|scene| {
            // An engine panic while painting must not take the window with it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                paint::page(scene, doc, top, extent, &frame, ground);
                if let Some(hud) = hud {
                    paint::overlay(scene, &mut hud.doc, &frame);
                }
            }));
        });
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        match self.view {
            View::Library => self.library_key(event_loop, key),
            View::Reading => self.reading_key(event_loop, key),
            View::Toc => self.toc_key(event_loop, key),
        }
    }

    fn library_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        let cols = self.cards_per_row();
        match key {
            Key::Named(NamedKey::Escape) => {
                if self.query.is_empty() {
                    event_loop.exit();
                } else {
                    self.query.clear();
                    self.reload_rows();
                }
            }
            Key::Named(NamedKey::Enter) => {
                self.open_selected();
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                self.query.pop();
                self.reload_rows();
            }
            Key::Named(NamedKey::ArrowRight) => self.move_selection(1),
            Key::Named(NamedKey::ArrowLeft) => self.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) => self.move_selection(cols),
            Key::Named(NamedKey::ArrowUp) => self.move_selection(-cols),
            Key::Named(NamedKey::PageDown) => self.turn_view(true),
            Key::Named(NamedKey::PageUp) => self.turn_view(false),
            Key::Named(NamedKey::Space) => {
                // Space types a space while searching, otherwise pages down.
                if self.query.is_empty() {
                    self.turn_view(true);
                } else {
                    self.query.push(' ');
                    self.reload_rows();
                }
            }
            // Typing searches. Sort and rescan need a modifier-free key that is
            // not a letter, so they live on F5 and Tab.
            Key::Named(NamedKey::Tab) => {
                self.sort = self.sort.next();
                println!("omaread: sorted by {}", self.sort.label());
                self.reload_rows();
            }
            Key::Named(NamedKey::F5) => self.rescan(),
            Key::Character(c) => {
                self.query.push_str(c.as_str());
                self.reload_rows();
            }
            _ => return,
        }
        self.lib_doc = None;
        self.request_redraw();
    }

    /// Open whatever is under the pointer. Coordinates are in CSS px relative
    /// to the page slice, so the page offset has to come back off the Y.
    fn on_click(&mut self) {
        if self.view == View::Reading {
            return;
        }
        let Some(doc) = self.doc() else { return };
        let margin = self.page_margin();
        let x = self.cursor.0 / self.scale;
        let y = self.cursor.1 / self.scale - margin + doc.pages.top_of(self.page);

        let Some(hit) = doc.doc.hit(x, y) else { return };
        match self.view {
            View::Library => {
                let Some(hash) = ancestor_attr(doc.dom(), hit.node_id, "data-hash") else {
                    return;
                };
                let Some(i) = self.rows.iter().position(|r| r.hash == hash) else { return };
                self.selected = i;
                self.open_selected();
            }
            View::Toc => {
                let Some(i) = ancestor_attr(doc.dom(), hit.node_id, "data-index")
                    .and_then(|v| v.parse().ok())
                else {
                    return;
                };
                self.open_toc_entry(i);
            }
            View::Reading => {}
        }
    }

    fn cards_per_row(&self) -> isize {
        // Cards are border-box, so the pitch is card width plus one gap; the
        // last card in a row needs no gap, hence the + GAP before dividing.
        const CARD: f32 = 150.0;
        const GAP: f32 = 30.0;
        let usable = (self.size.0 as f32 / self.scale) - 56.0;
        (((usable + GAP) / (CARD + GAP)).floor() as isize).max(1)
    }

    fn move_selection(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + by).clamp(0, last) as usize;
        // Follow the selection onto its page.
        if let Some(page) = self.lib_doc.as_ref().and_then(|d| page_of_index(d, self.selected)) {
            self.page = page;
        }
    }

    fn rescan(&mut self) {
        // A theme switch rewrites the rendered palette, and F5 already means
        // "pick up what changed on disk".
        self.chrome = style::omarchy_chrome();
        let Some(db) = self.db.clone() else { return };
        if let Ok(d) = db.lock() {
            let (seen, added) = library::scan(&d);
            println!("omaread: rescan — {seen} files, {added} new");
        }
        self.reload_rows();
    }

    fn reading_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        match key {
            Key::Named(NamedKey::Escape) => self.to_library(),
            Key::Named(NamedKey::Tab) => self.open_toc(),
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::PageDown) => self.turn(true),
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::PageUp) => self.turn(false),
            Key::Named(NamedKey::Space) => self.turn(true),
            Key::Named(NamedKey::ArrowDown) => self.turn_chapter(true),
            Key::Named(NamedKey::ArrowUp) => self.turn_chapter(false),
            Key::Character(c) => match c.as_str() {
                "q" => {
                    self.save_position();
                    event_loop.exit();
                }
                "l" => self.to_library(),
                "/" => {
                    self.find.clear();
                    self.finding = true;
                    self.open_toc();
                }
                "c" => {
                    println!(
                        "omaread: two-column is not paintable yet (blitz-paint resets the scene)"
                    );
                }
                "t" => {
                    self.style.theme = match self.style.theme {
                        Theme::White => Theme::Sepia,
                        Theme::Sepia => Theme::Grey,
                        Theme::Grey => Theme::Night,
                        Theme::Night => Theme::White,
                    };
                    println!("omaread: theme {:?}", self.style.theme);
                    self.restyle();
                }
                "+" | "=" => {
                    self.style.scale = (self.style.scale + 0.1).min(1.6);
                    println!("omaread: {:.0}px", self.style.font_px());
                    self.restyle();
                }
                "-" => {
                    self.style.scale = (self.style.scale - 0.1).max(0.8);
                    println!("omaread: {:.0}px", self.style.font_px());
                    self.restyle();
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Contents keys are deliberately few: this is a list you pass through, not
    /// a place to configure anything.
    fn toc_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        match key {
            Key::Named(NamedKey::Escape) => {
                // Esc backs out one step: query first, then the view.
                if self.finding && !self.find.is_empty() {
                    self.find.clear();
                } else {
                    self.close_toc();
                    return;
                }
            }
            Key::Named(NamedKey::Tab) => {
                self.close_toc();
                return;
            }
            Key::Named(NamedKey::Backspace) if self.finding => {
                self.find.pop();
                self.toc_sel = 0;
            }
            Key::Named(NamedKey::Space) if self.finding => {
                self.find.push(' ');
                self.toc_sel = 0;
            }
            // While searching every letter is query text, so the single-key
            // commands below are unreachable — which is what you want when you
            // are typing a word that happens to contain "q".
            Key::Character(c) if self.finding => {
                self.find.push_str(c.as_str());
                self.toc_sel = 0;
            }
            Key::Named(NamedKey::Enter) => {
                self.open_toc_entry(self.toc_sel);
                return;
            }
            Key::Named(NamedKey::ArrowDown) => self.move_toc_selection(1),
            Key::Named(NamedKey::ArrowUp) => self.move_toc_selection(-1),
            Key::Named(NamedKey::Home) => self.move_toc_selection(isize::MIN / 2),
            Key::Named(NamedKey::End) => self.move_toc_selection(isize::MAX / 2),
            Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                self.turn_view(true);
                return;
            }
            Key::Named(NamedKey::PageUp) => {
                self.turn_view(false);
                return;
            }
            Key::Character(c) => match c.as_str() {
                "q" => {
                    self.save_position();
                    event_loop.exit();
                    return;
                }
                "l" => {
                    self.to_library();
                    return;
                }
                _ => return,
            },
            _ => return,
        }
        // The selection is baked into the markup, so moving it rebuilds.
        self.toc_doc = None;
        self.request_redraw();
    }
}

/// The page a `data-index` row falls on. Both list views mark their rows this
/// way, so the selection can be followed onto its page.
fn page_of_index(doc: &Chapter, index: usize) -> Option<usize> {
    let node = find_by_attr(doc.dom(), 0, "data-index", &index.to_string())?;
    Some(doc.pages.page_containing(chapter::node_top(doc.dom(), node)?))
}

/// First element under `id` carrying `attr="value"`.
fn find_by_attr(
    dom: &blitz_dom::BaseDocument,
    id: usize,
    attr: &str,
    value: &str,
) -> Option<usize> {
    let node = dom.get_node(id)?;
    if let blitz_dom::NodeData::Element(el) = &node.data {
        if el.attrs.iter().any(|a| &*a.name.local == attr && &*a.value == value) {
            return Some(id);
        }
    }
    node.children
        .iter()
        .find_map(|c| find_by_attr(dom, *c, attr, value))
}

/// Walk up from a hit node to the nearest ancestor carrying `attr`.
fn ancestor_attr(dom: &blitz_dom::BaseDocument, mut id: usize, attr: &str) -> Option<String> {
    loop {
        let node = dom.get_node(id)?;
        if let blitz_dom::NodeData::Element(el) = &node.data {
            if let Some(a) = el.attrs.iter().find(|a| &*a.name.local == attr) {
                return Some(a.value.to_string());
            }
        }
        id = node.parent?;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Without an app_id the window reports an empty class, and every
        // Hyprland rule keyed on it silently never matches — including the one
        // shipped in assets/omarchy (CONTEXT.md §11).
        #[cfg(target_os = "linux")]
        use winit::platform::wayland::WindowAttributesExtWayland;
        let attrs = Window::default_attributes()
            .with_title("Omaread")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 1000.0));
        #[cfg(target_os = "linux")]
        let attrs = attrs.with_name("omaread", "omaread");
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let size = window.inner_size();
        self.size = (size.width.max(1), size.height.max(1));
        self.scale = window.scale_factor() as f32;

        eprintln!(
            "omaread: window {}x{} physical, scale {} -> {:.0}x{:.0} CSS px; measure {:.0}px",
            self.size.0, self.size.1, self.scale,
            self.size.0 as f32 / self.scale, self.size.1 as f32 / self.scale,
            33.0 * self.style.font_px(),
        );
        self.renderer.resume(window.clone(), self.size.0, self.size.1);
        self.window = Some(window);

        // The library needs no chapter; only reopen a book if one is open.
        if matches!(self.view, View::Reading) {
            let start = self.index;
            self.load_chapter(start);
        }
        if let Ok(p) = std::env::var("OMAREAD_START_PAGE") {
            if let Ok(n) = p.parse::<usize>() {
                self.page = n.saturating_sub(1).min(self.page_count().saturating_sub(1));
            }
        }
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.save_position();
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                self.size = (size.width.max(1), size.height.max(1));
                self.renderer.set_size(self.size.0, self.size.1);
                self.relayout();
                self.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let moved = self.cursor != (position.x as f32, position.y as f32);
                self.cursor = (position.x as f32, position.y as f32);
                if moved {
                    self.poke_hud();
                }
            }

            WindowEvent::MouseInput { state, button, .. }
                if state == ElementState::Pressed
                    && button == winit::event::MouseButton::Left =>
            {
                self.on_click();
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y,
                    MouseScrollDelta::PixelDelta(p) => -p.y as f32,
                };
                if dy.abs() > 0.5 {
                    self.turn(dy > 0.0);
                }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.on_key(event_loop, event.logical_key);
            }

            WindowEvent::RedrawRequested => self.redraw(),

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_resources();

        // The HUD is the only thing here that happens without an event, so it is
        // the only reason to ask the loop to wake on a clock.
        match self.hud_until {
            Some(at) if Instant::now() >= at => {
                self.hud_until = None;
                self.hud_shown = false;
                event_loop.set_control_flow(ControlFlow::Wait);
                self.request_redraw();
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => {}
        }
    }
}
