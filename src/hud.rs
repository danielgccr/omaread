//! The reading HUD: the book's title and how far through it you are.
//!
//! Chrome that appears on a mouse move and goes away again on its own, so the
//! page is uninterrupted while you read (CONTEXT.md §3 calls this a gesture
//! rather than a setting). Like every other surface here it is HTML/CSS through
//! the blitz pipeline; it reaches the screen over the page rather than instead
//! of it via `paint::overlay`.

use crate::grid::escape;

/// Height of a bar and the window inset, in CSS px. Both bars are in normal
/// flow, so the bottom one is placed by a computed margin.
const BAR: f32 = 34.0;
const INSET: f32 = 16.0;

/// `readout` is whatever the foot of the page should say — a percentage or a
/// page number; clicking it swaps which. `window_height` is in CSS pixels.
pub fn html(
    title: &str,
    readout: &str,
    columns: usize,
    on_highlight: bool,
    window_height: f32,
) -> String {
    // Top bar occupies [INSET, INSET + BAR]; this drops the bottom one to
    // [H - INSET - BAR, H - INSET].
    let push = (window_height - 2.0 * INSET - 2.0 * BAR).max(0.0);

    // Once the pointer is inside a highlight, the useful offer is removing it.
    let (mark_action, mark_label) = match on_highlight {
        true => ("unhighlight", "Remove"),
        false => ("highlight", "Highlight"),
    };

    format!(
        r#"<!DOCTYPE html>
<html><body>
<div class="hud">
  <span class="btn" data-hud="bookmark"><span class="ico" data-icon="bookmark"></span><span class="lbl">Bookmark</span></span>
  <span class="btn" data-hud="contents"><span class="ico" data-icon="contents"></span><span class="lbl">Contents</span></span>
  <span class="btn" data-hud="{mark_action}"><span class="ico" data-icon="highlight"></span><span class="lbl">{mark_label}</span></span>
  <span class="grow"></span>
  <span class="btn" data-hud="smaller">A−</span>
  <span class="btn" data-hud="bigger">A+</span>
  <span class="btn" data-hud="columns">{columns} col</span>
</div>
<div class="hud" style="margin-top: {push}px">
  <span class="btn" data-hud="library"><span class="ico" data-icon="back"></span><span class="lbl">Library</span></span>
  <span class="title">{title}</span>
  <span class="btn" data-hud="readout">{readout}</span>
</div>
</body></html>"#,
        title = escape(title),
        readout = escape(readout),
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
html body {{ margin: 0; padding: {INSET}px 24px; background: transparent; }}

html .hud {{
  display: flex;
  /* Not `baseline`: a flex container takes its baseline from its first item,
     and for the buttons with icons that is an empty 11px box, so they sat a few
     pixels above the plain-text ones. Centring does not care. */
  align-items: center;
  height: {BAR}px;
  padding: 7px 16px;
  border-radius: 9px;
  background: {panel};
  font-size: 13px;
}}
/* The title is the book, so it carries the weight. */
html .title {{
  flex-grow: 1;
  overflow: hidden;
  white-space: nowrap;
  font-weight: 600;
  padding-left: 12px;
}}
html .grow {{ flex-grow: 1; }}

/* Every control is a click target; `data-hud` is what the window hit-tests. */
html .btn {{
  color: {subtle};
  padding: 3px 9px;
  margin-left: 4px;
  border-radius: 6px;
  white-space: nowrap;
}}

/* An empty box the window paints an icon into; it only has to hold the space.
   The button is a flex row so the slot gets a real layout box: an inline-block
   inside text becomes a parley inline box, whose `final_layout` size is 0, and
   the window would have nowhere to paint. */
html .btn {{ display: flex; align-items: center; }}
html .ico {{
  width: 11px;
  height: 11px;
  margin-right: 6px;
}}
html .lbl {{ white-space: nowrap; }}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bars are placed by computed margins, so the arithmetic is the layout.
    #[test]
    fn the_bars_sit_at_the_head_and_foot_of_the_window() {
        let out = html("A Book", "42%", 1, false, 1000.0);
        let push = 1000.0 - 2.0 * INSET - 2.0 * BAR;
        assert!(out.contains(&format!("margin-top: {push}px")), "{out}");
        assert!(out.contains("42%"));
    }

    /// A window shorter than the bars must not produce a negative margin.
    #[test]
    fn a_tiny_window_does_not_push_the_bar_off_the_top() {
        let out = html("A Book", "0%", 1, false, 10.0);
        assert!(out.contains("margin-top: 0px"), "{out}");
    }

    /// Titles come from files off the internet, exactly as in the grid, and the
    /// readout is ours but goes through the same escaping.
    #[test]
    fn the_title_cannot_inject_markup() {
        let out = html(r#"<script>x</script>"#, "<b>", 2, false, 800.0);
        assert!(!out.contains("<script"), "{out}");
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.contains("<b>"));
    }

    /// Every control the window has to hit-test must reach the markup, and the
    /// title has to be the bold one.
    #[test]
    fn the_controls_are_click_targets_and_the_title_is_bold() {
        let out = html("A Book", "page 8 of 19", 2, false, 900.0);
        for what in [
            "bookmark", "contents", "highlight", "smaller", "bigger", "columns", "readout",
            "library",
        ] {
            assert!(out.contains(&format!(r#"data-hud="{what}""#)), "missing {what}: {out}");
        }
        assert!(out.contains("2 col"), "the columns control shows the current state");
        assert!(out.contains("page 8 of 19"));

        let css = stylesheet("#111", "#888", "#eee");
        assert!(css.contains("html .title {") && css.contains("font-weight: 600;"));
    }

    /// Once the pointer is inside a highlight, the useful offer is removing it.
    #[test]
    fn the_highlight_control_becomes_a_remove_control() {
        let plain = html("A Book", "5%", 1, false, 900.0);
        assert!(plain.contains(r#"data-hud="highlight""#));
        assert!(!plain.contains("unhighlight"));

        let inside = html("A Book", "5%", 1, true, 900.0);
        assert!(inside.contains(r#"data-hud="unhighlight""#), "{inside}");
        assert!(inside.contains(">Remove<"));
        // The icon slot stays, whichever way the button reads.
        assert!(inside.contains(r#"data-icon="highlight""#));
    }

    /// Every icon slot the window paints into has to exist in the markup.
    #[test]
    fn the_icon_slots_are_there_for_the_window_to_paint_into() {
        let out = html("A Book", "5%", 2, false, 900.0);
        for icon in ["bookmark", "contents", "highlight", "back"] {
            assert!(out.contains(&format!(r#"data-icon="{icon}""#)), "missing {icon}: {out}");
        }
        let css = stylesheet("#111", "#888", "#eee");
        assert!(css.contains("html .ico {"), "the slot needs a size to reserve");
    }

    /// Without a doctype html5ever reports "Unexpected token" once per parse —
    /// straight to stdout, from inside blitz, with no way to switch it off. The
    /// HUD is rebuilt on every page turn, so that was a stream of it.
    #[test]
    fn the_document_declares_a_doctype() {
        assert!(html("A", "1%", 1, false, 900.0).starts_with("<!DOCTYPE html>"));
        assert!(crate::toc::html(&[], "Contents", "t", 0, 0).starts_with("<!DOCTYPE html>"));
        assert!(
            crate::grid::html(&[], "", crate::db::Sort::Recent, &[], None, 0..0)
                .starts_with("<!DOCTYPE html>")
        );
    }

    /// The page has to show through the parts of the overlay that are not bar.
    #[test]
    fn the_overlay_ground_is_transparent() {
        let css = stylesheet("#111", "#888", "#eee");
        assert_eq!(css.matches("background: transparent").count(), 2, "html and body");
    }
}
