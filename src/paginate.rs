//! The line-snapped paginator.
//!
//! Taffy has no fragmentation and CSS multicol is unavailable (CONTEXT.md §3), so
//! pages are ours to compute. The chapter is laid out **once**, continuously, at
//! the measure width; a page is a *view* over that flow — a Y range.
//!
//! A break may fall anywhere that does not cut through an **atom**: a line of
//! prose, a table row band, a code line, an image. That single rule is what makes
//! a 300-row table and a 200-line code block both split across pages instead of
//! overflowing.

/// What a break must never cut through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomKind {
    /// A line box from an inline formatting context.
    Line,
    /// A table row band, derived from cell geometry (`<tr>` carries no box).
    Row,
    /// An unbreakable block: an image, a rule, a replaced element.
    Block,
}

#[derive(Debug, Clone, Copy)]
pub struct Atom {
    pub top: f32,
    pub bottom: f32,
    /// Atoms sharing a group are lines of one paragraph, or rows of one table.
    /// Widow and orphan control operates within a group.
    pub group: usize,
    pub kind: AtomKind,
    /// A break may not fall between this atom and the next one of another group.
    /// Set on headings so they are never stranded at the foot of a page.
    pub keep_with_next: bool,
}

/// Tolerance for treating a break as being *at* an atom's edge rather than
/// inside it.
///
/// Layout geometry is floating point and adjacency is approximate: consecutive
/// table row bands were observed overlapping by a pixel
/// (`18806..19292` then `19291..19833`), which made the only legal break between
/// them look like it fell inside the first row. With no exit, the paginator hard
/// cut straight through the table. A sub-pixel break is invisible; a hard cut is
/// not.
pub const EDGE_EPS: f32 = 1.0;

impl Atom {
    /// Would a page break at `y` split this atom?
    pub fn splits(&self, y: f32) -> bool {
        y > self.top + EDGE_EPS && y < self.bottom - EDGE_EPS
    }
}

/// Minimum lines of a paragraph left at the foot of a page.
const ORPHANS: usize = 2;
/// Minimum lines of a paragraph carried to the head of a page.
const WIDOWS: usize = 2;

/// Page boundaries as offsets into the flow. Always starts with 0.0.
#[derive(Debug, Clone, PartialEq)]
pub struct Pages {
    pub tops: Vec<f32>,
    pub page_height: f32,
    pub content_height: f32,
}

impl Pages {
    pub fn count(&self) -> usize {
        self.tops.len()
    }

    pub fn top_of(&self, page: usize) -> f32 {
        self.tops.get(page).copied().unwrap_or(0.0)
    }

    /// How much flow this page actually shows: the distance to the next page's
    /// top, or to the end of the content on the last page.
    ///
    /// This is almost never the full `page_height`. A break snaps *up* to a line
    /// boundary, so a page typically ends short of its nominal height — by 28px
    /// on a real page measured at 900. Anything painted past this point belongs
    /// to the next page, which is why the margin mask is positioned from here
    /// and not from `page_height`: masking lower leaves a slice of the next
    /// page's first line on show, cut off halfway down.
    pub fn extent_of(&self, page: usize) -> f32 {
        let top = self.top_of(page);
        let next = match self.tops.get(page + 1) {
            Some(&next) => next,
            None => self.content_height,
        };
        (next - top).clamp(0.0, self.page_height)
    }

    /// Which page a flow offset falls on. Used to keep the reader's place across
    /// a re-flow, and later to resolve a CFI to a page.
    pub fn page_containing(&self, y: f32) -> usize {
        match self.tops.iter().position(|&t| t > y) {
            Some(i) => i.saturating_sub(1),
            None => self.tops.len().saturating_sub(1),
        }
    }
}

/// Split a laid-out flow into pages.
///
/// `atoms` need not be sorted. `page_height` is the usable height of one page.
pub fn paginate(atoms: &[Atom], content_height: f32, page_height: f32) -> Pages {
    let mut atoms: Vec<Atom> = atoms.to_vec();
    atoms.sort_by(|a, b| a.top.total_cmp(&b.top).then(a.bottom.total_cmp(&b.bottom)));

    let mut tops = vec![0.0f32];

    if page_height <= 0.0 {
        return Pages { tops, page_height, content_height };
    }

    let mut top = 0.0f32;
    // Each iteration must advance `top`; the guard is belt and braces against a
    // pathological atom list turning this into a hang.
    let limit = (content_height / page_height).ceil() as usize + atoms.len() + 2;

    for _ in 0..limit {
        let ideal = top + page_height;
        if ideal >= content_height {
            break;
        }

        // Tier 1: every rule. Tier 2: only the invariant that must never break —
        // a page boundary may not split an atom. Tier 3: a single atom genuinely
        // taller than a page, so cut it rather than wedge the reader.
        let mut cut = snap(&atoms, top, ideal, true);
        if cut <= top {
            cut = snap(&atoms, top, ideal, false);
        }
        let cut = if cut <= top { ideal } else { cut };

        tops.push(cut);
        top = cut;
    }

    Pages { tops, page_height, content_height }
}

/// Largest legal break position at or below `ideal`, or `top` if none exists.
///
/// `strict` applies the typographic rules (keep-with-next, widows, orphans) on
/// top of the hard invariant that a break may not cut through an atom.
fn snap(atoms: &[Atom], top: f32, ideal: f32, strict: bool) -> f32 {
    let mut y = ideal;

    // Each rule can only move the break *up*, so this terminates.
    for _ in 0..(atoms.len() + 1) {
        let next = apply_rules(atoms, top, y, strict);
        if next >= y {
            return if y <= top { top } else { y };
        }
        y = next;
        if y <= top {
            return top;
        }
    }
    if y <= top { top } else { y }
}

/// One pass of the break rules. Returns a position <= `y`, or `y` if legal.
fn apply_rules(atoms: &[Atom], top: f32, y: f32, strict: bool) -> f32 {
    // 1. Never cut through an atom. This one is not negotiable.
    if let Some(a) = atoms.iter().find(|a| a.splits(y)) {
        return a.top;
    }

    if !strict {
        return y;
    }

    let before = atoms.iter().filter(|a| a.bottom <= y).max_by(|p, q| p.bottom.total_cmp(&q.bottom));
    let after = atoms.iter().filter(|a| a.top >= y).min_by(|p, q| p.top.total_cmp(&q.top));

    let (Some(before), Some(after)) = (before, after) else {
        return y;
    };

    // A group that began on an earlier page cannot be moved down: its start is
    // already above this page's top. Trying would push the break above `top` and
    // strand the paginator, which used to fall through to a hard cut through a
    // line. Leave it where it is.
    let movable = |group: usize| group_top(atoms, group) > top;

    // 2. A heading is never stranded at the foot of a page.
    if before.keep_with_next && before.group != after.group && movable(before.group) {
        return group_top(atoms, before.group);
    }

    // 3. Widows and orphans, within a paragraph that spans the break.
    if before.group == after.group {
        let group = before.group;
        let total = atoms.iter().filter(|a| a.group == group).count();
        let above = atoms.iter().filter(|a| a.group == group && a.bottom <= y).count();
        let below = total - above;

        // Too short to split at all: move the whole paragraph down.
        if total < ORPHANS + WIDOWS && movable(group) {
            return group_top(atoms, group);
        }
        if above < ORPHANS && movable(group) {
            return group_top(atoms, group);
        }
        if below < WIDOWS {
            // Push lines down until enough of the paragraph carries over, but
            // never above this page's top.
            let keep_above = total - WIDOWS;
            if let Some(a) = nth_of_group(atoms, group, keep_above) {
                if a.top > top {
                    return a.top;
                }
            }
        }
    }

    y
}

fn group_top(atoms: &[Atom], group: usize) -> f32 {
    atoms
        .iter()
        .filter(|a| a.group == group)
        .map(|a| a.top)
        .fold(f32::INFINITY, f32::min)
}

/// The `n`th atom (0-based) of a group, in flow order.
fn nth_of_group(atoms: &[Atom], group: usize, n: usize) -> Option<&Atom> {
    atoms.iter().filter(|a| a.group == group).nth(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask is placed from the extent, so this arithmetic is what keeps the
    /// next page's first line off the screen.
    #[test]
    fn extent_is_the_distance_to_the_next_page_not_the_nominal_height() {
        let pages = Pages {
            tops: vec![0.0, 872.0, 1744.0],
            page_height: 900.0,
            content_height: 2500.0,
        };
        assert_eq!(pages.extent_of(0), 872.0, "a break snaps up, so a page ends short");
        assert_eq!(pages.extent_of(1), 872.0);
        // The last page runs to the end of the content, never past it.
        assert_eq!(pages.extent_of(2), 756.0);
    }

    /// A last page with more content left than fits still shows only a page.
    #[test]
    fn extent_never_exceeds_the_page_height() {
        let pages = Pages {
            tops: vec![0.0],
            page_height: 900.0,
            content_height: 5000.0,
        };
        assert_eq!(pages.extent_of(0), 900.0);
    }

    use super::*;

    /// `count` lines of `h` px starting at `start`, all one paragraph.
    fn para(group: usize, start: f32, h: f32, count: usize) -> Vec<Atom> {
        (0..count)
            .map(|i| Atom {
                top: start + i as f32 * h,
                bottom: start + (i as f32 + 1.0) * h,
                group,
                kind: AtomKind::Line,
                keep_with_next: false,
            })
            .collect()
    }

    fn is_legal(atoms: &[Atom], y: f32) -> bool {
        !atoms.iter().any(|a| a.splits(y))
    }

    /// Regression: consecutive rows overlapping by a rounding pixel used to leave
    /// the paginator no legal break, forcing a hard cut through the table.
    #[test]
    fn rows_overlapping_by_a_rounding_pixel_still_break_cleanly() {
        let mut atoms = Vec::new();
        let mut y = 0.0f32;
        for i in 0..30 {
            let h = 480.0;
            atoms.push(Atom {
                top: y,
                bottom: y + h + 1.0, // overlaps the next row by 1px
                group: 9,
                kind: AtomKind::Row,
                keep_with_next: false,
            });
            y += h;
            let _ = i;
        }
        let pages = paginate(&atoms, y, 900.0);
        assert!(pages.count() > 5);
        for (n, &t) in pages.tops.iter().enumerate() {
            assert!(is_legal(&atoms, t), "page {n} top {t} splits a row");
        }
    }

    #[test]
    fn no_page_break_cuts_a_line() {
        // 100 lines of 32px = 3200px, pages of 500px.
        let atoms = para(1, 0.0, 32.0, 100);
        let pages = paginate(&atoms, 3200.0, 500.0);
        assert!(pages.count() > 1);
        for &t in &pages.tops {
            assert!(is_legal(&atoms, t), "page starts mid-line at {t}");
        }
    }

    #[test]
    fn pages_advance_and_cover_the_flow() {
        let atoms = para(1, 0.0, 32.0, 100);
        let pages = paginate(&atoms, 3200.0, 500.0);
        for w in pages.tops.windows(2) {
            assert!(w[1] > w[0], "pages must advance: {:?}", pages.tops);
            assert!(w[1] - w[0] <= 500.0, "page taller than the viewport");
        }
        let last = *pages.tops.last().unwrap();
        assert!(last < 3200.0, "a page starts past the end of the flow");
    }

    #[test]
    fn a_heading_is_never_stranded_at_the_foot_of_a_page() {
        // Heading at 480..520, then a paragraph. A 500px page would cut just
        // after the heading; the heading must move down with its text.
        let mut atoms = para(1, 0.0, 32.0, 15); // 0..480
        atoms.push(Atom {
            top: 480.0,
            bottom: 520.0,
            group: 2,
            kind: AtomKind::Line,
            keep_with_next: true,
        });
        atoms.extend(para(3, 520.0, 32.0, 40));

        let pages = paginate(&atoms, 1800.0, 500.0);
        assert_eq!(
            pages.tops[1], 480.0,
            "the break should fall before the heading, got {:?}",
            pages.tops
        );
    }

    #[test]
    fn orphans_and_widows_are_respected() {
        // One long paragraph; every break inside it must leave >=2 lines above
        // and carry >=2 lines below.
        let atoms = para(1, 0.0, 30.0, 200);
        let pages = paginate(&atoms, 6000.0, 300.0);

        for i in 1..pages.count() {
            let y = pages.tops[i];
            let above = atoms.iter().filter(|a| a.group == 1 && a.bottom <= y).count();
            let below = atoms.iter().filter(|a| a.group == 1 && a.top >= y).count();
            assert!(above >= ORPHANS, "orphan at page {i}: only {above} lines above");
            assert!(below >= WIDOWS, "widow at page {i}: only {below} lines below");
        }
    }

    #[test]
    fn a_short_paragraph_is_never_split() {
        // 3 lines cannot satisfy orphans+widows, so it moves down whole.
        let mut atoms = para(1, 0.0, 30.0, 16); // 0..480
        atoms.extend(para(2, 480.0, 30.0, 3)); // 480..570 straddles a 500px page
        atoms.extend(para(3, 570.0, 30.0, 40));

        let pages = paginate(&atoms, 1800.0, 500.0);
        let y = pages.tops[1];
        let split = atoms
            .iter()
            .any(|a| a.group == 2 && a.bottom <= y)
            && atoms.iter().any(|a| a.group == 2 && a.top >= y);
        assert!(!split, "3-line paragraph was split at {y}");
    }

    #[test]
    fn table_rows_split_between_bands() {
        // A table taller than a page must break between rows, never through one.
        let atoms: Vec<Atom> = (0..40)
            .map(|i| Atom {
                top: i as f32 * 74.0,
                bottom: (i as f32 + 1.0) * 74.0,
                group: 7,
                kind: AtomKind::Row,
                keep_with_next: false,
            })
            .collect();

        let pages = paginate(&atoms, 2960.0, 800.0);
        assert!(pages.count() > 1);
        for &t in &pages.tops {
            assert!(is_legal(&atoms, t), "break cuts a table row at {t}");
        }
    }

    #[test]
    fn an_atom_taller_than_a_page_still_advances() {
        // A 900px image in a 500px page: it cannot fit, but the reader must not
        // get stuck.
        let atoms = vec![Atom {
            top: 0.0,
            bottom: 900.0,
            group: 1,
            kind: AtomKind::Block,
            keep_with_next: false,
        }];
        let pages = paginate(&atoms, 900.0, 500.0);
        assert!(pages.count() >= 2, "pagination stalled on an oversized atom");
        for w in pages.tops.windows(2) {
            assert!(w[1] > w[0]);
        }
    }

    /// Regression: found by the library sweep. A paragraph that begins on an
    /// earlier page cannot be moved down, and trying used to strand `snap` above
    /// the page top and fall through to a hard cut through a line.
    #[test]
    fn a_group_spanning_many_pages_never_gets_cut() {
        // One paragraph far taller than several pages, preceded by a heading so
        // the keep-with-next rule is in play too.
        let mut atoms = vec![Atom {
            top: 0.0,
            bottom: 40.0,
            group: 1,
            kind: AtomKind::Line,
            keep_with_next: true,
        }];
        atoms.extend(para(2, 40.0, 29.0, 400)); // ~11600px in one paragraph

        let pages = paginate(&atoms, 11640.0, 500.0);
        assert!(pages.count() > 20, "expected many pages, got {}", pages.count());
        for (n, &t) in pages.tops.iter().enumerate() {
            assert!(is_legal(&atoms, t), "page {n} top {t} cuts a line");
        }
        for w in pages.tops.windows(2) {
            assert!(w[1] > w[0], "pages stopped advancing");
        }
    }

    #[test]
    fn empty_flow_is_one_page() {
        let pages = paginate(&[], 0.0, 500.0);
        assert_eq!(pages.count(), 1);
        assert_eq!(pages.tops, vec![0.0]);
    }

    #[test]
    fn page_containing_maps_offsets_back() {
        let atoms = para(1, 0.0, 32.0, 100);
        let pages = paginate(&atoms, 3200.0, 500.0);
        assert_eq!(pages.page_containing(0.0), 0);
        let second = pages.tops[1];
        assert_eq!(pages.page_containing(second), 1);
        assert_eq!(pages.page_containing(second - 1.0), 0);
        assert_eq!(pages.page_containing(f32::MAX), pages.count() - 1);
    }
}
