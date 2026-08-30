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
mod library;
mod paginate;
mod style;

use anyrender::{PaintScene, WindowRenderer};
use anyrender_vello::VelloWindowRenderer;
use blitz_dom::Point;
use kurbo::{Affine, Rect};
use peniko::{Color, Fill};
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
use style::{GUTTER_EM, MEASURE_EM, PAGE_MARGIN_EM, ReadingStyle, Theme};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

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

/// Which surface is on screen. The library is a document too, so both arms hold
/// a `Chapter`; only where the HTML comes from differs.
enum View {
    Library,
    Reading,
}

struct App {
    view: View,
    rows: Vec<BookRow>,
    query: String,
    sort: Sort,
    selected: usize,
    /// Rebuilt whenever the query, sort or selection changes.
    lib_doc: Option<Chapter>,
    book: Option<Book>,
    db: Option<Arc<Mutex<Db>>>,
    hash: String,
    path: String,
    /// Restored on startup, consumed once the chapter it points at is open.
    pending: Option<cfi::Cfi>,
    style: ReadingStyle,
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
            book: None,
            db,
            hash: String::new(),
            path: String::new(),
            pending: None,
            style: ReadingStyle::default(),
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
        println!("omaread: {} ({} chapters)", book.title, book.chapter_count());

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
        let (bg, fg, subtle, panel) = self.style.theme.chrome_colors();
        let html = grid::html(&self.rows, &self.query, self.sort, self.selected);
        let ua = grid::stylesheet(bg, fg, subtle, panel);

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

    fn to_library(&mut self) {
        self.save_position();
        if let Some(w) = &self.window {
            w.set_title("Omaread");
        }
        self.reload_rows();
        self.view = View::Library;
        self.request_redraw();
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
        self.chapter.as_ref().map_or(1, |c| c.pages.count())
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
            match chapter::load(&book, index, &self.style, vp, ph, self.callback()) {
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
        let loaded = match &mut self.chapter {
            Some(ch) => catch_unwind(AssertUnwindSafe(|| {
                for resource in pending {
                    ch.doc.load_resource(resource);
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
            View::Library => {
                let (bg, ..) = self.style.theme.chrome_colors();
                parse_hex(bg)
            }
            View::Reading => {
                let [r, g, b] = self.style.theme.background_rgb();
                Color::from_rgb8(r, g, b)
            }
        };

        // Disjoint field borrows: `render` takes the renderer mutably, the
        // closure needs the document mutably to set the page offset.
        let library = matches!(self.view, View::Library);
        if library && self.lib_doc.is_none() {
            self.build_library();
        }

        let App { renderer, chapter, lib_doc, .. } = self;
        let Some(ch) = (if library { lib_doc.as_mut() } else { chapter.as_mut() }) else {
            return;
        };
        let top = ch.pages.top_of(page);
        if std::env::var_os("OMAREAD_DEBUG_PAINT").is_some() {
            eprintln!(
                "PAINT page {}/{} top={top:.0} page_h={page_h:.0} margin={margin:.0} win={w}x{h}",
                page + 1,
                ch.pages.count()
            );
        }
        let doc = &mut ch.doc;

        renderer.render(|scene| {
            // An engine panic while painting must not take the window with it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // `viewport_scroll` is the only thing that moves painted content:
                // a layer transform repositions a clip shape but not the drawing
                // commands inside it. Offsetting by `top - margin` puts this
                // page's first line just below the top margin.
                doc.set_viewport_scroll(Point { x: 0.0, y: (top - margin) as f64 });

                // NOTE: paint_scene calls scene.reset() first, discarding
                // anything drawn before it — so the page ground and the margin
                // masks must come *after*. See CONTEXT.md §9.
                blitz_paint::paint_scene(scene, &*doc, scale, w, h);

                // The flow continues above and below this page. Mask it, so the
                // margins show the neighbouring lines' halves as clean paper.
                let band = |scene: &mut _, y0: f64, y1: f64| {
                    PaintScene::fill(
                        scene,
                        Fill::NonZero,
                        Affine::IDENTITY,
                        ground,
                        None,
                        &Rect::new(0.0, y0, w as f64, y1),
                    );
                };
                band(scene, 0.0, margin as f64 * scale);
                band(scene, (margin + page_h) as f64 * scale, h as f64);
            }));
        });
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        match self.view {
            View::Library => self.library_key(event_loop, key),
            View::Reading => self.reading_key(event_loop, key),
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
            Key::Named(NamedKey::PageDown) => self.turn(true),
            Key::Named(NamedKey::PageUp) => self.turn(false),
            Key::Named(NamedKey::Space) => {
                // Space types a space while searching, otherwise pages down.
                if self.query.is_empty() {
                    self.turn(true);
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

    /// Open the card under the pointer. Coordinates are in CSS px relative to
    /// the page slice, so the page offset has to come back off the Y.
    fn on_click(&mut self) {
        if !matches!(self.view, View::Library) {
            return;
        }
        let Some(doc) = &self.lib_doc else { return };
        let margin = self.page_margin();
        let x = self.cursor.0 / self.scale;
        let y = self.cursor.1 / self.scale - margin + doc.pages.top_of(self.page);

        let Some(hit) = doc.doc.hit(x, y) else { return };
        let Some(hash) = card_hash(doc.dom(), hit.node_id) else { return };
        let Some(i) = self.rows.iter().position(|r| r.hash == hash) else { return };
        self.selected = i;
        self.open_selected();
    }

    fn cards_per_row(&self) -> isize {
        // Card 150px + 12px padding + 30px gap, inside the body's 28px padding.
        let usable = (self.size.0 as f32 / self.scale) - 56.0;
        ((usable / 192.0).floor() as isize).max(1)
    }

    fn move_selection(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + by).clamp(0, last) as usize;
        // Follow the selection onto its page.
        if let Some(doc) = &self.lib_doc {
            if let Some(y) = self.selected_top(doc) {
                self.page = doc.pages.page_containing(y);
            }
        }
    }

    fn selected_top(&self, doc: &Chapter) -> Option<f32> {
        let want = self.selected.to_string();
        let node = find_by_attr(doc.dom(), 0, "data-index", &want)?;
        chapter::node_top(doc.dom(), node)
    }

    fn rescan(&mut self) {
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

/// Walk up from a hit node to the card that owns it.
fn card_hash(dom: &blitz_dom::BaseDocument, mut id: usize) -> Option<String> {
    loop {
        let node = dom.get_node(id)?;
        if let blitz_dom::NodeData::Element(el) = &node.data {
            if let Some(a) = el.attrs.iter().find(|a| &*a.name.local == "data-hash") {
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
        let attrs = Window::default_attributes()
            .with_title("Omaread")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 1000.0));
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

        let start = self.index;
        self.load_chapter(start);
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
                self.cursor = (position.x as f32, position.y as f32);
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

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.drain_resources();
    }
}
