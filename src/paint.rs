//! Painting a page, and compositing more than one document into a frame.
//!
//! `blitz_paint::paint_scene` calls `scene.reset()` on entry (CONTEXT.md §9), so
//! for a long time the page was the only thing that could be painted: a second
//! call erased the first. But that function is generic over `impl PaintScene`,
//! and the trait is ours to implement — so a wrapper whose `reset` does nothing
//! lets a second document be painted *over* the first. That is what [`NoReset`]
//! is, and it is the whole mechanism behind the reading HUD.

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

/// Paint a second document over whatever is already in the scene.
///
/// The document is laid out at window size and positions itself with margins,
/// so nothing here needs to know where the overlay wants to sit.
pub fn overlay(scene: &mut impl PaintScene, doc: &mut HtmlDocument, frame: &Frame) {
    doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
    let mut scene = NoReset(scene);
    blitz_paint::paint_scene(&mut scene, doc, frame.scale, frame.width, frame.height);
}

/// A `PaintScene` that forwards everything except `reset`.
///
/// Painting a document normally clears the frame first. Wrapping the scene in
/// this makes a second `paint_scene` compose instead of replace, which is the
/// only way to get two documents into one frame while `BlitzDomPainter`'s
/// fields stay `pub(crate)` and its `paint_scene` keeps resetting.
struct NoReset<'a, S: PaintScene>(&'a mut S);

impl<S: PaintScene> PaintScene for NoReset<'_, S> {
    /// The entire point.
    fn reset(&mut self) {}

    fn push_layer(
        &mut self,
        blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        clip: &impl Shape,
    ) {
        self.0.push_layer(blend, alpha, transform, clip);
    }

    fn pop_layer(&mut self) {
        self.0.pop_layer();
    }

    fn stroke<'a>(
        &mut self,
        style: &Stroke,
        transform: Affine,
        brush: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        self.0.stroke(style, transform, brush, brush_transform, shape);
    }

    fn fill<'a>(
        &mut self,
        style: Fill,
        transform: Affine,
        brush: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        self.0.fill(style, transform, brush, brush_transform, shape);
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
        self.0.draw_glyphs(
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
        self.0.draw_box_shadow(transform, rect, brush, radius, std_dev);
    }
}
