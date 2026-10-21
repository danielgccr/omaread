//! Building a laid-out chapter document from a book.
//!
//! blitz-dom is pre-1.0 and panics on real-world books (CONTEXT.md §8), so every
//! entry point here is wrapped: a hostile chapter must not take the app with it.

use crate::book::Book;
use crate::hyphen::Hyphenator_;
use crate::net::{BookNetProvider, ORIGIN};
use crate::paginate::{self, Atom, AtomKind, Pages};
use crate::style::{self, ReadingStyle};
use blitz_dom::net::Resource;
use blitz_dom::{BaseDocument, DocumentConfig, NodeData};
use blitz_html::HtmlDocument;
use blitz_traits::net::{NetProvider, SharedCallback};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

thread_local! {
    /// The font context every document starts from, cloned. Building one
    /// scans the system's fonts through fontconfig: 22ms, and blitz paid it for
    /// every document it made — two per library rebuild, one per chapter, menu,
    /// overlay — which was most of what made the library feel heavy. A clone
    /// shares the scan (fontique keeps it behind an `Arc`) and the bundled
    /// faces, registered here once.
    ///
    /// Supplying a context makes blitz skip its own list-bullet font. Its
    /// markers ask for "Bullet, monospace, sans-serif", so they come from the
    /// system monospace instead, which carries • ◦ ▪.
    static FONT_CTX: parley::FontContext = {
        let mut ctx = parley::FontContext::default();
        for face in FONTS {
            ctx.collection.register_fonts(parley::fontique::Blob::new(Arc::new(face) as _), None);
        }
        ctx
    };
}

/// The bundled reading faces, embedded in the binary (CONTEXT.md §3). Books'
/// own `@font-face` is ignored, so these are the faces the reader uses; the
/// system's fonts remain only as a last resort for glyphs none of these carry.
///
/// Literata ships as a variable font: fontique reads its `wght` axis and sets
/// the weight from CSS, so one file per slant covers every weight the base
/// stylesheet asks for. Charis SIL is here for coverage Literata lacks, and
/// carries no bold — a fallback face reached for a rare glyph can synthesise
/// one.
const FONTS: [&[u8]; 7] = [
    include_bytes!("../assets/fonts/Literata-Variable.ttf"),
    include_bytes!("../assets/fonts/Literata-Italic-Variable.ttf"),
    include_bytes!("../assets/fonts/CharisSIL-Regular.ttf"),
    include_bytes!("../assets/fonts/CharisSIL-Italic.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Italic.ttf"),
];

pub struct Chapter {
    pub doc: HtmlDocument,
    pub index: usize,
    pub pages: Pages,
}

impl Chapter {
    pub fn dom(&self) -> &BaseDocument {
        &self.doc
    }

    /// Total characters of laid-out inline text. Zero means nothing will paint.
    pub fn text_len(&self) -> usize {
        let mut n = 0;
        walk_elements(self.dom(), 0, &mut |_, el| {
            if let Some(tl) = el.inline_layout_data.as_ref() {
                n += tl.text.len();
            }
        });
        n
    }

    /// Line boxes across the chapter — the atoms Phase 2 paginates on.
    pub fn line_count(&self) -> usize {
        let mut n = 0;
        walk_elements(self.dom(), 0, &mut |_, el| {
            if let Some(tl) = el.inline_layout_data.as_ref() {
                n += tl.layout.lines().count();
            }
        });
        n
    }

    /// Total height of the laid-out flow, in CSS pixels.
    pub fn content_height(&self) -> f32 {
        content_height(self.dom())
    }
}

/// Build the page table for a laid-out document.
pub fn paginate_dom(dom: &BaseDocument, page_height: f32) -> Pages {
    let atoms = collect_atoms(dom);
    Pages { ..paginate::paginate(&atoms, content_height(dom), page_height) }
}

/// Everything a page break must not cut through.
///
/// Prose contributes line boxes, tables contribute row bands derived from cell
/// geometry (`<tr>` carries no layout box of its own — CONTEXT.md §9), and
/// replaced elements contribute their whole box.
pub fn collect_atoms(dom: &BaseDocument) -> Vec<Atom> {
    let mut atoms = Vec::new();
    // (absolute y of each cell, height, owning table id, a header cell) before
    // banding.
    let mut cells: Vec<(f32, f32, usize, bool)> = Vec::new();
    // Atoms found inside a table, held back to be absorbed into row bands.
    let mut in_table: Vec<Atom> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn walk(
        dom: &BaseDocument,
        id: usize,
        parent_y: f32,
        table: Option<usize>,
        thead: bool,
        atoms: &mut Vec<Atom>,
        cells: &mut Vec<(f32, f32, usize, bool)>,
        in_table: &mut Vec<Atom>,
    ) {
        let Some(node) = dom.get_node(id) else { return };
        let l = &node.final_layout;
        let abs_y = parent_y + l.location.y;

        let mut table = table;
        let mut thead = thead;

        // An anonymous block is a `div` to this match, and holds lines.
        if let Some(el) = node.element_data() {
            let tag = el.name.local.as_ref();

            match tag {
                "table" => (table, thead) = (Some(id), false),
                "thead" => thead = true,
                "td" | "th" => {
                    if let Some(t) = table {
                        cells.push((abs_y, l.size.height, t, thead || tag == "th"));
                    }
                }
                "img" | "svg" | "hr" | "video" if l.size.height > 0.0 => {
                    let atom = Atom {
                        top: abs_y,
                        bottom: abs_y + l.size.height,
                        group: id,
                        kind: AtomKind::Block,
                        keep_with_next: false,
                    };
                    if table.is_some() { in_table.push(atom) } else { atoms.push(atom) }
                }
                _ => {}
            }

            // `data-atom` marks a box a break must not cut through — a library
            // card, whose cover would otherwise land on one page and its title
            // on the next. The box *is* the atom, so its contents are not
            // walked: a clipped title lays out past the card it is clipped to,
            // and those stray lines vetoed every break down the page.
            let marked = el.attrs.iter().any(|a| &*a.name.local == "data-atom");
            if marked && l.size.height > 0.0 {
                let atom = Atom {
                    top: abs_y,
                    bottom: abs_y + l.size.height,
                    group: id,
                    kind: AtomKind::Block,
                    keep_with_next: false,
                };
                if table.is_some() { in_table.push(atom) } else { atoms.push(atom) }
                return;
            }

            if let Some(tl) = el.inline_layout_data.as_ref() {
                // The parley layout origin is the element's content box.
                let content_top = abs_y + l.border.top + l.padding.top;
                let heading = matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
                let s = tl.layout.scale();
                for line in tl.layout.lines() {
                    let m = line.metrics();
                    let top = content_top + m.min_coord / s;
                    let bottom = content_top + m.max_coord / s;
                    if bottom > top {
                        let atom = Atom {
                            top,
                            bottom,
                            group: id,
                            kind: AtomKind::Line,
                            keep_with_next: heading,
                        };
                        if table.is_some() { in_table.push(atom) } else { atoms.push(atom) }
                    }
                }
            }
        }

        for child in kids(dom, node).iter() {
            walk(dom, *child, abs_y, table, thead, atoms, cells, in_table);
        }
    }

    walk(dom, 0, 0.0, None, false, &mut atoms, &mut cells, &mut in_table);

    // Band the cells: same table, same top (within a pixel) is one row. A
    // table's header is its leading rows made only of header cells.
    let mut bands: Vec<Atom> = Vec::new();
    cells.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.total_cmp(&b.0)));
    let mut i = 0;
    let mut leading: Option<usize> = None;
    while i < cells.len() {
        let (top, mut height, table, mut head) = cells[i];
        let mut j = i + 1;
        while j < cells.len() && cells[j].2 == table && (cells[j].0 - top).abs() < 1.0 {
            height = height.max(cells[j].1);
            head &= cells[j].3;
            j += 1;
        }
        // Still in this table's leading header rows?
        let first = i == 0 || cells[i - 1].2 != table;
        head &= first || leading == Some(table);
        leading = head.then_some(table);
        if height > 0.0 {
            bands.push(Atom {
                top,
                bottom: top + height,
                group: table,
                kind: if head { AtomKind::Head } else { AtomKind::Row },
                keep_with_next: false,
            });
        }
        i = j;
    }

    // Inside a table the *row* is the atom. Lines within a cell overlap their
    // band, and keeping them vetoes breaks at the very row boundaries the band
    // exists to provide — a chain of overlapping atoms with no legal break,
    // which forces a hard cut. So they are absorbed instead.
    atoms.extend(absorb_into_bands(&mut bands, &in_table));
    separate_bands(&mut bands);
    atoms.extend(bands);

    atoms
}

/// Pull overlapping row bands apart so that every row boundary is a legal break
/// again.
///
/// Growing a band over a line that overflowed its cell can push its bottom past
/// the next band's top, and a run of those turns a table into one unbroken
/// forbidden span — longer than a page, so the paginator has no choice but to
/// cut. Seen for real: thirteen bands chained across 1022px of a 900px page.
///
/// The later band yields, not the earlier one. The overlap exists because text
/// from the earlier row hangs into the next row's box, so the break belongs
/// *below* that text; clamping the earlier band instead would put it straight
/// through the glyphs. A band the previous one now covers entirely is dropped —
/// that coverage is not lost, it moved.
fn separate_bands(bands: &mut Vec<Atom>) {
    bands.sort_by(|a, b| a.top.total_cmp(&b.top));
    let mut floor = f32::NEG_INFINITY;
    bands.retain_mut(|band| {
        band.top = band.top.max(floor);
        floor = floor.max(band.bottom);
        band.bottom > band.top
    });
}

/// Fold each in-table atom into the row band it overlaps most, growing the band
/// to cover it. Returns the atoms that belong to no band at all — a `<caption>`,
/// say — which stay atoms in their own right.
///
/// Growing matters as much as dropping. A band is derived from the *cell boxes*,
/// and a cell box can be shorter than the line inside it: table layout sizes the
/// row, and a taller face overflows it. A line left sticking out of its band is
/// a break the paginator can neither take nor snap past, and the whole page runs
/// out of legal breaks. Seen for real once the bundled faces landed — see §9.
fn absorb_into_bands(bands: &mut [Atom], inner: &[Atom]) -> Vec<Atom> {
    let overlap = |b: &Atom, a: &Atom| b.bottom.min(a.bottom) - b.top.max(a.top);
    let mut orphans = Vec::new();

    for atom in inner {
        let best = bands
            .iter_mut()
            .filter(|b| overlap(b, atom) > 0.0)
            .max_by(|x, y| overlap(x, atom).total_cmp(&overlap(y, atom)));
        match best {
            Some(band) => {
                band.top = band.top.min(atom.top);
                band.bottom = band.bottom.max(atom.bottom);
            }
            None => orphans.push(*atom),
        }
    }
    orphans
}

/// Load and lay out one chapter. Returns `Err` rather than unwinding if the
/// engine panics on the book.
pub fn load(
    book: &Book,
    index: usize,
    style: &ReadingStyle,
    viewport: Viewport,
    page_height: f32,
    hyphenator: Option<&Hyphenator_>,
    net_callback: SharedCallback<Resource>,
) -> Result<Chapter, String> {
    let raw = book.chapter_html(index)?;
    let href = book.chapter_href(index).unwrap_or_default().to_owned();

    // The whitelist policy: publisher CSS never reaches the engine.
    let mut html = raise_superscripts(&hoist_edge_spaces(&style::strip_publisher_css(&raw)));

    // Justified text without hyphenation opens rivers, badly so in Spanish and
    // Catalan. Parley breaks on U+00AD, so mark the words before parsing.
    if let Some(h) = hyphenator {
        html = crate::hyphen::mark_html(&html, h);
    }

    // Most books open on a cover page that paints nothing: the cover image is
    // wrapped in inline `<svg>`, which blitz does not parse into a tree (§9), so
    // the reader's first page is blank paper. 27 of 40 books sampled from the
    // real library do this. Give that page the title and the author instead.
    if index == 0 && blank_cover(&raw) {
        html = title_page(&book.title, &book.author);
    }

    let base_url = base_url_for(&href);
    let mut ua = style.stylesheet();
    // Images the publisher sized to the text stay in the line (style.rs).
    let dir = href.rsplit_once('/').map_or("", |(d, _)| d);
    for css in style::linked_stylesheets(&raw) {
        if let Some(text) = crate::net::in_archive_path(&format!("{dir}/{css}"))
            .and_then(|path| book.read_bytes(&path).ok())
        {
            ua += &style::inline_image_rules(&String::from_utf8_lossy(&text));
        }
    }
    let provider: Arc<dyn NetProvider<Resource>> =
        Arc::new(BookNetProvider::new(book.clone(), net_callback));

    let result = build(html, base_url, ua, Some(provider), viewport);

    match result {
        Ok(doc) => {
            let pages = paginate_dom(&doc, page_height);
            Ok(Chapter { doc, index, pages })
        }
        Err(()) => Err(format!("{PANICKED} on chapter {index} ({href}); skipping it")),
    }
}

/// Does this spine item paint nothing at all? No text, and no `<img>` — a cover
/// wrapped in inline `<svg>` reaches the engine as an empty tree.
fn blank_cover(raw: &str) -> bool {
    crate::search::text_of_html(raw).trim().is_empty()
        && !raw.to_ascii_lowercase().contains("<img")
}

/// A cover page with nothing on it, replaced by the book's own name.
///
/// Inline styles rather than the reading stylesheet: `strip_publisher_css` only
/// drops `<style>` and `<link>`, so an attribute is the one bit of CSS a
/// chapter document can carry, and this page is the only one that wants any.
fn title_page(title: &str, author: &str) -> String {
    let by = match author.trim().is_empty() {
        true => String::new(),
        false => format!(
            r#"<div style="font-size: 1.1em; margin-top: 2em;">{}</div>"#,
            crate::grid::escape(author)
        ),
    };
    format!(
        r#"<!DOCTYPE html><html><body>
<div style="margin-top: 30vh; text-align: center;">
  <div style="font-size: 2em; font-weight: 600; line-height: 1.25;">{title}</div>
  {by}
</div></body></html>"#,
        title = crate::grid::escape(title),
    )
}

/// How `load` words an engine panic, so a caller can tell it from a chapter
/// that is simply missing from the archive.
pub const PANICKED: &str = "the layout engine panicked";

/// Parse, style and lay out one document. Returns `Err(())` if the engine panicked.
fn build(
    html: String,
    base_url: String,
    ua: String,
    provider: Option<Arc<dyn NetProvider<Resource>>>,
    viewport: Viewport,
) -> Result<HtmlDocument, ()> {
    catch_unwind(AssertUnwindSafe(move || {
        let mut doc = HtmlDocument::from_html(
            &html,
            DocumentConfig {
                viewport: Some(viewport),
                base_url: Some(base_url),
                ua_stylesheets: Some(vec![ua]),
                net_provider: provider,
                font_ctx: Some(FONT_CTX.with(Clone::clone)),
                ..Default::default()
            },
        );

        doc.resolve(0.0);
        doc
    }))
    .map_err(|_| ())
}

/// Pages per chapter for a whole book at one layout.
///
/// The only honest way to say "page 27 of 336": a page count depends on the
/// layout, and one chapter's density does not predict the others — a spine with
/// 131 items, many of them a paragraph long, made a single-chapter estimate swing
/// between 76 and 342 pages for the same book.
///
/// Three seconds for that book, so callers cache the result.
#[allow(clippy::too_many_arguments)]
pub fn page_counts(
    book: &Book,
    style: &ReadingStyle,
    hyphenator: Option<&Hyphenator_>,
    width: u32,
    height: u32,
    scale: f32,
    page_height: f32,
) -> Vec<usize> {
    // A chapter the engine cannot lay out contributes no pages; it is skipped
    // when reading, too.
    (0..book.chapter_count())
        .map(|i| {
            load_offscreen(book, i, style, hyphenator, width, height, scale, page_height)
                .map_or(0, |ch| ch.pages.count())
        })
        .collect()
}

/// One chapter laid out away from the window, images and all, so its pages
/// are the ones reading it would show.
#[allow(clippy::too_many_arguments)]
pub fn load_offscreen(
    book: &Book,
    index: usize,
    style: &ReadingStyle,
    hyphenator: Option<&Hyphenator_>,
    width: u32,
    height: u32,
    scale: f32,
    page_height: f32,
) -> Option<Chapter> {
    let dark = style.dark();
    let (tx, rx) = std::sync::mpsc::channel();
    let cb: SharedCallback<Resource> = Arc::new(CollectCallback(tx));
    let vp = viewport(width, height, scale, dark);
    let mut ch = load(book, index, style, vp, page_height, hyphenator, cb).ok()?;
    // Images change how much fits, and the in-zip provider answers
    // immediately, so take what arrived before counting.
    for resource in rx.try_iter() {
        let _ = catch_unwind(AssertUnwindSafe(|| ch.doc.load_resource(resource)));
    }
    relayout(&mut ch, viewport(width, height, scale, dark), page_height);
    Some(ch)
}

/// Where a stored position starts in its chapter: the character it names when
/// it names one, the element's top otherwise.
pub fn top_of_cfi(ch: &Chapter, c: &crate::cfi::Cfi) -> Option<f32> {
    let (node, offset) = crate::cfi::locate(ch.dom(), c)?;
    offset
        .and_then(|off| highlight_rects(ch.dom(), node, off, 1).first().map(|r| r.1))
        .or_else(|| node_top(ch.dom(), node))
}

struct CollectCallback(std::sync::mpsc::Sender<Resource>);

impl blitz_traits::net::NetCallback<Resource> for CollectCallback {
    fn call(&self, _doc_id: usize, result: Result<Resource, Option<String>>) {
        if let Ok(resource) = result {
            let _ = self.0.send(resource);
        }
    }
}

/// Lay out an arbitrary document — used for the library view, which is HTML/CSS
/// through the same pipeline as a book.
pub fn layout_document(
    html: String,
    ua: String,
    provider: Option<Arc<dyn NetProvider<Resource>>>,
    viewport: Viewport,
    page_height: f32,
) -> Option<Chapter> {
    let doc = build(html, ORIGIN.to_string(), ua, provider, viewport).ok()?;
    let pages = paginate_dom(&doc, page_height);
    Some(Chapter { doc, index: usize::MAX, pages })
}

/// Move whitespace at the inner edge of an element to just outside it:
/// `<span>the </span><em>entire</em>` becomes `<span>the</span> <em>entire</em>`.
///
/// ponytail: parley 0.6 trims whitespace at the start and end of every style
/// span rather than of the line (`TreeBuilder::push_uncommitted_text`), so a
/// space inside an element's edge vanished and "the <em>entire</em> world"
/// in calibre's span-per-run markup read "theentireworld". Outside the
/// element the space is plain text between spans, which parley keeps. Delete
/// this when blitz moves to a parley that collapses across spans.
pub fn hoist_edge_spaces(html: &str) -> String {
    /// Elements with no content, so nothing to be at the edge of.
    const VOID: [&str; 14] = [
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];
    /// Where whitespace is content and must stay where it is.
    const PRESERVE: [&str; 2] = ["pre", "textarea"];
    enum Tok<'a> {
        Text(String),
        Open(&'a str),
        Other(&'a str),
    }
    let name = |tag: &str| {
        tag.trim_start_matches(['<', '/'])
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };

    let mut out: Vec<Tok> = Vec::new();
    let mut preserve = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        let lt = rest.find('<').unwrap_or(rest.len());
        let (text, tail) = rest.split_at(lt);
        if !text.is_empty() {
            let mut text = text.to_string();
            // Leading space after a run of opening tags goes before the run.
            let opens = out.iter().rev().take_while(|t| matches!(t, Tok::Open(_))).count();
            if preserve == 0 && opens > 0 && text.starts_with(|c: char| c.is_ascii_whitespace()) {
                text = text.trim_start().to_string();
                out.insert(out.len() - opens, Tok::Text(" ".into()));
            }
            out.push(Tok::Text(text));
        }
        if tail.is_empty() {
            break;
        }
        let gt = tail.find('>').map_or(tail.len(), |i| i + 1);
        let tag = &tail[..gt];
        rest = &tail[gt..];
        let n = name(tag);
        let closing = tag.starts_with("</");
        if PRESERVE.contains(&n.as_str()) {
            preserve = if closing { preserve.saturating_sub(1) } else { preserve + 1 };
        }
        // Trailing space before a closing tag goes after it.
        if closing && preserve == 0 {
            if let Some(Tok::Text(t)) = out.last_mut() {
                if t.ends_with(|c: char| c.is_ascii_whitespace()) {
                    t.truncate(t.trim_end().len());
                    out.push(Tok::Other(tag));
                    out.push(Tok::Text(" ".into()));
                    continue;
                }
            }
        }
        let opening = !closing
            && tag[1..].starts_with(|c: char| c.is_ascii_alphabetic())
            && !tag.ends_with("/>")
            && !VOID.contains(&n.as_str());
        out.push(if opening { Tok::Open(tag) } else { Tok::Other(tag) });
    }
    out.iter()
        .map(|t| match t {
            Tok::Text(s) => s.as_str(),
            Tok::Open(s) | Tok::Other(s) => s,
        })
        .collect()
}

/// Set a `<sup>` that is only a number in the font's superior digits:
/// `<sup>8</sup>` becomes `<sup data-raised>⁸</sup>`.
///
/// ponytail: blitz-dom 0.2 never reads `vertical-align` and parley 0.6 has no
/// baseline shift, so the style sheet can shrink a note reference but not lift
/// it. Literata carries the superiors, one character for one, so a CFI offset
/// and a highlight length mean what they meant. A `<sup>` holding anything
/// else (`er`, `*`, `1,2`) is left to the style sheet. Delete this when blitz
/// honours `vertical-align`.
pub fn raise_superscripts(html: &str) -> String {
    let raise = |c: char| match c {
        '0' => Some('⁰'),
        '1' => Some('¹'),
        '2' => Some('²'),
        '3' => Some('³'),
        '4'..='9' => char::from_u32(c as u32 - '4' as u32 + 0x2074),
        '+' => Some('⁺'),
        '-' => Some('⁻'),
        '=' => Some('⁼'),
        '(' | '[' => Some('⁽'),
        ')' | ']' => Some('⁾'),
        _ => None,
    };
    // Lowercasing ASCII keeps every byte where it was: MOBI writes `<SUP>`.
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find("<sup").map(|i| at + i) {
        let open_end = lower[i..].find('>').map(|j| i + j + 1);
        let close = open_end.and_then(|o| lower[o..].find("</sup>").map(|j| o + j));
        let (Some(o), Some(c)) = (open_end, close) else { break };
        let named = matches!(lower.as_bytes()[i + 4], b'>' | b' ' | b'\t' | b'\n' | b'\r');
        let inner = &html[o..c];
        // The text between the tags, as (start, end) byte spans of `inner`.
        let mut texts = Vec::new();
        let mut t = 0;
        while t < inner.len() {
            let lt = inner[t..].find('<').map_or(inner.len(), |j| t + j);
            texts.push((t, lt));
            t = inner[lt..].find('>').map_or(inner.len(), |j| lt + j + 1);
        }
        let text = || texts.iter().flat_map(|&(a, b)| inner[a..b].chars());
        let digits = text().any(|ch| ch.is_ascii_digit());
        if !named || !digits || !text().all(|ch| ch.is_whitespace() || raise(ch).is_some()) {
            out += &html[at..i + 4];
            at = i + 4;
            continue;
        }
        out += &html[at..i];
        out += "<sup data-raised";
        out += &html[i + 4..o];
        let mut last = 0;
        for (a, b) in texts {
            out += &inner[last..a];
            out.extend(inner[a..b].chars().map(|ch| raise(ch).unwrap_or(ch)));
            last = b;
        }
        out += &inner[last..];
        at = c;
    }
    out += &html[at..];
    out
}

/// Lay out a bare HTML fragment with the reading stylesheet. Used by tests to
/// assert geometry without needing a book on disk.
#[cfg(test)]
pub fn layout_fragment(body: &str, style: &ReadingStyle, viewport: Viewport) -> HtmlDocument {
    let html = format!("<html><body>{body}</body></html>");
    build(html, ORIGIN.to_string(), style.stylesheet(), None, viewport)
        .expect("engine panicked laying out a test fragment")
}

/// Absolute Y of a node in the flow, for resolving a CFI back to a page.
///
/// An inline element has no box of its own inside its paragraph's text
/// layout, so its `final_layout` says nothing about where it is. Its top is the
/// first line that draws its text: a note's `<small id>` back-link target sits
/// pages below the top of a long paragraph, and landing on that top looked like
/// following the link to the wrong page.
pub fn node_top(dom: &BaseDocument, target: usize) -> Option<f32> {
    fn walk(dom: &BaseDocument, id: usize, y: f32, target: usize) -> Option<f32> {
        let node = dom.get_node(id)?;
        let at = y + node.final_layout.location.y;
        if id == target {
            return Some(at);
        }
        kids(dom, node).iter().find_map(|c| walk(dom, *c, at, target))
    }
    let block = layout_parent(dom, target).and_then(|p| text_element(dom, p));
    match block.and_then(|b| inline_top(dom, b, target)) {
        Some(y) => Some(y),
        None => walk(dom, 0, 0.0, target),
    }
}

/// Top of the first line of `block`'s layout drawing text from `target` or
/// one of its descendants. `None` when it draws none, an empty anchor say.
fn inline_top(dom: &BaseDocument, block: usize, target: usize) -> Option<f32> {
    use parley::PositionedLayoutItem;
    let tl = text_layout(dom, block)?;
    let within = |mut id: usize| loop {
        if id == target {
            return true;
        }
        match dom.get_node(id).and_then(|n| n.parent) {
            Some(p) if id != block => id = p,
            _ => return false,
        }
    };
    let line = tl.layout.lines().find(|line| {
        line.items().any(|i| matches!(i, PositionedLayoutItem::GlyphRun(g) if within(g.style().brush.id)))
    })?;
    let l = &dom.get_node(block)?.final_layout;
    let content_top = node_top(dom, block)? + l.border.top + l.padding.top;
    Some(content_top + line.metrics().min_coord / tl.layout.scale())
}

/// Every `data-index` element with its absolute top, in one walk.
///
/// One walk, not one search per index: `find_by_attr` scans the whole tree, so
/// asking it 361 times to find out which cards are on a page is quadratic.
pub fn indexed_tops(dom: &BaseDocument) -> Vec<(usize, f32)> {
    fn walk(dom: &BaseDocument, id: usize, y: f32, out: &mut Vec<(usize, f32)>) {
        let Some(node) = dom.get_node(id) else { return };
        let at = y + node.final_layout.location.y;
        if let NodeData::Element(el) = &node.data {
            if let Some(a) = el.attrs.iter().find(|a| &*a.name.local == "data-index") {
                if let Ok(i) = a.value.parse::<usize>() {
                    out.push((i, at));
                }
            }
        }
        for c in kids(dom, node).iter() {
            walk(dom, *c, at, out);
        }
    }
    let mut out = Vec::new();
    walk(dom, 0, 0.0, &mut out);
    out
}

/// Absolute content-box origin of a node, measured the same way atoms are: the
/// parley layout inside an element starts at its content box, not its border box.
pub fn node_origin(dom: &BaseDocument, target: usize) -> Option<(f32, f32)> {
    fn walk(
        dom: &BaseDocument,
        id: usize,
        at: (f32, f32),
        target: usize,
    ) -> Option<(f32, f32)> {
        let node = dom.get_node(id)?;
        let l = &node.final_layout;
        let here = (at.0 + l.location.x, at.1 + l.location.y);
        if id == target {
            return Some((
                here.0 + l.border.left + l.padding.left,
                here.1 + l.border.top + l.padding.top,
            ));
        }
        kids(dom, node).iter().find_map(|c| walk(dom, *c, here, target))
    }
    walk(dom, 0, (0.0, 0.0), target)
}

/// Absolute border-box rectangle of a node, as `(x, y, width, height)`.
///
/// Used to find out where the HUD put a control, so an icon can be drawn into
/// it: the bundled faces have no symbol glyphs, so the icons are painted rather
/// than typeset.
pub fn node_rect(dom: &BaseDocument, target: usize) -> Option<(f32, f32, f32, f32)> {
    fn walk(
        dom: &BaseDocument,
        id: usize,
        at: (f32, f32),
        target: usize,
    ) -> Option<(f32, f32, f32, f32)> {
        let node = dom.get_node(id)?;
        let l = &node.final_layout;
        let here = (at.0 + l.location.x, at.1 + l.location.y);
        if id == target {
            return Some((here.0, here.1, l.size.width, l.size.height));
        }
        kids(dom, node).iter().find_map(|c| walk(dom, *c, here, target))
    }
    walk(dom, 0, (0.0, 0.0), target)
}

/// Nearest ancestor (or self) that owns an inline layout.
///
/// A selection lives in one parley layout, and only the element that
/// establishes an inline formatting context has one — a hit usually lands on a
/// text node or an inline `<em>` inside it.
pub fn text_element(dom: &BaseDocument, mut id: usize) -> Option<usize> {
    loop {
        if text_layout(dom, id).is_some() {
            return Some(id);
        }
        id = layout_parent(dom, id)?;
    }
}

/// An element's parley layout. Its coordinates are **device pixels**: blitz
/// lays text out at the window's scale, so divide by `layout.scale()` to get
/// CSS pixels and multiply to go back. At scale 1 the two agree, which is how
/// mixing them went unseen until Omarchy ran at 1.25x.
pub fn text_layout(dom: &BaseDocument, id: usize) -> Option<&blitz_dom::node::TextLayout> {
    dom.get_node(id)?.element_data()?.inline_layout_data.as_deref()
}

/// An element's tag, lowercase.
pub fn tag_of(dom: &BaseDocument, id: usize) -> Option<String> {
    match &dom.get_node(id)?.data {
        NodeData::Element(el) => Some(el.name.local.as_ref().to_ascii_lowercase()),
        _ => None,
    }
}

/// Is `id`, or anything under it, a heading? A note never holds one; a
/// section starts with one.
pub fn has_heading(dom: &BaseDocument, id: usize) -> bool {
    let mut found = false;
    walk_elements(dom, id, &mut |_, el| {
        found |= matches!(el.name.local.as_ref(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
    });
    found
}

/// All the laid-out text under a node, as one string.
pub fn text_of(dom: &BaseDocument, id: usize) -> String {
    let mut out = String::new();
    walk_elements(dom, id, &mut |_, el| {
        if let Some(tl) = el.inline_layout_data.as_ref() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(tl.text.trim());
        }
    });
    out.trim().to_string()
}

/// The text of the lines between two flow offsets, block by block, as
/// `(heading level, text)` with level 0 for anything not a heading: what a
/// screen reader is told is on the page. A line belongs to the page its middle
/// is on, the same lines the paginator put there.
pub fn page_text(dom: &BaseDocument, from: f32, to: f32) -> Vec<(u8, String)> {
    fn walk(dom: &BaseDocument, id: usize, parent_y: f32, span: (f32, f32), out: &mut Vec<(u8, String)>) {
        let Some(node) = dom.get_node(id) else { return };
        let l = &node.final_layout;
        let abs_y = parent_y + l.location.y;
        if let Some(el) = node.element_data() {
            if let Some(tl) = el.inline_layout_data.as_ref() {
                let content_top = abs_y + l.border.top + l.padding.top;
                let s = tl.layout.scale();
                let text: String = tl
                    .layout
                    .lines()
                    .filter(|line| {
                        let m = line.metrics();
                        let mid = content_top + (m.min_coord + m.max_coord) / 2.0 / s;
                        span.0 <= mid && mid < span.1
                    })
                    .filter_map(|line| tl.text.get(line.text_range()))
                    .collect();
                // Soft hyphens are for the line breaker, not for the ear.
                let text = text.replace('\u{AD}', "").split_whitespace().collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    let level = match el.name.local.as_ref() {
                        "h1" => 1,
                        "h2" => 2,
                        "h3" => 3,
                        "h4" => 4,
                        "h5" => 5,
                        "h6" => 6,
                        _ => 0,
                    };
                    out.push((level, text));
                }
            }
        }
        for c in kids(dom, node).iter() {
            walk(dom, *c, abs_y, span, out);
        }
    }
    let mut out = Vec::new();
    walk(dom, 0, 0.0, (from, to), &mut out);
    out
}

/// Rectangles covering a selection, in flow coordinates, as `(x0, y0, x1, y1)`.
pub fn selection_rects(
    dom: &BaseDocument,
    id: usize,
    sel: &parley::Selection,
) -> Vec<(f32, f32, f32, f32)> {
    let (Some(tl), Some((ox, oy))) = (text_layout(dom, id), node_origin(dom, id)) else {
        return Vec::new();
    };
    let s = tl.layout.scale() as f64;
    sel.geometry(&tl.layout)
        .into_iter()
        .map(|(b, _)| {
            (
                ox + (b.x0 / s) as f32,
                oy + (b.y0 / s) as f32,
                ox + (b.x1 / s) as f32,
                oy + (b.y1 / s) as f32,
            )
        })
        .collect()
}

/// Rectangles for a stored highlight: a character offset inside one element and
/// a length that runs on into the elements after it, converted back to the byte
/// ranges parley works in.
///
/// Offsets are stored as *visible characters* (`visible_count`) because that is
/// what a CFI means by `:offset`; parley counts bytes, soft hyphens included.
/// Converting at this boundary keeps the stored CFI honest without making the
/// rest of the code think in bytes.
pub fn highlight_rects(
    dom: &BaseDocument,
    id: usize,
    char_start: usize,
    char_len: usize,
) -> Vec<(f32, f32, f32, f32)> {
    let els = text_elements(dom);
    let Some(i) = els.iter().position(|&e| e == id) else { return Vec::new() };
    let (mut start, mut left, mut out) = (char_start, char_len, Vec::new());
    for &e in &els[i..] {
        let Some(tl) = text_layout(dom, e) else { continue };
        let n = visible_count(&tl.text);
        if left == 0 || start > n {
            break;
        }
        let take = left.min(n - start);
        let bytes = byte_of_visible(&tl.text, start)..byte_of_visible(&tl.text, start + take);
        out.extend(range_rects(dom, e, bytes));
        (start, left) = (0, left - take);
    }
    out
}

/// Rectangles over a byte range of one element's text.
pub fn range_rects(dom: &BaseDocument, id: usize, bytes: std::ops::Range<usize>) -> Vec<(f32, f32, f32, f32)> {
    let Some(tl) = text_layout(dom, id) else { return Vec::new() };
    if bytes.is_empty() {
        return Vec::new();
    }
    let sel = parley::Selection::new(
        parley::Cursor::from_byte_index(&tl.layout, bytes.start, parley::Affinity::Downstream),
        parley::Cursor::from_byte_index(&tl.layout, bytes.end, parley::Affinity::Upstream),
    );
    selection_rects(dom, id, &sel)
}

/// The children a walk over the laid-out page visits.
///
/// Text beside a block — `<li>words<ul>…</ul></li>`, a `<p>` holding a block
/// image — is wrapped by blitz in an anonymous block that exists only in the
/// layout tree. That block, not the `<li>`, holds the words' parley layout, and
/// their positions are measured from it. Where a node has such blocks this is
/// its layout children, each anonymous block standing in for the DOM children
/// it wraps. Everywhere else it is the DOM children: a paragraph's layout
/// children are only the boxes embedded in its text.
///
/// Lookups by id, attribute or text want the DOM and keep `children`.
pub fn kids<'a>(dom: &BaseDocument, node: &'a blitz_dom::Node) -> std::borrow::Cow<'a, [usize]> {
    match node.layout_children.borrow().as_ref() {
        Some(lc) if lc.iter().any(|&c| is_anonymous(dom, c)) => lc.clone().into(),
        _ => node.children.as_slice().into(),
    }
}

fn is_anonymous(dom: &BaseDocument, id: usize) -> bool {
    dom.get_node(id).is_some_and(|n| matches!(n.data, NodeData::AnonymousBlock(_)))
}

/// An element's anonymous blocks, in order (see `kids`). Empty for most.
pub fn anonymous_blocks(dom: &BaseDocument, id: usize) -> Vec<usize> {
    let Some(node) = dom.get_node(id) else { return Vec::new() };
    let lc = node.layout_children.borrow();
    lc.iter().flatten().copied().filter(|&c| is_anonymous(dom, c)).collect()
}

/// A node's parent as layout sees it: the anonymous block wrapping it, if
/// blitz made one, its DOM parent otherwise. blitz records neither on the
/// wrapped node — its `parent` is still the element that was split.
fn layout_parent(dom: &BaseDocument, id: usize) -> Option<usize> {
    let parent = dom.get_node(id)?.parent?;
    let wrapper = anonymous_blocks(dom, parent)
        .into_iter()
        .find(|&a| dom.get_node(a).is_some_and(|n| n.children.contains(&id)));
    Some(wrapper.unwrap_or(parent))
}

/// Every element that owns an inline layout, in reading order: the paragraphs a
/// selection or a highlight runs across.
pub fn text_elements(dom: &BaseDocument) -> Vec<usize> {
    let mut out = Vec::new();
    walk_elements(dom, 0, &mut |id, el| {
        if el.inline_layout_data.is_some() {
            out.push(id);
        }
    });
    out
}

/// The text between two points, `(element, byte)`, given in either order, as a
/// byte range per element: a selection dragged across paragraphs.
pub fn ranges_between(
    dom: &BaseDocument,
    a: (usize, usize),
    b: (usize, usize),
) -> Vec<(usize, std::ops::Range<usize>)> {
    let els = text_elements(dom);
    let at = |p: (usize, usize)| els.iter().position(|&e| e == p.0).map(|i| (i, p.1));
    let (Some(a), Some(b)) = (at(a), at(b)) else { return Vec::new() };
    let (first, last) = (a.min(b), a.max(b));
    (first.0..=last.0)
        .filter_map(|i| {
            let len = text_layout(dom, els[i])?.text.len();
            let start = if i == first.0 { first.1 } else { 0 };
            let end = if i == last.0 { last.1 } else { len };
            (start < end).then(|| (els[i], start..end))
        })
        .collect()
}

/// Character offset and count for a byte range in an element's text, plus the
/// text itself — what a highlight has to store, and what a copy puts on the
/// clipboard. All three leave out the soft hyphens the layout carries.
pub fn char_span(dom: &BaseDocument, id: usize, bytes: std::ops::Range<usize>) -> Option<(usize, usize, String)> {
    let tl = text_layout(dom, id)?;
    let text = tl.text.get(bytes.clone())?;
    let start = visible_count(tl.text.get(..bytes.start)?);
    Some((start, visible_count(text), text.replace(crate::hyphen::SOFT, "")))
}

/// Characters a reader sees: the text less the soft hyphens hyphenation put
/// in. Every stored offset counts these, so that a highlight or a reading
/// position does not move when the hyphenation does — new patterns, or a book
/// whose language is read differently.
pub fn visible_count(text: &str) -> usize {
    text.chars().filter(|&c| c != crate::hyphen::SOFT).count()
}

/// Byte index in the laid-out text where visible character `n` starts, or the
/// end of the text. The inverse of `visible_count`.
pub fn byte_of_visible(text: &str, n: usize) -> usize {
    text.char_indices()
        .filter(|&(_, c)| c != crate::hyphen::SOFT)
        .nth(n)
        .map_or(text.len(), |(b, _)| b)
}

/// The first element whose text contains `needle`, folded so an unaccented
/// query finds an accented word. Used to land a search hit on its page.
pub fn node_containing_text(dom: &BaseDocument, needle: &str) -> Option<usize> {
    let needle = crate::search::fold(needle);
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    if let Some(id) = first_with_text(dom, needle) {
        return Some(id);
    }

    // A phrase can straddle an inline tag — `la <em>resonancia</em> de` is two
    // text runs — and FTS5 matches terms that need not be adjacent at all. The
    // longest word is the most distinctive thing guaranteed to sit inside one
    // run, so fall back to that before giving up on the page.
    let longest = needle.split_whitespace().max_by_key(|w| w.chars().count())?;
    (longest.chars().count() >= 4 && longest != needle)
        .then(|| first_with_text(dom, longest))
        .flatten()
}

fn first_with_text(dom: &BaseDocument, folded_needle: &str) -> Option<usize> {
    let mut found = None;
    walk_elements(dom, 0, &mut |id, el| {
        if found.is_some() {
            return;
        }
        if let Some(tl) = el.inline_layout_data.as_ref() {
            if crate::search::fold(&tl.text).contains(folded_needle) {
                found = Some(id);
            }
        }
    });
    found
}

/// Character offset within `id` of the first character at or below `y`.
///
/// A page rarely begins where a paragraph does: the CFI names the paragraph, so
/// on its own it can only resume where that paragraph *started*, which for the
/// long ones is a page or two back. This is the rest of the address.
pub fn char_at(dom: &BaseDocument, id: usize, y: f32) -> Option<usize> {
    let tl = text_layout(dom, id)?;
    let (_, oy) = node_origin(dom, id)?;
    // In the layout's own units, which are device pixels (`layout.scale()`).
    let local = (y - oy) * tl.layout.scale();
    // By the line's middle, not its bottom: at a fractional scale the line
    // above a page break can end a hair past it, and naming that line puts a
    // re-flow one page back.
    let line = tl.layout.lines().find(|l| {
        let m = l.metrics();
        (m.min_coord + m.max_coord) / 2.0 > local
    })?;
    let byte = line.text_range().start;
    Some(visible_count(tl.text.get(..byte)?))
}

/// The node a page begins at — the first atom at or after the page top.
/// This is what a CFI is generated from. A line the break sits a hair above the
/// foot of (`EDGE_EPS`) is the page before's.
pub fn node_at(dom: &BaseDocument, y: f32) -> Option<usize> {
    collect_atoms(dom)
        .into_iter()
        .filter(|a| a.bottom > y + paginate::EDGE_EPS)
        .min_by(|a, b| a.top.total_cmp(&b.top))
        .map(|a| a.group)
}

/// Absolute layout rect of the first element with the given tag.
pub fn element_rect(dom: &BaseDocument, tag: &str) -> Option<(f32, f32, f32, f32)> {
    fn walk(
        dom: &BaseDocument, id: usize, parent: (f32, f32), tag: &str,
    ) -> Option<(f32, f32, f32, f32)> {
        let node = dom.get_node(id)?;
        let l = &node.final_layout;
        let at = (parent.0 + l.location.x, parent.1 + l.location.y);
        if let blitz_dom::NodeData::Element(el) = &node.data {
            if el.name.local.as_ref() == tag {
                return Some((at.0, at.1, l.size.width, l.size.height));
            }
        }
        kids(dom, node).iter().find_map(|c| walk(dom, *c, at, tag))
    }
    walk(dom, 0, (0.0, 0.0), tag)
}

/// Re-lay-out after a viewport or style change. Returns false if the engine panicked.
pub fn relayout(chapter: &mut Chapter, viewport: Viewport, page_height: f32) -> bool {
    let ok = catch_unwind(AssertUnwindSafe(|| {
        chapter.doc.set_viewport(viewport);
        chapter.doc.resolve(0.0);
    }))
    .is_ok();
    if ok {
        chapter.pages = paginate_dom(&chapter.doc, page_height);
    }
    ok
}

/// Resolve a chapter href against the synthetic in-archive origin, so that
/// relative `<img src="assets/x.png">` lands back inside the archive.
fn base_url_for(chapter_href: &str) -> String {
    // Hrefs arrive slash-prefixed (`/OEBPS/ch04.html`); ORIGIN already ends in a
    // slash, so trim it here or the extra empty segment shifts every resolution.
    let href = chapter_href.trim_start_matches('/');
    match href.rfind('/') {
        Some(i) => format!("{ORIGIN}{}/", &href[..i]),
        None => ORIGIN.to_string(),
    }
}

pub fn viewport(width: u32, height: u32, scale: f32, dark: bool) -> Viewport {
    Viewport::new(
        width,
        height,
        scale,
        if dark { ColorScheme::Dark } else { ColorScheme::Light },
    )
}

/// Walk the layout tree accumulating absolute Y to find the flow's full height.
/// `final_layout` is parent-relative, so this cannot be read off the root.
fn walk_elements(
    dom: &BaseDocument,
    id: usize,
    f: &mut impl FnMut(usize, &blitz_dom::ElementData),
) {
    let Some(node) = dom.get_node(id) else { return };
    if let Some(el) = node.element_data() {
        f(id, el);
    }
    for child in kids(dom, node).iter() {
        walk_elements(dom, *child, f);
    }
}

fn content_height(dom: &BaseDocument) -> f32 {
    fn walk(dom: &BaseDocument, id: usize, parent_y: f32, max_y: &mut f32) {
        let Some(node) = dom.get_node(id) else { return };
        let abs_y = parent_y + node.final_layout.location.y;
        *max_y = max_y.max(abs_y + node.final_layout.size.height);
        for child in kids(dom, node).iter() {
            walk(dom, *child, abs_y, max_y);
        }
    }
    let mut max_y = 0.0;
    walk(dom, 0, 0.0, &mut max_y);
    max_y
}

#[cfg(test)]
mod tests {
    /// Text beside a nested list sits in anonymous blocks only the layout tree
    /// has (`kids`). Unseen, its lines were no atoms — a break could cut one —
    /// its hyphens were not drawn, and it could not be selected or bookmarked.
    #[test]
    fn text_beside_a_block_is_seen_and_anchored() {
        let st = crate::style::ReadingStyle::default();
        let doc = super::layout_fragment(
            "<ul><li>Alpha beta gamma<ul><li>inner</li></ul>delta epsilon</li></ul>",
            &st,
            super::viewport(760, 1000, 1.0, false),
        );
        let lines = super::collect_atoms(&doc).iter().filter(|a| a.kind == super::AtomKind::Line).count();
        assert_eq!(lines, 3, "both runs of the outer item and the inner one");

        let li = (0..10_000).find(|&i| super::tag_of(&doc, i).as_deref() == Some("li")).unwrap();
        let blocks = super::anonymous_blocks(&doc, li);
        assert_eq!(blocks.len(), 2, "before and after the nested list");
        let els = super::text_elements(&doc);
        assert!(blocks.iter().all(|b| els.contains(b)), "selectable: {els:?} {blocks:?}");
        let words = super::node_containing_text(&doc, "delta").unwrap();
        assert_eq!(super::text_element(&doc, words), Some(blocks[1]), "a hit climbs to its block");

        // A CFI names the <li>, with the offset run on through the first block.
        let c = crate::cfi::at(&doc, blocks[1], Some(2), 3).expect("a cfi for anonymous text");
        assert_eq!(crate::cfi::resolve(&doc, &c), Some(li));
        assert_eq!(c.offset, Some("Alpha beta gamma".len() + 2));
        assert_eq!(crate::cfi::locate(&doc, &c), Some((blocks[1], Some(2))));
        assert!(super::node_top(&doc, blocks[1]).unwrap() > super::node_top(&doc, blocks[0]).unwrap());
    }

    /// Save and restore have to agree about *which page*, not which paragraph.
    /// A paragraph three pages long used to resume at its first page, because
    /// the CFI named the element and nothing else.
    #[test]
    fn a_page_inside_a_long_paragraph_comes_back_to_itself() {
        let long = "Una frase que se repite para llenar varias paginas de texto. ".repeat(400);
        let doc = super::layout_document(
            format!("<html><body><p>{long}</p></body></html>"),
            crate::style::ReadingStyle::default().stylesheet(),
            None,
            super::viewport(900, 700, 1.0, false),
            600.0,
        )
        .expect("the page must lay out");

        assert!(doc.pages.count() > 3, "the fixture must span pages");
        for page in 0..doc.pages.count() {
            let top = doc.pages.top_of(page);
            let node = super::node_at(doc.dom(), top).expect("no node at the page top");

            // Saved: the paragraph, and which character of it the page starts on.
            let off = super::char_at(doc.dom(), node, top).expect("no character offset");
            // Restored: that character's own line decides the page.
            let y = super::highlight_rects(doc.dom(), node, off, 1)
                .first()
                .map(|r| r.1)
                .expect("the character has no box");

            assert_eq!(doc.pages.page_containing(y), page, "page {page} came back wrong");
        }

        // Without the offset every page of that paragraph resolves to the first.
        let node = super::node_at(doc.dom(), doc.pages.top_of(2)).unwrap();
        let bare = super::node_top(doc.dom(), node).unwrap();
        assert_eq!(doc.pages.page_containing(bare), 0, "the paragraph starts on page 1");
    }

    /// At a fractional scale the line above a page break can end a hair past
    /// it; naming that line sent a resize back to the page before.
    #[test]
    fn a_page_start_survives_a_fractional_scale() {
        let p = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. ".repeat(6);
        let mut doc = super::layout_document(
            format!("<html><body style='margin:0;font-size:20px;line-height:1.5'>{}</body></html>", format!("<p>{p}</p>").repeat(30)),
            String::new(),
            None,
            super::viewport(900, 700, 1.25, false),
            480.0,
        )
        .expect("the page must lay out");
        // A resize lays out in place, and that is where the break moved.
        assert!(super::relayout(&mut doc, super::viewport(900, 700, 1.25, false), 480.0));

        for page in 0..doc.pages.count() {
            let top = doc.pages.top_of(page);
            let Some(node) = super::node_at(doc.dom(), top) else { continue };
            let off = super::char_at(doc.dom(), node, top).expect("no character offset");
            let y = super::highlight_rects(doc.dom(), node, off, 1)[0].1;
            assert_eq!(doc.pages.page_containing(y), page, "page {page} came back wrong");
        }
    }

    /// An image keeps its own size, or shrinks to the column — never grows.
    /// `display: block` stretched every image to the column's width: SICP's
    /// 2×9 vertical ellipsis became a blurred slab most of a page tall.
    #[test]
    fn images_shrink_to_the_column_but_never_grow() {
        use blitz_dom::net::Resource;
        use blitz_dom::util::ImageType;
        fn img(dom: &blitz_dom::BaseDocument, id: usize, want: &str) -> Option<usize> {
            let node = dom.get_node(id)?;
            if node.element_data().is_some_and(|e| e.attr(blitz_dom::local_name!("id")) == Some(want)) {
                return Some(id);
            }
            node.children.iter().find_map(|c| img(dom, *c, want))
        }

        let vp = || super::viewport(900, 1000, 1.0, false);
        let mut doc = super::layout_document(
            r#"<html><body><p>a <img id="dots"/> b</p><div><img id="wide"/></div></body></html>"#.into(),
            crate::style::ReadingStyle::default().stylesheet(),
            None,
            vp(),
            900.0,
        )
        .expect("the page must lay out");
        let (dots, wide) = (img(doc.dom(), 0, "dots").unwrap(), img(doc.dom(), 0, "wide").unwrap());
        for (id, w, h) in [(dots, 2, 9), (wide, 2000, 800)] {
            let rgba = std::sync::Arc::new(vec![0u8; (w * h * 4) as usize]);
            doc.doc.load_resource(Resource::Image(id, ImageType::Image, w, h, rgba));
        }
        super::relayout(&mut doc, vp(), 900.0);

        let (_, _, w, h) = super::node_rect(doc.dom(), dots).unwrap();
        assert_eq!((w, h), (2.0, 9.0), "a small image was stretched");
        let (_, _, w, h) = super::node_rect(doc.dom(), wide).unwrap();
        assert!(w < 900.0 && (h / w - 0.4).abs() < 0.01, "a wide image must fit, in proportion: {w}×{h}");
    }

    /// A cover that reaches the engine as an empty tree gets the book's name; a
    /// cover that actually paints must keep its image.
    #[test]
    fn only_a_cover_that_paints_nothing_is_replaced() {
        let svg = r#"<html><body><svg viewBox="0 0 1 1"><image xlink:href="c.jpg"/></svg></body></html>"#;
        let img = r#"<html><body><div><img src="cover.jpeg" alt="Cover"/></div></body></html>"#;
        let prose = "<html><body><p>Llamadme Ismael.</p></body></html>";

        assert!(super::blank_cover(svg));
        assert!(!super::blank_cover(img), "a real cover image must survive");
        assert!(!super::blank_cover(&img.to_uppercase().replace("XLINK", "xlink")));
        assert!(!super::blank_cover(prose));

        // Book metadata is untrusted; it must not inject into the page.
        let page = super::title_page("<script>x</script>", "A & B");
        assert!(!page.contains("<script"), "{page}");
        assert!(page.contains("A &amp; B"));
        // No author, no empty line where one would be.
        assert!(!super::title_page("Solo", "  ").contains("margin-top: 2em"));
    }

    use super::absorb_into_bands;
    use crate::paginate::{Atom, AtomKind};

    fn atom(top: f32, bottom: f32, kind: AtomKind) -> Atom {
        Atom { top, bottom, group: 1, kind, keep_with_next: false }
    }

    /// A cell box can be shorter than the line inside it, so a row band has to
    /// grow over its lines rather than only swallow the ones that already fit.
    /// The real case: band 4577..4604 holding a 29px line at 4586..4615, which
    /// left an atom sticking 11px out of the row and cost the page every legal
    /// break it had.
    #[test]
    fn a_row_band_grows_over_a_line_that_overflows_its_cell() {
        let mut bands = [atom(4577.0, 4604.0, AtomKind::Row)];
        let line = atom(4586.0, 4615.0, AtomKind::Line);

        let orphans = absorb_into_bands(&mut bands, &[line]);

        assert!(orphans.is_empty(), "the line belongs to the row, not to the flow");
        assert_eq!((bands[0].top, bands[0].bottom), (4577.0, 4615.0));
        assert!(!bands[0].splits(4620.0), "a break past the grown band is legal");
        assert!(bands[0].splits(4610.0), "a break inside it still is not");
    }

    /// Lines go to the band they sit in, not merely the first one they touch.
    #[test]
    fn a_line_lands_in_the_band_it_overlaps_most() {
        let mut bands = [
            atom(100.0, 130.0, AtomKind::Row),
            atom(130.0, 200.0, AtomKind::Row),
        ];
        let orphans = absorb_into_bands(&mut bands, &[atom(128.0, 160.0, AtomKind::Line)]);

        assert!(orphans.is_empty());
        assert_eq!(bands[0].bottom, 130.0, "the band it barely touches must not grow");
        assert_eq!(bands[1].top, 128.0);
    }

    /// Chained bands leave a table with no legal break anywhere in it, so the
    /// paginator cuts. Pulling them apart puts a break back on every boundary.
    #[test]
    fn overlapping_row_bands_are_pulled_apart() {
        let mut bands = vec![
            atom(4577.0, 4615.0, AtomKind::Row),
            atom(4604.0, 4758.0, AtomKind::Row),
            atom(4754.0, 4823.0, AtomKind::Row),
        ];
        super::separate_bands(&mut bands);

        let edges: Vec<(f32, f32)> = bands.iter().map(|b| (b.top, b.bottom)).collect();
        assert_eq!(edges, [(4577.0, 4615.0), (4615.0, 4758.0), (4758.0, 4823.0)]);
        for b in &bands {
            assert!(!b.splits(4615.0) && !b.splits(4758.0), "a row boundary must be breakable");
        }
    }

    /// A band the previous one swallowed whole has nothing left to contribute;
    /// keeping it as a zero- or negative-height atom would be a break veto at a
    /// point with no content.
    #[test]
    fn a_band_swallowed_whole_is_dropped() {
        let mut bands = vec![
            atom(100.0, 400.0, AtomKind::Row),
            atom(120.0, 300.0, AtomKind::Row),
            atom(390.0, 450.0, AtomKind::Row),
        ];
        super::separate_bands(&mut bands);

        let edges: Vec<(f32, f32)> = bands.iter().map(|b| (b.top, b.bottom)).collect();
        assert_eq!(edges, [(100.0, 400.0), (400.0, 450.0)]);
    }

    /// Following a search hit has to land on the word: an unaccented query must
    /// find the accented text, and a phrase broken by an inline tag must still
    /// resolve rather than dumping the reader at the chapter top.
    #[test]
    fn a_search_hit_locates_its_text() {
        let st = style();
        let vp = viewport(900, 1000, 1.0, false);
        let doc = layout_fragment(
            "<p id=\"a\">Nada aquí.</p>\
             <p id=\"b\">Sin embargo, la resonancia de la <em>tipografía</em> siempre.</p>",
            &st,
            vp,
        );

        let hit = super::node_containing_text(&doc, "resonancia").expect("plain word");
        let top = super::node_top(&doc, hit).expect("hit has a position");
        assert!(top > 0.0, "the hit is in the second paragraph, not the first");

        // Accents folded, the way the index matched it.
        assert_eq!(super::node_containing_text(&doc, "RESONANCIA"), Some(hit));
        assert_eq!(super::node_containing_text(&doc, "tipografia"), Some(hit));

        // The phrase spans an <em>, so only the longest-word fallback can find it.
        assert!(super::node_containing_text(&doc, "de la tipografia siempre").is_some());

        assert_eq!(super::node_containing_text(&doc, "   "), None);
        assert_eq!(super::node_containing_text(&doc, "zzzznotaword"), None);
    }

    /// Content inside a table but outside every row — a caption — is still an
    /// atom of its own; dropping it would let a break cut straight through it.
    #[test]
    fn table_content_outside_every_row_stays_an_atom() {
        let mut bands = [atom(100.0, 130.0, AtomKind::Row)];
        let caption = atom(60.0, 90.0, AtomKind::Line);

        let orphans = absorb_into_bands(&mut bands, &[caption]);

        assert_eq!(orphans.len(), 1);
        assert_eq!((orphans[0].top, orphans[0].bottom), (60.0, 90.0));
        assert_eq!((bands[0].top, bands[0].bottom), (100.0, 130.0));
    }

    /// The bundled faces must register, and must register under exactly the
    /// family names the stylesheets ask for. A truncated download or a font
    /// renamed upstream would otherwise fail silently: layout falls back to a
    /// system face and everything still *looks* like text.
    #[test]
    fn the_bundled_faces_register_under_the_names_the_stylesheets_ask_for() {
        use fontique::{Blob, Collection, CollectionOptions};
        use std::sync::Arc;

        let sheet = crate::style::ReadingStyle::default().stylesheet();
        let mut collection = Collection::new(CollectionOptions {
            system_fonts: false,
            ..Default::default()
        });

        let mut families = Vec::new();
        for (i, face) in super::FONTS.iter().enumerate() {
            let blob = Blob::new(Arc::new(*face) as _);
            let registered = collection.register_fonts(blob, None);
            assert!(!registered.is_empty(), "bundled face {i} registered nothing");
            for (family, fonts) in registered {
                assert!(!fonts.is_empty(), "bundled face {i} registered no fonts");
                let name = collection
                    .family_name(family)
                    .expect("registered family has no name")
                    .to_string();
                assert!(
                    sheet.contains(&name),
                    "bundled family {name:?} is not named by the reading stylesheet, \
                     so nothing will ever use it"
                );
                families.push(name);
            }
        }

        families.sort();
        families.dedup();
        assert_eq!(families, ["Charis SIL", "IBM Plex Mono", "Literata"]);
    }

    use super::*;
    use crate::cfi;
    use crate::net::ORIGIN;
    use crate::style::{GUTTER_EM, MEASURE_EM, ReadingStyle, Theme};

    /// The Phase 3 milestone: a saved position must survive a font-size change.
    /// A page *number* cannot do this, which is why positions anchor to CFI.
    #[test]
    fn hyphenation_moves_no_offset_and_hides_no_word() {
        let body = "<p>Many died of smallpox, an extraordinarily contagious disease.</p>";
        let st = ReadingStyle::default();
        let h = crate::hyphen::Hyphenator_::for_language("en").unwrap();
        let plain = layout_fragment(body, &st, viewport(760, 1000, 1.0, false));
        let marked = layout_fragment(&crate::hyphen::mark_html(body, &h), &st, viewport(760, 1000, 1.0, false));

        let word = |d: &BaseDocument| {
            let p = node_containing_text(d, "extraordinarily").expect("found despite soft hyphens");
            let text = &text_layout(d, p).unwrap().text;
            let (b0, b1) = (text.find("ex").unwrap(), text.find(" con").unwrap());
            (p, char_span(d, p, b0..b1).unwrap())
        };
        let ((p0, a), (p1, b)) = (word(&plain), word(&marked));
        assert!(text_layout(&marked, p1).unwrap().text.contains('\u{AD}'), "the marked one is hyphenated");
        assert_eq!(a, b, "same offset, length and text with or without hyphenation");
        assert_eq!(b.2, "extraordinarily");
        assert_eq!(highlight_rects(&plain, p0, a.0, a.1).len(), highlight_rects(&marked, p1, b.0, b.1).len());
        assert!(!highlight_rects(&marked, p1, b.0, b.1).is_empty());
    }

    /// A link target on an inline element is where its text is, not at the
    /// top of its paragraph: a note's back-link pointed at a `<small id>` at the
    /// end of a long paragraph and landed a page early.
    #[test]
    fn an_inline_target_is_found_on_its_own_line() {
        let words = "palabra ".repeat(400);
        let body = format!(r#"<p>{words}<sup><small id="r4">[4]</small></sup></p>"#);
        let doc = layout_fragment(&body, &ReadingStyle::default(), viewport(400, 1000, 1.0, false));
        let p = node_containing_text(&doc, "palabra").unwrap();
        let small = (0..10_000).find(|&i| tag_of(&doc, i).as_deref() == Some("small")).unwrap();
        let (para, at) = (node_top(&doc, p).unwrap(), node_top(&doc, small).unwrap());
        let bottom = para + node_rect(&doc, p).unwrap().3;
        assert!(at > para + 1000.0, "landed at {at}, paragraph starts at {para}");
        assert!(at < bottom, "past the paragraph: {at} >= {bottom}");
        // A block keeps its own box.
        assert_eq!(node_top(&doc, p), Some(para));
    }

    #[test]
    fn a_selection_runs_across_paragraphs() {
        let body = "<p>First paragraph.</p><p>Middle one.</p><p>Last paragraph.</p>";
        let doc = layout_fragment(body, &ReadingStyle::default(), viewport(760, 1000, 1.0, false));
        let p = |t| node_containing_text(&doc, t).unwrap();
        let (a, m, z) = (p("First"), p("Middle"), p("Last"));

        // "paragraph." … "Last", dragged down or up: the same three pieces.
        let down = ranges_between(&doc, (a, 6), (z, 4));
        assert_eq!(down, ranges_between(&doc, (z, 4), (a, 6)));
        let texts: Vec<_> = down.iter().map(|(id, r)| char_span(&doc, *id, r.clone()).unwrap().2).collect();
        assert_eq!(texts, ["paragraph.", "Middle one.", "Last"]);
        assert_eq!(down[1].0, m);

        // A highlight stored as one offset and length paints all three.
        let one = highlight_rects(&doc, a, 6, 10).len();
        assert_eq!(highlight_rects(&doc, a, 6, 10 + 11 + 4).len(), one * 3);
        assert!(highlight_rects(&doc, a, 99, 4).is_empty(), "an offset past the end paints nothing");
    }

    #[test]
    fn spaces_at_element_edges_survive_layout() {
        let html = r#"<p><span class="a">that the </span><span class="i">entire</span><span class="a"> world is</span></p>"#;
        let st = ReadingStyle::default();
        let doc = layout_fragment(&hoist_edge_spaces(html), &st, viewport(760, 1000, 1.0, false));
        assert_eq!(text_of(&doc, 0), "that the entire world is");

        assert_eq!(hoist_edge_spaces("a<em><b> x</b></em>"), "a <em><b>x</b></em>", "through a run of tags");
        assert_eq!(hoist_edge_spaces("<i>y </i></b>z"), "<i>y</i></b> z", "and out of nested ones");
        assert_eq!(hoist_edge_spaces("<pre><code>  x\n </code></pre>"), "<pre><code>  x\n </code></pre>", "pre is left alone");
        assert_eq!(hoist_edge_spaces("a<br/> b<img src=x> c"), "a<br/> b<img src=x> c", "void tags have no edge");
    }

    #[test]
    fn a_note_reference_is_set_in_superior_digits() {
        let r = raise_superscripts;
        assert_eq!(
            r(r#"a lake.<a href="n.html#f8"><sup class="calibre6">18</sup></a>"#),
            r#"a lake.<a href="n.html#f8"><sup data-raised class="calibre6">¹⁸</sup></a>"#
        );
        assert_eq!(r("x<SUP><small>[4]</small></SUP>"), "x<sup data-raised><small>⁽⁴⁾</small></SUP>");
        assert_eq!(r("1<sup>er</sup> 2<sup>1,2</sup>"), "1<sup>er</sup> 2<sup>1,2</sup>", "not a number: left alone");
        assert_eq!(r("<supx>3</supx><sup>"), "<supx>3</supx><sup>", "another tag, and an unclosed one");
    }

    #[test]
    fn lines_are_measured_in_css_pixels_at_fractional_scale() {
        // blitz lays parley out at device scale, so a line's metrics are in
        // device pixels. Taken raw, the tenth line of a paragraph sat two
        // lines lower than it was painted at 1.25, and page breaks sliced
        // lines in half (Omarchy at 1.25x).
        let long = "Un párrafo largo que ocupa muchas líneas en la página. ".repeat(30);
        let body = format!("<p>{long}</p><p>{long}</p>");
        let st = ReadingStyle { theme: Theme::White, scale: 1.0, omarchy: None };
        let doc = layout_fragment(&body, &st, viewport(950, 1250, 1.25, false));
        let atoms = collect_atoms(&doc);
        assert!(atoms.len() > 20, "need a long paragraph");
        for a in &atoms {
            let (_, y, _, h) = node_rect(&doc, a.group).unwrap();
            assert!(a.top >= y - 1.0 && a.bottom <= y + h + 1.0, "line {}..{} outside its paragraph {y}..{}", a.top, a.bottom, y + h);
        }
        let lines: usize = page_text(&doc, 0.0, f32::MAX).iter().map(|(_, t)| t.len()).sum();
        let half = page_text(&doc, 0.0, atoms[atoms.len() / 2].top).iter().map(|(_, t)| t.len()).sum::<usize>();
        assert!(half * 3 > lines && half * 3 < lines * 2, "half the lines hold about half the text: {half} of {lines}");
    }

    #[test]
    fn page_text_is_the_lines_on_that_page() {
        let body: String = std::iter::once("<h2>Uno</h2>".to_string())
            .chain((0..40).map(|i| format!("<p>Párrafo {i}, con bastante texto para que la línea se rompa varias veces.</p>")))
            .collect();
        let st = ReadingStyle { theme: Theme::White, scale: 1.0, omarchy: None };
        let doc = layout_fragment(&body, &st, viewport(760, 1000, 1.0, false));
        let pages = paginate_dom(&doc, 900.0);
        assert!(pages.count() > 1, "need two pages");

        let first = page_text(&doc, pages.top_of(0), pages.top_of(1));
        let second = page_text(&doc, pages.top_of(1), pages.top_of(1) + pages.extent_of(1));
        assert_eq!(first[0], (2, "Uno".to_string()), "the heading, with its level");
        assert!(first[1].1.starts_with("Párrafo 0,"), "{:?}", first[1]);
        assert!(!second.iter().any(|(_, t)| t == "Uno"), "nothing from page one on page two");
        assert!(!first.iter().chain(&second).any(|(_, t)| t.contains('\u{AD}')), "no soft hyphens");
    }

    #[test]
    fn a_saved_position_survives_a_reflow() {
        let body: String = (0..60)
            .map(|i| format!("<p>Párrafo número {i} con bastante texto para que la línea se rompa varias veces y la página cambie al variar el cuerpo.</p>"))
            .collect();

        let small = ReadingStyle { theme: Theme::White, scale: 1.0, omarchy: None };
        let vp = viewport(760, 1000, 1.0, false);
        let doc = layout_fragment(&body, &small, vp);
        let pages_small = paginate_dom(&doc, 900.0);
        assert!(pages_small.count() > 3, "need several pages to test with");

        // Anchor page 3.
        let page = 3;
        let top = pages_small.top_of(page);
        let node = node_at(&doc, top).expect("a node at the page top");
        let saved = cfi::of_node(&doc, node, 7).expect("a cfi");
        let text_at_save = doc
            .get_node(node)
            .and_then(|n| n.element_data())
            .and_then(|e| e.inline_layout_data.as_ref())
            .map(|t| t.text.clone());

        // Reopen at a larger body size: different line breaking, different pages.
        let big = ReadingStyle { theme: Theme::White, scale: 1.4, omarchy: None };
        let doc2 = layout_fragment(&body, &big, viewport(760, 1000, 1.0, false));
        let pages_big = paginate_dom(&doc2, 900.0);
        assert_ne!(
            pages_big.count(),
            pages_small.count(),
            "font change should have re-flowed the text"
        );

        let node2 = cfi::resolve(&doc2, &saved).expect("cfi resolves after reflow");
        let y2 = node_top(&doc2, node2).expect("node has a position");
        let page2 = pages_big.page_containing(y2);

        // Same paragraph, wherever it now falls.
        let text_after = doc2
            .get_node(node2)
            .and_then(|n| n.element_data())
            .and_then(|e| e.inline_layout_data.as_ref())
            .map(|t| t.text.clone());
        assert_eq!(text_at_save, text_after, "resumed at a different paragraph");
        assert!(page2 < pages_big.count());
    }

    fn style() -> ReadingStyle {
        ReadingStyle { theme: Theme::White, scale: 1.0, omarchy: None }
    }

    /// The column must sit in the middle of the window, not against its left
    /// edge, at every width wider than the measure.
    #[test]
    fn one_column_text_is_centred() {
        let st = style();
        let em = st.font_px();
        let column = (MEASURE_EM + 2.0 * GUTTER_EM) * em;

        for width in [900u32, 1200, 1600, 2400, 3840] {
            let vp = viewport(width, 1000, 1.0, false);
            let doc = layout_fragment("<p>Un texto cualquiera para medir.</p>", &st, vp);
            let (x, _, w, _) = element_rect(&doc, "body").expect("body has a layout box");

            assert!(
                (w - column).abs() < 1.0,
                "at {width}px the column should be {column:.0}px wide, got {w:.0}px"
            );

            let left = x;
            let right = width as f32 - (x + w);
            assert!(
                (left - right).abs() < 1.0,
                "at {width}px the column is not centred: {left:.0}px left, {right:.0}px right"
            );
            assert!(
                left > 0.0,
                "at {width}px the column is flush against the edge"
            );
        }
    }

    /// Narrower than the measure, the column fills the window but must keep its
    /// gutters rather than running text to the very edge.
    #[test]
    fn narrow_windows_keep_their_gutters() {
        let st = style();
        let vp = viewport(400, 1000, 1.0, false);
        let doc = layout_fragment("<p>Estrecho.</p>", &st, vp);
        let (bx, _, bw, _) = element_rect(&doc, "body").unwrap();
        let (px, _, pw, _) = element_rect(&doc, "p").unwrap();

        assert!((bw - 400.0).abs() < 1.0, "body should fill a narrow window, got {bw:.0}px");
        let gutter = GUTTER_EM * st.font_px();
        assert!(
            (px - bx - gutter).abs() < 1.0,
            "left gutter should be {gutter:.0}px, text starts {:.0}px in",
            px - bx
        );
        assert!(
            (bx + bw - (px + pw) - gutter).abs() < 1.0,
            "right gutter should be {gutter:.0}px"
        );
    }

    #[test]
    fn base_url_uses_the_chapter_directory() {
        assert_eq!(base_url_for("/OEBPS/ch09.html"), format!("{ORIGIN}OEBPS/"));
        assert_eq!(base_url_for("OEBPS/ch09.html"), format!("{ORIGIN}OEBPS/"));
        assert_eq!(base_url_for("/a/b/c.xhtml"), format!("{ORIGIN}a/b/"));
        assert_eq!(base_url_for("/ch01.html"), ORIGIN);
        assert_eq!(base_url_for("ch01.html"), ORIGIN);
    }


    /// A theme switch rebuilds the chapter and finds the page again by its
    /// CFI. The paginator may cut a hair inside a line (`EDGE_EPS`), and
    /// reading that line as the page before's sent the reader back a page.
    /// Page heights are swept so some cut lands in that hair.
    #[test]
    fn a_page_start_survives_a_rebuild() {
        let p = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. ".repeat(5);
        let html = format!(
            "<html><body style='margin:0;font-size:26px;line-height:1.6'>{}</body></html>",
            format!("<h2>Head</h2><p>{p}</p><p>{p}</p>").repeat(8)
        );
        let doc = super::layout_document(html, String::new(), None, super::viewport(900, 1300, 1.25, false), 910.0)
            .expect("the page must lay out");
        let mut wrong = Vec::new();
        for tenth in 0..420 {
            let pages = super::paginate_dom(doc.dom(), 900.0 + tenth as f32 / 10.0);
            for page in 0..pages.count() {
                let top = pages.top_of(page);
                let node = super::node_at(doc.dom(), top).expect("no node at the page top");
                let c = crate::cfi::at(doc.dom(), node, super::char_at(doc.dom(), node, top), 0).expect("no CFI");
                let y = super::top_of_cfi(&doc, &c).expect("the CFI must resolve");
                if pages.page_containing(y) != page {
                    wrong.push((tenth, page));
                }
            }
        }
        assert!(wrong.is_empty(), "came back on the page before: {wrong:?}");
    }
}
