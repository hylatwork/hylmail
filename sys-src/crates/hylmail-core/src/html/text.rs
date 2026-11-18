//! Turning HTML into text, and text back into HTML.
//!
//! Three jobs live here:
//!
//! * [`html_to_text`] produces the plain text alternative of a message that
//!   only carried HTML.  A "text/plain" version that still contains tags is
//!   worse than none, so this is applied whenever the sender did not provide
//!   one.
//! * [`make_snippet`] builds the one-line preview of the message list from
//!   either representation.
//! * [`plain_to_html`] and [`quote_reply`] are the composer's direction: they
//!   escape user input and lay out a reply with the quoted original.

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{Html, Selector};

/// Maximum length of a stored snippet.
pub const SNIPPET_LENGTH: usize = 200;

/// Block level elements that end a line of text.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// Converts HTML into readable plain text.
///
/// Block level tags become line breaks, `<li>` becomes a bullet, links keep
/// their text and the href is appended in angle brackets so that a text-only
/// reader can still reach the target.  Runs of whitespace are collapsed, but
/// line breaks are preserved.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let document = Html::parse_fragment(html);
    let mut buffer = String::with_capacity(html.len() / 2);
    walk(document.tree.root(), &mut buffer);
    collapse(&buffer)
}

/// Recursively renders a DOM subtree.
fn walk(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&text.text),
        Node::Element(element) => {
            let name = element.name();
            let is_break = BLOCK_ELEMENTS.contains(&name);
            if is_break && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if name == "li" {
                out.push_str("• ");
            }
            for child in node.children() {
                walk(child, out);
            }
            if name == "a" {
                if let Some(href) = element.attr("href") {
                    let href = href.trim();
                    if href.starts_with("http://") || href.starts_with("https://") {
                        if !out.ends_with(' ') && !out.ends_with('\n') {
                            out.push(' ');
                        }
                        out.push('<');
                        out.push_str(href);
                        out.push('>');
                    }
                }
            }
            if name == "img" {
                if let Some(alt) = element.attr("alt") {
                    let alt = alt.trim();
                    if !alt.is_empty() {
                        if !out.ends_with(' ') && !out.ends_with('\n') {
                            out.push(' ');
                        }
                        out.push_str(&format!("[{alt}] "));
                    }
                }
            }
            if is_break && !out.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {
            for child in node.children() {
                walk(child, out);
            }
        }
    }
}

/// Squeezes blank lines and trailing spaces out of a rendered text.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_break = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            pending_break = !out.is_empty();
            continue;
        }
        if pending_break {
            out.push('\n');
            pending_break = false;
        } else if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Builds the one-line preview used by the message list.
///
/// `html` is only touched when no plain text is available: a message that
/// carries both representations must be previewed from the author's own text,
/// not from a rendering of their markup.
#[must_use]
pub fn make_snippet(plain: Option<&str>, html: Option<&str>) -> String {
    let source = match plain {
        Some(text) if !text.trim().is_empty() => text.to_string(),
        _ => match html {
            Some(html) if !html.trim().is_empty() => html_to_text(html),
            _ => return String::new(),
        },
    };
    let flattened = source.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flattened, SNIPPET_LENGTH)
}

/// Cuts a string at a character boundary and appends an ellipsis.
#[must_use]
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// Escapes text so that it can be embedded into an HTML body.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
    out
}

/// Converts plain text into a minimal HTML body.
///
/// Blank lines start a new paragraph, single line breaks become `<br>`.  The
/// output is already escaped, so it can be handed to the composer.
#[must_use]
pub fn plain_to_html(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len() + 16);
    for paragraph in normalized.split("\n\n") {
        let paragraph = paragraph.trim_matches('\n');
        if paragraph.trim().is_empty() {
            continue;
        }
        out.push_str("<p>");
        for (index, line) in paragraph.split('\n').enumerate() {
            if index > 0 {
                out.push_str("<br>");
            }
            out.push_str(&escape(line));
        }
        out.push_str("</p>");
    }
    out
}

/// Builds the quoted original of a reply.
///
/// The result is the HTML the composer inserts below the cursor: the original
/// text in a `blockquote`, preceded by the attribution line the user expects.
#[must_use]
pub fn quote_reply(sender: &str, date: &str, text: &str) -> String {
    let mut out = String::new();
    out.push_str("<p>Am ");
    out.push_str(&escape(date));
    out.push_str(" schrieb ");
    out.push_str(&escape(sender));
    out.push_str(":</p><blockquote>");
    for paragraph in text.replace("\r\n", "\n").split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        out.push_str("<p>");
        for (index, line) in paragraph.split('\n').enumerate() {
            if index > 0 {
                out.push_str("<br>");
            }
            out.push_str(&escape(line.trim_start_matches('>')));
        }
        out.push_str("</p>");
    }
    out.push_str("</blockquote>");
    out
}

/// Returns `true` when a string looks like HTML rather than plain text.
#[must_use]
pub fn looks_like_html(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    ["<p", "<div", "<br", "<table", "<span", "<a "]
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Counts the links of a body, used by the "many links" hint.
#[must_use]
pub fn link_count(html: &str) -> usize {
    count_selector(html, "a[href]")
}

/// Counts the images of a body, including inline `cid:` images.
#[must_use]
pub fn image_count(html: &str) -> usize {
    count_selector(html, "img[src]")
}

/// Counts the elements matching `selector`, and zero for a bad selector.
fn count_selector(html: &str, selector: &str) -> usize {
    let document = Html::parse_fragment(html);
    let Ok(selector) = Selector::parse(selector) else {
        return 0;
    };
    document.select(&selector).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_line_breaks() {
        let text = html_to_text("<p>Hallo</p><p>Welt</p>");
        assert_eq!(text, "Hallo\nWelt");
    }

    #[test]
    fn lists_get_bullets() {
        let text = html_to_text("<ul><li>eins</li><li>zwei</li></ul>");
        assert_eq!(text, "• eins\n• zwei");
    }

    #[test]
    fn links_keep_text_and_target() {
        let text = html_to_text("<a href=\"https://example.org/a\">Rechnung</a>");
        assert_eq!(text, "Rechnung <https://example.org/a>");
    }

    #[test]
    fn javascript_targets_are_not_reproduced() {
        let text = html_to_text("<a href=\"javascript:alert(1)\">klick</a>");
        assert_eq!(text, "klick");
    }

    #[test]
    fn alt_text_stands_in_for_an_image() {
        let text = html_to_text("Vorher<img src=\"cid:x\" alt=\"Logo\">Nachher");
        assert_eq!(text, "Vorher [Logo] Nachher");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(html_to_text("<p>a&amp;b &lt;c&gt;</p>"), "a&b <c>");
    }

    #[test]
    fn snippet_prefers_the_plain_text() {
        let snippet = make_snippet(
            Some("  Betreff   der Nachricht\n\nmehr  "),
            Some("<p>x</p>"),
        );
        assert_eq!(snippet, "Betreff der Nachricht mehr");
    }

    #[test]
    fn snippet_falls_back_to_a_rendered_html_body() {
        let snippet = make_snippet(None, Some("<p>Hallo <b>Welt</b></p>"));
        assert_eq!(snippet, "Hallo Welt");
    }

    #[test]
    fn snippet_is_truncated_with_an_ellipsis() {
        let long = "a".repeat(500);
        let snippet = make_snippet(Some(&long), None);
        assert_eq!(snippet.chars().count(), SNIPPET_LENGTH);
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn snippet_of_an_empty_message_is_empty() {
        assert!(make_snippet(None, None).is_empty());
        assert!(make_snippet(Some("   "), Some("  ")).is_empty());
    }

    #[test]
    fn escaping_covers_the_dangerous_characters() {
        assert_eq!(
            escape("<script>\"x\" & 'y'</script>"),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn plain_text_becomes_paragraphs() {
        assert_eq!(
            plain_to_html("Hallo\nWelt\n\nZweiter Absatz"),
            "<p>Hallo<br>Welt</p><p>Zweiter Absatz</p>"
        );
    }

    #[test]
    fn plain_to_html_escapes_markup() {
        let html = plain_to_html("<b>nicht fett</b>");
        assert_eq!(html, "<p>&lt;b&gt;nicht fett&lt;/b&gt;</p>");
    }

    #[test]
    fn quoting_produces_the_expected_markup() {
        let html = quote_reply(
            "Ada <ada@example.org>",
            "12.11.2026",
            "Zeile eins\nZeile zwei",
        );
        assert!(html.contains("Am 12.11.2026 schrieb Ada &lt;ada@example.org&gt;:"));
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("Zeile eins<br>Zeile zwei"));
    }

    #[test]
    fn quoting_strips_existing_quote_markers() {
        let html = quote_reply("Ada", "heute", "> schon zitiert");
        assert!(html.contains("schon zitiert"));
        assert!(!html.contains("&gt;"));
    }

    #[test]
    fn html_detection_is_heuristic_but_cheap() {
        assert!(looks_like_html("<p>Hallo</p>"));
        assert!(looks_like_html("Hallo<br>Welt"));
        assert!(!looks_like_html("nur Text"));
    }

    #[test]
    fn link_count_only_counts_real_links() {
        assert_eq!(link_count("<a href=\"#\">a</a><a>b</a>"), 1);
    }
}
