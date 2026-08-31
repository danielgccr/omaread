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
    let [path, chapter_arg, page_arg, out, rest @ ..] = args else {
        eprintln!(
            "usage: omaread --shot <book.epub> <chapter> <page> <out.ppm> [hud] [theme]\n\
             chapter and page are 1-based; page may instead be text to land on;\n\
             size comes from OMAREAD_SHOT_SIZE=WxH"
        );
        return 2;
    };

    let (w, h) = size_from_env();
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
    let make_viewport = || chapter::viewport(w, h, 1.0, theme == Theme::Night);

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
    let top = ch.pages.top_of(page);

    let mut hud_doc = with_hud
        .then(|| {
            let within = match ch.pages.content_height > 0.0 {
                true => top / ch.pages.content_height,
                false => 0.0,
            };
            let percent = (book.progress(index, within) * 100.0).round() as u8;
            let (_, fg, subtle, panel) = theme.chrome_colors();
            chapter::layout_document(
                hud::html(&book.title, percent, h as f32),
                hud::stylesheet(fg, subtle, panel),
                None,
                make_viewport(),
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

    let mut renderer = VelloImageRenderer::new(w, h);
    let mut rgba = Vec::new();
    renderer.render_to_vec(
        |scene| {
            paint::page(scene, &mut ch.doc, top, extent, &frame, Color::from_rgb8(r, g, b));
            if let Some(c) = hud_doc.as_mut() {
                paint::overlay(scene, &mut c.doc, &frame);
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
