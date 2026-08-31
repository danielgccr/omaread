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

Shipped, in `assets/fonts/`, all from `google/fonts`, each with its `OFL.txt`
(~3.7MB embedded via `include_bytes!`):

| Face | Files | Why |
|---|---|---|
| Literata | variable, upright + italic | the reading face |
| Charis SIL | static regular + italic | coverage Literata lacks |
| IBM Plex Mono | static regular, bold, italic | `pre` / `code` |

Literata is a **variable** font and that is the point: `fontique` reads its
`wght` axis and sets the weight from CSS, so one file per slant covers every
weight the base stylesheet asks for, statics for none of them. Charis SIL is a
fallback reached for a rare glyph, so it carries no bold — a synthesised one is
fine there.

The faces are registered per document through `Resource::Font`, not through
`DocumentConfig::font_ctx`. Supplying that field makes blitz skip registering
its own list-bullet font, and that font is `pub(crate)` — a shared context costs
`<li>` markers. Share one, and vendor a bullet font, only if this shows up in a
profile.

The system's fonts stay available as a last resort for glyphs none of the three
carry. "Bundled only" is about ignoring the *book's* fonts, not about rendering
tofu.

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

All three are reachable from the HUD as well as from the keyboard (§8). **Font
size is stored globally**, not per book: it describes the reader, not the book.

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
- collections / tags (one `tags` table; a collection *is* a tag)
- **FTS5 over extracted chapter text** (built in Phase 6: ~400MB for 360 books,
  indexed on open or via `omaread --index`)

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
- Watched folders live in `~/.config/omaread/folders.txt`, one path per line,
  seeded with `~/Documents` and `~/Downloads` on first run. A newline list rather
  than TOML: it holds paths and nothing else.
- **No inotify.** A scan runs at startup and on F5. Live watching is a
  convenience, not a correctness requirement; add `notify` when rescanning by
  hand actually becomes annoying.

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

### Exit does not run destructors

`main` ends with `std::process::exit(0)`, deliberately.

**Every clean exit dumped core.** Dropping the renderer tears down wgpu's
GLES/EGL instance — a backend this app never renders with, which exists only
because wgpu enumerates all of them — and NVIDIA's egl-wayland layer then
marshals a Wayland request on a dead proxy:

```
wl_proxy_marshal_flags            libwayland-client   <- SIGSEGV
libnvidia-egl-wayland2 / libEGL_nvidia
<wgpu_hal::gles::egl::Inner as Drop>::drop
drop_glue<VelloWindowRenderer>
drop_glue<App>                    <- end of main
```

Ten cores on this machine before it was looked at, every one the same stack.

Two fixes were tried and rejected as insufficient before this one: releasing the
surface via `WindowRenderer::suspend()` in winit's `exiting` hook (the crash is
in the *instance*, not the surface), and the observation that `App` dropped
`window` before `renderer` (true, and worth knowing, but not the cause). Only
skipping the teardown works.

Nothing here needs a destructor to be correct: reading position, marks and tags
are committed to SQLite before the loop returns, and WAL means a process that
simply stops loses nothing. The OS reclaims the GPU and the fonts. Remove the
`exit` when wgpu or the driver can survive its own teardown.

`OMAREAD_EXIT_AFTER=<ms>` exists because of this: shutdown is a real code path
with real bugs in it, and it could not be reached from a script without a window
manager. It is the same path `q`, Esc and the compositor's close button all take.

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

**Two-column paints** as of Phase 8 — see below. The paginator supported it from
here; only the compositing was missing.

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

**Phase 4 complete** — the library. `src/library.rs` scans the watched folders
(seeded in `~/.config/omaread/folders.txt`, one path per line), identifies books
by SHA-256, and extracts title, author and cover with `rbook`. Opening a book
copies it into `~/.local/share/omaread/library/` as `Author - Title.epub`;
originals are never touched. Files that vanish become ghost rows, keeping their
reading progress. `src/grid.rs` is the library view, authored in HTML/CSS and
rendered through the same blitz-dom pipeline as a book — covers come from SQLite
through a `CoverProvider` on the `omaread-cover://` origin, the same hermetic
pattern as the book provider.

Measured on the real library: **372 files, 361 unique books** (11 collapsed by
content hash — the same titles sitting in two folders), **358 with covers**.

Cards are 225×348 with 21px titles and 18px authors — a 1.5× enlargement of the
first sizing, geometry and caption alike, which was too small to read a spine at
arm's length — and **a row holds at most seven** however wide
the window gets: past seven, a shelf of covers turns into a contact sheet. The
grid is capped at that width and centred, so a 4K screen gets a shelf rather than
a wall. The cap lives in `grid.rs` as one number with one piece of arithmetic
(`grid::per_row`) that both the stylesheet and arrow-key navigation use; a second
copy in `main.rs` is exactly how the selection starts stepping onto columns that
are not there.

The card under the pointer takes a light blue wash, and the selected card an
outline. Both are painted rather than marked up, for the same reason: a `:hover`
class would mean rebuilding the document on every mouse move, and a rebuild
re-requests covers. The wash is drawn through `paint::bands`, the same call the
reading highlights use, so hover cost nothing but a colour and a rect.

Type to search, Tab to change sort, F5 to rescan, Enter or click to open, Esc or
`l` to come back. Book metadata is escaped on the way into the markup and there
is a test for it: titles come from files off the internet and must not be able to
inject into the view.

**Phase 5 in progress** — reading polish. Two of the four are in:

*Hyphenation.* `src/hyphen.rs` runs Knuth-Liang patterns (the `hyphenation`
crate, `embed_all`) over the text of a chapter and inserts `U+00AD`; Parley's
UAX-14 breaker does the rest. Patterns are chosen from the book's `dc:language`,
and an unknown language gets no hyphenation rather than another language's
rules. `code` and `pre` are left alone.

*Contents.* `src/toc.rs` is the navigation view, HTML/CSS through the same
pipeline as the grid and the book. `book::read_toc` flattens the EPUB 3 `nav`
(or the EPUB 2 NCX — rbook falls back on its own) into spine targets. Tab opens
it from the reading view and closes it again; ↑/↓ select, Enter or a click
navigates, and it opens on the entry covering where you already are.

Three decisions worth keeping:

- **A full view, not a floating panel.** A panel over the page means two
  `paint_scene` calls into one scene, and the second resets the first — the same
  §9 blocker that holds up two-column. Nothing about the model changes when that
  lifts: the contents are already a standalone flow, so they can be composited
  into a side panel then.
- **Entries carry their href fragment**, and navigation resolves it to an
  element and then to a page. Books that keep several chapters in one spine file
  would otherwise send every entry to page 1 of it.
- **Entries that do not resolve to a spine item are dropped, and a book with no
  usable navigation falls back to its spine.** A dead line is worse than no
  line, and the contents key should always do something.

`--check` now also reports contents entries, books falling back to the spine,
and fragments that do not resolve in their chapter — the corpus is the only
pressure this gets before 1.0 (§13).

Shared plumbing that had to move: all three views are paginated documents, so
`page_count` and paging are view-aware. That fixed a real bug in passing — the
library's PageUp/PageDown called the *reading* page turn, which reads the open
chapter's page count and so did nothing at all with no book open.

*Fonts.* The three bundled families are in `assets/fonts/` and embedded in the
binary; see §3 for what ships and why. Until now the reader asked for Literata
and got whatever system serif fontconfig offered, so the measure, the
justification and the hyphenation were all tuned against a face that was never
going to ship.

The switch is visible in the sweep: the 19 books in `~/Documents` went from
**9859 pages to 10940** at the same window size — an 11% reflow, which is the
proof that the bundled faces reach layout rather than merely registering. It
also surfaced a real paginator bug (§9, row bands shorter than their lines);
that is fixed, and the sweep is back to **0 panics, 0 breaks cutting an atom, 0
stalls** across 786 chapters.

*Chrome, in part.* Moving the pointer raises a HUD over the page with the
book's title and how far through it you are, and it puts itself away again after
a couple of seconds — the gesture §3 asks for rather than a setting. It is a
blitz document like every other surface here, painted over the page through
`paint::NoReset` (§9). Progress is weighted by the byte length of each spine
item, so a two-page foreword is not the same fraction of a book as a sixty-page
chapter.

Reading progress, the contents and the library all now go through one paint
path (`src/paint.rs`), and `omaread --shot <book> <ch> <page> <out.ppm>` renders
a page headlessly through that same path. That last one is not a toy: there is
no way to eyeball a Wayland window from a script, and it is what found the mask
bug above — the page *looked* fine, and only a row-by-row darkness profile
showed the mask landing inside a glyph band.

**Full-library sweep, 372 books:** 20986 chapters, 183642 pages, **0 engine
panics, 0 stalls**, 20174 contents entries. Four breaks cut an atom; three of
them cut a table row taller than a whole page (up to 1338px against a 900px
page), which is unavoidable and now reported separately as `BIG` so the harness
is not permanently red. **One is real** — a 35px line cut in *Cómo dejar de ser
tu peor enemigo* ch.4 — and is the next paginator bug to chase. 554 dangling
contents fragments across a handful of books whose own navigation is broken; 8
books have no usable navigation at all and fall back to their spine.

*Chrome, the rest.* The app chrome follows the active Omarchy theme, via the
mechanism §11 asks for and nothing more: `assets/omarchy/omaread.css.tpl` goes
in `~/.config/omarchy/themed/`, Omarchy renders it into the active theme
directory on a theme switch, and Omaread reads four custom properties out of the
result (`--bg`, `--fg`, `--subtle`, `--panel`). No theme parsing, nothing to keep
in sync with upstream. Off Omarchy, or before the template is installed, the
chrome keeps following the reading theme. F5 re-reads it, so a theme switch does
not need a restart. A value that is not a `#rrggbb` is refused rather than
passed on — an unrendered `{{ background }}` would otherwise paint the library
black.

`assets/omarchy/omaread.conf` is the suggested Hyprland window rule and
keybinding. Writing it turned up a real bug: the window had **no `app_id` at
all**, so every rule keyed on `class:` would have silently never matched. winit
does not set one unless asked; it is set now, and `hyprctl clients` reports
`class: omaread`.

**The invisible-chrome toggle is the pointer gesture**, and it is done. §3 is
explicit that it is a gesture rather than a setting, so there is no key and no
persisted flag: the reading view has no chrome until the pointer moves, and the
HUD leaves again on its own. Nothing further to build here.

**Phase 5 complete** — hyphenation, contents, bundled faces, HUD, and Omarchy
theming. Themes and font size have worked since Phase 1.

**Phase 6 complete** — search. One FTS5 index over extracted chapter text
answers both questions §4 asks it to.

*Extraction* reads the raw XHTML (`search::text_of_html`), not a laid-out
document: indexing must not cost a Stylo/Taffy/Parley pass per chapter. It is a
tag scanner, and the parts that matter are the ones that bite — block tags
become word breaks (or `<p>one</p><p>two</p>` indexes as "onetwo"), `<script>`
content is skipped by *finding its close tag* rather than counting depth
(`1 < 2` inside a script otherwise looks like a tag and eats the rest of the
chapter), soft hyphens we inserted never reach the index, and an unterminated
tag at EOF is dropped rather than indexed as prose.

*Tokenising* is `unicode61 remove_diacritics 2`, which is not optional for this
library: it is mostly Spanish and Catalan, and without it "cancion" does not find
"canción" — which is how people type when they are searching.

*Queries* are never handed to FTS5 raw. Every token is quoted, which strips all
operator meaning, and the last gets a `*` so hits appear while you are still
typing. A syntax error here would read as "nothing found", which is the worst
failure available.

*In-book search is the contents view.* Both are a list of places in this book, so
`/` reuses the same document, keys, paging and hit-testing that Tab uses; a
search hit is a `TocEntry` carrying `find` instead of a `fragment`. Following one
locates the text in the laid-out chapter and lands on its page — folding accents
the same way the index did, and falling back to the query's longest word when the
phrase straddles an inline tag.

*Typing in the library now searches the pages too*, off the same index, for the
cost of one `OR` and a subquery.

*The library has a search field at the right of its bar*, showing the query and
a caret, with up to six completions under it. Not an `<input>`: keystrokes
already arrive through the window and the field only has to show the query, so a
focus and editing model for one box would be pure cost. The completions are
FTS5's own term list (`fts5vocab`), ranked by how many chapters use the word —
so they are real words from the shelf, in the folded form you would type
("histerica", not "histérica"). Author names are mixed in, because a library
where nothing has been indexed yet would otherwise never suggest anything. They
are clickable, through the same hit-testing the cards use. Suggestions render
only while there is a query, so the grid shifts down once rather than twitching
on every keystroke.

`omaread --shot library <out.ppm> [query]` renders the library through the same
paint path, which is how the above was checked.

**Measured on the real library:** `--index` indexed **359 books, 19626 chapter
rows, in 14 seconds**. The database is **406MB** — which is why nothing indexes
the whole library on its own: opening a book indexes that book (cheap, and it is
about to be read anyway), and `omaread --index` is the explicit backfill. Spot
checks against real prose: "cancion" hits accented text across four books, and
following the accent-free query "tipografia siempre estaba" lands on page 7 of
ch.10 of Postman, on the line "la resonancia de la tipografía siempre estaba
presente".

**Phase 7 complete** — bookmarks, highlights, notes, selection and copy.

*Sub-paragraph CFI, at last.* `Cfi` already parsed and printed `:offset`; Phase 7
is what generates one. Offsets are stored as **characters**, not bytes, because
that is what `:offset` means and because folding an accent changes a byte length
but never a character count. parley counts bytes, so the conversion happens at
that boundary and nowhere else.

*Selection is parley's.* `Cursor`/`Selection` do hit-testing, extension and
selection geometry; parley is already pinned by blitz-dom, so naming it directly
cost one line of `Cargo.toml` and no new code. A press anchors, a drag extends, a
press with no drag is a click and clears. **One paragraph at a time** — a
selection lives in one parley layout, and stitching several is a bigger job than
it looks; a drag past the end clamps to the paragraph rather than doing nothing.

*Bookmarks and highlights are one table.* A bookmark is a highlight with no span,
so `marks` holds both: CFI, length in characters, the words themselves, and a
note. `UNIQUE(file_hash, cfi, length)` is what makes `b` a toggle rather than a
duplicate factory. The list comes back in reading order — by spine and offset,
because CFIs only sort correctly as text by accident.

*The marks list is the contents view*, again. Contents, search hits and marks are
all "places in this book", so the `finding` bool became a `TocMode` and all three
share one document, one key handler and one hit-test path. An entry now carries a
`fragment`, a `find`, **or** a `cfi`, and following it uses whichever it has.

*Painting.* Highlights and the selection are flow-coordinate rectangles filled
after the page, clipped to the page band so a highlight belonging to the next
page cannot bleed into this one's margin. On top of the glyphs, translucent —
which is what a highlighter does.

*Copy* shells out to `wl-copy`. Wayland is the target (§1), so that is one
process instead of one dependency; it says so plainly if wl-clipboard is missing.

*Notes* are typed in the marks list (`n`), which reuses the same typing mode the
search box uses.

Verified by rendering: `--shot` given text instead of a page number now also
highlights it, and the result puts a yellow band across exactly "resonancia de la
tipografía siempre estaba presente", correctly split per line, from an
accent-free query — the same `highlight_rects` path a stored mark paints through.

Keys: `b` bookmark · drag to select · `h` highlight · `y` copy · `m` marks, then
`n` note and `x` delete.

**Phase 8, two-column** — the last of §3's three user-facing controls, and the
one that had never worked. `c` toggles it.

Two columns are two pages of one flow side by side: the document is laid out at
*column* width and painted twice, at two scroll offsets, the second through
`paint::Compose` with an x translation. Not CSS multicol, which Taffy does not
have, and better for a reader anyway — the columns are pages you turn, not a
scroll that snakes.

What the implementation actually turned on:

- **`Compose` grew a transform.** The Phase 7 wrapper already swallowed
  `reset`; pre-multiplying an `Affine` into every forwarded drawing command is
  what moves the content. §9's warning that a layer transform moves the clip and
  not the content is still true — this is not a layer.
- **Masks are per column, not per frame.** Two columns are two pages, and a
  break lands where it lands: their extents differ by up to a line, so one
  full-width band would clip the longer column or leak the shorter one's next
  line.
- **The document is painted at column width**, so its own background stays
  inside its column.
- **A ragged end is clean paper.** Past the last page a column gets ground
  rather than whatever the previous frame left there.
- **The pointer knows its column.** A selection in the right-hand column belongs
  to a different page of the flow, so the column under the pointer decides both
  the vertical offset and the x.
- **The HUD spans the window**, not a column: only the page is laid out narrow.
- **One column outside the reading view.** The library and the contents are
  single documents at window width; that decision now lives in one pure function
  (`columns_for`) with the width test §3 asks for.

Verified by rendering at 1800×1000: the left column ends "…A veces tiene el
poder" and the right begins "de implicarse en nuestros conceptos de piedad…" —
continuous, no gap, no repeat — and on the last page the right column is blank
paper with the HUD across the foot.

**Collections and tags** — §7 lists both; they are **one mechanism**. A
collection is a named group of books and so is a tag, so there is one `tags`
table, one filter and one gesture rather than two of each. Presented as tags,
because that is the word that also covers "a book in two collections".

- **`#tag` in the search box filters by tag.** No new view, no new mode: the box
  that already exists gains a prefix, and the suggestions under it turn into the
  tags in use with their book counts.
- **F2 tags the selected book**, borrowing the same box to type into. A
  modifier-free non-letter key, for the reason the codebase already gives for
  Tab and F5: letters go to the search box.
- **One key, both directions.** Typing a tag the book already has removes it. A
  separate "untag" would need its own key *and* its own way to name the tag.
- **Tags are folded and hyphenated on the way in**, so "Sci Fi", "sci-fi" and
  "SCI-FI" are one group and an accent does not split one either — the same
  `search::fold` the index and the highlights use.
- Rows carry their tags from **one** query, not one per book; the cards show them
  under the author.

Verified by rendering: `#ensayo` in the box, "4 matching “#ensayo”" in the bar,
and the tag under the author on the cards.

**The HUD became the reading chrome.** It had one bar; it now has two, and both
are click targets.

- **Top bar:** Bookmark, Contents, Highlight on the left; `A−`, `A+` and the
  column toggle on the right. Every one of §3's three controls is now reachable
  by mouse as well as by key, which is what makes the keys optional rather than
  required knowledge.
- **Bottom bar:** the book's title, in **semibold** — it is the book, so it
  carries the weight — and a readout that **toggles between the percentage and
  the page number on a single click**.
- Both bars sit in the page's own margins, so nothing covers prose. Buttons carry
  `data-hud`, and a press in the reading view asks the HUD first: only if the
  pointer missed it does the page start a selection.
- There is a test that every control is *hittable*, not merely drawn — it scans
  across both bars and asserts each `data-hud` occupies real window coordinates.
  Rendering a button and being able to click it are different claims.

**Font size is universal.** It is a property of the reader's eyes, not of the
book, so it is stored once in `~/.config/omaread/settings.txt` and applies to
every book from then on. `folders.txt` said to add a real config format when
there was a second thing to configure; this is it, and it is `key=value` lines
hand-parsed in twelve lines rather than a TOML dependency. Writing one setting
leaves the others alone, and there is a test for that, because a font-size change
silently dropping another setting is exactly the bug that format invites.

Verified: `font-scale=1.40` on disk gives a 924px measure at startup (28px × 33em)
where the default gives 660px.

**Icons are painted, not typeset.** The bundled faces carry no symbol glyphs —
checked, not assumed: no ☰, no ⚑, no ✎ in Literata, Charis SIL or IBM Plex Mono,
and a missing glyph is visible tofu. So the HUD reserves an empty box per control
and the window fills it with a kurbo path (`paint::Icon`): a pennant, three
rules, a marker, a back triangle. No font, no SVG plumbing, no asset, and it
takes the chrome's own colour.

Two things this turned up:

- **SVG only arrives as an image.** `svg` is a default feature of blitz-dom and
  blitz-paint, but inline `<svg>` markup is not parsed into a tree — SVG reaches
  a document as `Resource::Svg` through the net provider. Icons that way would
  mean an origin, a provider, async delivery and routing it to the HUD document.
- **An inline-block inside text has no Taffy box.** The first attempt reserved
  space correctly and painted nothing: `final_layout.size` was 0×0, because an
  inline-level box inside an inline formatting context lives in the parley layout
  as an inline box. Making each button `display: flex` gives the slot a real box.
  Worth remembering the next time something is laid out but unpaintable.

**Page numbers span the book, and are measured rather than estimated.** Two
estimates were built and both thrown away: chapter page counts alone put the same
book at 342 then 764 pages as you moved through it, and switching to *fractional*
pages (to stop a 200-byte chapter claiming a whole one) only moved the swing to
76–342. One chapter's density does not predict a 131-item spine where many items
are a paragraph long.

So `chapter::page_counts` lays out every chapter once — three seconds for that
book, images included, since they change how much fits — and the result is cached
in `pagination`, keyed by book *and* layout, because a page count means nothing
without a font size and a column width. It runs from the click that asks for page
numbers, never from a redraw. At a layout that has not been measured the readout
falls back to the chapter's own numbering, which is true, rather than an estimate
that is not. Stable at 393 pages from chapters 8, 60 and 120 of the same book.

**Removing a highlight is in the menu.** A press inside a stored highlight
remembers it, and the Highlight control becomes Remove — the useful offer once
the pointer is already inside one. `x` in the marks list still works.

**The bottom bar opens with a Library button**, back arrow first, then the title.

Next: AZW3/MOBI import and AccessKit — the last two items of §7. MOBI is the only
one needing a converter, so it is the odd one out and probably last.

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

**`align-items: baseline` puts a flex container's buttons at different heights.**
A flex container takes its baseline from its first item, and for the HUD buttons
carrying an icon that is an *empty* 11px box — so those sat a few pixels above
the plain-text ones and the two ends of the bar did not line up. `center` does
not consult baselines and does not care.

**A document with no doctype costs one `ERROR: Unexpected token` per parse.**
html5ever's tree builder calls `unexpected()` for the first tag in the Initial
insertion mode, and blitz-html prints every parse error with a bare `println!` —
to stdout, from inside the dependency, with no switch. The books were innocent:
their XHTML carries a doctype and parses silently. The noise was ours, and the
HUD generated most of it, because it is rebuilt on every page turn.

Every document Omaread authors now starts `<!DOCTYPE html>`, and there is a test
that says so for all three. Reading a book prints nothing; so does the library.
Layout is unchanged, because blitz builds its stylist with
`QuirksMode::NoQuirks` whatever the parser decided.

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

**`paint_scene` is generic over `impl PaintScene`, and that is the way out.**
The reset that blocked compositing (above) happens on *our* value: the trait is
public and implementable, so a wrapper that forwards every method and makes
`reset` a no-op lets a second document paint *over* the first instead of
erasing it. `paint::NoReset` is that wrapper, about forty lines of forwarding,
and the reading HUD is painted through it.

This retires the second half of the `paint_scene` note above, and **two-column
never needed an upstream release** — only this wrapper. Painting the page ground
and masks after the call is still required.

A *layer* transform moves the clip shape and not the content (below), but a
transform pre-multiplied into every drawing command does move the content, which
is what puts the second column on the right-hand side.

**A page is shorter than the page height, and the mask has to know it.** Breaks
snap *up* to an atom boundary, so a page ends short of its nominal height — 872
of 900 on a measured page. The margin mask was placed at `margin + page_height`
regardless, which left that 28px gap showing the *next* page's first line,
guillotined halfway down. It reads exactly like the last line being cut off,
which is how it was reported. Masking from `Pages::extent_of(page)` — the
distance to the next break — is the fix; nothing about the pagination changed,
only where the paper starts again.

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

**A table row band can be shorter than the line inside it.** Bands are derived
from cell boxes (`<tr>` has none, §9 above), and table layout sizes a row from
those boxes — but a line box can overflow its cell. Bundling Literata made lines
29px where the fallback face gave 26px, and cells sized at 27px no longer held
them. Every overflowing line then stuck out of its band as an atom the paginator
could neither break at nor snap past, and a page ran out of legal breaks
entirely: one hard cut through a line, in *The Well-Grounded Rubyist* ch.5.

The fix is two steps, and both are needed:

- **Absorb**, don't just drop. Every atom found inside a table is folded into
  the row band it overlaps most, *growing* the band to cover it. The old rule
  dropped only atoms already fully contained, which is the same thing whenever
  the geometry is exact and no help at all when it isn't. Content inside a table
  but in no row — a `<caption>` — stays an atom of its own.
- **Then separate.** Growing pushes a band's bottom past the next band's top,
  and a run of those turns a table into one unbroken forbidden span: thirteen
  chained bands across 1022px of a 900px page, which is a forced cut. The later
  band yields, because the overlap is text from the earlier row hanging into the
  next row's box and the break belongs *below* that text.

Both steps are pure functions with unit tests (`absorb_into_bands`,
`separate_bands`). This is the second time floating adjacency inside a table has
cost a hard cut, and the second time the sweep found it rather than the eye.

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
  theme files ourselves. **Done**, in `assets/omarchy/`. Omarchy renders every
  `~/.config/omarchy/themed/*.tpl` into
  `~/.local/state/omarchy/current/theme/`, so the read side is one file and four
  custom properties.
- Ship a suggested Hyprland window rule and keybinding. **Done**, same
  directory. The window sets `app_id = omaread`; without it the class is empty
  and no rule matches.
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

## 12b. Measured performance

Numbers from this machine, release build, 372-book library. `OMAREAD_DEBUG_TIME=1`
prints frame, chapter and grid timings; `OMAREAD_EXIT_AFTER=<ms>` makes
start-to-ready measurable from a script.

| | |
|---|---|
| Ready to read a book | **0.93–1.03s** |
| Ready to the library | **1.05–1.09s** |
| Frame (page turn within a chapter) | **1–5ms** |
| Chapter load + paginate, prose | **23–42ms** |
| Chapter load + paginate, Géron ch.9 (156 images) | **114ms** |
| Full grid rebuild, 361 rows | **233–249ms** |

Two things this found and fixed:

- **The library rebuilt on every arrow key.** Selection was a `.selected` class
  in the markup, so moving it rebuilt the document — which re-requests every
  cover: 1781ms per keypress. The selection is now *painted* as an outline
  (`paint::outline`), so arrows cost a frame. An outline also does not tint the
  cover underneath. `data-index` stays in the markup because that is how the
  window finds the card to outline.
- **The first chapter was laid out twice at startup.** `open_path` loaded it at a
  guessed window size, then `resumed` loaded it again at the real one. Skipped
  when there is no window yet — 114ms off the start of an illustrated book.

What the measurements say to leave alone: the reading view is fine. Frames are
1–5ms even though highlights are re-queried from SQLite every frame, so that
`ponytail:` note stays a note.

**The library loaded 358 covers to show about fourteen.** Isolated by building
the grid with the cover provider removed: **1817ms with covers, 91ms without** —
95% of the cost, for images nobody could see.

So the grid is built twice. The first pass carries no covers and exists only to
find out which cards this page shows (`visible_cards`, off one walk collecting
`data-index` tops — asking `find_by_attr` per card would be quadratic); the
second gives those cards their covers. Everything off the page keeps the title
jacket it already had for coverless books. A page turn rebuilds, because a
different page shows different cards.

**A card must be the same box whether or not it shows a cover.** Loading covers
only for the visible page needs to know which cards are visible, which is decided
by pagination — and a cover is an `<img>`, which `collect_atoms` makes an
unbreakable block, while a missing cover used to be a text jacket, which a page
break may fall inside. So the page fitted two rows of jackets where it fits one
row of covers, the measuring pass disagreed with the real grid, and the grid
rebuilt itself forever: 33 rebuilds and climbing.

Every card now carries an `<img>`, with or without a `src`. The title jacket is
gone; the title was always printed under the card anyway. The measuring pass uses
the *same* markup as the real grid and simply has no provider to fetch through,
so the two paginate identically and the answer is stable. Two rebuilds over eight
seconds — the first build and the resize the compositor does after mapping — and
then it settles.

Two smaller things the same hunt turned up: `.card` had no `flex-shrink: 0`, and
the mouse wheel called the *reading* page turn in every view, so scrolling the
library crossed chapters and saved a reading position. There is now a test that
lays the grid out at five window widths and asserts the row holds exactly what
`grid::per_row` claims — the arithmetic and the stylesheet agreeing is the whole
basis for arrow-key navigation, and asserting it against the arithmetic alone
proves nothing.

That is what the **Library button needing four presses** actually was. The click
always worked: a press matched `data-hud="library"` first time. But
`to_library()` then blocked the event loop for nearly two seconds with no
repaint, so the presses that arrived meanwhile queued up and were delivered
*after* the switch — in the library, where a click opens whatever card is under
the pointer, putting you straight back in the book. Measure before believing a
report about clicks.

Still unfixed and now much smaller: covers are decoded at whatever size the
publisher shipped. Downscaling at import (and §4's decoded LRU) would take the
remaining ~140ms down again; noted at `net::CoverProvider`.

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
