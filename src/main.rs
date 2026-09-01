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

/// A highlighter is opaque enough to see and transparent enough to read
/// through; the selection is the same idea in the system's blue.
pub const HIGHLIGHT: Color = Color::from_rgba8(0xff, 0xd6, 0x2e, 0x66);
const SELECTION: Color = Color::from_rgba8(0x0a, 0x84, 0xff, 0x40);
/// The library's selected card, outlined rather than filled so the cover shows.
pub const CARD_OUTLINE: Color = Color::from_rgb8(0x0a, 0x84, 0xff);
/// The card under the pointer. Light enough to read the cover through.
const HOVER: Color = Color::from_rgba8(0x0a, 0x84, 0xff, 0x24);
/// A settings row under the pointer. Neutral, not the accent: the row already
/// *in force* is tinted blue, and two blues a shade apart say nothing.
const MENU_HOVER: Color = Color::from_rgba8(0x80, 0x80, 0x80, 0x38);
/// The HUD control under the pointer. Heavier than the card wash: it sits on a
/// flat panel rather than a photograph, so a cover-safe tint reads as nothing.
const HUD_HOVER: Color = Color::from_rgba8(0x0a, 0x84, 0xff, 0x3d);

#[derive(Clone, Copy)]
struct Slide {
    from: usize,
    forward: bool,
    at: Instant,
}

/// How long a page takes to slide out of the way. Long enough to see which way
/// the page went, short enough that holding the key still turns pages quickly.
const SLIDE: Duration = Duration::from_millis(160);
/// Wake-up interval while a page is sliding. 60fps: a 160ms slide is ten
/// frames, and asking for them twice that fast only queues work the compositor
/// has not been asked for yet.
const FRAME: Duration = Duration::from_millis(16);

/// How long the reading HUD lingers after the pointer stops moving.
const HUD_LINGER: Duration = Duration::from_millis(2200);

/// Below this window width, a second column would squeeze the measure into
/// something unreadable, so two-column mode silently falls back to one.
const TWO_COLUMN_MIN_EM: f32 = 2.0 * (MEASURE_EM + 2.0 * GUTTER_EM);

/// Whether two columns actually fit, and are wanted.
///
/// Below the minimum the measure would squeeze into something unreadable, so it
/// silently falls back to one (CONTEXT.md §3). The library and the contents are
/// single documents laid out at window width, so they are always one column.
fn columns_for(requested: usize, css_width: f32, font_px: f32, reading: bool) -> usize {
    let fits = css_width >= TWO_COLUMN_MIN_EM * font_px;
    match requested >= 2 && fits && reading {
        true => 2,
        false => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::columns_for;
    use crate::style::{GUTTER_EM, MEASURE_EM};

    /// A taller window shows more cards, and the covers were chosen for the old
    /// height — so the newly revealed row came up bare. Resizing re-paginates the
    /// grid in place; it has to notice the visible set changed.
    /// The CSS and `grid::per_row` must agree about how many cards fit, or arrow
    /// keys step onto columns that are not there. Asserted against a real
    /// layout, not against the arithmetic on its own.
    #[test]
    fn the_laid_out_row_holds_what_per_row_says() {
        use crate::db::BookRow;

        let rows: Vec<BookRow> = (0..40)
            .map(|i| BookRow { hash: format!("h{i}"), title: format!("L{i}"), ..Default::default() })
            .collect();
        let ua = crate::grid::stylesheet("#fff", "#111", "#888", "#eee");

        for width in [900u32, 1200, 1517, 2000, 3840] {
            let doc = crate::chapter::layout_document(
                crate::grid::html(&rows, "", crate::db::Sort::Recent, &[], None, 0..rows.len()),
                ua.clone(),
                None,
                crate::chapter::viewport(width, 2400, 1.0, false),
                2300.0,
            )
            .expect("the grid must lay out");

            let tops = crate::chapter::indexed_tops(doc.dom());
            let first = tops.first().map(|&(_, y)| y).unwrap_or(0.0);
            let in_row = tops.iter().filter(|&&(_, y)| (y - first).abs() < 1.0).count();

            assert_eq!(
                in_row,
                crate::grid::per_row(width as f32),
                "at {width}px the layout put {in_row} in a row"
            );
        }
    }

    /// A superscript note reference is a ten-pixel target inside a sentence.
    /// Landing beside it has to count, or footnotes are unopenable.
    #[test]
    fn a_near_miss_still_finds_the_note_reference() {
        let html = r##"<html><body><p>Una frase larga que ocupa una linea entera y
            termina con una nota<sup><a href="#n1">2</a></sup> y sigue despues
            con mas palabras para llenar la medida.</p>
            <p id="n1">2 La nota, que es corta.</p></body></html>"##;
        let doc = crate::chapter::layout_document(
            html.to_string(),
            crate::style::ReadingStyle::default().stylesheet(),
            None,
            crate::chapter::viewport(900, 700, 1.0, false),
            600.0,
        )
        .expect("the page must lay out");

        // Sweep the paragraph for the one point that is exactly on the glyph.
        let par = crate::chapter::node_containing_text(doc.dom(), "termina con una nota")
            .expect("no paragraph");
        let (px, py, pw, ph) = crate::chapter::node_rect(doc.dom(), par).expect("no box");
        let mut on: Option<(f32, f32)> = None;
        let (mut y, mut x) = (py, px);
        while y < py + ph && on.is_none() {
            x = px;
            while x < px + pw {
                if doc
                    .doc
                    .hit(x, y)
                    .and_then(|h| super::ancestor_attr(doc.dom(), h.node_id, "href"))
                    .is_some()
                {
                    on = Some((x, y));
                    break;
                }
                x += 1.0;
            }
            y += 1.0;
        }
        let (hx, hy) = on.expect("the link is not hittable anywhere");

        // Six pixels off is a miss for the engine and a hit for a reader.
        assert_eq!(super::href_at(&doc.doc, hx, hy).as_deref(), Some("#n1"));
        assert_eq!(super::href_at(&doc.doc, hx - 6.0, hy).as_deref(), Some("#n1"));
        assert_eq!(super::href_at(&doc.doc, hx, hy + 6.0).as_deref(), Some("#n1"));
        // Far away is still a miss, or every click would follow something.
        assert_eq!(super::href_at(&doc.doc, px, py + ph - 1.0), None);
    }

    /// A note is shown where you are; a section is somewhere to go.
    #[test]
    fn a_short_block_is_a_note_and_a_section_is_a_place() {
        assert!(super::is_note("p", "1. Ibíd., p. 233."), "a bibliography entry");
        assert!(super::is_note("aside", "A footnote, as EPUB 3 would write one."));
        assert!(super::is_note("li", "Postman, N., Amusing Ourselves to Death, 1985."));

        // A heading is where a section starts: that is a page to turn to.
        assert!(!super::is_note("h2", "Capítulo 4"));
        assert!(!super::is_note("section", "Capítulo 4"));
        // So is a whole chapter that happens to sit in a <div>.
        assert!(!super::is_note("div", &"palabra ".repeat(200)));
        // And an empty target says nothing worth a popup.
        assert!(!super::is_note("p", "   "));
    }

    /// The page you left goes the way you turned, and the one arriving comes
    /// from the other side. Backwards is the mirror, or the gesture lies.
    #[test]
    fn a_turn_slides_the_way_it_was_turned() {
        let w = 900.0;
        assert_eq!(super::slide_offsets(true, 0.0, w), (0.0, w), "the new page waits offstage right");
        assert_eq!(super::slide_offsets(true, 1.0, w), (-w, 0.0), "and lands where the old one was");
        assert_eq!(super::slide_offsets(false, 0.0, w), (0.0, -w), "turning back, it waits at the left");
        assert_eq!(super::slide_offsets(false, 1.0, w), (w, 0.0));
        // Halfway, the two are adjacent: no gap, no overlap.
        let (out, into) = super::slide_offsets(true, 0.5, w);
        assert!((into - out - w).abs() < 0.01, "{out} and {into} must abut");
    }

    #[test]
    fn a_taller_window_reveals_cards_the_covers_did_not_cover() {
        use crate::db::BookRow;

        let rows: Vec<BookRow> = (0..40)
            .map(|i| BookRow {
                hash: format!("h{i}"),
                title: format!("Libro {i}"),
                author: "Autor".into(),
                has_cover: true,
                ..Default::default()
            })
            .collect();
        let ua = crate::grid::stylesheet("#fff", "#111", "#888", "#eee");
        let markup = crate::grid::html(
            &rows,
            "",
            crate::db::Sort::Recent,
            &[],
            None,
            0..rows.len(),
        );

        // A window with room for one row, then one with room for more.
        let short = 620.0_f32;
        let tall = 1400.0_f32;
        let mut doc = crate::chapter::layout_document(
            markup,
            ua,
            None,
            crate::chapter::viewport(1500, short as u32, 1.0, false),
            short - 100.0,
        )
        .expect("the grid must lay out");

        let before = super::visible_cards(&doc, 0);
        crate::chapter::relayout(
            &mut doc,
            crate::chapter::viewport(1500, tall as u32, 1.0, false),
            tall - 100.0,
        );
        let after = super::visible_cards(&doc, 0);

        assert!(
            after.end > before.end,
            "a taller window should show more cards: {before:?} -> {after:?}"
        );
    }

    /// Rendering a control is not the same as being able to click it: the HUD is
    /// hit-tested at window coordinates, so every button has to actually occupy
    /// some. Scans across both bars rather than guessing exact text widths.
    #[test]
    fn every_hud_control_is_hittable() {
        use std::collections::HashSet;

        let (w, h) = (1200u32, 900u32);
        let doc = crate::chapter::layout_document(
            crate::hud::html("Un libro", "page 8 of 19", 2, false, h as f32, Some("Back to page 7")),
            crate::hud::stylesheet("#111", "#888", "#eee"),
            None,
            crate::chapter::viewport(w, h, 1.0, false),
            h as f32,
        )
        .expect("the HUD must lay out");

        // Middle of the top bar, and of the bottom one.
        let mut found: HashSet<String> = HashSet::new();
        for y in [33.0_f32, h as f32 - 33.0] {
            for x in (0..w).step_by(4) {
                if let Some(hit) = doc.doc.hit(x as f32, y) {
                    if let Some(what) =
                        crate::ancestor_attr(doc.dom(), hit.node_id, "data-hud")
                    {
                        found.insert(what);
                    }
                }
            }
        }

        for want in ["bookmark", "contents", "highlight", "smaller", "bigger", "columns", "readout"]
        {
            assert!(found.contains(want), "{want} is drawn but not clickable: {found:?}");
        }
    }

    /// Two columns are only worth having when each still gets a real measure;
    /// below that the fallback has to be silent, not a squeezed 38ch column.
    #[test]
    fn two_columns_need_the_width_and_the_reading_view() {
        let em = 20.0;
        let min = 2.0 * (MEASURE_EM + 2.0 * GUTTER_EM) * em;

        assert_eq!(columns_for(2, min, em, true), 2);
        assert_eq!(columns_for(2, min - 1.0, em, true), 1, "too narrow falls back");
        assert_eq!(columns_for(1, min * 2.0, em, true), 1, "not asked for");
        // The library and the contents are one document at window width.
        assert_eq!(columns_for(2, min, em, false), 1);
        // A bigger font needs a wider window for the same two columns.
        assert_eq!(columns_for(2, min, em * 1.6, true), 1);
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `#rrggbb` to a colour. The chrome palette is authored as CSS strings so the
/// stylesheet and the window ground cannot drift apart.
pub fn parse_hex(s: &str) -> Color {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0);
    Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn main() {
    // wgpu enumerates every backend at instance creation, and enumerating the
    // GL one dlopens Mesa: libLLVM and libgallium, 108MB of resident memory for
    // a path vello cannot use anyway (it needs compute shaders). Vulkan keeps
    // both the hardware driver and lavapipe, the software fallback.
    //
    // Before any thread exists, and never over a choice the user has made.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        unsafe { std::env::set_var("WGPU_BACKEND", "vulkan") };
    }

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
             Library:  type to search · #tag to filter · F2 tag a book\n\
                       Enter open · Tab sort · F5 rescan · Esc clear/quit\n\
             Reading:  ←/→ page · ↑/↓ chapter · Tab contents · / search · t theme · +/- size · c columns · l library · q quit\n\
                       move the mouse for the menus: marks, contents, size, columns\n\
             Contents: ↑/↓ select · Enter go · Tab or Esc back\n\
             Marks:    m list · b bookmark · drag to select · h highlight · y copy\n\
                       in the list: n note · x delete"
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

    // ponytail: leave without running destructors. Dropping the renderer tears
    // down wgpu's GLES/EGL instance — a backend this app never renders with, it
    // exists only because wgpu enumerates all of them — and NVIDIA's
    // egl-wayland layer then marshals a Wayland request on a dead proxy and
    // segfaults. Every clean exit dumped core; see `exiting` for the stack.
    //
    // Nothing here needs a destructor to be correct: reading position and marks
    // are committed to SQLite before this point, and WAL means a process that
    // simply stops loses nothing. The OS reclaims the GPU and the fonts.
    //
    // Remove this when wgpu or the driver can survive its own teardown.
    app.save_position();
    std::process::exit(0);
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
/// What the contents view is listing. All three are "places in this book", so
/// they share one document, one set of keys and one hit-test path.
#[derive(Clone, Copy, PartialEq)]
enum TocMode {
    Contents,
    Search,
    Marks,
}

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
    /// Completions for the library search box, refreshed with the rows.
    suggestions: Vec<(String, String)>,
    /// Cards the current grid document carries covers for. When the page moves
    /// off this range the grid has to be built again.
    lib_covers: std::ops::Range<usize>,
    /// The page those covers were chosen for; turning past it needs a rebuild.
    lib_page: usize,
    /// The library card under the pointer, for hover feedback.
    hover_card: Option<usize>,
    /// The `data-hud` control under the pointer, same idea.
    hover_hud: Option<String>,
    /// And the `data-set` row of the settings panel under it.
    hover_menu: Option<String>,
    /// Where the reader was before following a link or a contents entry, so the
    /// bar can offer them the way back: the chapter, its page, and how to say so.
    ///
    /// The wording is captured on the way out, while that position is still the
    /// current one — it is the only moment the whole-book page number or the
    /// percentage can be worked out exactly.
    back: Option<(usize, usize, String)>,
    /// A footnote or reference on show over the page.
    popup: Option<Chapter>,
    /// The settings panel over the library, and the folder path being typed
    /// into it, if any.
    menu: Option<Chapter>,
    /// Where the panel was anchored when it opened, in window CSS pixels.
    menu_at: Option<(f32, f32)>,
    adding_folder: Option<String>,
    /// A page turn in flight: the page being left, and which way it went.
    ///
    /// ponytail: within one chapter only. Turning across a chapter boundary
    /// loads a different document and the outgoing page is already gone, so
    /// that turn cuts. Keep both documents alive if it ever grates.
    slide: Option<Slide>,
    /// A tag being typed for the selected book. Tagging borrows the search box.
    tagging: Option<String>,
    /// The contents list. Rebuilt on every open and on every selection move.
    toc_doc: Option<Chapter>,
    /// Title and progress, painted over the page while the pointer is active.
    hud_doc: Option<Chapter>,
    /// What `hud_doc` was built from; it is rebuilt only when this changes,
    /// because pointer motion arrives far faster than the text does.
    hud_key: String,
    hud_shown: bool,
    /// The foot of the page shows a percentage until you click it, then a page
    /// number. Clicking again swaps back.
    show_page: bool,
    /// The stored highlight the last press landed inside, if any. What makes the
    /// menu offer "Remove" instead of "Highlight".
    sel_mark: Option<i64>,
    /// Measured pages per chapter, and the layout they were measured at. This is
    /// what lets the readout say "page 27 of 336" and mean it.
    chapter_pages: Option<Vec<usize>>,
    pages_layout: String,
    /// In-book search: the query, and whether the contents view is showing hits
    /// instead of navigation. Both lists are "places in this book", so they are
    /// the same view.
    find: String,
    toc_mode: TocMode,
    /// Marks for the open book; index-aligned with the list while in Marks mode.
    marks: Vec<db::Mark>,
    /// Live selection: the element it lives in, and parley's selection over it.
    sel: Option<(usize, parley::Selection)>,
    dragging: bool,
    /// Mark being annotated, and the note as typed so far.
    noting: Option<i64>,
    note: String,
    /// When to put the HUD away. Drives `ControlFlow::WaitUntil`.
    hud_until: Option<Instant>,
    /// `OMAREAD_EXIT_AFTER=<ms>` quits by itself. Shutdown is a real code path
    /// with real bugs in it (see `exiting`), and it cannot be tested by hand or
    /// in CI without a way to reach it that needs no window manager.
    exit_at: Option<Instant>,
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
            suggestions: Vec::new(),
            lib_covers: 0..0,
            lib_page: 0,
            hover_card: None,
            hover_hud: None,
            hover_menu: None,
            slide: None,
            back: None,
            popup: None,
            menu: None,
            menu_at: None,
            adding_folder: None,
            tagging: None,
            toc_doc: None,
            hud_doc: None,
            hud_key: String::new(),
            hud_shown: false,
            show_page: false,
            sel_mark: None,
            chapter_pages: None,
            pages_layout: String::new(),
            find: String::new(),
            toc_mode: TocMode::Contents,
            marks: Vec::new(),
            sel: None,
            dragging: false,
            noting: None,
            note: String::new(),
            hud_until: None,
            exit_at: std::env::var("OMAREAD_EXIT_AFTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|ms| Instant::now() + Duration::from_millis(ms)),
            toc_sel: 0,
            resume_page: 0,
            book: None,
            db,
            hash: String::new(),
            path: String::new(),
            pending: None,
            hyphenator: None,
            style: ReadingStyle {
                scale: style::setting("font-scale")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1.0),
                theme: match style::setting("theme").as_deref() {
                    Some("white") => Theme::White,
                    Some("grey") => Theme::Grey,
                    Some("night") => Theme::Night,
                    Some("sepia") => Theme::Sepia,
                    _ => ReadingStyle::default().theme,
                },
            },
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
        self.suggestions = self.completions();
        self.lib_doc = None;
    }

    /// What to offer under the search box: tags while tagging or filtering by
    /// one, words and authors otherwise.
    fn completions(&self) -> Vec<(String, String)> {
        let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) else { return Vec::new() };

        let tag_prefix = match (&self.tagging, self.query.trim().strip_prefix('#')) {
            (Some(t), _) => Some(t.as_str()),
            (None, Some(q)) => Some(q),
            (None, None) => None,
        };
        if let Some(prefix) = tag_prefix {
            return db
                .all_tags(prefix)
                .into_iter()
                .map(|(tag, n)| (format!("#{tag}"), format!("{n} book{}", plural(n))))
                .collect();
        }
        match self.query.trim().is_empty() {
            true => Vec::new(),
            false => db.suggestions(&self.query, 6),
        }
    }

    /// Toggle the typed tag on the selected book. One key, both directions.
    fn apply_tag(&mut self) {
        let Some(tag) = self.tagging.take() else { return };
        let Some(row) = self.rows.get(self.selected).cloned() else { return };
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            match db.toggle_tag(&row.hash, &tag) {
                Ok(true) => println!("omaread: tagged {} #{tag}", row.title),
                Ok(false) => println!("omaread: untagged {} #{tag}", row.title),
                Err(e) => eprintln!("omaread: could not tag: {e}"),
            }
        }
        self.reload_rows();
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
        // Before the window exists the size is a guess, and `resumed` reloads
        // at the real one — laying the chapter out twice at startup, which for
        // an illustrated book is a wasted 120ms.
        if self.window.is_some() {
            self.load_chapter(start);
        }
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
    /// Lay out the library grid.
    ///
    /// Twice, deliberately. Loading a cover costs far more than laying out a
    /// card — 1817ms against 91ms for 361 books, measured — and only about
    /// fourteen covers are ever on screen. The first pass carries no covers and
    /// exists solely to find out which cards this page shows; the second gives
    /// those cards their covers. Before this, returning to the library blocked
    /// for nearly two seconds and swallowed the clicks that arrived meanwhile.
    fn build_library(&mut self) {
        let started = Instant::now();
        let (bg, fg, subtle, panel) = self.chrome();
        let ua = grid::stylesheet(&bg, &fg, &subtle, &panel);
        let markup = |covers: std::ops::Range<usize>| {
            grid::html(
                &self.rows,
                &self.query,
                self.sort,
                &self.suggestions,
                self.tagging.as_deref(),
                covers,
            )
        };

        // The probe carries the *same* markup as the real grid — every card with
        // its `<img>` — and simply has no provider to fetch through. Anything
        // less paginates differently: a blank jacket is text, which a page break
        // may fall inside, while a cover is an image block that cannot be split,
        // so a probe made of jackets fitted two rows where covers fit one.
        let probe = chapter::layout_document(
            markup(0..self.rows.len()),
            ua.clone(),
            None,
            self.viewport(),
            self.page_height(),
        );
        let visible = probe
            .as_ref()
            .map(|d| visible_cards(d, self.page))
            .unwrap_or(0..self.rows.len());

        let provider = self.db.clone().map(|db| {
            Arc::new(net::CoverProvider::new(db, self.callback()))
                as Arc<dyn blitz_traits::net::NetProvider<Resource>>
        });
        self.lib_doc = chapter::layout_document(
            markup(visible.clone()),
            ua,
            provider,
            self.viewport(),
            self.page_height(),
        );
        self.lib_covers = visible;
        self.page = self
            .page
            .min(self.lib_doc.as_ref().map_or(0, |c| c.pages.count().saturating_sub(1)));
        self.lib_page = self.page;

        if std::env::var_os("OMAREAD_DEBUG_TIME").is_some() {
            eprintln!(
                "TIME grid rebuild: {} rows, covers {:?} in {:.0}ms",
                self.rows.len(),
                self.lib_covers,
                started.elapsed().as_secs_f32() * 1000.0
            );
        }
    }


    /// Lay out the HUD, unless the one in hand already says the right thing.
    fn build_hud(&mut self) {
        let Some(book) = self.book.clone() else { return };
        let readout = self.readout();
        let cols = self.effective_columns();
        let height = self.size.1 as f32 / self.scale;
        let on_mark = self.sel_mark.is_some();
        let back = self.back.as_ref().map(|(_, _, label)| label.clone());
        let key = format!(
            "{readout}|{cols}|{on_mark}|{height}|{:?}|{}|{:?}",
            self.style.theme,
            book.title,
            back,
        );
        if key == self.hud_key && self.hud_doc.is_some() {
            return;
        }

        let (_, fg, subtle, panel) = self.chrome();
        let html = hud::html(&book.title, &readout, cols, on_mark, height, back.as_deref());
        let ua = hud::stylesheet(&fg, &subtle, &panel);
        // The HUD spans the window; only the page is laid out at column width.
        let vp = chapter::viewport(
            self.size.0,
            self.size.1,
            self.scale,
            self.style.theme == Theme::Night,
        );
        self.hud_doc = chapter::layout_document(html, ua, None, vp, self.page_height());
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

    /// Marks, selection, notes and copy — everything Phase 7 adds.
    fn open_marks(&mut self) {
        self.marks = self.load_marks();
        self.toc_mode = TocMode::Marks;
        self.open_toc();
    }

    fn load_marks(&self) -> Vec<db::Mark> {
        self.db
            .as_ref()
            .and_then(|d| d.lock().ok())
            .map(|d| d.marks(&self.hash))
            .unwrap_or_default()
    }

    /// CFI of the element this page begins at — the anchor for a bookmark, and
    /// the same one reading progress uses.
    fn cfi_of_page(&self) -> Option<cfi::Cfi> {
        let ch = self.chapter.as_ref()?;
        let node = chapter::node_at(ch.dom(), ch.pages.top_of(self.page))?;
        cfi::of_node(ch.dom(), node, self.index)
    }

    fn toggle_bookmark(&mut self) {
        let Some(cfi) = self.cfi_of_page() else { return };
        let cfi = cfi.to_string();
        let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) else { return };

        match db.bookmark_at(&self.hash, &cfi) {
            Some(id) => {
                let _ = db.remove_mark(id);
                println!("omaread: bookmark removed");
            }
            None => {
                let mark = db::Mark { cfi, ..Default::default() };
                match db.add_mark(&self.hash, &mark) {
                    Ok(()) => println!("omaread: bookmarked page {}", self.page + 1),
                    Err(e) => eprintln!("omaread: could not bookmark: {e}"),
                }
            }
        }
    }

    /// The selection as (element, byte range).
    fn selected(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let (node, sel) = self.sel.as_ref()?;
        let range = sel.text_range();
        (!range.is_empty()).then(|| (*node, range))
    }

    fn selected_text(&self) -> Option<String> {
        let (node, range) = self.selected()?;
        let ch = self.chapter.as_ref()?;
        chapter::char_span(ch.dom(), node, range).map(|(_, _, text)| text)
    }

    fn highlight_selection(&mut self) {
        let Some((node, range)) = self.selected() else {
            println!("omaread: nothing selected — drag across a paragraph first");
            return;
        };
        let Some(ch) = self.chapter.as_ref() else { return };
        let Some((start, len, text)) = chapter::char_span(ch.dom(), node, range) else { return };
        let Some(mut cfi) = cfi::of_node(ch.dom(), node, self.index) else { return };
        cfi.offset = Some(start);

        let mark = db::Mark {
            cfi: cfi.to_string(),
            length: len,
            text,
            ..Default::default()
        };
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            match db.add_mark(&self.hash, &mark) {
                Ok(()) => println!("omaread: highlighted “{}”", mark.text),
                Err(e) => eprintln!("omaread: could not highlight: {e}"),
            }
        }
        self.sel = None;
        self.request_redraw();
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selected_text() else {
            println!("omaread: nothing selected");
            return;
        };
        // ponytail: wl-copy rather than a clipboard crate. Wayland is the
        // target (§1) and this is one process instead of one dependency; swap in
        // a crate if a non-Wayland target ever matters.
        use std::io::Write;
        use std::process::{Command, Stdio};
        let spawned = Command::new("wl-copy").stdin(Stdio::piped()).spawn();
        match spawned {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
                println!("omaread: copied {} characters", text.chars().count());
            }
            Err(e) => eprintln!("omaread: could not run wl-copy ({e}); install wl-clipboard"),
        }
    }

    fn begin_note(&mut self) {
        let Some(mark) = self.marks.get(self.toc_sel) else { return };
        self.note = mark.note.clone();
        self.noting = Some(mark.id);
    }

    fn save_note(&mut self) {
        let Some(id) = self.noting.take() else { return };
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            if let Err(e) = db.set_note(id, &self.note) {
                eprintln!("omaread: could not save note: {e}");
            }
        }
        self.note.clear();
        self.marks = self.load_marks();
    }

    fn delete_mark(&mut self) {
        let Some(mark) = self.marks.get(self.toc_sel) else { return };
        let id = mark.id;
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            let _ = db.remove_mark(id);
        }
        self.marks = self.load_marks();
        self.toc_sel = self.toc_sel.min(self.marks.len().saturating_sub(1));
    }

    /// Flow-coordinate rectangles to paint over this page: the live selection,
    /// and every stored highlight in this chapter.
    fn selection_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let Some(ch) = self.chapter.as_ref() else { return Vec::new() };
        match &self.sel {
            Some((node, sel)) => chapter::selection_rects(ch.dom(), *node, sel),
            None => Vec::new(),
        }
    }

    /// ponytail: re-queried every frame. It is one indexed read on a WAL
    /// database and only matters while dragging; cache per chapter if a profile
    /// ever shows it.
    fn highlight_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let (Some(ch), Some(db)) = (
            self.chapter.as_ref(),
            self.db.as_ref().and_then(|d| d.lock().ok()),
        ) else {
            return Vec::new();
        };
        db.marks_in(&self.hash, self.index)
            .iter()
            .filter_map(|m| {
                let c = cfi::Cfi::parse(&m.cfi)?;
                let node = cfi::resolve(ch.dom(), &c)?;
                Some(chapter::highlight_rects(ch.dom(), node, c.offset?, m.length))
            })
            .flatten()
            .collect()
    }

    /// Begin a selection at the pointer. Coordinates arrive in physical pixels.
    fn begin_selection(&mut self) {
        // Remember a highlight under the press, so the menu can offer to remove
        // it — "select a highlight, then remove it from the menu".
        self.sel_mark = self.mark_at(self.pointer_in_flow());
        let Some(local) = self.pointer_in_text() else {
            self.sel = None;
            self.request_redraw();
            return;
        };
        let (node, x, y) = local;
        let Some(ch) = self.chapter.as_ref() else { return };
        let Some(tl) = chapter::text_layout(ch.dom(), node) else { return };
        self.sel = Some((node, parley::Selection::from_point(&tl.layout, x, y)));
        self.dragging = true;
    }

    /// Extend the selection. The anchor's element is kept: a selection lives in
    /// one parley layout.
    ///
    /// ponytail: one paragraph at a time. Selecting across paragraphs means
    /// stitching several layouts and is a bigger job than it looks; parley
    /// clamps a drag past the end, so dragging down selects to the paragraph's
    /// end rather than doing nothing.
    fn extend_selection(&mut self) {
        let Some((node, sel)) = self.sel.take() else { return };
        let Some(ch) = self.chapter.as_ref() else { return };
        let (Some(tl), Some((ox, oy))) = (
            chapter::text_layout(ch.dom(), node),
            chapter::node_origin(ch.dom(), node),
        ) else {
            return;
        };
        let (fx, fy) = self.pointer_in_flow();
        let next = sel.extend_to_point(&tl.layout, fx - ox, fy - oy);
        self.sel = Some((node, next));
        self.request_redraw();
    }

    /// Pointer position in flow coordinates (CSS pixels into the chapter).
    ///
    /// In two-column mode the right half of the window is a different page of
    /// the same flow, so the column under the pointer decides both the vertical
    /// offset and how much to take off the x.
    fn pointer_in_flow(&self) -> (f32, f32) {
        let cols = self.effective_columns();
        let col_w = (self.size.0 as f32 / self.scale) / cols as f32;
        let x = self.cursor.0 / self.scale;
        let col = ((x / col_w).floor() as usize).min(cols - 1);
        let page = (self.page + col).min(self.page_count().saturating_sub(1));
        let top = self.chapter.as_ref().map_or(0.0, |c| c.pages.top_of(page));
        (
            x - col as f32 * col_w,
            self.cursor.1 / self.scale - self.page_margin() + top,
        )
    }

    /// The text element under the pointer, with pointer coordinates local to it.
    fn pointer_in_text(&self) -> Option<(usize, f32, f32)> {
        let ch = self.chapter.as_ref()?;
        let (fx, fy) = self.pointer_in_flow();
        let hit = ch.doc.hit(fx, fy)?;
        let node = chapter::text_element(ch.dom(), hit.node_id)?;
        let (ox, oy) = chapter::node_origin(ch.dom(), node)?;
        Some((node, fx - ox, fy - oy))
    }

    /// Open the settings panel under the cog, and remember where that was.
    ///
    /// The anchor is taken once, here. Changing the theme drops the shelf
    /// document to rebuild it with the new palette, and a panel that asked the
    /// shelf where the cog is *while it was gone* jumped to the left margin.
    fn open_menu(&mut self) {
        // The cog is in the shelf's flow, and the shelf is a page, so its window
        // position is the flow position plus the page margin. Clamped so a cog
        // near the right edge does not hang the panel off the screen.
        let win_w = self.size.0 as f32 / self.scale;
        let margin = self.page_margin();
        self.menu_at = self
            .lib_doc
            .as_ref()
            .and_then(|d| find_by_attr(d.dom(), 0, "data-icon", "gear"))
            .and_then(|n| chapter::node_rect(self.lib_doc.as_ref()?.dom(), n))
            .map(|(x, y, _, h)| {
                (
                    (x - 10.0).clamp(
                        grid::SIDE_PAD as f32,
                        (win_w - grid::MENU_W - grid::SIDE_PAD as f32).max(0.0),
                    ),
                    y + h + margin + 10.0,
                )
            });
        self.build_menu();
    }

    /// Lay out the settings panel where `open_menu` put it.
    fn build_menu(&mut self) {
        let (_, fg, subtle, panel) = self.chrome();
        let (left, top) = self.menu_at.unwrap_or((grid::SIDE_PAD as f32, 74.0));
        let folders: Vec<String> =
            library::folders().iter().map(|p| p.display().to_string()).collect();
        let html = grid::menu_html(
            &format!("{:?}", self.style.theme),
            &folders,
            self.adding_folder.as_deref(),
        );
        let vp = chapter::viewport(
            self.size.0,
            self.size.1,
            self.scale,
            self.style.theme == Theme::Night,
        );
        self.menu = chapter::layout_document(
            html,
            grid::menu_stylesheet(&fg, &subtle, &panel, left, top),
            None,
            vp,
            self.page_height(),
        );
        self.request_redraw();
    }

    /// The panel row under the pointer, by its `data-set` name.
    fn menu_under_pointer(&self) -> Option<String> {
        let menu = self.menu.as_ref()?;
        let (x, y) = (self.cursor.0 / self.scale, self.cursor.1 / self.scale);
        let hit = menu.doc.hit(x, y)?;
        ancestor_attr(menu.dom(), hit.node_id, "data-set")
    }

    /// A press inside the settings panel. Returns false when the panel is not
    /// open, or the press missed every control — which closes it.
    fn menu_action(&mut self) -> bool {
        if self.menu.is_none() {
            return false;
        }
        let Some(what) = self.menu_under_pointer() else {
            self.menu = None;
            self.adding_folder = None;
            self.request_redraw();
            return true;
        };

        match what.split_once(':') {
            Some(("theme", name)) => {
                self.style.theme = match name {
                    "white" => Theme::White,
                    "grey" => Theme::Grey,
                    "night" => Theme::Night,
                    _ => Theme::Sepia,
                };
                style::set_setting("theme", name);
                self.lib_doc = None;
                self.hud_doc = None;
            }
            Some(("drop", path)) => {
                let keep: Vec<std::path::PathBuf> = library::folders()
                    .into_iter()
                    .filter(|p| p.display().to_string() != path)
                    .collect();
                if let Err(e) = library::set_folders(&keep) {
                    eprintln!("omaread: {e}");
                }
            }
            _ if what == "add" => self.adding_folder = Some(String::new()),
            _ => {}
        }
        self.build_menu();
        true
    }

    /// Add whatever folder was typed, and pick up what is in it.
    fn add_folder(&mut self) {
        let Some(typed) = self.adding_folder.take() else { return };
        let dir = library::expand_home(typed.trim());
        if !dir.is_dir() {
            eprintln!("omaread: {} is not a folder", dir.display());
            self.build_menu();
            return;
        }
        let mut dirs = library::folders();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
        if let Err(e) = library::set_folders(&dirs) {
            eprintln!("omaread: {e}");
        }
        self.rescan();
        self.build_menu();
    }

    /// Follow whatever `<a>` is under the pointer. Returns true when it dealt
    /// with the press, so the page does not also start a selection.
    ///
    /// A footnote or a bibliography entry opens where you are; anything longer
    /// is a place, so go there — and remember where you were.
    fn link_action(&mut self) -> bool {
        if self.view != View::Reading {
            return false;
        }
        // A popup takes the next click, whatever it lands on.
        if self.popup.take().is_some() {
            self.request_redraw();
            return true;
        }
        let href = {
            let Some(ch) = self.chapter.as_ref() else { return false };
            let (fx, fy) = self.pointer_in_flow();
            match href_at(&ch.doc, fx, fy) {
                Some(h) => h,
                None => return false,
            }
        };
        // Off-book links are §3's confirm-then-xdg-open, which is not built:
        // better to do nothing than to open a browser without asking.
        if href.contains("://") || href.starts_with("mailto:") {
            eprintln!("omaread: external link {href} (not opened)");
            return true;
        }
        let Some((spine, frag)) = self.resolve_href(&href) else { return false };

        if let Some(text) = self.note_at(spine, frag.as_deref()) {
            self.open_note(&text);
            return true;
        }

        self.back = Some((self.index, self.page, self.here_label()));
        self.hud_doc = None;
        self.go_to(spine, frag.as_deref());
        true
    }

    /// Split a book-relative href into the spine item it names and its fragment.
    fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        let (path, frag) = match href.split_once('#') {
            Some((p, f)) => (p, Some(f.to_string())),
            None => (href, None),
        };
        let book = self.book.as_ref()?;
        if path.is_empty() {
            return Some((self.index, frag));
        }
        // Relative to the chapter it was found in, and normalised the same way
        // the resource provider normalises: same input, same answer.
        let here = book.chapter_href(self.index).unwrap_or_default();
        let dir = here.rsplit_once('/').map_or("", |(d, _)| d);
        let target = net::in_archive_path(&format!("{dir}/{path}"))?;
        (0..book.chapter_count())
            .find(|&i| {
                book.chapter_href(i).and_then(net::in_archive_path).as_deref() == Some(&target)
            })
            .map(|i| (i, frag))
    }

    /// The text of a link target, when the target is a note rather than a place.
    ///
    /// A footnote, an endnote and a bibliography entry are all a short block of
    /// text; a section is a heading, a wrapper, or pages of prose. That is the
    /// whole test — the `epub:type="noteref"` a spec would use is carried by 4
    /// books of 60 in the real library, so it cannot be the rule.
    fn note_at(&self, spine: usize, frag: Option<&str>) -> Option<String> {
        let frag = frag?;
        let owned;
        let ch = match spine == self.index {
            true => self.chapter.as_ref()?,
            // Notes usually live in their own spine item, so lay it out — a
            // chapter is 20–40ms and this is a click, not a frame.
            false => {
                let book = self.book.clone()?;
                let cb = self.callback();
                owned = chapter::load(
                    &book,
                    spine,
                    &self.style,
                    self.viewport(),
                    self.page_height(),
                    self.hyphenator.as_ref(),
                    cb,
                )
                .ok()?;
                &owned
            }
        };
        let node = find_by_attr(ch.dom(), 0, "id", frag)?;
        let tag = chapter::tag_of(ch.dom(), node)?;
        let text = chapter::text_of(ch.dom(), node);
        is_note(&tag, &text).then_some(text)
    }

    fn open_note(&mut self, text: &str) {
        let (_, fg, subtle, panel) = self.chrome();
        let vp = chapter::viewport(
            self.size.0,
            self.size.1,
            self.scale,
            self.style.theme == Theme::Night,
        );
        self.popup = chapter::layout_document(
            hud::note_html(text),
            hud::note_stylesheet(&fg, &subtle, &panel),
            None,
            vp,
            self.page_height(),
        );
        self.request_redraw();
    }

    /// Go to a spine item, landing on a fragment's page when there is one.
    fn go_to(&mut self, spine: usize, frag: Option<&str>) {
        if spine != self.index {
            self.load_chapter(spine);
        }
        self.page = match frag
            .and_then(|f| {
                let ch = self.chapter.as_ref()?;
                let node = find_by_attr(ch.dom(), 0, "id", f)?;
                Some(ch.pages.page_containing(chapter::node_top(ch.dom(), node)?))
            }) {
            Some(page) => page,
            None if spine != self.index => 0,
            None => self.page,
        };
        self.resume_page = self.page;
        self.save_position();
        self.request_redraw();
    }

    /// Back to where the last link was followed from.
    fn go_back(&mut self) {
        let Some((spine, page, _)) = self.back.take() else { return };
        if spine != self.index {
            self.load_chapter(spine);
        }
        self.page = page.min(self.page_count().saturating_sub(1));
        self.resume_page = self.page;
        self.hud_doc = None;
        self.save_position();
        self.request_redraw();
    }

    /// Where the reader is, worded for the way-back button. Always about the
    /// whole book: a chapter page number beside a whole-book percentage reads as
    /// a contradiction — "back to page 5" from 32% of the way in.
    fn here_label(&self) -> String {
        match self.measured_pages() {
            Some(_) => format!("Back to page {}", self.book_page().0),
            // Not measured, so there is no honest page number for the book —
            // the readout says a percentage here too.
            None => format!("Back to {}%", (self.progress() * 100.0).round() as u8),
        }
    }

    /// What the foot of the page says.
    ///
    /// Both readings are about the whole book. An unmeasured layout used to fall
    /// back to the chapter's own numbering — true, but "page 3 of 8" beside a
    /// book you are 37% through reads as a bug, and two columns hit it every
    /// time because halving the column width is a different layout.
    fn readout(&self) -> String {
        match (self.show_page, self.measured_pages().is_some()) {
            (true, true) => {
                let (page, total) = self.book_page();
                format!("page {page} of {total}")
            }
            _ => format!("{}%", (self.progress() * 100.0).round() as u8),
        }
    }

    /// Page number across the whole book, not the chapter.
    ///
    /// The current chapter is measured — it is laid out — so its pages-per-byte
    /// converts the book's byte length into a page count. Chapters are only ever
    /// paginated on demand, and doing every one of a 131-chapter book to get an
    /// exact total would cost seconds on open.
    ///
    /// ponytail: an estimate, so the total drifts a few percent as you move
    /// between chapters of different density. Cache measured page counts per
    /// (chapter, font size, column width) if it ever needs to be exact.
    fn book_page(&self) -> (usize, usize) {
        match self.measured_pages() {
            Some(per_chapter) => {
                let before: usize = per_chapter.iter().take(self.index).sum();
                let total: usize = per_chapter.iter().sum();
                ((before + self.page + 1).min(total.max(1)), total.max(1))
            }
            // Not measured at this layout: the chapter's own numbering, which is
            // true, rather than an estimate that is not.
            None => (self.page + 1, self.page_count()),
        }
    }

    /// The cached counts, but only if they were measured at the layout now on
    /// screen — pagination depends on font size and column width.
    fn measured_pages(&self) -> Option<&Vec<usize>> {
        (self.pages_layout == self.layout_key()).then_some(self.chapter_pages.as_ref()?)
    }

    fn layout_key(&self) -> String {
        format!("{:.0}x{}", self.style.font_px(), self.column_width())
    }

    /// Measure every chapter so the readout can give a real page number.
    ///
    /// Cached in the database per book and layout, so this runs once: three
    /// seconds for a 131-chapter book, and instant every time after. Called from
    /// the click that asks for page numbers, never from a redraw.
    fn measure_book(&mut self) {
        let key = self.layout_key();
        if self.pages_layout == key && self.chapter_pages.is_some() {
            return;
        }
        let Some(book) = self.book.clone() else { return };
        let count = book.chapter_count();

        if let Some(cached) = self
            .db
            .as_ref()
            .and_then(|d| d.lock().ok())
            .and_then(|d| d.pagination(&self.hash, &key, count))
        {
            self.chapter_pages = Some(cached);
            self.pages_layout = key;
            return;
        }

        println!("omaread: measuring {count} chapters for page numbers…");
        let started = Instant::now();
        let pages = chapter::page_counts(
            &book,
            &self.style,
            self.hyphenator.as_ref(),
            self.column_width(),
            self.size.1,
            self.scale,
            self.page_height(),
        );

        let total: usize = pages.iter().sum();
        println!(
            "omaread: {total} pages at {key} ({:.1}s)",
            started.elapsed().as_secs_f32()
        );
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            if let Err(e) = db.save_pagination(&self.hash, &key, &pages) {
                eprintln!("omaread: could not save page counts: {e}");
            }
        }
        self.chapter_pages = Some(pages);
        self.pages_layout = key;
    }

    /// The stored highlight containing a flow position, if any.
    fn mark_at(&self, flow: (f32, f32)) -> Option<i64> {
        let ch = self.chapter.as_ref()?;
        let db = self.db.as_ref()?.lock().ok()?;
        db.marks_in(&self.hash, self.index).into_iter().find_map(|m| {
            let c = cfi::Cfi::parse(&m.cfi)?;
            let node = cfi::resolve(ch.dom(), &c)?;
            let rects = chapter::highlight_rects(ch.dom(), node, c.offset?, m.length);
            let inside = rects.iter().any(|&(x0, y0, x1, y1)| {
                flow.0 >= x0 && flow.0 <= x1 && flow.1 >= y0 && flow.1 <= y1
            });
            inside.then_some(m.id)
        })
    }

    fn remove_selected_mark(&mut self) {
        let Some(id) = self.sel_mark.take() else { return };
        if let Some(db) = self.db.as_ref().and_then(|d| d.lock().ok()) {
            match db.remove_mark(id) {
                Ok(()) => println!("omaread: highlight removed"),
                Err(e) => eprintln!("omaread: could not remove: {e}"),
            }
        }
        self.sel = None;
        self.request_redraw();
    }

    /// Where each HUD icon goes, as `(icon, rect)` in CSS pixels. The HUD
    /// reserves an empty box per control and the window fills it in.
    fn hud_icons(&self) -> Vec<(paint::Icon, (f32, f32, f32, f32))> {
        let Some(hud) = &self.hud_doc else { return Vec::new() };
        [
            ("bookmark", paint::Icon::Bookmark),
            ("contents", paint::Icon::Contents),
            ("highlight", paint::Icon::Highlight),
            ("back", paint::Icon::Back),
        ]
        .into_iter()
        .filter_map(|(name, icon)| {
            let node = find_by_attr(hud.dom(), 0, "data-icon", name)?;
            Some((icon, chapter::node_rect(hud.dom(), node)?))
        })
        .collect()
    }

    /// A control in the HUD under the pointer, if any. Returns true when it
    /// handled the click, so the page does not also start a selection.
    /// The control under the pointer, by its `data-hud` name.
    ///
    /// The HUD is laid out at window size and painted unscrolled, so screen CSS
    /// pixels are its own coordinates.
    fn hud_under_pointer(&self) -> Option<String> {
        if !self.hud_shown || self.view != View::Reading {
            return None;
        }
        let hud = self.hud_doc.as_ref()?;
        let (x, y) = (self.cursor.0 / self.scale, self.cursor.1 / self.scale);
        let hit = hud.doc.hit(x, y)?;
        ancestor_attr(hud.dom(), hit.node_id, "data-hud")
    }

    fn hud_action(&mut self) -> bool {
        let Some(what) = self.hud_under_pointer() else { return false };

        match what.as_str() {
            "bookmark" => self.toggle_bookmark(),
            "contents" => {
                self.toc_mode = TocMode::Contents;
                self.open_toc();
            }
            "back" => self.go_back(),
            "highlight" => self.highlight_selection(),
            "unhighlight" => self.remove_selected_mark(),
            "library" => {
                self.to_library();
                return true;
            }
            "smaller" => self.set_font_scale(self.style.scale - 0.1),
            "bigger" => self.set_font_scale(self.style.scale + 0.1),
            "columns" => self.toggle_columns(),
            "readout" => {
                self.show_page = !self.show_page;
                if self.show_page {
                    self.measure_book();
                }
            }
            _ => return false,
        }
        // Acting on the HUD is using the HUD, so keep it up.
        self.hud_doc = None;
        self.poke_hud();
        self.request_redraw();
        true
    }

    /// Font size is a property of the reader, not of the book, so it is stored
    /// once and applies to every book from then on.
    fn set_font_scale(&mut self, scale: f32) {
        let scale = scale.clamp(0.8, 1.6);
        if (scale - self.style.scale).abs() < f32::EPSILON {
            return;
        }
        self.style.scale = scale;
        style::set_setting("font-scale", &format!("{scale:.2}"));
        println!("omaread: {:.0}px", self.style.font_px());
        self.restyle();
        if self.show_page {
            self.measure_book();
        }
    }

    fn toggle_columns(&mut self) {
        self.columns = if self.columns == 2 { 1 } else { 2 };
        let got = self.effective_columns();
        println!(
            "omaread: {got} column{}{}",
            plural(got),
            match self.columns == 2 && got == 1 {
                true => " — window too narrow for two",
                false => "",
            }
        );
        // The document is laid out at column width, so the flow has to be
        // re-measured — and page numbers are counted per layout, so they are
        // too. Only on a deliberate change: a window drag is a layout per pixel.
        self.relayout();
        if self.show_page {
            self.measure_book();
        }
        self.request_redraw();
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
        // `page` is one field for all three views, so coming back from page 47
        // of a book landed on page 47 of the shelf. The shelf starts at the top,
        // on the book just closed — which has usually moved to the front of a
        // Recent sort, so the old index pointed at somebody else.
        self.selected = self
            .rows
            .iter()
            .position(|r| r.hash == self.hash)
            .unwrap_or(0);
        self.page = 0;
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
        let n = entries.len();
        let (heading, subtitle) = match self.toc_mode {
            TocMode::Search => (
                "Search",
                format!("“{}” — {n} hit{} in this book", self.find, plural(n)),
            ),
            TocMode::Marks => (
                "Marks",
                match &self.noting {
                    Some(_) => format!("note: {}|", self.note),
                    None => format!("{n} bookmark{} and highlights", plural(n)),
                },
            ),
            TocMode::Contents => ("Contents", book.title.clone()),
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
        match self.toc_mode {
            TocMode::Contents => return (*book.toc).clone(),
            TocMode::Marks => {
                return self
                    .marks
                    .iter()
                    .map(|m| {
                        let spine = cfi::Cfi::parse(&m.cfi).map_or(0, |c| c.spine);
                        let kind = if m.is_bookmark() { "Bookmark" } else { "Highlight" };
                        let body = match (m.text.is_empty(), m.note.is_empty()) {
                            (true, true) => format!("chapter {}", spine + 1),
                            (true, false) => m.note.clone(),
                            (false, true) => m.text.clone(),
                            (false, false) => format!("{} — {}", m.text, m.note),
                        };
                        book::TocEntry {
                            label: format!("{kind} · {body}"),
                            depth: 0,
                            spine,
                            fragment: None,
                            find: None,
                            cfi: Some(m.cfi.clone()),
                        }
                    })
                    .collect();
            }
            TocMode::Search => {}
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
                    cfi: None,
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
        self.toc_sel = match self.toc_mode {
            TocMode::Contents => toc.iter().rposition(|e| e.spine <= self.index).unwrap_or(0),
            // A search starts at the best hit; marks are in reading order.
            _ => 0,
        };
        self.view = View::Toc;
        self.page = 0;
        self.build_toc();
        self.page = self.toc_doc.as_ref().and_then(|d| page_of_index(d, self.toc_sel)).unwrap_or(0);
        self.request_redraw();
    }

    fn close_toc(&mut self) {
        self.toc_mode = TocMode::Contents;
        self.noting = None;
        self.view = View::Reading;
        self.page = self.resume_page.min(self.page_count().saturating_sub(1));
        self.request_redraw();
    }

    /// Navigate to a contents entry, landing on its fragment's page when it has
    /// one — books that keep several chapters in one spine file would otherwise
    /// send every entry to page 1.
    fn open_toc_entry(&mut self, i: usize) {
        let Some(entry) = self.book.as_ref().and_then(|b| b.toc.get(i).cloned()) else { return };
        if self.chapter.is_some() {
            self.back = Some((self.index, self.resume_page, self.here_label()));
            self.hud_doc = None;
        }
        self.view = View::Reading;
        if entry.spine == self.index && self.chapter.is_some() {
            self.page = 0;
        } else {
            self.load_chapter(entry.spine);
        }
        // A stored mark knows its element exactly.
        if let Some(raw) = &entry.cfi {
            match cfi::Cfi::parse(raw)
                .and_then(|c| {
                    let ch = self.chapter.as_ref()?;
                    let node = cfi::resolve(ch.dom(), &c)?;
                    Some(ch.pages.page_containing(chapter::node_top(ch.dom(), node)?))
                }) {
                Some(page) => self.page = page,
                None => eprintln!("omaread: {raw} no longer resolves"),
            }
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
    fn effective_columns(&self) -> usize {
        columns_for(
            self.columns,
            self.size.0 as f32 / self.scale,
            self.style.font_px(),
            self.view == View::Reading,
        )
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
        // The character the page began on, when the saved position names one;
        // the paragraph's own top otherwise.
        let y = c
            .offset
            .and_then(|off| {
                chapter::highlight_rects(ch.dom(), node, off, 1).first().map(|r| r.1)
            })
            .or_else(|| chapter::node_top(ch.dom(), node));
        if let Some(y) = y {
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
        let Some(mut c) = cfi::of_node(ch.dom(), node, self.index) else { return };
        // Which character of it the page starts on. Without this the address is
        // the paragraph, and a paragraph three pages long resumes at its first.
        c.offset = chapter::char_at(ch.dom(), node, top);
        let title = self.book.as_ref().map(|b| b.title.as_str()).unwrap_or("");
        let progress = self.progress();
        if let Err(e) =
            db.save_progress(&self.hash, &self.path, title, &c.to_string(), progress)
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
        let started = Instant::now();
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
                    if std::env::var_os("OMAREAD_DEBUG_TIME").is_some() {
                        eprintln!(
                            "TIME chapter {index} load+paginate: {:.0}ms",
                            started.elapsed().as_secs_f32() * 1000.0
                        );
                    }
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
        let was = self.page;
        if forward {
            let last = self.page_count().saturating_sub(1);
            if self.page + step <= last {
                self.page += step;
                self.slide = Some(Slide { from: was, forward, at: Instant::now() });
            } else if self.index + 1 < self.chapter_count() {
                self.load_chapter(self.index + 1);
            } else {
                return;
            }
        } else if self.page >= step {
            self.page -= step;
            self.slide = Some(Slide { from: was, forward, at: Instant::now() });
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
        // Laid out for the old window; it comes back on the next click.
        self.popup = None;
        if self.view != View::Reading {
            let vp = self.viewport();
            let ph = self.page_height();
            if let Some(doc) = self.doc_mut() {
                chapter::relayout(doc, vp, ph);
                let last = doc.pages.count().saturating_sub(1);
                self.page = self.page.min(last);
            }
            // Re-paginating in place moves the page boundaries, so a resize can
            // put cards on screen that this document was never given covers for
            // — the newly revealed row came up bare. Rebuild when the set of
            // visible cards actually changes, which during a drag is a handful
            // of times rather than every pixel.
            if self.view == View::Library {
                let now = self.lib_doc.as_ref().map(|d| visible_cards(d, self.page));
                if now.is_some_and(|now| now != self.lib_covers) {
                    self.lib_doc = None;
                }
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
        let started = Instant::now();
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
            // A different page shows different cards, and only the cards on the
            // page carry covers.
            View::Library if self.lib_doc.is_none() || self.page != self.lib_page => {
                self.build_library();
                // A selection on another page has nothing to outline here. Only
                // possible under a sort that does not put the last-read book
                // first, so the second build is rare.
                match self.lib_doc.as_ref().and_then(|d| page_of_index(d, self.selected)) {
                    Some(p) if p != self.page => {
                        self.page = p;
                        self.build_library();
                    }
                    _ => {}
                }
            }
            View::Toc if self.toc_doc.is_none() => self.build_toc(),
            View::Reading if self.hud_shown => self.build_hud(),
            _ => {}
        }
        let showing_hud = self.hud_shown && self.view == View::Reading;
        let cols = self.effective_columns();
        // Both borrow self immutably, so they cannot wait until after the
        // disjoint-field destructure below.
        let (highlights, selection) = match self.view {
            View::Reading => (self.highlight_rects(), self.selection_rects()),
            _ => (Vec::new(), Vec::new()),
        };
        // The library's selection is painted, so arrow keys cost a frame instead
        // of a rebuild — a rebuild re-requests every cover.
        let card_rect = |index: usize| -> Option<(f32, f32, f32, f32)> {
            let doc = self.lib_doc.as_ref()?;
            let node = find_by_attr(doc.dom(), 0, "data-index", &index.to_string())?;
            chapter::node_rect(doc.dom(), node)
        };
        let (selected_card, hovered_card) = match self.view {
            View::Library => (
                card_rect(self.selected),
                self.hover_card.filter(|i| Some(*i) != Some(self.selected)).and_then(card_rect),
            ),
            _ => (None, None),
        };
        let icons = match (showing_hud, self.view) {
            (true, _) => self.hud_icons(),
            // The library's own control: the cog left of the search box.
            (false, View::Library) => self
                .lib_doc
                .as_ref()
                .and_then(|d| find_by_attr(d.dom(), 0, "data-icon", "gear"))
                .and_then(|n| {
                    let d = self.lib_doc.as_ref()?;
                    Some(vec![(paint::Icon::Gear, chapter::node_rect(d.dom(), n)?)])
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let ink = parse_hex(&self.chrome().1);

        // Disjoint field borrows: `render` takes the renderer mutably, the
        // closure needs the document mutably to set the page offset. That rules
        // out `doc_mut`, which borrows all of `self`.
        let hover_hud = self.hover_hud.clone();
        let hover_menu = self.hover_menu.clone();
        // How far through a page turn this frame is, eased out, and which page
        // is on its way off. Only in the reading view: the library rebuilds its
        // grid on a page turn to fetch that page's covers, so the outgoing page
        // would lose its covers halfway across.
        let slide = self.slide.filter(|_| self.view == View::Reading).and_then(|s| {
            let t = s.at.elapsed().as_secs_f32() / SLIDE.as_secs_f32();
            (t < 1.0).then(|| (s, 1.0 - (1.0 - t) * (1.0 - t)))
        });
        let App { renderer, chapter, lib_doc, toc_doc, hud_doc, popup, menu, view, .. } = self;
        let hud = if showing_hud { hud_doc.as_mut() } else { None };
        let Some(ch) = (match view {
            View::Library => lib_doc.as_mut(),
            View::Toc => toc_doc.as_mut(),
            View::Reading => chapter.as_mut(),
        }) else {
            return;
        };
        if std::env::var_os("OMAREAD_DEBUG_PAINT").is_some() {
            eprintln!(
                "PAINT page {}/{} cols={cols} top={:.0} page_h={page_h:.0} win={w}x{h}",
                page + 1,
                ch.pages.count(),
                ch.pages.top_of(page),
            );
        }
        let count = ch.pages.count();
        // Read the flow slices before borrowing the document mutably: each
        // column is a different page of the same flow.
        let slices: Vec<(f32, f32)> = (0..cols)
            .map(|c| page + c)
            .map(|p| match p < count {
                true => (ch.pages.top_of(p), ch.pages.extent_of(p)),
                false => (0.0, 0.0),
            })
            .collect();
        // The page being left, in the same flow: a turn within one chapter is
        // two slices of one document, which is exactly what two columns are.
        let outgoing: Vec<(f32, f32)> = match slide {
            Some((s, _)) => (0..cols)
                .map(|c| s.from + c)
                .map(|p| match p < count {
                    true => (ch.pages.top_of(p), ch.pages.extent_of(p)),
                    false => (0.0, 0.0),
                })
                .collect(),
            None => Vec::new(),
        };
        let doc = &mut ch.doc;
        let frame = paint::Frame {
            width: w,
            height: h,
            scale,
            margin,
            page_height: page_h,
        };
        let col_w = (w as f32 / scale as f32) / cols as f32;

        renderer.render(|scene| {
            // An engine panic while painting must not take the window with it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // Two columns are two pages of one flow side by side, not CSS
                // multicol: the same document painted twice at two scroll
                // offsets (CONTEXT.md §3).
                if let Some((s, t)) = slide {
                    // The old page walks off the way you turned, the new one
                    // follows it in. Both are the same document at two offsets.
                    let (out_x, in_x) = slide_offsets(s.forward, t, w as f32 / scale as f32);
                    paint::clear(scene, ground, &frame);
                    for (dx, pages) in [(out_x, &outgoing), (in_x, &slices)] {
                        for (col, &(top, extent)) in pages.iter().enumerate() {
                            let x = dx + col as f32 * col_w;
                            paint::column(scene, doc, top, extent, x, col_w, &frame, ground, false);
                        }
                    }
                } else {
                for (col, &(top, extent)) in slices.iter().enumerate() {
                    let x = col as f32 * col_w;
                    if page + col >= count {
                        // Past the end of the chapter: clean paper, not whatever
                        // the previous frame left there.
                        paint::column(scene, doc, 0.0, 0.0, x, col_w, &frame, ground, col == 0);
                        continue;
                    }
                    paint::column(scene, doc, top, extent, x, col_w, &frame, ground, col == 0);
                    paint::bands(scene, &highlights, HIGHLIGHT, top, extent, x, &frame);
                    paint::bands(scene, &selection, SELECTION, top, extent, x, &frame);
                }
                }
                // A wash under the pointer, before the outline, so a card that
                // is both hovered and selected still reads as selected.
                if let Some((cx, cy, cw, chh)) = hovered_card {
                    let (top, extent) = slices.first().copied().unwrap_or((0.0, 0.0));
                    paint::bands(
                        scene,
                        &[(cx, cy, cx + cw, cy + chh)],
                        HOVER,
                        top,
                        extent,
                        0.0,
                        &frame,
                    );
                }
                if let Some((cx, cy, cw, chh)) = selected_card {
                    // Flow -> screen: the grid is a page like any other.
                    let (top, _) = slices.first().copied().unwrap_or((0.0, 0.0));
                    paint::outline(
                        scene,
                        (cx, cy - top + margin, cw, chh),
                        CARD_OUTLINE,
                        2.0,
                        scale,
                    );
                }
                if let Some(note) = popup.as_mut() {
                    paint::overlay(scene, &mut note.doc, &frame);
                }
                // The library's cog, in flow coordinates like the selection
                // outline: the shelf is a page, so it carries the page margin.
                if matches!(view, View::Library) {
                    let (top, _) = slices.first().copied().unwrap_or((0.0, 0.0));
                    for &(icon, (cx, cy, cw, chh)) in &icons {
                        paint::icon(scene, icon, (cx, cy - top + margin, cw, chh), ink, scale);
                    }
                }
                if let Some(m) = menu.as_mut() {
                    paint::overlay(scene, &mut m.doc, &frame);
                    if let Some(rect) = hover_menu
                        .as_deref()
                        .and_then(|r| find_by_attr(m.dom(), 0, "data-set", r))
                        .and_then(|n| chapter::node_rect(m.dom(), n))
                    {
                        paint::wash(scene, rect, MENU_HOVER, scale);
                    }
                }
                if let Some(hud) = hud {
                    paint::overlay(scene, &mut hud.doc, &frame);
                    for &(icon, rect) in &icons {
                        paint::icon(scene, icon, rect, ink, scale);
                    }
                    // Over the bar, not under it: the bar's ground is opaque.
                    if let Some(rect) = hover_hud
                        .as_deref()
                        .and_then(|w| find_by_attr(hud.dom(), 0, "data-hud", w))
                        .and_then(|n| chapter::node_rect(hud.dom(), n))
                    {
                        paint::wash(scene, rect, HUD_HOVER, scale);
                    }
                }
            }));
        });

        if std::env::var_os("OMAREAD_DEBUG_TIME").is_some() {
            eprintln!("TIME frame: {:.1}ms", started.elapsed().as_secs_f32() * 1000.0);
        }
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        // A note is read and dismissed; nothing else happens while it is up.
        if self.popup.take().is_some() {
            self.request_redraw();
            return;
        }
        match self.view {
            View::Library => self.library_key(event_loop, key),
            View::Reading => self.reading_key(event_loop, key),
            View::Toc => self.toc_key(event_loop, key),
        }
    }

    fn library_key(&mut self, event_loop: &ActiveEventLoop, key: Key) {
        let cols = self.cards_per_row();

        // The panel takes every key while it is up, the same way the tag box
        // below takes every letter while it is being typed.
        if self.menu.is_some() {
            match (key, self.adding_folder.is_some()) {
                (Key::Named(NamedKey::Escape), true) => self.adding_folder = None,
                (Key::Named(NamedKey::Escape), false) => self.menu = None,
                (Key::Named(NamedKey::Enter), true) => return self.add_folder(),
                (Key::Named(NamedKey::Backspace), true) => {
                    if let Some(t) = self.adding_folder.as_mut() {
                        t.pop();
                    }
                }
                (Key::Character(c), true) => {
                    if let Some(t) = self.adding_folder.as_mut() {
                        t.push_str(c.as_str());
                    }
                }
                (Key::Named(NamedKey::Space), true) => {
                    if let Some(t) = self.adding_folder.as_mut() {
                        t.push(' ');
                    }
                }
                (Key::Named(NamedKey::F5), _) => self.rescan(),
                _ => return,
            }
            match self.menu.is_some() {
                true => self.build_menu(),
                false => self.request_redraw(),
            }
            return;
        }

        // While a tag is being typed every letter is tag text, so this runs
        // before the search-and-navigate keys below.
        if self.tagging.is_some() {
            match key {
                Key::Named(NamedKey::Escape) => self.tagging = None,
                Key::Named(NamedKey::Enter) => self.apply_tag(),
                Key::Named(NamedKey::Backspace) => {
                    if let Some(t) = self.tagging.as_mut() {
                        t.pop();
                    }
                }
                Key::Character(c) => {
                    if let Some(t) = self.tagging.as_mut() {
                        t.push_str(c.as_str());
                    }
                }
                Key::Named(NamedKey::Space) => {
                    if let Some(t) = self.tagging.as_mut() {
                        t.push('-');
                    }
                }
                _ => return,
            }
            self.suggestions = self.completions();
            self.lib_doc = None;
            self.request_redraw();
            return;
        }

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
            // Moving the selection paints; it does not rebuild.
            Key::Named(NamedKey::ArrowRight) => return self.move_selection(1),
            Key::Named(NamedKey::ArrowLeft) => return self.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) => return self.move_selection(cols),
            Key::Named(NamedKey::ArrowUp) => return self.move_selection(-cols),
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
            // A modifier-free key that is not a letter, for the same reason
            // sort and rescan are: letters go to the search box.
            Key::Named(NamedKey::F2) if !self.rows.is_empty() => {
                self.tagging = Some(String::new());
                self.suggestions = self.completions();
            }
            Key::Character(c) => {
                self.query.push_str(c.as_str());
                self.reload_rows();
            }
            _ => return,
        }
        self.lib_doc = None;
        self.request_redraw();
    }

    /// The library card under the pointer, if any.
    fn card_under_pointer(&self) -> Option<usize> {
        if self.view != View::Library {
            return None;
        }
        let doc = self.lib_doc.as_ref()?;
        let x = self.cursor.0 / self.scale;
        let y = self.cursor.1 / self.scale - self.page_margin() + doc.pages.top_of(self.page);
        let hit = doc.doc.hit(x, y)?;
        ancestor_attr(doc.dom(), hit.node_id, "data-index")?.parse().ok()
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
                // The gear opens the panel; a press anywhere else in it is
                // handled by `menu_action`, which ran before this.
                if ancestor_attr(doc.dom(), hit.node_id, "data-menu").is_some() {
                    self.open_menu();
                    return;
                }
                // A suggestion is a click target too; check it first, since a
                // card and a suggestion are both just boxes in this document.
                if let Some(text) = ancestor_attr(doc.dom(), hit.node_id, "data-suggest") {
                    self.query = text;
                    self.reload_rows();
                    self.request_redraw();
                    return;
                }
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
        // The grid owns its own geometry; a second copy of these numbers here is
        // how arrow keys start stepping onto columns that do not exist.
        grid::per_row(self.size.0 as f32 / self.scale) as isize
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
        self.request_redraw();
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
                    self.toc_mode = TocMode::Search;
                    self.open_toc();
                }
                "m" => self.open_marks(),
                "b" => self.toggle_bookmark(),
                "h" => self.highlight_selection(),
                "y" => self.copy_selection(),
                "c" => self.toggle_columns(),
                "t" => {
                    self.style.theme = match self.style.theme {
                        Theme::White => Theme::Sepia,
                        Theme::Sepia => Theme::Grey,
                        Theme::Grey => Theme::Night,
                        Theme::Night => Theme::White,
                    };
                    println!("omaread: theme {:?}", self.style.theme);
                    style::set_setting("theme", &format!("{:?}", self.style.theme).to_lowercase());
                    self.restyle();
                }
                "+" | "=" => self.set_font_scale(self.style.scale + 0.1),
                "-" => self.set_font_scale(self.style.scale - 0.1),
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
                if self.noting.is_some() {
                    self.noting = None;
                    self.note.clear();
                } else if self.toc_mode == TocMode::Search && !self.find.is_empty() {
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
            Key::Named(NamedKey::Backspace) if self.noting.is_some() => {
                self.note.pop();
            }
            Key::Named(NamedKey::Space) if self.noting.is_some() => self.note.push(' '),
            Key::Character(c) if self.noting.is_some() => self.note.push_str(c.as_str()),
            Key::Named(NamedKey::Backspace) if self.toc_mode == TocMode::Search => {
                self.find.pop();
                self.toc_sel = 0;
            }
            Key::Named(NamedKey::Space) if self.toc_mode == TocMode::Search => {
                self.find.push(' ');
                self.toc_sel = 0;
            }
            // While searching every letter is query text, so the single-key
            // commands below are unreachable — which is what you want when you
            // are typing a word that happens to contain "q".
            Key::Character(c) if self.toc_mode == TocMode::Search => {
                self.find.push_str(c.as_str());
                self.toc_sel = 0;
            }
            // Marks mode has room for commands, because nothing is being typed.
            Key::Character(c) if self.toc_mode == TocMode::Marks => match c.as_str() {
                "n" => self.begin_note(),
                "x" => self.delete_mark(),
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
            Key::Named(NamedKey::Enter) => {
                if self.noting.is_some() {
                    self.save_note();
                } else {
                    self.open_toc_entry(self.toc_sel);
                    return;
                }
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

/// The `href` under a point in the flow, or within a few pixels of it.
///
/// A note reference is a superscript digit — a 10px target in a 660px measure,
/// and it sits inside a sentence, so the pointer lands beside it as often as on
/// it. The exact point wins; the ring is only consulted when it hits nothing.
///
/// ponytail: eight probes at a fixed radius. Measuring the glyph's own box
/// would be exact, but an inline element has no Taffy box to measure
/// (CONTEXT.md §9) — the run lives in the paragraph's parley layout.
fn href_at(doc: &blitz_html::HtmlDocument, x: f32, y: f32) -> Option<String> {
    const R: f32 = 7.0;
    [
        (0.0, 0.0),
        (-R, 0.0),
        (R, 0.0),
        (0.0, -R),
        (0.0, R),
        (-R, -R),
        (R, -R),
        (-R, R),
        (R, R),
    ]
    .into_iter()
    .find_map(|(dx, dy)| {
        let hit = doc.hit(x + dx, y + dy)?;
        ancestor_attr(doc, hit.node_id, "href")
    })
}

/// Is this link target a note to read here, or a place to go to?
///
/// A footnote, an endnote and a bibliography entry are all a short block of
/// text; a section is a heading, a wrapper, or pages of prose. That is the whole
/// test — `epub:type="noteref"`, which a spec would key on, is carried by 4
/// books of 60 sampled from the real library, so it cannot be the rule.
///
/// ponytail: a length cut-off. A very long footnote goes to its page like a
/// section; key on `epub:type`/`role` as well if a real book gets it wrong.
fn is_note(tag: &str, text: &str) -> bool {
    const NOTE_MAX: usize = 700;
    matches!(tag, "p" | "li" | "aside" | "div" | "span" | "td")
        && !text.trim().is_empty()
        && text.chars().count() <= NOTE_MAX
}

/// Where the outgoing and incoming pages sit, `t` of the way through a turn.
///
/// Turning forward walks the old page off to the left and brings the new one in
/// from the right; turning back is the same motion mirrored, which is the whole
/// point — the direction is the feedback.
fn slide_offsets(forward: bool, t: f32, width: f32) -> (f32, f32) {
    let dir = if forward { -1.0 } else { 1.0 };
    (dir * t * width, (dir * t - dir) * width)
}

/// Which cards sit on `page`, from a laid-out grid.
fn visible_cards(doc: &Chapter, page: usize) -> std::ops::Range<usize> {
    let top = doc.pages.top_of(page);
    let bottom = top + doc.pages.extent_of(page);
    let on_page: Vec<usize> = chapter::indexed_tops(doc.dom())
        .into_iter()
        .filter(|&(_, y)| y >= top - 1.0 && y < bottom)
        .map(|(i, _)| i)
        .collect();
    match (on_page.first(), on_page.last()) {
        (Some(&a), Some(&b)) => a..b + 1,
        _ => 0..0,
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
pub fn ancestor_attr(dom: &blitz_dom::BaseDocument, mut id: usize, attr: &str) -> Option<String> {
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
            // The pointer left, so nothing is under it.
            WindowEvent::CursorLeft { .. } => {
                if self.hover_card.take().is_some()
                    | self.hover_hud.take().is_some()
                    | self.hover_menu.take().is_some()
                {
                    self.request_redraw();
                }
            }

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
                    if self.dragging {
                        self.extend_selection();
                    }
                    // Repaint only when the card under the pointer changes, not
                    // on every motion event.
                    let over = self.card_under_pointer();
                    if over != self.hover_card {
                        self.hover_card = over;
                        self.request_redraw();
                    }
                    self.poke_hud();
                    let on = self.hud_under_pointer();
                    if on != self.hover_hud {
                        self.hover_hud = on;
                        self.request_redraw();
                    }
                    let row = self.menu_under_pointer();
                    if row != self.hover_menu {
                        self.hover_menu = row;
                        self.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. }
                if button == winit::event::MouseButton::Left =>
            {
                match state {
                    // In the page, a press anchors a selection; in the library
                    // and the contents it opens whatever is under the pointer.
                    ElementState::Pressed if self.view == View::Reading => {
                        // The HUD is on top of the page, so it gets the click,
                        // then a link, and only then does the page take it.
                        if !self.hud_action() && !self.link_action() {
                            self.begin_selection();
                        }
                    }
                    // The panel is over the shelf, so it gets the press first.
                    ElementState::Pressed if self.menu_action() => {}
                    ElementState::Pressed => self.on_click(),
                    ElementState::Released => {
                        self.dragging = false;
                        // A press with no drag is a click, not a selection.
                        if self.sel.as_ref().is_some_and(|(_, s)| s.is_collapsed()) {
                            self.sel = None;
                            self.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y,
                    MouseScrollDelta::PixelDelta(p) => -p.y as f32,
                };
                if dy.abs() > 0.5 {
                    // The reading turn crosses chapters and saves a position;
                    // the library and the contents just page their document.
                    match self.view {
                        View::Reading => self.turn(dy > 0.0),
                        _ => self.turn_view(dy > 0.0),
                    }
                }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.on_key(event_loop, event.logical_key);
            }

            // The guard is around the whole redraw, not only around painting
            // the documents: acquiring the surface texture happens outside that
            // inner one, and wgpu turns a compositor that does not hand a buffer
            // back in time into `failed to get surface texture: Timeout`. A
            // frame nobody could present is a frame worth losing, not a window.
            WindowEvent::RedrawRequested => {
                if catch_unwind(AssertUnwindSafe(|| self.redraw())).is_err() {
                    eprintln!("omaread: dropped a frame the compositor would not take");
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_resources();

        if let Some(at) = self.exit_at {
            if Instant::now() >= at {
                self.save_position();
                event_loop.exit();
                return;
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            return;
        }

        // A page turn animates, so it drives the clock while it lasts.
        if let Some(s) = self.slide {
            if s.at.elapsed() >= SLIDE {
                self.slide = None;
            }
            self.request_redraw();
            event_loop
                .set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
            return;
        }

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
