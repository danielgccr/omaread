//! The table of contents, authored in HTML/CSS.
//!
//! A full view rather than a panel floating over the page. Painting a panel
//! would mean two `paint_scene` calls into one scene and the second resets the
//! first (CONTEXT.md §9) — the same upstream blocker that holds up two-column.
//! Nothing about the model changes when that lifts: this document is already a
//! standalone flow, so it can be composited into a side panel then.

use crate::book::TocEntry;
use crate::grid::escape;

/// Deeper nesting than this reads as noise, so everything below sits at the
/// same indent. Indent widths live in the stylesheet, as `.d0`..`.d3` — a class
/// goes through the same cascade the grid already relies on, where an inline
/// `style` attribute would be one more thing to have to trust.
const MAX_DEPTH: usize = 3;

pub fn html(
    entries: &[TocEntry],
    heading: &str,
    subtitle: &str,
    current: usize,
    selected: usize,
) -> String {
    let rows: String = entries
        .iter()
        .enumerate()
        .map(|(i, e)| row(i, e, e.spine == current, i == selected))
        .collect();

    let body = match entries.is_empty() {
        true => r#"<div class="empty">Nothing found.</div>"#.to_string(),
        false => format!(r#"<div class="list">{rows}</div>"#),
    };

    format!(
        r#"<html><body>
<div class="bar">
  <span class="title">{heading}</span>
  <span class="meta">{subtitle}</span>
</div>
{body}
</body></html>"#,
        heading = escape(heading),
        subtitle = escape(subtitle),
    )
}

fn row(index: usize, e: &TocEntry, here: bool, selected: bool) -> String {
    let mut classes = format!("row d{}", e.depth.min(MAX_DEPTH));
    if here {
        classes.push_str(" here");
    }
    if selected {
        classes.push_str(" selected");
    }
    format!(
        r#"<div class="{classes}" data-index="{index}">{label}</div>"#,
        label = escape(&e.label),
    )
}

/// The contents' own stylesheet, supplied as a UA sheet.
///
/// `html`-prefixed for the same reason every other sheet here is: blitz-dom
/// applies its defaults *after* ours (CONTEXT.md §9).
pub fn stylesheet(bg: &str, fg: &str, subtle: &str, panel: &str) -> String {
    format!(
        r#"
html {{
  box-sizing: border-box;
  font-family: "Literata", "Charis SIL", serif;
  font-size: 15px;
  background: {bg};
  color: {fg};
}}
html *, html *::before, html *::after {{ box-sizing: inherit; }}
html body {{ margin: 0; padding: 0 28px 32px 28px; }}

html .bar {{
  padding: 22px 4px 18px 4px;
  display: flex;
  align-items: baseline;
}}
html .title {{ font-size: 26px; font-weight: 600; }}
html .meta {{ font-size: 13px; color: {subtle}; padding-left: 14px; }}

/* A contents list is a column of short lines; the reading measure does not
   apply, but an unbounded one on a wide window is a scan across the desk. */
html .list {{ max-width: 640px; }}

html .row {{
  font-size: 16px;
  line-height: 1.35;
  padding-top: 7px;
  padding-bottom: 7px;
  padding-right: 12px;
  border-radius: 7px;
}}
html .row.d0 {{ padding-left: 14px; }}
html .row.d1 {{ padding-left: 36px; }}
html .row.d2 {{ padding-left: 58px; }}
html .row.d3 {{ padding-left: 80px; }}
html .empty {{ padding: 40px 14px; color: {subtle}; font-size: 15px; }}
html .row.here {{ font-weight: 600; }}
html .row.selected {{ background: {panel}; }}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, depth: usize, spine: usize) -> TocEntry {
        TocEntry { label: label.into(), depth, spine, fragment: None, find: None }
    }

    /// Labels come out of files off the internet, exactly like book metadata in
    /// the grid; they must not be able to inject markup into the view.
    #[test]
    fn labels_cannot_inject_markup() {
        let e = [entry(r#"<script>x</script>" onclick="y"#, 0, 0)];
        let out = html(&e, "Contents", "<b>title</b>", 0, 0);
        assert!(!out.contains("<script"), "script tag survived: {out}");
        assert!(!out.contains("<b>"), "raw markup in the title survived");
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains(r#"data-index="0""#));
    }

    /// Depth reaches the markup as a class the stylesheet indents, and stops
    /// deepening past MAX_DEPTH rather than marching off the left margin.
    #[test]
    fn nesting_indents_and_stops_indenting() {
        let e = [entry("Part", 0, 0), entry("Chapter", 1, 1), entry("Deep", 9, 2)];
        // No chapter in common, so nothing picks up `here` and the depth
        // classes are all this asserts on.
        let out = html(&e, "Contents", "t", 99, 99);
        let css = stylesheet("#fff", "#111", "#888", "#eee");
        assert!(out.contains(r#"class="row d0""#), "{out}");
        assert!(out.contains(r#"class="row d1""#));
        assert!(out.contains(&format!(r#"class="row d{MAX_DEPTH}""#)));
        for d in 0..=MAX_DEPTH {
            assert!(css.contains(&format!("html .row.d{d} {{")), "no indent rule for d{d}");
        }
    }

    #[test]
    fn the_current_chapter_and_the_selection_are_marked() {
        let e = [entry("One", 0, 0), entry("Two", 0, 4), entry("Three", 0, 9)];
        let out = html(&e, "Contents", "t", 4, 2);
        assert!(out.contains(r#"class="row d0 here" data-index="1""#), "{out}");
        assert!(out.contains(r#"class="row d0 selected" data-index="2""#), "{out}");
    }

    /// A document that does not build never paints a first frame, so the
    /// contents key would open a blank window.
    #[test]
    fn the_contents_lay_out() {
        let e: Vec<TocEntry> = (0..60)
            .map(|i| entry(&format!("Capítulo {i}"), i % 3, i))
            .collect();
        let ua = stylesheet("#fff", "#111", "#888", "#eee");
        let doc = crate::chapter::layout_document(
            html(&e, "Contents", "Un libro", 7, 7),
            ua,
            None,
            crate::chapter::viewport(1200, 900, 1.0, false),
            800.0,
        )
        .expect("contents document failed to lay out");
        assert!(doc.content_height() > 100.0, "contents have no height");
        assert!(doc.pages.count() > 1, "60 entries should not fit one page");
    }
}
