//! EPUB container access. Everything that reads from the .epub goes through here.

use rbook::Epub;
use std::path::Path;
use std::sync::{Arc, Mutex};

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

        let spine: Vec<String> = epub
            .spine()
            .iter()
            .filter_map(|entry| entry.manifest_entry())
            .map(|entry| entry.href().as_str().to_string())
            .collect();

        if spine.is_empty() {
            return Err("epub has an empty spine".into());
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(epub)),
            spine: Arc::new(spine),
            title,
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

