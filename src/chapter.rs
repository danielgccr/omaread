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
use blitz_dom::{BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_traits::net::{NetProvider, SharedCallback};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

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
        walk_elements(self.dom(), 0, &mut |el| {
            if let Some(tl) = el.inline_layout_data.as_ref() {
                n += tl.text.len();
            }
        });
        n
    }

    /// Line boxes across the chapter — the atoms Phase 2 paginates on.
    pub fn line_count(&self) -> usize {
        let mut n = 0;
        walk_elements(self.dom(), 0, &mut |el| {
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

    /// Recompute pages for a new page height, preserving the reader's place.
    pub fn repaginate(&mut self, page_height: f32) {
        let anchor = self.pages.top_of(self.pages.page_containing(self.pages.top_of(0)));
        let _ = anchor;
        self.pages = paginate_dom(self.dom(), page_height);
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
    // (absolute y of each cell, height, owning table id) before banding.
    let mut cells: Vec<(f32, f32, usize)> = Vec::new();

    fn walk(
        dom: &BaseDocument,
        id: usize,
        parent_y: f32,
        table: Option<usize>,
        atoms: &mut Vec<Atom>,
        cells: &mut Vec<(f32, f32, usize)>,
    ) {
        let Some(node) = dom.get_node(id) else { return };
        let l = &node.final_layout;
        let abs_y = parent_y + l.location.y;

        let mut table = table;

        if let blitz_dom::NodeData::Element(el) = &node.data {
            let tag = el.name.local.as_ref();

            match tag {
                "table" => table = Some(id),
                "td" | "th" => {
                    if let Some(t) = table {
                        cells.push((abs_y, l.size.height, t));
                    }
                }
                "img" | "svg" | "hr" | "video" if l.size.height > 0.0 => {
                    atoms.push(Atom {
                        top: abs_y,
                        bottom: abs_y + l.size.height,
                        group: id,
                        kind: AtomKind::Block,
                        keep_with_next: false,
                    });
                }
                _ => {}
            }

            if let Some(tl) = el.inline_layout_data.as_ref() {
                // The parley layout origin is the element's content box.
                let content_top = abs_y + l.border.top + l.padding.top;
                let heading = matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
                for line in tl.layout.lines() {
                    let m = line.metrics();
                    let top = content_top + m.min_coord;
                    let bottom = content_top + m.max_coord;
                    if bottom > top {
                        atoms.push(Atom {
                            top,
                            bottom,
                            group: id,
                            kind: AtomKind::Line,
                            keep_with_next: heading,
                        });
                    }
                }
            }
        }

        for child in &node.children {
            walk(dom, *child, abs_y, table, atoms, cells);
        }
    }

    walk(dom, 0, 0.0, None, &mut atoms, &mut cells);

    // Band the cells: same table, same top (within a pixel) is one row.
    let mut bands: Vec<Atom> = Vec::new();
    cells.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.total_cmp(&b.0)));
    let mut i = 0;
    while i < cells.len() {
        let (top, mut height, table) = cells[i];
        let mut j = i + 1;
        while j < cells.len() && cells[j].2 == table && (cells[j].0 - top).abs() < 1.0 {
            height = height.max(cells[j].1);
            j += 1;
        }
        if height > 0.0 {
            bands.push(Atom {
                top,
                bottom: top + height,
                group: table,
                kind: AtomKind::Row,
                keep_with_next: false,
            });
        }
        i = j;
    }

    // Inside a table the *row* is the atom. The lines within a cell overlap their
    // band, and if they are kept they veto breaks at the very row boundaries the
    // band exists to provide — producing a chain of overlapping atoms with no
    // legal break, which forced a hard cut through a line. Drop them.
    if !bands.is_empty() {
        atoms.retain(|a| {
            !bands
                .iter()
                .any(|b| a.top >= b.top && a.bottom <= b.bottom)
        });
        atoms.extend(bands);
    }

    atoms
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
    let mut html = style::strip_publisher_css(&raw);

    // Justified text without hyphenation opens rivers, badly so in Spanish and
    // Catalan. Parley breaks on U+00AD, so mark the words before parsing.
    if let Some(h) = hyphenator {
        html = crate::hyphen::mark_html(&html, h);
    }

    let base_url = base_url_for(&href);
    let ua = style.stylesheet();
    let provider: Arc<dyn NetProvider<Resource>> =
        Arc::new(BookNetProvider::new(book.clone(), net_callback));

    let result = build(html, base_url, ua, Some(provider), viewport);

    match result {
        Ok(doc) => {
            let pages = paginate_dom(&doc, page_height);
            Ok(Chapter { doc, index, pages })
        }
        Err(()) => Err(format!(
            "the layout engine panicked on chapter {index} ({href}); skipping it"
        )),
    }
}

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
                ..Default::default()
            },
        );
        doc.resolve(0.0);
        doc
    }))
    .map_err(|_| ())
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

/// Lay out a bare HTML fragment with the reading stylesheet. Used by tests to
/// assert geometry without needing a book on disk.
#[cfg(test)]
pub fn layout_fragment(body: &str, style: &ReadingStyle, viewport: Viewport) -> HtmlDocument {
    let html = format!("<html><body>{body}</body></html>");
    build(html, ORIGIN.to_string(), style.stylesheet(), None, viewport)
        .expect("engine panicked laying out a test fragment")
}

/// Absolute Y of a node in the flow, for resolving a CFI back to a page.
pub fn node_top(dom: &BaseDocument, target: usize) -> Option<f32> {
    fn walk(dom: &BaseDocument, id: usize, y: f32, target: usize) -> Option<f32> {
        let node = dom.get_node(id)?;
        let at = y + node.final_layout.location.y;
        if id == target {
            return Some(at);
        }
        node.children.iter().find_map(|c| walk(dom, *c, at, target))
    }
    walk(dom, 0, 0.0, target)
}

/// The node a page begins at — the first atom at or after the page top.
/// This is what a CFI is generated from.
pub fn node_at(dom: &BaseDocument, y: f32) -> Option<usize> {
    collect_atoms(dom)
        .into_iter()
        .filter(|a| a.bottom > y)
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
        node.children.iter().find_map(|c| walk(dom, *c, at, tag))
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
fn walk_elements(dom: &BaseDocument, id: usize, f: &mut impl FnMut(&blitz_dom::ElementData)) {
    let Some(node) = dom.get_node(id) else { return };
    if let blitz_dom::NodeData::Element(el) = &node.data {
        f(el);
    }
    for child in &node.children {
        walk_elements(dom, *child, f);
    }
}

fn content_height(dom: &BaseDocument) -> f32 {
    fn walk(dom: &BaseDocument, id: usize, parent_y: f32, max_y: &mut f32) {
        let Some(node) = dom.get_node(id) else { return };
        let abs_y = parent_y + node.final_layout.location.y;
        *max_y = max_y.max(abs_y + node.final_layout.size.height);
        for child in &node.children {
            walk(dom, *child, abs_y, max_y);
        }
    }
    let mut max_y = 0.0;
    walk(dom, 0, 0.0, &mut max_y);
    max_y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfi;
    use crate::net::ORIGIN;
    use crate::style::{GUTTER_EM, MEASURE_EM, ReadingStyle, Theme};

    /// The Phase 3 milestone: a saved position must survive a font-size change.
    /// A page *number* cannot do this, which is why positions anchor to CFI.
    #[test]
    fn a_saved_position_survives_a_reflow() {
        let body: String = (0..60)
            .map(|i| format!("<p>Párrafo número {i} con bastante texto para que la línea se rompa varias veces y la página cambie al variar el cuerpo.</p>"))
            .collect();

        let small = ReadingStyle { theme: Theme::White, scale: 1.0 };
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
        let big = ReadingStyle { theme: Theme::White, scale: 1.4 };
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
        ReadingStyle { theme: Theme::White, scale: 1.0 }
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
}
