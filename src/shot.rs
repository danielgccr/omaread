//! Headless page render — what the reader actually paints, as an image file.
//!
//! There is no way to eyeball a Wayland window from a script, and "it laid out
//! without panicking" is not the same as "it looks right". This renders through
//! the very same `paint` path the window uses, so what comes out is what would
//! have been on screen.
//!
//! Writes a binary PPM: every image library is a dependency, and a P6 header
//! plus RGB triples is nine lines. `magick shot.ppm shot.png` if you want a PNG.

use crate::book::Book;
use crate::chapter;
use crate::hud;
use crate::paint::{self, Frame};
use crate::style::{PAGE_MARGIN_EM, ReadingStyle, Theme};
use anyrender::ImageRenderer;
use anyrender_vello::VelloImageRenderer;
use blitz_dom::net::Resource;
use blitz_traits::net::{NetCallback, SharedCallback};
use peniko::Color;
use std::sync::Arc;

struct Discard;
impl NetCallback<Resource> for Discard {
    fn call(&self, _doc_id: usize, _result: Result<Resource, Option<String>>) {}
}

pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("library") {
        return library_shot(&args[1..]);
    }
    let [path, chapter_arg, page_arg, out, rest @ ..] = args else {
        eprintln!(
            "usage: omaread --shot <book.epub> <chapter> <page> <out.ppm> [hud] [theme]\n\
             chapter and page are 1-based; page may instead be text to land on;\n\
             size comes from OMAREAD_SHOT_SIZE=WxH"
        );
        return 2;
    };

    let (w, h) = size_from_env();
    // Two columns are two pages of one flow, so the document is laid out at
    // column width — exactly what the window does.
    let cols: u32 = std::env::var("OMAREAD_SHOT_COLUMNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .clamp(1, 2);
    let col_w = w / cols;
    let with_hud = rest.iter().any(|a| a == "hud");
    let theme = match rest.iter().find(|a| *a != "hud").map(String::as_str) {
        Some("sepia") => Theme::Sepia,
        Some("grey") => Theme::Grey,
        Some("night") => Theme::Night,
        _ => Theme::White,
    };

    let style = ReadingStyle { theme, ..ReadingStyle::default() };
    let margin = PAGE_MARGIN_EM * style.font_px();
    let page_height = (h as f32 - 2.0 * margin).max(1.0);
    let make_viewport = || chapter::viewport(col_w, h, 1.0, theme == Theme::Night);

    let book = match Book::open(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("omaread: {e}");
            return 1;
        }
    };

    let index = chapter_arg.parse::<usize>().unwrap_or(1).saturating_sub(1);
    let cb: SharedCallback<Resource> = Arc::new(Discard);
    let mut ch = match chapter::load(&book, index, &style, make_viewport(), page_height, None, cb) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("omaread: {e}");
            return 1;
        }
    };

    // A number is a page; anything else is text to land on, which is exactly
    // what following a search hit does.
    let page = match page_arg.parse::<usize>() {
        Ok(n) => n.saturating_sub(1),
        Err(_) => chapter::node_containing_text(ch.dom(), page_arg)
            .and_then(|n| chapter::node_top(ch.dom(), n))
            .map(|y| ch.pages.page_containing(y))
            .unwrap_or_else(|| {
                eprintln!("omaread: “{page_arg}” not found in this chapter");
                0
            }),
    };
    let page = page.min(ch.pages.count().saturating_sub(1));

    // Given text rather than a page number, highlight it too: that exercises the
    // same geometry a stored highlight is painted with.
    let highlights: Vec<(f32, f32, f32, f32)> = match page_arg.parse::<usize>() {
        Ok(_) => Vec::new(),
        Err(_) => chapter::node_containing_text(ch.dom(), page_arg)
            .and_then(|node| {
                let tl = chapter::text_layout(ch.dom(), node)?;
                let (at, len) = crate::search::char_match(&tl.text, page_arg)?;
                Some(chapter::highlight_rects(ch.dom(), node, at, len))
            })
            .unwrap_or_default(),
    };
    let top = ch.pages.top_of(page);

    let mut hud_doc = with_hud
        .then(|| {
            let within = match ch.pages.content_height > 0.0 {
                true => top / ch.pages.content_height,
                false => 0.0,
            };
            // `pages` shows the whole-book page number instead, through the
            // same arithmetic the window uses.
            let readout = match rest.iter().any(|a| a == "pages") {
                // Measures the whole book, the same way the window does when you
                // ask it for page numbers.
                true => {
                    let counts = chapter::page_counts(
                        &book, &style, None, col_w, h, 1.0, page_height,
                    );
                    let before: usize = counts.iter().take(index).sum();
                    let total: usize = counts.iter().sum();
                    format!("page {} of {}", (before + page + 1).min(total.max(1)), total.max(1))
                }
                false => format!("{}%", (book.progress(index, within) * 100.0).round() as u8),
            };

            let (_, fg, subtle, panel) = theme.chrome_colors();
            chapter::layout_document(
                hud::html(&book.title, &readout, cols as usize, false, h as f32),
                hud::stylesheet(fg, subtle, panel),
                None,
                // The HUD spans the window, not a column.
                chapter::viewport(w, h, 1.0, theme == Theme::Night),
                page_height,
            )
        })
        .flatten();

    // A page ends where the break landed, not at the nominal page height; the
    // gap between the two is what used to leak the next page's first line.
    let extent = ch.pages.extent_of(page);
    if std::env::var_os("OMAREAD_DEBUG_PAGES").is_some() {
        eprintln!(
            "shot: top={top:.1} extent={extent:.1} of page_height={page_height:.1} \
             -> content ends at row {:.1}, mask starts at {:.1}",
            margin + extent,
            margin + extent,
        );
    }

    let [r, g, b] = theme.background_rgb();
    // `OMAREAD_SHOT_NOMASK=1` drops the bottom mask down to the window edge, so
    // any ink it would normally have covered shows up instead of vanishing.
    // That is how you tell "the page ends here" from "the page is being cut".
    let painted_height = match std::env::var_os("OMAREAD_SHOT_NOMASK").is_some() {
        true => h as f32 - margin,
        false => page_height,
    };
    let frame = Frame { width: w, height: h, scale: 1.0, margin, page_height: painted_height };

    let count = ch.pages.count();
    let slices: Vec<(f32, f32)> = (0..cols as usize)
        .map(|c| page + c)
        .map(|p| match p < count {
            true => (ch.pages.top_of(p), ch.pages.extent_of(p)),
            false => (0.0, 0.0),
        })
        .collect();
    let column_css = col_w as f32;
    let ground = Color::from_rgb8(r, g, b);

    let mut renderer = VelloImageRenderer::new(w, h);
    let mut rgba = Vec::new();
    renderer.render_to_vec(
        |scene| {
            for (col, &(ctop, cextent)) in slices.iter().enumerate() {
                let x = col as f32 * column_css;
                let blank = page + col >= count;
                let (ctop, cextent) = if blank { (0.0, 0.0) } else { (ctop, cextent) };
                paint::column(
                    scene,
                    &mut ch.doc,
                    ctop,
                    cextent,
                    x,
                    column_css,
                    &frame,
                    ground,
                    col == 0,
                );
                if !blank {
                    paint::bands(
                        scene,
                        &highlights,
                        crate::HIGHLIGHT,
                        ctop,
                        cextent,
                        x,
                        &frame,
                    );
                }
            }
            if let Some(c) = hud_doc.as_mut() {
                // Icons are painted into the boxes the HUD reserved, exactly as
                // the window does it.
                let icons: Vec<(paint::Icon, (f32,f32,f32,f32))> = [
                    ("bookmark", paint::Icon::Bookmark),
                    ("contents", paint::Icon::Contents),
                    ("highlight", paint::Icon::Highlight),
                    ("back", paint::Icon::Back),
                ]
                .into_iter()
                .filter_map(|(name, icon)| {
                    let node = crate::find_by_attr(c.dom(), 0, "data-icon", name)?;
                    Some((icon, chapter::node_rect(c.dom(), node)?))
                })
                .collect();

                if std::env::var_os("OMAREAD_DEBUG_PAINT").is_some() {
                    eprintln!("ICONS {icons:?}");
                }
                paint::overlay(scene, &mut c.doc, &frame);
                let ink = crate::parse_hex(theme.chrome_colors().1);
                for (icon, rect) in icons {
                    paint::icon(scene, icon, rect, ink, 1.0);
                }
            }
        },
        &mut rgba,
    );

    match write_ppm(out, &rgba, w, h) {
        Ok(()) => {
            println!(
                "{out}: {w}x{h}, chapter {}/{}, page {}/{}",
                index + 1,
                book.chapter_count(),
                page + 1,
                ch.pages.count()
            );
            0
        }
        Err(e) => {
            eprintln!("omaread: {e}");
            1
        }
    }
}

/// Render the library view. The grid is a document like any other, so this is
/// the same paint path — which is the point: a shot that renders differently
/// from the window verifies nothing.
///
/// Covers stay blank: they arrive through the resource callback, which needs an
/// event loop to drain. Layout is what this is for.
fn library_shot(args: &[String]) -> i32 {
    let [out, rest @ ..] = args else {
        eprintln!("usage: omaread --shot library <out.ppm> [query]");
        return 2;
    };
    let query = rest.first().cloned().unwrap_or_default();

    let db = match crate::db::Db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("omaread: {e}");
            return 1;
        }
    };
    let rows = db.books(&query, crate::db::Sort::Recent).unwrap_or_default();
    let suggestions = match query.trim().is_empty() {
        true => Vec::new(),
        false => db.suggestions(&query, 6),
    };
    println!("library: {} books, {} suggestions", rows.len(), suggestions.len());

    let (w, h) = size_from_env();
    let style = ReadingStyle::default();
    let margin = PAGE_MARGIN_EM * style.font_px();
    let page_height = (h as f32 - 2.0 * margin).max(1.0);
    let (bg, fg, subtle, panel) = Theme::White.chrome_colors();

    let Some(mut doc) = chapter::layout_document(
        crate::grid::html(&rows, &query, crate::db::Sort::Recent, &suggestions, None, 0..rows.len()),
        crate::grid::stylesheet(bg, fg, subtle, panel),
        None,
        chapter::viewport(w, h, 1.0, false),
        page_height,
    ) else {
        eprintln!("omaread: the library document failed to lay out");
        return 1;
    };

    let frame = Frame { width: w, height: h, scale: 1.0, margin, page_height };
    let extent = doc.pages.extent_of(0);
    let mut renderer = VelloImageRenderer::new(w, h);
    let mut rgba = Vec::new();
    renderer.render_to_vec(
        |scene| {
            paint::page(scene, &mut doc.doc, 0.0, extent, &frame, crate::parse_hex(bg));
            // Outline the first card, the way the window outlines the selection.
            if let Some(rect) = crate::find_by_attr(doc.dom(), 0, "data-index", "0")
                .and_then(|n| chapter::node_rect(doc.dom(), n))
            {
                paint::outline(
                    scene,
                    (rect.0, rect.1 + margin, rect.2, rect.3),
                    crate::CARD_OUTLINE,
                    2.0,
                    1.0,
                );
            }
        },
        &mut rgba,
    );

    match write_ppm(out, &rgba, w, h) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("omaread: {e}");
            1
        }
    }
}

fn size_from_env() -> (u32, u32) {
    let spec = std::env::var("OMAREAD_SHOT_SIZE").unwrap_or_default();
    let parsed = spec.split_once(['x', 'X']).and_then(|(w, h)| {
        Some((w.trim().parse::<u32>().ok()?.max(1), h.trim().parse::<u32>().ok()?.max(1)))
    });
    parsed.unwrap_or((900, 1000))
}

fn write_ppm(path: &str, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{w} {h}\n255\n")?;
    for px in rgba.chunks_exact(4) {
        out.write_all(&px[..3])?;
    }
    out.flush()
}
