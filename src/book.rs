//! EPUB container access. Everything that reads from the .epub goes through here.

use rbook::Epub;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One navigable line in the table of contents.
#[derive(Clone, Debug, PartialEq)]
pub struct TocEntry {
    pub label: String,
    /// Nesting level, 0 for top-level entries.
    pub depth: usize,
    /// Spine item the entry lands in.
    pub spine: usize,
    /// `id` within that chapter, taken from the href fragment. Plenty of books
    /// hang a whole TOC off one spine file; without this every entry opens on
    /// page 1 of it.
    pub fragment: Option<String>,
}

/// An open book. Cheap to clone; the archive is shared.
///
/// Note on the href space: rbook yields absolute, slash-prefixed archive paths
/// (`/OEBPS/ch04.html`) and `read_resource_*` accepts either those or paths
/// relative to the OPF directory. A bare `OEBPS/x.png` is read as OPF-relative
/// and resolves to `/OEBPS/OEBPS/x.png`, so the leading slash must be preserved
/// end to end.
#[derive(Clone)]
pub struct Book {
    inner: Arc<Mutex<Epub>>,
    /// Spine hrefs, in reading order, relative to the archive root.
    spine: Arc<Vec<String>>,
    pub title: String,
    /// `dc:language`, used to pick hyphenation patterns.
    pub language: String,
    /// Flattened navigation, in document order. Never empty — see `read_toc`.
    pub toc: Arc<Vec<TocEntry>>,
}

impl Book {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let epub = Epub::open(path.as_ref()).map_err(|e| format!("open epub: {e}"))?;

        let title = epub
            .metadata()
            .title()
            .map(|t| t.value().to_string())
            .unwrap_or_else(|| {
                path.as_ref()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".into())
            });

        let language = epub
            .metadata()
            .language()
            .map(|l| l.value().to_string())
            .unwrap_or_default();

        let spine: Vec<String> = epub
            .spine()
            .iter()
            .filter_map(|entry| entry.manifest_entry())
            .map(|entry| entry.href().as_str().to_string())
            .collect();

        if spine.is_empty() {
            return Err("epub has an empty spine".into());
        }

        let toc = read_toc(&epub, &spine);

        Ok(Self {
            inner: Arc::new(Mutex::new(epub)),
            spine: Arc::new(spine),
            title,
            language,
            toc: Arc::new(toc),
        })
    }

    pub fn chapter_count(&self) -> usize {
        self.spine.len()
    }

    /// Href of the nth spine item, archive-root-relative.
    pub fn chapter_href(&self, index: usize) -> Option<&str> {
        self.spine.get(index).map(String::as_str)
    }

    /// Raw XHTML of the nth spine item.
    pub fn chapter_html(&self, index: usize) -> Result<String, String> {
        let href = self
            .chapter_href(index)
            .ok_or_else(|| format!("no spine item {index}"))?
            .to_owned();
        self.read_str(&href)
    }

    pub fn read_str(&self, href: &str) -> Result<String, String> {
        let epub = self.inner.lock().map_err(|_| "book lock poisoned")?;
        epub.read_resource_str(href)
            .map_err(|e| format!("read {href}: {e}"))
    }

    pub fn read_bytes(&self, href: &str) -> Result<Vec<u8>, String> {
        let epub = self.inner.lock().map_err(|_| "book lock poisoned")?;
        epub.read_resource_bytes(href)
            .map_err(|e| format!("read {href}: {e}"))
    }
}

/// Flatten the EPUB 3 `nav` (or the EPUB 2 NCX — rbook falls back on its own)
/// into a list of spine targets.
///
/// Entries that do not resolve to a spine item are dropped: there is nowhere to
/// navigate to, and showing a dead line is worse than showing none. A book with
/// no usable navigation at all falls back to its spine, so the contents key
/// always does something.
fn read_toc(epub: &Epub, spine: &[String]) -> Vec<TocEntry> {
    let mut out = Vec::new();

    if let Some(root) = epub.toc().contents() {
        for entry in root.flatten() {
            let label = entry.label().split_whitespace().collect::<Vec<_>>().join(" ");
            if label.is_empty() {
                continue;
            }
            // `manifest_entry` looks the target up by path, so a fragment does
            // not stop it resolving; the fragment itself is read off the href.
            let Some(href) = entry.manifest_entry().map(|m| m.href().as_str().to_string())
            else {
                continue;
            };
            let Some(index) = spine.iter().position(|s| *s == href) else {
                continue;
            };
            out.push(TocEntry {
                label,
                depth: entry.depth(),
                spine: index,
                fragment: entry
                    .href()
                    .and_then(|h| h.fragment())
                    .map(str::to_string),
            });
        }
    }

    // rbook counts depth from the nav root, so the top level is 1 in some books
    // and deeper in others. Indentation is relative; rebase it on the shallowest
    // entry present.
    if let Some(base) = out.iter().map(|e| e.depth).min() {
        for e in &mut out {
            e.depth -= base;
        }
    }

    if out.is_empty() {
        out = (0..spine.len())
            .map(|i| TocEntry {
                label: format!("Chapter {}", i + 1),
                depth: 0,
                spine: i,
                fragment: None,
            })
            .collect();
    }
    out
}

