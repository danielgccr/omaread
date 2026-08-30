# Omaread — context

A proper EPUB reader for Omarchy. Pure Rust, no webview, Apple Books aesthetic.

Omarchy is the **reference platform**, not a dependency: built and tuned there,
runs on any Wayland desktop. Single maintainer, private until 1.0.

This file is the decision record. It exists so that no session — human or agent —
relitigates a settled question. If you disagree with something here, change it
here first.

---

## 1. What Omaread is

- **EPUB only.** AZW3 and MOBI are converted to EPUB at import. PDF is explicitly
  out of scope — zathura exists and is good.
- **Pure Rust, no webview.** No WebKitGTK, no Servo embedding, no Electron.
- **Wayland-native**, rendered on the GPU via wgpu.
- **Hermetic.** The application never makes a network request.
- **GPL-3.0-or-later.**

Target user is the maintainer first. 1.0 ships when the Full feature set below is
done; there are no public releases before that, and therefore no schema or format
compatibility obligations until 1.0.

## 2. Stack

| Layer | Choice |
|---|---|
| Container parsing | `rbook` |
| HTML parsing | `blitz-html` (`HtmlDocument::from_html`) |
| DOM + CSS + layout | `blitz-dom` (Stylo + Taffy + Parley) |
| Text shaping | `parley` (pinned at 0.6.0 by blitz-dom) |
| Paint | `anyrender_vello` → wgpu |
| Windowing | `winit` |
| Storage | SQLite (`rusqlite`, bundled) + FTS5 |
| i18n | `fluent-rs` |
| Hyphenation | Knuth-Liang patterns, soft-hyphen insertion |

**There is no Iced.** The application shell — library grid, chrome, settings — is
authored in HTML/CSS and rendered through the same `blitz-dom` pipeline as the
book. One layout engine, one paint path, one styling language.

`blitz-dom` is pre-1.0 and moves. Vendor it. Forking is an accepted outcome.

## 3. Rendering decisions

### CSS policy: whitelist

The publisher's stylesheets are **stripped**, not cascaded. Omaread's own base
stylesheet is supplied via `DocumentConfig::ua_stylesheets`. A fixed set of
publisher declarations is honored:

`font-style`, `font-weight`, `text-align`, margins, `text-indent`, image sizing,
`<pre>` treatment.

Everything else is rejected. A "publisher styles" toggle is post-1.0.

This is what makes a pure-Rust engine tractable: the overwhelming majority of a
browser's complexity is honoring arbitrary author CSS, and Apple Books largely
overrides it anyway.

### Fonts: bundled only

Ship Literata, Charis SIL, and a monospace face. All OFL — bundling in an
application binary is permitted; ship the license texts. Modification requires
renaming, so don't modify them.

Every book's `@font-face` is ignored. **Consequence: font de-obfuscation is code
that never gets written.** Several books in the test library carry Adobe-obfuscated
fonts (`enc#RC`); Omaread neither needs nor wants them.

### Base stylesheet

```
body            20px default, user scale 0.8–1.6×
measure         33em (~66ch at default), hard cap 40em (~80ch)
line-height     1.6
alignment       justify + soft-hyphen insertion per dc:language (en/es/ca)
paragraph       text-indent: 1.2em; margin: 0
                first para after any heading: text-indent: 0
headings        1.5 / 1.3 / 1.15em, weight 600, ragged-right
                margin-top: 2em; margin-bottom: 0.6em
blockquote      margin-inline: 2em; 0.95em
pre / code      bundled mono, 0.9em, tinted panel, pre-wrap (never scrolls)
img             block, centered, max-width: 100%, max-height: 85vh
figcaption      0.85em, subtle, ragged
links           internal → navigate; external → confirm dialog → xdg-open
themes          White / Sepia / Grey / Night
TOC panel       light-grey ground (app chrome, not book CSS)
```

Measure is **fixed by the stylesheet**, not a user setting. 80ch is a design
ceiling, not a knob.

The editorial paragraph convention (indent, no inter-paragraph gap) is deliberate
and is the main thing that makes a page read as a page rather than a web article.

`pre` wraps and never scrolls. In a paginated reader there is no sideways. Long
code lines in technical books will wrap imperfectly. This is accepted.

### User-facing controls

Three: **theme, font size, columns (1 or 2).** Plus the invisible-chrome toggle,
which is a gesture rather than a setting. Resist adding more; every knob is a
combination that has to look good.

### Pagination

Taffy implements Flexbox, Grid and block layout. **It has no multi-column and no
fragmentation.** CSS multicol — how every webview-based reader paginates — is
unavailable. Pagination is ours to implement.

The model:

- A chapter is laid out **once, continuously**, at the measure width.
- Pages are a **view** over that flow: a Y-translate on the paint scene.
- Breaks land on **atomic-unit boundaries**, never inside one:
  - prose → line boxes (`parley::Layout::lines()`, `Line::metrics()`)
  - tables → row bands (see below)
  - `<pre>` → code lines
- A block taller than one page splits across pages rather than overflowing.
- Two-column mode is **two viewports side by side** taking pages `2n` / `2n+1`
  from the single flow. It is *not* CSS `column-count`. This is strictly better
  for a reader: the columns are pages you turn, not a scroll that snakes.
- Two-column **auto-disables below ~70em of available width** rather than
  squeezing into unreadable ~38ch columns.
- Widows/orphans and "never break directly after a heading" are **paginator
  rules**, not CSS. The `orphans`/`widows` properties do not exist here.
- Table page breaks: repeat the header row on each continuation page.

Because pages are a view over a stable flow, changing the font size re-flows and
re-paginates without invalidating anything persisted (see CFI below).

## 4. Data

### Position: EPUB CFI

Reading positions, bookmarks and highlights all anchor to **EPUB CFI**
(`epubcfi(/6/14!/4/2/1:341)`).

Page indexes are not acceptable — they change meaning when font size changes, and
the app ships a font-size control. CFI additionally survives round-tripping to and
from other readers, and is the only anchor that doesn't corner us when highlights
arrive.

### SQLite

One database. Holds:

- book metadata, cover images as BLOB
- reading progress (CFI), bookmarks, highlights, notes
- collections / tags
- **FTS5 over extracted chapter text**

FTS5 gives in-book search and cross-library full-text search from one index, and
the text has to be extracted anyway for CFI resolution. Search across the whole
library is something Apple Books does poorly; it is nearly free here.

**No on-disk cache.** Decoded images live in an in-memory LRU and die with the
process. Do not cache layout — the engine's layout output will change underneath
us while blitz-dom is pre-1.0.

### Library on disk

- Books are **copied** into `~/.local/share/omaread/library/`, canonically renamed
  `Author - Title.epub`. Originals are left alone.
- Dedupe by **SHA-256** of file contents. Re-importing the same book is a no-op
  regardless of filename, which matters — the test library has the same books
  under different Anna's-Archive filenames in two directories.
- **Watched folders are read-only.** Omaread never renames or moves a file it does
  not own.
- Books deleted on disk keep **ghost rows**, preserving reading progress.
- AZW3/MOBI are converted to EPUB **at import**. The converted EPUB lives in the
  managed library and is what highlights anchor into. The reader core is
  single-format forever; adding a format means writing a converter, not auditing
  every subsystem.
- Config in `~/.config/omaread/`. Cache: nowhere.

## 5. Security and privacy posture

**Hermetic. `blitz-net` is never linked.**

- A custom `NetProvider` (via `DocumentConfig::net_provider`) resolves resources
  **only inside the book's own zip**, with zip-slip and path-traversal defenses.
- Remote `<img>`, remote CSS, `<iframe>` → placeholder, no request.
- No JS engine exists anywhere in the Blitz stack, so scripted EPUB 3 content
  simply does not run. This is a feature.
- External links: confirmation dialog showing the real destination, then
  `xdg-open` via portal. Nothing leaves without an explicit choice.

Not linking an HTTP client at all makes the guarantee structural rather than a
code path someone can regress. A reading application knows what you read and
when; that data must not be able to leave.

## 6. Robustness

- `panic = "unwind"`. **Not `abort`.**
- `catch_unwind` around chapter layout and paint. A book that panics the engine
  renders an error page naming the book and chapter; the app and the library
  survive.
- Book-level quarantine (a book that panics twice gets flagged rather than
  reopened) is a 1.0 nicety, not needed early.

This is not hypothetical. See §8 — real books from the test library panicked
`blitz-dom` twice within minutes of first contact.

## 7. Scope of 1.0 (Full)

Library grid, search, sort · reading with pagination, themes, font size · TOC
navigation · CFI position persistence · text selection and copy · search inside
book · bookmarks · highlights and notes · screen-reader support via AccessKit ·
collections and tags · AZW3/MOBI import.

`accessibility` and `accesskit` are **default features of blitz-dom**, so
screen-reader support is nearer to free than it looks. A reading application
without it would be a poor choice.

## 8. Status

**Phase 1 complete** — a chapter renders on screen. `winit` + `anyrender_vello`,
hermetic in-zip resource provider, base stylesheet via `ua_stylesheets`,
`catch_unwind` around load/relayout/paint, theme and font-size cycling, chapter
navigation, scrolling. 10 unit tests green, including layout assertions that the
one-column measure is centred at 900/1200/1600/2400/3840px and keeps its gutters
below the measure width.

Measured on a real chapter at a 1526px window: `<body>` 730px wide with 398px
either side, `<p>` exactly 660px with 433px either side.

Verified against real books: Postman *Divertirse hasta morir* (es) renders
justified Spanish prose with editorial indents; Géron *Hands-On ML* ch.7 renders
headings, italics, links and all images with **zero missing and zero blocked
resources**.

**Phase 2 complete** (one column) — the line-snapped paginator. A chapter is laid
out once, continuously; pages are Y ranges over that flow. Breaks land on atom
boundaries: line boxes for prose, cell-derived row bands for tables, block boxes
for images and rules. Widows and orphans are held at 2 lines, headings are never
stranded at the foot of a page, and an atom taller than a page still advances
rather than wedging the reader. Pages have their own vertical margins, masked
after paint. Turning crosses chapter boundaries in both directions, and the
reader's place survives a re-flow because it is carried as a *flow offset*, not a
page number.

`omaread --check <book.epub>...` is the headless conformance run: it opens every
chapter, paginates, and reports engine panics, breaks that cut an atom, stalls
and empty chapters. This is the CI harness §13 asks for.

**Two-column is specified and paginated but not yet paintable** — see the
`paint_scene` reset note below.

**Phase 3 complete** — CFI and persistence. `src/cfi.rs` generates, parses and
resolves the structural CFI subset (`epubcfi(/6/N!/4/2/6)`, optional `:offset`
parsed but not generated); `src/db.rs` keeps reading progress in SQLite keyed by
SHA-256 of the file, so a move or rename does not lose your place. Position is
saved on every page turn and on exit, restored on open.

Verified end to end: open a book with no arguments, land back on chapter 9 page
9/19; resize the window, the text re-flows to 15 pages and the position tracks to
page 7 at the equivalent flow offset. A page number could not do that — which is
the whole reason for §4's CFI decision.

Precision is **paragraph level**: the CFI names the element a page begins at, not
a character within it. Resuming at a paragraph start is right for a reader;
character offsets arrive when highlights need them (Phase 7).

Next: Phase 4, the library.

## 9. Spike findings (verified, not assumed)

A throwaway binary was run against real books from the test library. Results:

**Line-box geometry is reachable.** `Node.final_layout` is a public field
(taffy `Layout`: parent-relative x/y/width/height).
`ElementData.inline_layout_data: Option<Box<TextLayout>>` is public, and
`TextLayout.layout` is a public `parley::Layout`. Accumulating `final_layout.location.y`
down the tree gives absolute positions; `layout.lines()` + `Line::metrics()` gives
per-line geometry. **Pagination is viable.**

Measured, at 660px measure / 900px page:

| Chapter | Content height | Line boxes | Pages |
|---|---|---|---|
| Postman, *Divertirse hasta morir*, ch.8 (es) | 20184px | 602 | ~23 |
| Géron, *Hands-On ML*, ch.9 (en, tables + figures) | 39055px | 1047 | ~44 |

**Tables lay out.** `blitz-dom` has `layout/table.rs` mapping `<table>` onto Taffy
grid. But **`<tr>` is not a layout box** — every `<tr>` reports `height = 0.0` at an
identical Y. The geometry is on the **cells**. Row bands must be derived by
grouping `<td>`/`<th>` boxes by Y coordinate:

```
row bands      : 12
   band y=20792.0 h=34.0   (header)
   band y=20826.0 h=74.0
   band y=20900.0 h=74.0
   band y=21122.0 h=106.0
```

Clean, real heights, correct page assignment. **Table splitting is implementable** —
group cells by Y, break between bands.

**`blitz-dom` panics on real-world books.** Two distinct panics inside minutes:

1. `<link rel="stylesheet" href="stylesheet.css">` with no `base_url` →
   `panicked at document.rs:550: to be able to resolve stylesheet.css`
2. `<img src="assets/mlsp_0901.png">` with no `base_url` → same panic site

Mitigations, both of which were already the design:
- stripping publisher CSS (the whitelist policy) removes cause 1 entirely
- a `base_url` plus the in-zip resource provider removes cause 2
- `catch_unwind` catches whatever is left

Malformed HTML (`ERROR: Duplicate attribute` in the Géron book) is reported and
recovered from without panicking.

**`DocumentConfig` exposes exactly the hooks the design needs:** `ua_stylesheets`
for the base stylesheet, `net_provider` for the hermetic resource provider,
`base_url` for in-zip resolution, `font_ctx` for bundled fonts.

**`parley` 0.6.0 has `Alignment::Justify`**, correctly implemented (last line
excluded, no-ops on non-positive free space). It has **no hyphenation** — that is
ours: run Knuth-Liang patterns over extracted text, insert `U+00AD`, and Parley's
UAX-14 line breaker treats soft hyphen as a break opportunity (class BA). This is
a text-preparation step, not an engine change.

**`blitz_paint::paint_scene` calls `scene.reset()` on entry** (render.rs:70),
discarding everything drawn into the scene before it. Two consequences:

1. Page background and margin masks must be drawn **after** the call, not before.
2. **Two-column cannot be painted** as two `paint_scene` calls into one scene —
   the second erases the first. Composing per-column sub-scenes is also blocked:
   `VelloScenePainter.inner` is `pub(crate)`. Needs an upstream non-resetting
   paint entry point, or access to the inner scene. The paginator itself already
   supports two columns (pages `2n`/`2n+1` of one flow); only painting is blocked.

**A layer transform positions the clip shape, not the content.**
`push_clip_layer(transform, clip)` moves where the clip *is*; drawing commands
inside the layer are unaffected. The only thing that translates painted content
is `BaseDocument::set_viewport_scroll`, which takes both axes in document
coordinates. That is how a page offset is applied.

**blitz-dom applies its own default UA stylesheet *after* `ua_stylesheets`.**
A bare `body {}` in our sheet loses to its `body { margin: 8px }`, and a bare
`p {}` loses to its `p { margin: 1em 0 }` — so the base stylesheet silently lost
every collision: no centring, and web-style paragraph gaps instead of the
editorial convention. **Every selector in `style.rs` is therefore prefixed with
`html`** to raise specificity just enough to win. Keep that prefix when adding
rules. Caught by a layout assertion, not by looking at it — screenshots made it
look approximately right.

**Layout geometry is floating point; adjacency is approximate.** Consecutive
table row bands were observed *overlapping by a pixel* — `18806..19292` followed
by `19291..19833`. The only legal break between them therefore looked like it
fell inside the first row, the snap chain had no exit, and the paginator hard cut
straight through the table. Containment is now tested with `EDGE_EPS = 1.0`
(`Atom::splits`), and `--check` uses the same predicate so the checker and the
invariant cannot disagree. A sub-pixel break is invisible; a hard cut is not.

**Inside a table the row is the atom.** Line boxes within a cell overlap their
row band, and keeping them made those lines veto breaks at the very row
boundaries the band exists to provide. `collect_atoms` drops any atom fully
contained in a row band.

**Pagination must never fall back to a hard cut without first relaxing.** The
first library sweep found three chapters where a page boundary cut through a
line. Cause: a paragraph or heading that *began on an earlier page* cannot be
moved down, but the widow/orphan and keep-with-next rules tried anyway, pushing
the candidate break above the page top; the paginator then fell through to an
unconditional cut at the ideal offset. Fixed with two changes — a `movable`
guard so a group starting at or above the page top is left alone, and a tiered
snap: all rules, then the no-cut invariant alone, and only then a hard cut for a
single atom genuinely taller than a page. Regression test:
`a_group_spanning_many_pages_never_gets_cut`.

**rbook's href space is absolute and slash-prefixed.** `manifest_entry().href()`
yields `/OEBPS/ch04.html`. `read_resource_*` accepts either that form or a path
relative to the OPF directory — so a bare `OEBPS/assets/x.png` is read as
OPF-relative and silently resolves to `/OEBPS/OEBPS/assets/x.png`, which fails.
**Preserve the leading slash end to end.** This cost every image in a chapter
before it was caught; the fix is in `net::in_archive_path` and
`chapter::base_url_for`, both covered by tests.

**Version note:** `blitz-dom` resolves to **0.2.4**, not the `0.1.0-alpha.4` that
public search results suggest. Considerably more mature than advertised.

## 10. Test library facts

- 372 EPUBs across `~/Documents`, `~/Downloads`, `~/Downloads/Telegram Desktop`.
- Mixed Spanish, Catalan, English. EPUB 2 and EPUB 3 both present.
- Several books duplicated across directories under different filenames — this is
  what the SHA-256 dedupe is for.
- **No DRM.** Files carrying `META-INF/encryption.xml` use Adobe *font
  obfuscation* (`enc#RC` over a single `.otf`), not content DRM. The whole library
  is readable, and since book fonts are ignored anyway, this is a non-issue.
- Heaviest case: Géron *Hands-On ML* — 156 PNGs, 6 embedded OTFs, 2 CSS files,
  code blocks, tables. This is the book to test against.

## 11. Omarchy integration

Omarchy is the reference platform. Nothing here is a hard dependency — every
integration degrades to a sensible default elsewhere.

- **Theming is split.** App chrome (library grid, TOC panel, settings) follows the
  active Omarchy theme. The **reading surface keeps its own four themes**
  (White / Sepia / Grey / Night) and ignores the system palette. Sepia is a paper
  decision, not a UI one, and a palette tuned for a terminal is not tuned for 400
  pages of prose. Off Omarchy, chrome falls back to its own neutral palette.
- Ship an `omaread.css.tpl` for Omarchy's template-rendering theme mechanism
  (cf. `~/.config/omarchy/themed/alacritty.toml.tpl.sample`) rather than parsing
  theme files ourselves.
- Ship a suggested Hyprland window rule and keybinding.
- **Binary is `omaread`.** Not `omarchy-read` — `omarchy-*` is upstream's
  namespace; squatting it invites confusion and makes upstream inclusion harder,
  not easier.

## 12. Packaging and platform

- AUR first; Flatpak at 1.0.
- Design for the sandbox from day one: XDG base directories everywhere, zero
  hardcoded paths, file dialogs through `xdg-desktop-portal` (`ashpd`/`rfd`).
  Retrofitting this is miserable, and it is the same discipline that makes watched
  folders behave correctly under a sandbox.
- UI in English and Spanish. `fluent-rs` scaffolding from the first commit even
  while only English strings exist — retrofitting extracted strings is worse.
- Hyphenation patterns for `en`/`es`/`ca` regardless of UI language. Unhyphenated
  justified Spanish produces rivers of whitespace; this is load-bearing for the
  aesthetic, not an i18n nicety.

## 13. Testing

Because there are no public releases before 1.0, no stranger's broken EPUB will
be seen for a long time. The substitute:

- **Conformance corpus in CI** — epubtest.org, the W3C EPUB test suite, Calibre's
  pathological samples.
- **Headless smoke run over all 372 local books** via `omaread --check`: assert no
  engine panic, no page break cutting an atom, no pagination stall, non-empty
  text per chapter. The first sweep found three real breaks that cut a line —
  see §9.

That corpus is the only external pressure the renderer will get. Treat a new
panic in it as a release blocker.

## 14. Rejected, with reasons

| Rejected | Why |
|---|---|
| WebKitGTK webview | Wanted pure Rust. Cost is accepted knowingly: everything a browser gives free is now ours to build. |
| Iced | Once you own a CSS engine, the library grid is HTML/CSS. Two layout systems and two paint paths for no gain. |
| Full CSS cascade | The whitelist is what makes a hand-owned engine tractable, and a uniform reading surface is a design position. |
| Honoring book `@font-face` | Inconsistent typography, plus a de-obfuscation subsystem, for nothing. |
| `column-count: 2` | Taffy has no multicol. The dual-viewport approach is better here anyway. |
| Page-index positions | Break under the font-size control the app ships. |
| `panic = "abort"` | Alpha engine, hostile input. One bad book would kill the app mid-chapter. |
| On-disk layout cache | Layout output will change under us while blitz-dom is pre-1.0. |
| PDF support | zathura is good. Scope discipline. |
| Managed library that *moves* originals | Nobody wants an app relocating 372 files. |
