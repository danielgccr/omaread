//! The library view, authored in HTML/CSS.
//!
//! Omaread has a CSS engine, so the grid is a document rendered through the same
//! blitz-dom pipeline as a book (CONTEXT.md §2). One layout engine, one paint
//! path, one styling language.

use crate::db::{BookRow, Sort};

/// Cover images resolve through this origin; the provider reads the blob out of
/// SQLite rather than touching the disk.
pub const COVER_ORIGIN: &str = "omaread-cover://cover/";

/// Card geometry, in px. ~1:1.55, the usual book proportion.
const COVER_W: u32 = 150;
const COVER_H: u32 = 232;

pub fn html(rows: &[BookRow], query: &str, sort: Sort, selected: usize) -> String {
    let cards: String = rows
        .iter()
        .enumerate()
        .map(|(i, b)| card(i, b, i == selected))
        .collect();

    let count = rows.len();
    let heading = if query.trim().is_empty() {
        format!("{count} book{}", if count == 1 { "" } else { "s" })
    } else {
        format!("{count} matching “{}”", escape(query))
    };

    let body = if rows.is_empty() {
        r#"<div class="empty">Nothing here yet. Put some .epub files in a watched
           folder and press r to rescan.</div>"#
            .to_string()
    } else {
        format!(r#"<div class="grid">{cards}</div>"#)
    };

    format!(
        r#"<html><body>
<div class="bar">
  <span class="title">Library</span>
  <span class="meta">{heading} · sorted by {sort}</span>
</div>
{body}
</body></html>"#,
        sort = sort.label()
    )
}

fn card(index: usize, b: &BookRow, selected: bool) -> String {
    let cover = match b.cover.is_some() {
        true => format!(
            r#"<img class="cover" src="{COVER_ORIGIN}{hash}"/>"#,
            hash = escape(&b.hash)
        ),
        // No cover in the file: set the title as the jacket rather than a blank.
        false => format!(r#"<div class="cover blank">{}</div>"#, escape(&b.title)),
    };

    let mut classes = String::from("card");
    if selected {
        classes.push_str(" selected");
    }
    if b.missing {
        classes.push_str(" missing");
    }

    format!(
        r#"<div class="{classes}" data-index="{index}" data-hash="{hash}">
  {cover}
  <div class="dot">{dot}</div>
  <div class="name">{title}</div>
  <div class="by">{author}</div>
</div>"#,
        hash = escape(&b.hash),
        title = escape(&b.title),
        author = escape(if b.missing { "file missing" } else { &b.author }),
        dot = if b.started { "•" } else { "" },
    )
}

/// The library's own stylesheet, supplied as a UA sheet.
///
/// Every selector is prefixed with `html` for the same reason the reading
/// stylesheet is: blitz-dom applies its defaults *after* ours (CONTEXT.md §9).
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

html .grid {{ display: flex; flex-wrap: wrap; gap: 30px; }}

html .card {{
  width: {COVER_W}px;
  padding: 6px;
  border-radius: 10px;
}}
html .card.selected {{ background: {panel}; }}
html .card.missing {{ opacity: 0.45; }}

html .cover {{
  display: block;
  width: {COVER_W}px;
  height: {COVER_H}px;
  border-radius: 5px;
}}
html .cover.blank {{
  background: {panel};
  font-size: 13px;
  padding: 12px;
  overflow: hidden;
}}

html .dot {{ height: 12px; font-size: 15px; color: #0a84ff; }}
html .name {{ font-size: 14px; line-height: 1.25; }}
html .by {{ font-size: 12px; color: {subtle}; padding-top: 2px; }}

html .empty {{ padding: 40px 4px; color: {subtle}; font-size: 15px; }}
"#
    )
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, author: &str) -> BookRow {
        BookRow {
            hash: "abc123".into(),
            path: "/x.epub".into(),
            title: title.into(),
            author: author.into(),
            ..Default::default()
        }
    }

    /// Book metadata is untrusted input from files off the internet; it must not
    /// be able to inject markup into the library view.
    #[test]
    fn metadata_cannot_inject_markup() {
        let nasty = row(
            r#"<script>x</script>" onload="y"#,
            "<img src=x onerror=1>",
        );
        let out = html(&[nasty], "", Sort::Recent, 0);
        assert!(!out.contains("<script"), "script tag survived: {out}");
        assert!(!out.contains("<img src=x"), "raw img survived");
        assert!(out.contains("&lt;script&gt;"));
        // The data-hash attribute must stay parseable.
        assert!(out.contains(r#"data-hash="abc123""#));
    }

    #[test]
    fn query_is_escaped_in_the_heading() {
        let out = html(&[], "<b>", Sort::Title, 0);
        assert!(!out.contains("<b>"));
        assert!(out.contains("&lt;b&gt;"));
    }

    #[test]
    fn selection_and_missing_state_reach_the_markup() {
        let mut a = row("A", "x");
        let mut b = row("B", "y");
        b.hash = "def".into();
        b.missing = true;
        a.started = true;
        let out = html(&[a, b], "", Sort::Recent, 1);
        assert!(out.contains(r#"class="card""#) || out.contains("card "), "{out}");
        assert!(out.contains("selected"));
        assert!(out.contains("missing"));
        assert!(out.contains("data-index=\"1\""));
    }

    #[test]
    fn a_book_without_a_cover_still_gets_a_jacket() {
        let out = html(&[row("Sin portada", "N")], "", Sort::Recent, 0);
        assert!(out.contains("cover blank"));
        assert!(out.contains("Sin portada"));
    }
}
