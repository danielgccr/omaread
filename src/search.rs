//! Full-text search: getting text out of a chapter, and queries into FTS5.
//!
//! The text has to be extracted anyway for CFI resolution (CONTEXT.md §4), and
//! one FTS5 index over it answers both "find this in this book" and "find this
//! anywhere in the library" — the second being the thing Apple Books does badly
//! and that is nearly free here.
//!
//! Extraction reads the raw XHTML rather than a laid-out document on purpose:
//! indexing must not cost a full Stylo/Taffy/Parley pass per chapter.

/// Elements whose text is not prose and must not be indexed.
const SKIP: [&str; 4] = ["script", "style", "head", "title"];

/// Plain text of a chapter's XHTML, whitespace collapsed.
///
/// A tag scanner, not a parser. It is wrong on `<` inside an attribute value,
/// which is illegal in XML anyway, and it does not care about nesting beyond
/// the skip list. For feeding a tokeniser that is enough.
pub fn text_of_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;

    while let Some(lt) = rest.find('<') {
        push_text(&mut out, &rest[..lt]);
        let tail = &rest[lt..];

        // Comments and CDATA are not tags and may contain '>' freely.
        if let Some(after) = skip_bracketed(tail) {
            rest = after;
            continue;
        }

        // An unterminated tag at the end of the input is markup, not prose;
        // pushing it as text would put `<p class="x` in the index.
        let Some(gt) = tail.find('>') else { return out.trim_end().to_string() };
        let tag = &tail[..=gt];
        rest = &tail[gt + 1..];

        let Some(name) = tag_name(tag) else { continue };

        // Inside `<script>` the content is not markup: `1 < 2` would otherwise
        // look like a tag and swallow the rest of the chapter looking for its
        // '>'. So jump to the close tag by name instead of counting depth.
        if SKIP.contains(&name.as_str()) && !tag.starts_with("</") && !tag.ends_with("/>") {
            rest = skip_element(rest, &name);
            push_space(&mut out);
            continue;
        }
        if is_break(&name) {
            push_space(&mut out);
        }
    }
    push_text(&mut out, rest);
    out.trim_end().to_string()
}

/// Everything after this element's close tag; the rest of the input if it never
/// closes, which is a malformed chapter and not worth indexing past.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let lower = rest.to_ascii_lowercase();
    let close = format!("</{name}");
    match lower.find(&close) {
        Some(at) => match rest[at..].find('>') {
            Some(gt) => &rest[at + gt + 1..],
            None => "",
        },
        None => "",
    }
}

/// `<!-- ... -->`, `<![CDATA[ ... ]]>` and `<!DOCTYPE ...>`: returns what
/// follows, or `None` if this is an ordinary tag.
fn skip_bracketed(tail: &str) -> Option<&str> {
    for (open, close) in [("<!--", "-->"), ("<![CDATA[", "]]>")] {
        if let Some(body) = tail.strip_prefix(open) {
            return Some(body.find(close).map_or("", |i| &body[i + close.len()..]));
        }
    }
    None
}

/// Elements that separate words: without this, `<p>one</p><p>two</p>` indexes
/// as "onetwo" and neither word is findable.
fn is_break(name: &str) -> bool {
    !matches!(
        name,
        "a" | "b" | "i" | "em" | "strong" | "span" | "small" | "sub" | "sup" | "u" | "abbr"
    )
}

fn tag_name(tag: &str) -> Option<String> {
    let body = tag.trim_start_matches('<').trim_start_matches('/');
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn push_space(out: &mut String) {
    if !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
}

fn push_text(out: &mut String, raw: &str) {
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        push_ws(out, &rest[..amp]);
        let tail = &rest[amp..];
        match tail.find(';').filter(|&i| i <= 10) {
            Some(end) => {
                out.push_str(&entity(&tail[1..end]));
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    push_ws(out, rest);
}

/// Whitespace is collapsed to single spaces: the index has no use for the
/// publisher's indentation, and it doubles the size of every row.
fn push_ws(out: &mut String, raw: &str) {
    for c in raw.chars() {
        if c.is_whitespace() {
            push_space(out);
        } else {
            out.push(c);
        }
    }
}

fn entity(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        "nbsp" => " ".into(),
        "mdash" => "—".into(),
        "ndash" => "–".into(),
        "hellip" => "…".into(),
        "shy" => String::new(),
        n => n
            .strip_prefix('#')
            .and_then(|d| match d.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => d.parse().ok(),
            })
            .and_then(char::from_u32)
            .map(String::from)
            // Not an entity we know: leave it as written rather than eat it.
            .unwrap_or_else(|| format!("&{n};")),
    }
}

/// Turn what someone typed into an FTS5 MATCH expression.
///
/// User input is never an FTS expression: a bare `"` or a stray `NEAR` is a
/// syntax error, and an error here would look like "no results". Every token is
/// quoted, which strips all operator meaning, and the last one gets a `*` so
/// results appear while you are still typing.
///
/// Returns `None` when there is nothing to search for.
pub fn fts_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| if i == last { format!("\"{t}\"*") } else { format!("\"{t}\"") })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Lowercase and strip the accents Spanish and Catalan actually use.
///
/// FTS5 folds diacritics when it matches, so a hit found by searching "cancion"
/// has to be findable again inside the laid-out chapter or the jump lands at the
/// top of the chapter instead of on the word.
///
/// ponytail: a hand-written table for en/es/ca, the languages §12 commits to.
/// Swap in real Unicode NFD stripping if the UI ever grows past them.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ý' | 'ÿ' => 'y',
            '\u{00ad}' => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_become_word_breaks_and_inline_tags_do_not() {
        assert_eq!(text_of_html("<p>one</p><p>two</p>"), "one two");
        assert_eq!(text_of_html("<p>sur<em>real</em>ism</p>"), "surrealism");
        assert_eq!(text_of_html("a<br/>b"), "a b");
    }

    #[test]
    fn script_style_and_comments_are_not_prose() {
        assert_eq!(text_of_html("<p>a</p><script>var x = 1 < 2;</script><p>b</p>"), "a b");
        assert_eq!(text_of_html("<style>p{color:red}</style><p>c</p>"), "c");
        assert_eq!(text_of_html("<p>a<!-- <b>hidden</b> -->b</p>"), "ab");
        assert_eq!(text_of_html("<p><![CDATA[raw <stuff>]]>x</p>"), "x");
    }

    #[test]
    fn entities_and_whitespace_are_normalised() {
        assert_eq!(text_of_html("<p>caf&#233; &amp; cr&#xE8;me</p>"), "café & crème");
        assert_eq!(text_of_html("<p>a\n\n   \t b</p>"), "a b");
        // Soft hyphens are inserted by us and must not reach the index.
        assert_eq!(text_of_html("<p>ma&shy;ñana</p>"), "mañana");
        // An entity we do not know survives instead of eating the text.
        assert_eq!(text_of_html("<p>&frac12; x</p>"), "&frac12; x");
    }

    #[test]
    fn unterminated_markup_does_not_lose_everything() {
        assert_eq!(text_of_html("<p>kept</p><p class=\"x"), "kept");
        assert_eq!(text_of_html("plain, no tags"), "plain, no tags");
    }

    /// The soft hyphens we insert must not stop a search from matching, and an
    /// unaccented query has to find the accented word.
    #[test]
    fn folding_matches_how_people_type() {
        assert_eq!(fold("Canción"), "cancion");
        assert_eq!(fold("MAÑANA"), "manana");
        assert_eq!(fold("Coneixença"), "coneixenca");
        assert!(fold("ma\u{00ad}ñana").contains("ma nana"));
    }

    /// A query is user input at a trust boundary: FTS5 syntax errors read as
    /// "nothing found", which is the worst possible failure here.
    #[test]
    fn queries_are_quoted_and_prefix_matched() {
        assert_eq!(fts_query("resonancia"), Some("\"resonancia\"*".into()));
        assert_eq!(fts_query("la metáfora"), Some("\"la\" \"metáfora\"*".into()));
        // Operators and punctuation carry no meaning.
        assert_eq!(fts_query("NEAR(a b)"), Some("\"NEAR\" \"a\" \"b\"*".into()));
        assert_eq!(fts_query("\"quoted\""), Some("\"quoted\"*".into()));
        assert_eq!(fts_query("a AND* b"), Some("\"a\" \"AND\" \"b\"*".into()));
        assert_eq!(fts_query("   "), None);
        assert_eq!(fts_query("!!!"), None);
    }
}
