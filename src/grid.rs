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
pub const COVER_W: u32 = 225;
pub const COVER_H: u32 = 348;
/// Every card is this tall, whatever its title says: the cover, the started
/// dot, two lines of title and the author line. Titles run to five lines in a
/// real library, and one long one made its whole row tall enough to push the
/// next row off the page — 332px of a 1992px page left blank. Anything past two
/// lines is clipped; the cover is the thing being browsed.
pub const CARD_H: u32 = COVER_H + 12 + 53 + 3 + 23;
/// Between cards, and at the sides of the page.
pub const GAP: u32 = 30;
pub const SIDE_PAD: u32 = 28;
/// A row holds no more than this however wide the window gets: past seven, a
/// shelf of covers turns into a contact sheet.
pub const MAX_PER_ROW: usize = 7;

/// Width the grid may occupy: `MAX_PER_ROW` cards and the gaps between them.
const fn grid_width() -> u32 {
    MAX_PER_ROW as u32 * COVER_W + (MAX_PER_ROW as u32 - 1) * GAP
}

/// How many cards fit a window this wide, capped at `MAX_PER_ROW`.
///
/// The grid's own `max-width` is what actually enforces the cap; this is the
/// same arithmetic for arrow-key navigation. They have to agree, or the
/// selection steps onto a column that is not there — which is why the numbers
/// live here and not in two places.
pub fn per_row(css_width: f32) -> usize {
    let usable = css_width - 2.0 * SIDE_PAD as f32;
    let pitch = (COVER_W + GAP) as f32;
    (((usable + GAP as f32) / pitch).floor() as usize).clamp(1, MAX_PER_ROW)
}

pub fn html(
    rows: &[BookRow],
    query: &str,
    sort: Sort,
    suggestions: &[(String, String)],
    // `Some(partial tag)` while a tag is being typed for the selected book.
    tagging: Option<&str>,
    // Cards to give a real cover to. Everything else gets its title as a
    // jacket: loading a cover nobody can see cost 1.7s of every rebuild.
    covers: std::ops::Range<usize>,
) -> String {
    let cards: String =
        rows.iter().enumerate().map(|(i, b)| card(i, b, covers.contains(&i))).collect();

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
        r#"<!DOCTYPE html>
<html><body>
<div class="bar">
  <div class="brand">
    <span class="title">Library ({heading})</span>
    <span class="meta"> · sorted by {sort}</span>
  </div>
  {search}
</div>
{body}
</body></html>"#,
        sort = sort.label(),
        search = search_form(query, suggestions, tagging),
    )
}

/// The search field, and what typing so far could become.
///
/// Not an `<input>`: keystrokes already arrive through the window, and the field
/// only has to *show* the query. A real input would mean a focus and editing
/// model for one box.
fn search_form(
    query: &str,
    suggestions: &[(String, String)],
    tagging: Option<&str>,
) -> String {
    let field = match (tagging, query.is_empty()) {
        // Tagging borrows the same box: one place where typing shows up.
        (Some(tag), _) => format!(
            r#"<span class="hint">tag </span><span class="typed">#{}</span>"#,
            escape(tag)
        ),
        (None, true) => r#"<span class="hint">Type to search</span>"#.to_string(),
        (None, false) => format!(r#"<span class="typed">{}</span>"#, escape(query)),
    };

    // Suggestions only exist while searching, so the grid shifts down once on
    // the first keystroke rather than twitching on every one.
    let list: String = suggestions
        .iter()
        .map(|(text, hint)| {
            format!(
                r#"<div class="sug" data-suggest="{full}"><span class="sugtext">{t}</span><span class="sughint">{h}</span></div>"#,
                full = escape(text),
                // One line each: a full title wrapped to three and turned the
                // menu into a wall. Clicking still searches the whole thing.
                t = escape(&clamp(text, 30)),
                h = escape(&clamp(hint, 20)),
            )
        })
        .collect();
    let list = match list.is_empty() {
        true => String::new(),
        false => format!(r#"<div class="suggest">{list}</div>"#),
    };

    format!(
        r#"<div class="search">
  <div class="field">{field}<span class="caret">|</span></div>
  {list}
</div>"#
    )
}

/// A card has room for two lines of title and one of author; anything longer
/// used to make its whole row taller and push the next row off the page.
///
/// ~17 characters of a long word fit a 225px line at 21px, so two lines is a
/// 34-character budget, cut back to a word boundary.
// ponytail: a character count, not a text measurement — a line of narrow glyphs
// can still wrap to a third line, which lands in the 30px gap between rows
// rather than on the next cover. Measure with parley if that ever collides.
fn clamp(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let head = cut.rsplit_once(' ').map_or(cut.as_str(), |(h, _)| h);
    format!("{head}…")
}

/// No selection state: the selected card is outlined by the window, so that
/// moving the selection does not rebuild the document and re-decode every cover.
fn card(index: usize, b: &BookRow, with_cover: bool) -> String {
    // Always an `<img>`, cover or not. The paginator makes an image an
    // unbreakable block and a text jacket a run of lines, so a card's box used
    // to depend on whether it had a cover — which made "which cards are on this
    // page" depend on which cards were given covers, which is circular, and the
    // grid rebuilt itself forever. The title is already under the card anyway.
    let cover = match b.has_cover && with_cover {
        true => format!(
            r#"<img class="cover" src="{COVER_ORIGIN}{hash}"/>"#,
            hash = escape(&b.hash)
        ),
        false => r#"<img class="cover blank"/>"#.to_string(),
    };

    let mut classes = String::from("card");
    if b.missing {
        classes.push_str(" missing");
    }

    format!(
        r#"<div class="{classes}" data-atom data-index="{index}" data-hash="{hash}">
  {cover}
  <div class="dot">{dot}</div>
  <div class="name">{title}</div>
  <div class="by">{author}</div>
  {tags}
</div>"#,
        tags = match b.tags.is_empty() {
            true => String::new(),
            false => format!(
                r#"<div class="tags">{}</div>"#,
                escape(&b.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "))
            ),
        },
        hash = escape(&b.hash),
        title = escape(&clamp(&b.title, 34)),
        author = escape(&clamp(if b.missing { "file missing" } else { &b.author }, 24)),
        dot = if b.started { "•" } else { "" },
    )
}

/// The library's own stylesheet, supplied as a UA sheet.
///
/// Every selector is prefixed with `html` for the same reason the reading
/// stylesheet is: blitz-dom applies its defaults *after* ours (CONTEXT.md §9).
pub fn stylesheet(bg: &str, fg: &str, subtle: &str, panel: &str) -> String {
    let grid_w = grid_width();
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
html body {{ margin: 0; padding: 0 {SIDE_PAD}px 32px {SIDE_PAD}px; }}

/* Capped and centred like the grid: on a 4K screen a full-width bar put the
   heading at the far left and the search box a metre away at the right, with
   the shelf of covers stranded in the middle. */
html .bar {{
  max-width: {grid_w}px;
  margin: 0 auto;
  padding: 22px 0 18px 0;
  display: flex;
  align-items: flex-start;
}}
html .brand {{ flex-grow: 1; }}
html .title {{ font-size: 34px; font-weight: 600; }}
html .meta {{ font-size: 14px; color: {subtle}; padding-left: 14px; }}

/* The search column sits at the right of the bar. */
html .search {{ width: 420px; padding-left: 28px; }}
html .field {{
  background: {panel};
  border-radius: 8px;
  padding: 9px 13px;
  font-size: 14px;
  overflow: hidden;
}}
html .hint {{ color: {subtle}; }}
html .caret {{ color: #0a84ff; padding-left: 1px; }}

/* Its own ground: the list used to float on the page background, which put
   plain text under the search box with nothing to say it was a menu. */
html .suggest {{
  background: {panel};
  border-radius: 8px;
  padding: 7px 0;
  margin-top: 7px;
}}
html .sug {{
  display: flex;
  align-items: baseline;
  font-size: 13px;
  padding: 5px 13px;
  border-radius: 6px;
}}
html .sugtext {{ flex-grow: 1; overflow: hidden; }}
html .sughint {{ color: {subtle}; font-size: 12px; padding-left: 12px; }}

/* Capped and centred, so a wide screen gets a shelf rather than a wall. */
html .grid {{
  display: flex;
  flex-wrap: wrap;
  gap: {GAP}px;
  max-width: {grid_w}px;
  margin: 0 auto;
}}

/* No padding: it existed to inset the old `.selected` background, and the
   selection is painted now. Without it the card's width is exactly the pitch
   `per_row` counts in. */
html .card {{
  width: {COVER_W}px;
  /* Same box for every card, and the paginator is told not to cut through one
     (`data-atom`), so a page holds whole rows of whole cards. */
  height: {CARD_H}px;
  overflow: hidden;
  /* Flex items shrink by default, and shrunken cards make the CSS disagree with
     `per_row`: seven were fitting a window the arithmetic said held five. */
  flex-shrink: 0;
  border-radius: 10px;
}}
html .card.missing {{ opacity: 0.45; }}

html .cover {{
  display: block;
  width: 100%;
  height: {COVER_H}px;
  border-radius: 5px;
}}
html .cover.blank {{ background: {panel}; }}

html .dot {{ height: 12px; font-size: 15px; color: #0a84ff; }}
html .name {{ font-size: 21px; line-height: 1.25; }}
html .by {{ font-size: 18px; color: {subtle}; padding-top: 3px; }}
html .tags {{ font-size: 11px; color: {subtle}; padding-top: 3px; }}

html .empty {{ padding: 40px 4px; color: {subtle}; font-size: 15px; }}
"#
    )
}

pub fn escape(s: &str) -> String {
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
        let out = html(&[nasty], "", Sort::Recent, &[], None, 0..1);
        assert!(!out.contains("<script"), "script tag survived: {out}");
        assert!(!out.contains("<img src=x"), "raw img survived");
        assert!(out.contains("&lt;script&gt;"));
        // The data-hash attribute must stay parseable.
        assert!(out.contains(r#"data-hash="abc123""#));
    }

    #[test]
    fn query_is_escaped_in_the_heading() {
        let out = html(&[], "<b>", Sort::Title, &[], None, 0..0);
        assert!(!out.contains("<b>"));
        assert!(out.contains("&lt;b&gt;"));
    }

    /// `data-index` has to survive: it is how the window finds the card to
    /// outline now that selection is painted rather than marked up.
    #[test]
    fn missing_state_and_indices_reach_the_markup() {
        let mut a = row("A", "x");
        let mut b = row("B", "y");
        b.hash = "def".into();
        b.missing = true;
        a.started = true;
        let out = html(&[a, b], "", Sort::Recent, &[], None, 0..2);
        assert!(out.contains(r#"class="card""#) || out.contains("card "), "{out}");
        assert!(out.contains("missing"));
        assert!(out.contains("data-index=\"1\""));
        assert!(!out.contains("selected"), "selection is painted, not marked up");
    }

    /// The grid must actually lay out — a failure here means a blank window,
    /// because a document that does not build never paints a first frame.
    #[test]
    fn the_grid_lays_out() {
        let rows: Vec<BookRow> = (0..40)
            .map(|i| {
                let mut r = row(&format!("Libro {i}"), "Autor");
                r.hash = format!("h{i}");
                r
            })
            .collect();
        let html = html(&rows, "", Sort::Recent, &[], None, 0..40);
        let ua = stylesheet("#fff", "#111", "#888", "#eee");
        let doc = crate::chapter::layout_document(
            html,
            ua,
            None,
            crate::chapter::viewport(1200, 900, 1.0, false),
            800.0,
        );
        let doc = doc.expect("grid document failed to lay out");
        assert!(doc.content_height() > 100.0, "grid has no height");
        assert!(doc.text_len() > 0, "grid has no text");
    }

    /// Suggestions are text from books and from the index; both are untrusted.
    #[test]
    fn the_search_form_shows_the_query_and_escapes_suggestions() {
        let sugs = vec![
            ("resonancia".to_string(), "12 chapters".to_string()),
            ("<img src=x>".to_string(), "author".to_string()),
        ];
        let out = html(&[], "reso", Sort::Recent, &sugs, None, 0..0);
        assert!(out.contains(r#"<span class="typed">reso</span>"#), "{out}");
        assert!(out.contains(r#"data-suggest="resonancia""#));
        assert!(out.contains("12 chapters"));
        assert!(!out.contains("<img src=x"), "suggestion injected markup");
        assert!(out.contains("&lt;img src=x&gt;"));
    }

    /// Empty query: a placeholder, and no suggestion block to push the grid down.
    #[test]
    fn an_empty_query_shows_a_placeholder_and_no_suggestions() {
        let out = html(&[], "", Sort::Recent, &[], None, 0..0);
        assert!(out.contains("Type to search"));
        assert!(!out.contains("class=\"suggest\""));
        assert!(out.contains("class=\"caret\""));
    }

    /// Tags show on the card and in the box while being typed; both are user
    /// text and both go through the same escaping as everything else here.
    #[test]
    fn tags_show_on_the_card_and_in_the_box() {
        let mut r = row("Un libro", "Autor");
        r.tags = vec!["scifi".into(), "<img src=x>".into()];
        let out = html(&[r], "", Sort::Recent, &[], Some("sci<b>"), 0..1);

        assert!(out.contains("#scifi"), "{out}");
        assert!(!out.contains("<img src=x"), "a tag injected markup");
        // The box shows what is being typed, not the search placeholder.
        assert!(out.contains(r#"<span class="typed">#sci&lt;b&gt;</span>"#), "{out}");
        assert!(!out.contains("Type to search"));
    }

    /// Only the cards on the page get a real cover; the rest fall back to the
    /// title jacket. Loading all 358 covers cost 1.7s of every rebuild.
    #[test]
    fn only_the_named_cards_carry_covers() {
        let rows: Vec<BookRow> = (0..6)
            .map(|i| {
                let mut r = row(&format!("Libro {i}"), "Autor");
                r.hash = format!("h{i}");
                r.has_cover = true;
                r
            })
            .collect();

        let out = html(&rows, "", Sort::Recent, &[], None, 2..4);
        assert_eq!(out.matches("<img class=\"cover\"").count(), 2, "only two covers");
        assert!(out.contains(&format!("{COVER_ORIGIN}h2")));
        assert!(out.contains(&format!("{COVER_ORIGIN}h3")));
        assert!(!out.contains(&format!("{COVER_ORIGIN}h0")), "off-page cover requested");
        // The others are still cards, just wearing their title.
        assert_eq!(out.matches("cover blank").count(), 4);
        assert_eq!(out.matches("data-index=").count(), 6, "every book still listed");
    }

    /// A five-line title used to make its whole row tall enough to push the next
    /// row off the page — 332px of a 1992px page left blank — and a break could
    /// land between a cover and its own title. Cards are one fixed box now, and
    /// the paginator is told not to cut through one.
    #[test]
    fn pages_hold_whole_rows_of_equal_cards() {
        let rows: Vec<BookRow> = (0..60)
            .map(|i| {
                let long = "Un titulo interminable que ocupaba cinco lineas                             enteras y empujaba la fila siguiente fuera de la pagina";
                let mut r = row(if i % 3 == 0 { "Corto" } else { long }, "Autor");
                r.hash = format!("h{i}");
                r
            })
            .collect();
        let doc = crate::chapter::layout_document(
            html(&rows, "", Sort::Recent, &[], None, 0..rows.len()),
            stylesheet("#fff", "#111", "#888", "#eee"),
            None,
            crate::chapter::viewport(1896, 2092, 1.0, false),
            1992.0,
        )
        .expect("the grid must lay out");

        let mut tops: Vec<f32> = crate::chapter::indexed_tops(doc.dom())
            .into_iter()
            .map(|(_, y)| y)
            .collect();
        tops.sort_by(f32::total_cmp);
        tops.dedup_by(|a, b| (*a - *b).abs() < 1.0);

        // Every row is the same height, whatever its titles say.
        for pair in tops.windows(2) {
            assert!(
                (pair[1] - pair[0] - (CARD_H + GAP) as f32).abs() < 1.0,
                "rows are not evenly pitched: {tops:?}"
            );
        }

        // And no page starts in the middle of one.
        for &t in &doc.pages.tops {
            let split = tops.iter().find(|&&y| t > y + 1.0 && t < y + CARD_H as f32 - 1.0);
            assert!(split.is_none(), "page at {t} cuts the card at {split:?}");
        }
        assert!(doc.pages.count() > 1, "the fixture must span several pages");
    }

    /// The CSS cap and the arithmetic arrow keys use must not drift apart.
    #[test]
    fn a_row_holds_at_most_seven_however_wide_the_window() {
        // Exactly enough for the cap, and far more than enough.
        let need = (grid_width() + 2 * SIDE_PAD) as f32;
        assert_eq!(per_row(need), MAX_PER_ROW);
        assert_eq!(per_row(3840.0), MAX_PER_ROW, "a 4K window still gets seven");
        assert_eq!(per_row(need - 1.0), MAX_PER_ROW - 1, "one pixel short drops a column");

        // Narrow windows count honestly, and never reach zero.
        let pitch = (COVER_W + GAP) as f32;
        assert_eq!(per_row(2.0 * SIDE_PAD as f32 + COVER_W as f32), 1);
        assert_eq!(per_row(2.0 * SIDE_PAD as f32 + pitch + COVER_W as f32), 2);
        assert_eq!(per_row(10.0), 1, "never zero columns");

        // The stylesheet has to carry the same cap the arithmetic assumes.
        let css = stylesheet("#fff", "#111", "#888", "#eee");
        assert!(css.contains(&format!("max-width: {}px", grid_width())), "{css}");
    }

    /// Every card is the same box whether or not it shows a cover: an image is
    /// an unbreakable block to the paginator and text is not, so a card whose
    /// height depended on having a cover made the visible set depend on the
    /// cover set — and the grid rebuilt itself in a loop.
    #[test]
    fn a_card_is_the_same_box_with_or_without_a_cover() {
        let mut with = row("Con portada", "A");
        with.has_cover = true;
        let without = row("Sin portada", "N");

        let a = html(&[with], "", Sort::Recent, &[], None, 0..1);
        let b = html(&[without], "", Sort::Recent, &[], None, 0..1);
        assert!(a.contains(r#"<img class="cover" src="#), "{a}");
        assert!(b.contains(r#"<img class="cover blank"/>"#), "{b}");
        // Same element either way, so the same box and the same atom.
        assert_eq!(a.matches("<img class=\"cover").count(), 1);
        assert_eq!(b.matches("<img class=\"cover").count(), 1);
        // The title still names the book, under the card.
        assert!(b.contains("Sin portada"));
    }
}

