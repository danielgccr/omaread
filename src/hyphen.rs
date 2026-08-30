//! Soft-hyphen insertion.
//!
//! Parley justifies (`Alignment::Justify`) but has no hyphenation, so
//! unhyphenated justified Spanish and Catalan open rivers of whitespace down the
//! page. Knuth-Liang patterns give the break points; `U+00AD` marks them, and
//! Parley's UAX-14 line breaker treats soft hyphen as a break opportunity
//! (class BA). That is the whole mechanism — a text-preparation step, not an
//! engine change (CONTEXT.md §9).

use hyphenation::{Hyphenator, Language, Load, Standard};

pub const SOFT: char = '\u{00ad}';

/// Words shorter than this are left alone: breaking them saves nothing and
/// looks worse.
const MIN_WORD: usize = 5;

pub struct Hyphenator_ {
    dict: Standard,
}

impl Hyphenator_ {
    /// Load patterns for a `dc:language` tag. Unknown or unsupported languages
    /// get no hyphenation rather than the wrong language's rules.
    pub fn for_language(tag: &str) -> Option<Self> {
        let lang = language_of(tag)?;
        Standard::from_embedded(lang).ok().map(|dict| Self { dict })
    }

    /// Insert soft hyphens into every word of a plain-text run.
    pub fn mark(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + text.len() / 8);
        let mut word = String::new();

        for c in text.chars() {
            if c.is_alphabetic() || c == '\u{2019}' || c == '\'' {
                word.push(c);
            } else {
                self.flush(&mut word, &mut out);
                out.push(c);
            }
        }
        self.flush(&mut word, &mut out);
        out
    }

    fn flush(&self, word: &mut String, out: &mut String) {
        if word.is_empty() {
            return;
        }
        if word.chars().count() < MIN_WORD {
            out.push_str(word);
            word.clear();
            return;
        }
        let marked = self.dict.hyphenate(word);
        let mut last = 0;
        for b in marked.breaks.iter().copied() {
            out.push_str(&word[last..b]);
            out.push(SOFT);
            last = b;
        }
        out.push_str(&word[last..]);
        word.clear();
    }
}

/// Map a BCP-47 tag onto an embedded pattern set. Only the languages the test
/// library actually contains are wired up; the rest fall through to no
/// hyphenation, which is correct-but-loose rather than wrong.
fn language_of(tag: &str) -> Option<Language> {
    let primary = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    Some(match primary.as_str() {
        "en" => Language::EnglishUS,
        "es" => Language::Spanish,
        "ca" => Language::Catalan,
        "pt" => Language::Portuguese,
        "fr" => Language::French,
        "de" => Language::German1996,
        "it" => Language::Italian,
        _ => return None,
    })
}

/// Insert soft hyphens into the text of an HTML document, leaving markup alone.
///
/// ponytail: a scan over the HTML string rather than a DOM pass. Text is
/// hyphenated only outside tags, only in elements where hyphenation makes sense,
/// and only in runs of plain letters — so entities (`&amp;`), attribute values
/// and code are never touched. A DOM-level pass would be tidier but needs the
/// text put back through the parser anyway.
pub fn mark_html(html: &str, h: &Hyphenator_) -> String {
    /// Elements whose text must never be hyphenated.
    const SKIP: [&str; 5] = ["pre", "code", "script", "style", "kbd"];

    let mut out = String::with_capacity(html.len() + html.len() / 8);
    let mut rest = html;
    let mut skip_depth = 0usize;

    while let Some(lt) = rest.find('<') {
        let (text, tail) = rest.split_at(lt);

        if skip_depth == 0 {
            out.push_str(&hyphenate_text(text, h));
        } else {
            out.push_str(text);
        }

        let Some(gt) = tail.find('>') else {
            out.push_str(tail);
            return out;
        };
        let tag = &tail[..=gt];
        out.push_str(tag);
        rest = &tail[gt + 1..];

        let name = tag_name(tag);
        if let Some(name) = name {
            let closing = tag.starts_with("</");
            let self_closing = tag.ends_with("/>");
            if SKIP.contains(&name.as_str()) && !self_closing {
                if closing {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
            }
        }
    }

    if skip_depth == 0 {
        out.push_str(&hyphenate_text(rest, h));
    } else {
        out.push_str(rest);
    }
    out
}

/// Hyphenate a text run, skipping anything containing an entity so `&amp;`
/// cannot be split down the middle.
fn hyphenate_text(text: &str, h: &Hyphenator_) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    text.split_inclusive(';')
        .map(|piece| {
            if piece.contains('&') {
                piece.to_string()
            } else {
                h.mark(piece)
            }
        })
        .collect()
}

fn tag_name(tag: &str) -> Option<String> {
    let inner = tag.trim_start_matches('<').trim_start_matches('/');
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn es() -> Hyphenator_ {
        Hyphenator_::for_language("es").expect("Spanish patterns")
    }

    #[test]
    fn breaks_spanish_words() {
        let h = es();
        let out = h.mark("predicadora");
        assert!(out.contains(SOFT), "no break points in {out:?}");
        // The word itself must survive intact once the marks are removed.
        assert_eq!(out.replace(SOFT, ""), "predicadora");
    }

    #[test]
    fn short_words_are_left_alone() {
        let h = es();
        for w in ["de", "la", "los", "casa"] {
            assert_eq!(h.mark(w), w, "{w} was hyphenated");
        }
    }

    #[test]
    fn punctuation_and_spacing_survive() {
        let h = es();
        let src = "«Los dunkers», oficialmente denominado —Iglesia—.";
        let out = h.mark(src);
        assert_eq!(out.replace(SOFT, ""), src);
    }

    #[test]
    fn unknown_languages_get_nothing() {
        assert!(Hyphenator_::for_language("ja").is_none());
        assert!(Hyphenator_::for_language("").is_none());
        assert!(Hyphenator_::for_language("es-ES").is_some(), "region tags should work");
    }

    #[test]
    fn markup_is_never_touched() {
        let h = es();
        let html = r#"<p class="predicadora">predicadora evangelista</p>"#;
        let out = mark_html(html, &h);
        assert!(
            out.contains(r#"class="predicadora""#),
            "attribute was hyphenated: {out}"
        );
        assert!(out.contains(SOFT), "body text was not hyphenated");
        assert_eq!(out.replace(SOFT, ""), html);
    }

    #[test]
    fn code_and_pre_are_skipped() {
        let h = es();
        let html = "<p>predicadora</p><pre>predicadora ejemplo</pre><p>predicadora</p>";
        let out = mark_html(html, &h);
        let pre = &out[out.find("<pre>").unwrap()..out.find("</pre>").unwrap()];
        assert!(!pre.contains(SOFT), "pre was hyphenated: {pre:?}");
        assert_eq!(out.matches(SOFT).count() > 0, true);
        assert_eq!(out.replace(SOFT, ""), html);
    }

    #[test]
    fn entities_are_not_split() {
        let h = es();
        let html = "<p>predicadora &amp; evangelista &#8212; final</p>";
        let out = mark_html(html, &h);
        assert!(out.contains("&amp;"), "entity broken: {out}");
        assert!(out.contains("&#8212;"), "numeric entity broken: {out}");
        assert_eq!(out.replace(SOFT, ""), html);
    }

    #[test]
    fn unterminated_markup_does_not_lose_text() {
        let h = es();
        let html = "<p>predicadora <em>evangelista";
        let out = mark_html(html, &h);
        assert_eq!(out.replace(SOFT, ""), html);
    }
}
