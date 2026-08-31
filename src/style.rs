//! The base stylesheet — Omaread's typography, supplied as a UA stylesheet.
//!
//! Under the whitelist CSS policy (CONTEXT.md §3) the publisher's stylesheets are
//! stripped, not cascaded. This file is therefore what every book looks like.

/// Text measure, in em. ~66 characters. Fixed by design, not a user setting.
pub const MEASURE_EM: f32 = 33.0;

/// Breathing room either side of the measure, in em. Also the minimum gutter
/// when the window is narrower than the measure.
pub const GUTTER_EM: f32 = 1.75;

/// Vertical margin above and below every page.
///
/// This belongs to the *page*, not the document: a page is a slice of a
/// continuous flow, so padding on `body` would only inset the first and last
/// page and leave every page in between running to the window edge.
pub const PAGE_MARGIN_EM: f32 = 2.5;

/// Reading themes. Paper decisions, deliberately independent of the desktop theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    White,
    Sepia,
    Grey,
    Night,
}

impl Theme {
    /// Page background as linear RGB components, for painting the window ground.
    pub fn background_rgb(self) -> [u8; 3] {
        match self {
            Theme::White => [0xfd, 0xfd, 0xfe],
            Theme::Sepia => [0xf8, 0xf1, 0xe3],
            Theme::Grey => [0xd6, 0xd6, 0xd2],
            Theme::Night => [0x14, 0x14, 0x16],
        }
    }

    /// Palette for the app chrome — library grid, panels.
    ///
    /// Off Omarchy this is the reader's own neutral ground. Following the
    /// active Omarchy theme here (CONTEXT.md §11) is the integration still to
    /// come; the *reading surface* deliberately keeps its own four themes.
    pub fn chrome_colors(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Theme::White => ("#fbfbfd", "#1c1c1e", "#8e8e93", "#ececf0"),
            Theme::Sepia => ("#f6efe1", "#4f321c", "#96785f", "#e8ddc6"),
            Theme::Grey => ("#d6d6d2", "#33332f", "#6e6e6a", "#c4c4bf"),
            Theme::Night => ("#111113", "#e5e5ea", "#6e6e73", "#232327"),
        }
    }

    /// (background, text, subtle)
    fn colors(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Theme::White => ("#fdfdfe", "#1c1c1e", "#8e8e93"),
            Theme::Sepia => ("#f8f1e3", "#4f321c", "#96785f"),
            Theme::Grey => ("#d6d6d2", "#33332f", "#6e6e6a"),
            Theme::Night => ("#141416", "#e5e5ea", "#6e6e73"),
        }
    }
}

/// Reading surface parameters. Measure is fixed by design; only these three are
/// user-facing (CONTEXT.md §3).
pub struct ReadingStyle {
    pub theme: Theme,
    /// Multiplier on the 20px base. Clamped to 0.8..=1.6.
    pub scale: f32,
}

impl Default for ReadingStyle {
    fn default() -> Self {
        Self { theme: Theme::Sepia, scale: 1.0 }
    }
}

impl ReadingStyle {
    pub fn font_px(&self) -> f32 {
        20.0 * self.scale.clamp(0.8, 1.6)
    }

    /// The UA stylesheet for the reading surface.
///
/// Every selector is prefixed with `html` on purpose. blitz-dom applies its own
/// default UA sheet *after* this one, so a bare `body {{ }}` here loses to its
/// `body {{ margin: 8px }}` and a bare `p {{ }}` loses to its `p {{ margin: 1em 0 }}`.
/// The descendant prefix raises specificity just enough to win.
    pub fn stylesheet(&self) -> String {
        let (bg, fg, subtle) = self.theme.colors();
        let px = self.font_px();
        let gutter = GUTTER_EM;
        let measure = MEASURE_EM + 2.0 * GUTTER_EM;

        format!(
            r#"
html {{
  box-sizing: border-box;
  font-family: "Literata", "Charis SIL", serif;
  font-size: {px}px;
  line-height: 1.6;
  background: {bg};
  color: {fg};
}}
*, *::before, *::after {{ box-sizing: inherit; }}

html body {{
  /* The measure is 33em of text; border-box means the gutters live inside the
     max-width, so the column is exactly 33em wide however the padding changes.
     `margin: 0 auto` centres it in one-column mode at every window width. */
  max-width: {measure}em;
  margin: 0 auto;
  /* Horizontal only. Vertical breathing room is the page's job, not the
     document's — see PAGE_MARGIN_EM. */
  padding: 0 {gutter}em;
  text-align: justify;
  hyphens: auto;
}}

/* Editorial convention: indent, no inter-paragraph gap. */
p {{ margin: 0; text-indent: 1.2em; }}
h1 + p, h2 + p, h3 + p, h4 + p, h5 + p, h6 + p,
blockquote + p, hr + p, p:first-child {{ text-indent: 0; }}

html h1,
html h2,
html h3,
html h4,
html h5,
html h6 {{
  text-align: left;
  hyphens: none;
  font-weight: 600;
  margin: 2em 0 0.6em 0;
  line-height: 1.25;
}}
h1 {{ font-size: 1.5em; }}
h2 {{ font-size: 1.3em; }}
h3 {{ font-size: 1.15em; }}
h4, h5, h6 {{ font-size: 1em; }}

blockquote {{ margin: 1em 2em; font-size: 0.95em; }}
blockquote p {{ text-indent: 0; }}

/* No sideways in a paginated reader: code wraps, never scrolls. */
pre, code, kbd, samp {{ font-family: "IBM Plex Mono", monospace; }}
html pre {{
  white-space: pre-wrap;
  overflow-wrap: break-word;
  font-size: 0.9em;
  line-height: 1.45;
  text-align: left;
  hyphens: none;
  padding: 0.8em 1em;
  margin: 1.2em 0;
  border-radius: 6px;
  background: rgba(127, 127, 127, 0.12);
}}
code {{ font-size: 0.9em; }}

html img,
html svg {{
  display: block;
  margin: 1.2em auto;
  max-width: 100%;
  max-height: 85vh;
}}
figure {{ margin: 1.4em 0; }}
html figcaption {{
  font-size: 0.85em;
  color: {subtle};
  text-align: center;
  text-indent: 0;
  hyphens: none;
}}

ul, ol {{ margin: 1em 0; padding-left: 1.6em; }}
li {{ text-indent: 0; margin: 0.3em 0; }}

html table {{
  border-collapse: collapse;
  margin: 1.4em 0;
  font-size: 0.9em;
  text-align: left;
  hyphens: none;
}}
html th,
html td {{
  border-bottom: 1px solid rgba(127, 127, 127, 0.3);
  padding: 0.45em 0.7em;
  text-indent: 0;
}}
th {{ font-weight: 600; }}

a {{ color: inherit; text-decoration-color: {subtle}; }}
html hr {{
  border: 0;
  border-top: 1px solid rgba(127, 127, 127, 0.3);
  margin: 2em auto;
  width: 30%;
}}
sup, sub {{ line-height: 0; }}
"#
        )
    }
}

/// Strip the publisher's CSS.
///
/// This *is* the whitelist policy (CONTEXT.md §3), and it also removes the
/// commonest cause of blitz-dom panicking on real books: a `<link>` or `<style>`
/// whose href cannot be resolved.
pub fn strip_publisher_css(html: &str) -> String {
    let without_style = drop_spans(html, "<style", "</style>");
    drop_link_elements(&without_style)
}

fn drop_link_elements(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("<link") {
        out.push_str(&rest[..i]);
        match rest[i..].find('>') {
            Some(j) => rest = &rest[i + j + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Drop every `open..close` span, inclusive of the delimiters.
fn drop_spans(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(close) {
            Some(j) => rest = &rest[i + j + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// App chrome as CSS colour strings: (background, foreground, subtle, panel).
pub type Chrome = (String, String, String, String);

/// Where Omarchy leaves the palette it rendered for us.
const OMARCHY_CSS: &str = ".local/state/omarchy/current/theme/omaread.css";

/// The chrome palette Omarchy rendered, if there is one.
///
/// Omarchy renders every `~/.config/omarchy/themed/*.tpl` into the active theme
/// directory on a theme change, so the whole integration is a file read and four
/// custom properties (CONTEXT.md §11). Nothing here parses a theme, and nothing
/// has to track upstream's format. `None` off Omarchy, or when the template is
/// not installed — the reader then uses its own palette.
pub fn omarchy_chrome() -> Option<Chrome> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(OMARCHY_CSS);
    parse_chrome(&std::fs::read_to_string(path).ok()?)
}

/// Pull `--bg`, `--fg`, `--subtle` and `--panel` out of a rendered stylesheet.
///
/// Anything not a `#rrggbb` is rejected rather than passed on: a half-rendered
/// template would otherwise paint the whole library black, since that is what an
/// unparseable colour becomes.
fn parse_chrome(css: &str) -> Option<Chrome> {
    let value = |name: &str| -> Option<String> {
        let key = format!("--{name}:");
        let at = css.find(&key)? + key.len();
        let raw = css[at..].split(';').next()?.trim();
        let hex = raw.len() == 7
            && raw.starts_with('#')
            && raw[1..].chars().all(|c| c.is_ascii_hexdigit());
        hex.then(|| raw.to_string())
    };
    Some((value("bg")?, value("fg")?, value("subtle")?, value("panel")?))
}

#[cfg(test)]
mod tests {
    /// The rendered template is the whole Omarchy integration, so its parse is
    /// the thing that must not quietly do the wrong thing.
    #[test]
    fn the_omarchy_palette_is_read_or_refused() {
        let good = ":root {\n  --bg: #2e3440;\n  --fg: #d8dee9;\n\
                    --subtle: #4c566a;\n  --panel: #434c5e;\n}\n";
        assert_eq!(
            super::parse_chrome(good),
            Some((
                "#2e3440".to_string(),
                "#d8dee9".to_string(),
                "#4c566a".to_string(),
                "#434c5e".to_string(),
            ))
        );

        // A key that merely starts the same must not be mistaken for `--bg`.
        assert_eq!(super::parse_chrome("--background: #2e3440;"), None);
        // Missing one property is not a palette.
        assert_eq!(super::parse_chrome("--bg: #2e3440; --fg: #fff;"), None);
        // An unrendered template would otherwise become black.
        assert_eq!(
            super::parse_chrome(
                "--bg: {{ background }}; --fg: #d8dee9; --subtle: #4c566a; --panel: #434c5e;"
            ),
            None
        );
    }

    use super::*;

    #[test]
    fn strips_style_and_link() {
        let html = r#"<html><head>
            <link rel="stylesheet" href="stylesheet.css"/>
            <style>p { color: red }</style>
            <title>keep</title></head><body><p>text</p></body></html>"#;
        let out = strip_publisher_css(html);
        assert!(!out.contains("<link"), "link survived: {out}");
        assert!(!out.contains("<style"), "style survived: {out}");
        assert!(!out.contains("color: red"), "css body survived: {out}");
        assert!(out.contains("<title>keep</title>"));
        assert!(out.contains("<p>text</p>"));
    }

    #[test]
    fn survives_unterminated_markup() {
        assert!(!strip_publisher_css("<p>a</p><style>oops").contains("oops"));
        assert!(!strip_publisher_css("<p>a</p><link rel=x").contains("<link"));
    }

    #[test]
    fn font_scale_is_clamped() {
        let s = ReadingStyle { theme: Theme::White, scale: 99.0 };
        assert_eq!(s.font_px(), 32.0);
        let s = ReadingStyle { theme: Theme::White, scale: 0.01 };
        assert_eq!(s.font_px(), 16.0);
    }

    #[test]
    fn stylesheet_reflects_theme_and_scale() {
        let s = ReadingStyle { theme: Theme::Night, scale: 1.2 };
        let css = s.stylesheet();
        assert!(css.contains("#141416"), "night background missing");
        assert!(css.contains("font-size: 24px"), "scale not applied");
    }
}
