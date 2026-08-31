//! The reading HUD: the book's title and how far through it you are.
//!
//! Chrome that appears on a mouse move and goes away again on its own, so the
//! page is uninterrupted while you read (CONTEXT.md §3 calls this a gesture
//! rather than a setting). Like every other surface here it is HTML/CSS through
//! the blitz pipeline; it reaches the screen over the page rather than instead
//! of it via `paint::overlay`.

use crate::grid::escape;

/// Height of the bar and its distance from the foot of the window, in CSS px.
/// The document is laid out at window size and pushed down by a margin, so
/// these two are what decide where it lands.
const BAR: f32 = 34.0;
const INSET: f32 = 16.0;

/// `window_height` is in CSS pixels.
pub fn html(title: &str, percent: u8, window_height: f32) -> String {
    let push = (window_height - BAR - INSET).max(0.0);
    format!(
        r#"<html><body>
<div class="hud" style="margin-top: {push}px">
  <span class="title">{title}</span>
  <span class="pct">{percent}%</span>
</div>
</body></html>"#,
        title = escape(title),
    )
}

/// `html`-prefixed for the same reason every sheet here is: blitz-dom applies
/// its own defaults *after* ours (CONTEXT.md §9).
///
/// The page shows through, so `html` and `body` must stay transparent — only
/// the bar itself paints a ground.
pub fn stylesheet(fg: &str, subtle: &str, panel: &str) -> String {
    format!(
        r#"
html {{
  box-sizing: border-box;
  font-family: "Literata", "Charis SIL", serif;
  background: transparent;
  color: {fg};
}}
html *, html *::before, html *::after {{ box-sizing: inherit; }}
html body {{ margin: 0; padding: 0 24px; background: transparent; }}

html .hud {{
  display: flex;
  align-items: baseline;
  height: {BAR}px;
  padding: 7px 16px;
  border-radius: 9px;
  background: {panel};
  font-size: 13px;
}}
html .title {{
  flex-grow: 1;
  overflow: hidden;
  white-space: nowrap;
}}
html .pct {{ color: {subtle}; padding-left: 16px; }}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar is placed by a computed margin, so the arithmetic is the layout.
    #[test]
    fn the_bar_sits_at_the_foot_of_the_window() {
        let out = html("A Book", 42, 1000.0);
        assert!(out.contains(&format!("margin-top: {}px", 1000.0 - BAR - INSET)), "{out}");
        assert!(out.contains("42%"));
    }

    /// A window shorter than the bar must not produce a negative margin.
    #[test]
    fn a_tiny_window_does_not_push_the_bar_off_the_top() {
        let out = html("A Book", 0, 10.0);
        assert!(out.contains("margin-top: 0px"), "{out}");
    }

    /// Titles come from files off the internet, exactly as in the grid.
    #[test]
    fn the_title_cannot_inject_markup() {
        let out = html(r#"<script>x</script>"#, 7, 800.0);
        assert!(!out.contains("<script"), "{out}");
        assert!(out.contains("&lt;script&gt;"));
    }

    /// The page has to show through the parts of the overlay that are not bar.
    #[test]
    fn the_overlay_ground_is_transparent() {
        let css = stylesheet("#111", "#888", "#eee");
        assert!(css.contains("html {\n  box-sizing: border-box;"));
        assert_eq!(css.matches("background: transparent").count(), 2, "html and body");
    }
}
