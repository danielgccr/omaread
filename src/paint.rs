//! Painting a page, and compositing more than one document into a frame.
//!
//! `blitz_paint::paint_scene` calls `scene.reset()` on entry (CONTEXT.md §9), so
//! for a long time the page was the only thing that could be painted: a second
//! call erased the first. But that function is generic over `impl PaintScene`,
//! and the trait is ours to implement — so a wrapper whose `reset` does nothing
//! lets a second document be painted *over* the first, and one that also
//! pre-multiplies every transform can put it somewhere else on the screen.
//!
//! That wrapper is [`Compose`], and it is the whole mechanism behind both the
//! reading HUD and the second column. §9 notes that a *layer* transform moves
//! the clip shape rather than the content; a transform applied to every drawing
//! command is a different thing, and it does move the content.

use anyrender::{Glyph, NormalizedCoord, PaintRef, PaintScene};
use blitz_html::HtmlDocument;
use blitz_dom::Point;
use kurbo::{Affine, Rect, Shape, Stroke};
use peniko::{BlendMode, Color, Fill, FontData, StyleRef};

/// Window geometry for one frame, in the units each part of the paint path
/// wants: physical pixels for the surface, CSS pixels for the page box.
#[derive(Clone, Copy)]
pub struct Frame {
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// CSS pixels.
    pub margin: f32,
    pub page_height: f32,
}

/// Paint one column: the slice `[top, top + extent)` of the flow, at `x`, with
/// everything above and below masked back to the page ground.
///
/// `first` resets the scene, so the first column of a frame clears it and the
/// rest compose on top. The masks are per column because two columns are two
/// different pages, and a break lands where it lands — a single full-width band
/// would clip the longer column or leak the shorter one's next line.
///
/// `x` and `width` are in CSS pixels.
#[allow(clippy::too_many_arguments)]
pub fn column(
    scene: &mut impl PaintScene,
    doc: &mut HtmlDocument,
    top: f32,
    extent: f32,
    x: f32,
    width: f32,
    frame: &Frame,
    ground: Color,
    first: bool,
) {
    let Frame { height, scale, margin, page_height, .. } = *frame;
    let extent = extent.clamp(0.0, page_height);

    doc.set_viewport_scroll(Point { x: 0.0, y: (top - margin) as f64 });

    // Painting at column width keeps the document's own background inside the
    // column; the scene is otherwise identical to a one-column frame.
    let w = (width * scale as f32).round() as u32;
    if first {
        blitz_paint::paint_scene(scene, doc, scale, w, height);
    } else {
        let shift = Affine::translate((x as f64 * scale, 0.0));
        let mut shifted = Compose { inner: scene, shift };
        blitz_paint::paint_scene(&mut shifted, doc, scale, w, height);
    }

    ground_band(scene, x, width, 0.0, margin, ground, frame);
    ground_band(scene, x, width, margin + extent, height as f32 / scale as f32, ground, frame);
}

/// Fill a horizontal band of one column with the page ground, in CSS pixels.
fn ground_band(
    scene: &mut impl PaintScene,
    x: f32,
    width: f32,
    y0: f32,
    y1: f32,
    ground: Color,
    frame: &Frame,
) {
    if y1 <= y0 {
        return;
    }
    let s = frame.scale;
    PaintScene::fill(
        scene,
        Fill::NonZero,
        Affine::IDENTITY,
        ground,
        None,
        &Rect::new(x as f64 * s, y0 as f64 * s, (x + width) as f64 * s, y1 as f64 * s),
    );
}

/// Blank the frame to the page ground.
///
/// A column painted with `first` resets the scene itself, but only at x = 0 —
/// during a page turn every column is offset, so the reset has to happen here
/// instead.
pub fn clear(scene: &mut impl PaintScene, ground: Color, frame: &Frame) {
    PaintScene::reset(scene);
    PaintScene::fill(
        scene,
        Fill::NonZero,
        Affine::IDENTITY,
        ground,
        None,
        &Rect::new(0.0, 0.0, frame.width as f64, frame.height as f64),
    );
}

/// Paint one page of a flow: the slice `[top, top + extent)`, with everything
/// above and below masked back to the page ground.
///
/// `extent` is the page's real height — `Pages::extent_of` — not the nominal
/// `page_height`. A break snaps up to a line boundary, so the two differ by up
/// to a line, and masking from the nominal height leaves the next page's first
/// line showing, sliced through the middle.
///
/// This resets the scene, so it goes first in a frame.
pub fn page(
    scene: &mut impl PaintScene,
    doc: &mut HtmlDocument,
    top: f32,
    extent: f32,
    frame: &Frame,
    ground: Color,
) {
    let Frame { width, height, scale, margin, page_height } = *frame;
    let extent = extent.clamp(0.0, page_height);

    // `viewport_scroll` is the only thing that moves painted content: a layer
    // transform repositions a clip shape but not the drawing commands inside it.
    // Offsetting by `top - margin` puts this page's first line just below the
    // top margin.
    doc.set_viewport_scroll(Point { x: 0.0, y: (top - margin) as f64 });

    // NOTE: paint_scene resets the scene, discarding anything drawn before it,
    // so the margin masks must come *after*.
    blitz_paint::paint_scene(scene, doc, scale, width, height);

    // The flow continues above and below this page. Mask it, so the margins
    // show the neighbouring lines' halves as clean paper.
    let mut band = |y0: f64, y1: f64| {
        PaintScene::fill(
            scene,
            Fill::NonZero,
            Affine::IDENTITY,
            ground,
            None,
            &Rect::new(0.0, y0, width as f64, y1),
        );
    };
    band(0.0, margin as f64 * scale);
    band((margin + extent) as f64 * scale, height as f64);
}

/// Stroke a rounded outline around a rectangle given in CSS pixels.
///
/// The library's selection is painted rather than marked up: baking a
/// `.selected` class into the grid meant rebuilding the document on every arrow
/// key, and rebuilding re-requests every cover — 358 JPEG decodes, 1.4s, per
/// keypress. An outline also does not tint the cover it sits on.
pub fn outline(
    scene: &mut impl PaintScene,
    rect: (f32, f32, f32, f32),
    color: Color,
    width: f64,
    scale: f64,
) {
    let (x, y, w, h) = rect;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = kurbo::RoundedRect::new(
        x as f64 * scale,
        y as f64 * scale,
        (x + w) as f64 * scale,
        (y + h) as f64 * scale,
        6.0 * scale,
    );
    PaintScene::stroke(
        scene,
        &Stroke::new(width * scale),
        Affine::IDENTITY,
        color,
        None,
        &r,
    );
}

/// Fill a rounded rectangle given in CSS pixels — the wash under the pointer.
///
/// Painted *over* the surface, translucent, for the same reason the outline is
/// painted at all: a `:hover` class would mean rebuilding the document on every
/// mouse move. The HUD's own ground is opaque, so a wash beneath it would not
/// show at all.
pub fn wash(scene: &mut impl PaintScene, rect: (f32, f32, f32, f32), color: Color, scale: f64) {
    let (x, y, w, h) = rect;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = kurbo::RoundedRect::new(
        x as f64 * scale,
        y as f64 * scale,
        (x + w) as f64 * scale,
        (y + h) as f64 * scale,
        6.0 * scale,
    );
    PaintScene::fill(scene, Fill::NonZero, Affine::IDENTITY, color, None, &r);
}

/// Which glyph a HUD control wants. The bundled faces carry no symbol glyphs
/// (checked: no ☰, no ⚑, no ✎ in Literata, Charis SIL or IBM Plex Mono), and a
/// missing glyph is visible tofu — so the icons are drawn, not typeset.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Icon {
    /// A pennant: bookmark.
    Bookmark,
    /// Three rules: contents.
    Contents,
    /// A marker stroke: highlight.
    Highlight,
    /// A left-pointing triangle: back to the library.
    Back,
}

/// Draw an icon inside a rectangle given in CSS pixels.
///
/// Shapes are described in a unit box and scaled, so they stay sharp at any
/// window scale and follow whatever colour the chrome is using.
pub fn icon(scene: &mut impl PaintScene, which: Icon, rect: (f32, f32, f32, f32), color: Color, scale: f64) {
    let (rx, ry, rw, rh) = rect;
    if rw <= 0.0 || rh <= 0.0 {
        return;
    }
    // Square, centred, and inset a little so it sits with the cap height.
    let side = rw.min(rh);
    let (ox, oy) = (rx + (rw - side) / 2.0, ry + (rh - side) / 2.0);
    let at = |ux: f64, uy: f64| {
        ((ox as f64 + ux * side as f64) * scale, (oy as f64 + uy * side as f64) * scale)
    };
    let mut path = kurbo::BezPath::new();

    match which {
        Icon::Bookmark => {
            for (i, (ux, uy)) in
                [(0.22, 0.08), (0.78, 0.08), (0.78, 0.92), (0.5, 0.68), (0.22, 0.92)]
                    .into_iter()
                    .enumerate()
            {
                let p = at(ux, uy);
                match i {
                    0 => path.move_to(p),
                    _ => path.line_to(p),
                }
            }
            path.close_path();
        }
        Icon::Contents => {
            for uy in [0.2, 0.47, 0.74] {
                let (x0, y0) = at(0.14, uy);
                let (x1, y1) = at(0.86, uy + 0.13);
                path.move_to((x0, y0));
                path.line_to((x1, y0));
                path.line_to((x1, y1));
                path.line_to((x0, y1));
                path.close_path();
            }
        }
        Icon::Highlight => {
            // A diagonal marker, with the stroke it just laid down beneath it.
            for (i, (ux, uy)) in
                [(0.10, 0.62), (0.62, 0.10), (0.82, 0.30), (0.30, 0.82)].into_iter().enumerate()
            {
                let p = at(ux, uy);
                match i {
                    0 => path.move_to(p),
                    _ => path.line_to(p),
                }
            }
            path.close_path();
            let (x0, y0) = at(0.10, 0.86);
            let (x1, y1) = at(0.90, 0.99);
            path.move_to((x0, y0));
            path.line_to((x1, y0));
            path.line_to((x1, y1));
            path.line_to((x0, y1));
            path.close_path();
        }
        Icon::Back => {
            for (i, (ux, uy)) in
                [(0.68, 0.12), (0.68, 0.88), (0.22, 0.5)].into_iter().enumerate()
            {
                let p = at(ux, uy);
                match i {
                    0 => path.move_to(p),
                    _ => path.line_to(p),
                }
            }
            path.close_path();
        }
    }

    PaintScene::fill(scene, Fill::NonZero, Affine::IDENTITY, color, None, &path);
}

/// Fill flow-coordinate rectangles over the page: a live selection, or a stored
/// highlight.
///
/// Drawn after the page, so it sits on top of the glyphs — which is what a
/// highlighter pen does. Rectangles are clipped to the page band, or a highlight
/// belonging to the next page would bleed into this one's margin.
#[allow(clippy::too_many_arguments)]
pub fn bands(
    scene: &mut impl PaintScene,
    rects: &[(f32, f32, f32, f32)],
    color: Color,
    top: f32,
    extent: f32,
    dx: f32,
    frame: &Frame,
) {
    let Frame { scale, margin, .. } = *frame;
    let (lo, hi) = (margin, margin + extent.min(frame.page_height));

    for &(x0, y0, x1, y1) in rects {
        // Flow -> screen, the same shift the page itself gets.
        let (sy0, sy1) = (y0 - top + margin, y1 - top + margin);
        let (cy0, cy1) = (sy0.max(lo), sy1.min(hi));
        if cy1 <= cy0 || x1 <= x0 {
            continue;
        }
        PaintScene::fill(
            scene,
            Fill::NonZero,
            Affine::IDENTITY,
            color,
            None,
            &Rect::new(
                ((x0 + dx) as f64) * scale,
                (cy0 as f64) * scale,
                ((x1 + dx) as f64) * scale,
                (cy1 as f64) * scale,
            ),
        );
    }
}

/// Paint a second document over whatever is already in the scene.
///
/// The document is laid out at window size and positions itself with margins,
/// so nothing here needs to know where the overlay wants to sit.
pub fn overlay(scene: &mut impl PaintScene, doc: &mut HtmlDocument, frame: &Frame) {
    doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
    let mut scene = Compose { inner: scene, shift: Affine::IDENTITY };
    blitz_paint::paint_scene(&mut scene, doc, frame.scale, frame.width, frame.height);
}

/// A `PaintScene` that composes instead of replacing: it swallows `reset` and
/// pre-multiplies `shift` into every transform it forwards.
///
/// Swallowing the reset is what lets a second `paint_scene` land on top of the
/// first while `BlitzDomPainter`'s fields stay `pub(crate)`. The shift is what
/// puts it somewhere else — the second column, or nothing at all for an overlay.
struct Compose<'a, S: PaintScene> {
    inner: &'a mut S,
    shift: Affine,
}

impl<S: PaintScene> PaintScene for Compose<'_, S> {
    /// Half the point.
    fn reset(&mut self) {}

    fn push_layer(
        &mut self,
        blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        clip: &impl Shape,
    ) {
        self.inner.push_layer(blend, alpha, self.shift * transform, clip);
    }

    fn pop_layer(&mut self) {
        self.inner.pop_layer();
    }

    fn stroke<'a>(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        self.inner.stroke(style, self.shift * transform, brush, brush_transform, shape);
    }

    fn fill<'a>(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        self.inner.fill(style, self.shift * transform, brush, brush_transform, shape);
    }

    fn draw_glyphs<'a, 's: 'a>(
        &'s mut self,
        font: &'a FontData,
        font_size: f32,
        hint: bool,
        normalized_coords: &'a [NormalizedCoord],
        style: impl Into<StyleRef<'a>>,
        brush: impl Into<PaintRef<'a>>,
        brush_alpha: f32,
        transform: Affine,
        glyph_transform: Option<Affine>,
        glyphs: impl Iterator<Item = Glyph>,
    ) {
        let transform = self.shift * transform;
        self.inner.draw_glyphs(
            font,
            font_size,
            hint,
            normalized_coords,
            style,
            brush,
            brush_alpha,
            transform,
            glyph_transform,
            glyphs,
        );
    }

    fn draw_box_shadow(
        &mut self,
        transform: Affine,
        rect: Rect,
        brush: Color,
        radius: f64,
        std_dev: f64,
    ) {
        self.inner.draw_box_shadow(self.shift * transform, rect, brush, radius, std_dev);
    }
}
