//! A small HTML renderer for the reading pane.
//!
//! HylMail ships its own sanitizer, so the markup that reaches this module is
//! already restricted to a safe subset.  Instead of embedding a browser the
//! sanitised HTML is scanned for tags and turned into an [`egui`] rich text
//! job: headings, emphasis, lists, links, tables and images become plain
//! styled text.
//!
//! Two deliberate limits, both visible to the user:
//!
//! * `egui`'s default fonts have no bold face, so "bold" is rendered as a
//!   slightly larger glyph and a brighter colour.
//! * Block layout is approximated with blank lines rather than measured, which
//!   keeps the renderer free of a layout engine and honest about being a
//!   reader, not a browser.

use egui::text::LayoutJob;
use egui::{FontFamily, FontId, Stroke, TextFormat};

use crate::theme::Palette;

/// One run of text with the formatting collected from its tags.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fragment {
    /// The text itself, with entities decoded.
    pub text: String,
    /// Emphasised text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text, used for links.
    pub underline: bool,
    /// Horizontal scale, `1.0` for body text.
    pub scale: f32,
    /// Monospace text, for `code` and `pre`.
    pub monospace: bool,
    /// The `href` of the link this run belongs to.
    pub link: Option<String>,
    /// The colour the markup declared, if any (`FR-READ-7`).
    pub color: Option<[u8; 3]>,
}

/// A rendered message body: the styled runs plus every link it contained.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Document {
    /// The styled runs, in reading order.
    pub fragments: Vec<Fragment>,
    /// Every link of the body, in encounter order.
    pub links: Vec<Link>,
    /// Every image the body referenced, in encounter order.
    pub images: Vec<ImageRef>,
}

/// One `<img>` of the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef {
    /// The `src` as written in the (sanitised) markup.
    pub src: String,
    /// The `alt` text, if any.
    pub alt: String,
}

/// A link found in the body, with the text the reader saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    /// The visible text of the anchor.
    pub label: String,
    /// The `href` the anchor points at.
    pub href: String,
}

impl Document {
    /// Whether the body rendered to nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fragments
            .iter()
            .all(|fragment| fragment.text.trim().is_empty())
    }

    /// The rendered text without any formatting, for copying.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for fragment in &self.fragments {
            if fragment.text == "\n" && out.ends_with('\n') {
                continue;
            }
            out.push_str(&fragment.text);
        }
        out.trim().to_string()
    }

    /// Builds the `egui` job for a base font size.
    ///
    /// `original_colours` renders the message in the colours it declared, which
    /// is what a newsletter keeps (`FR-READ-7`); when it is `false` the runs use
    /// the palette, so a simple message is colour-adapted for the dark theme.
    #[must_use]
    pub fn layout(&self, palette: &Palette, base_size: f32, original_colours: bool) -> LayoutJob {
        let mut job = LayoutJob::default();
        for fragment in &self.fragments {
            let family = if fragment.monospace {
                FontFamily::Monospace
            } else {
                FontFamily::Proportional
            };
            let size = (base_size * fragment.scale).clamp(8.0, 48.0);
            let mut format = TextFormat {
                font_id: FontId::new(size, family),
                color: palette.text,
                italics: fragment.italic,
                underline: Stroke::NONE,
                ..TextFormat::default()
            };
            if fragment.link.is_some() {
                format.color = palette.accent;
                format.underline = Stroke::new(1.0_f32, palette.accent);
            } else if original_colours {
                if let Some(rgb) = fragment.color {
                    format.color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                }
            }
            if fragment.underline && fragment.link.is_none() {
                format.underline = Stroke::new(1.0_f32, palette.text_dim);
            }
            job.append(&fragment.text, 0.0, format);
        }
        job
    }
}

/// The tag state inherited by the text inside an element.
#[derive(Debug, Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    scale: f32,
    monospace: bool,
    link: Option<String>,
    color: Option<[u8; 3]>,
}

impl Style {
    fn fragment(&self, text: String) -> Fragment {
        Fragment {
            bold: self.bold,
            italic: self.italic,
            underline: self.underline || self.link.is_some(),
            scale: if self.scale > 0.0 { self.scale } else { 1.0 },
            monospace: self.monospace,
            link: self.link.clone(),
            color: self.color,
            text,
        }
    }
}

/// The tag kinds the scanner reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    None,
    Blank,
    Line,
}

/// Parses sanitised HTML into a drawable [`Document`].
#[must_use]
pub(crate) fn render(html: &str) -> Document {
    let mut document = Document::default();
    let mut stack: Vec<Style> = vec![Style::default()];
    let mut pending_blank = false;
    let mut preformatted = 0usize;

    let mut rest = html;
    while let Some(open) = rest.find('<') {
        let text = &rest[..open];
        push_text(
            &mut document,
            &stack,
            text,
            preformatted > 0,
            &mut pending_blank,
        );
        let Some(close) = rest[open..].find('>').map(|offset| open + offset) else {
            // A stray `<` at the end of the body: keep it as text.
            push_text(
                &mut document,
                &stack,
                &rest[open..],
                preformatted > 0,
                &mut pending_blank,
            );
            rest = "";
            break;
        };
        let raw_tag = &rest[open + 1..close];
        rest = &rest[close + 1..];
        apply_tag(
            &mut document,
            &mut stack,
            raw_tag,
            &mut pending_blank,
            &mut preformatted,
        );
    }
    push_text(
        &mut document,
        &stack,
        rest,
        preformatted > 0,
        &mut pending_blank,
    );
    while document
        .fragments
        .last()
        .is_some_and(|fragment| fragment.text.trim().is_empty())
    {
        document.fragments.pop();
    }
    document
}

/// Appends one text run, collapsing whitespace outside `pre`.
fn push_text(
    document: &mut Document,
    stack: &[Style],
    text: &str,
    preformatted: bool,
    pending_blank: &mut bool,
) {
    let decoded = decode_entities(text);
    let value = if preformatted {
        decoded
    } else {
        collapse_whitespace(&decoded)
    };
    if value.is_empty() {
        return;
    }
    if *pending_blank {
        *pending_blank = false;
        let style = stack.last().cloned().unwrap_or_default();
        document.fragments.push(style.fragment("\n\n".to_string()));
    }
    let style = stack.last().cloned().unwrap_or_default();
    if style.link.is_some() {
        let label = value.trim().to_string();
        if !label.is_empty() {
            if let Some(href) = style.link.clone() {
                if !document
                    .links
                    .iter()
                    .any(|link| link.href == href && link.label == label)
                {
                    document.links.push(Link { label, href });
                }
            }
        }
    }
    document.fragments.push(style.fragment(value));
}

/// Interprets one tag.
fn apply_tag(
    document: &mut Document,
    stack: &mut Vec<Style>,
    raw_tag: &str,
    pending_blank: &mut bool,
    preformatted: &mut usize,
) {
    let raw_tag = raw_tag.trim();
    let closing = raw_tag.starts_with('/');
    let body = raw_tag.trim_start_matches('/');
    let (name, attributes) = match body.find(|c: char| c.is_whitespace() || c == '/') {
        Some(index) => (&body[..index], &body[index..]),
        None => (body, ""),
    };
    let name = name.to_ascii_lowercase();
    let block = block_of(&name);
    // The colour this tag declares, applied to every run inside it (`FR-READ-7`).
    let tag_color = if closing { None } else { colour_of(attributes) };

    match (closing, name.as_str()) {
        (_, "br") => push_line(document, stack, pending_blank),
        (_, "hr") => {
            document
                .fragments
                .push(style_of(stack).fragment("\n".to_string()));
            document
                .fragments
                .push(style_of(stack).fragment("─".repeat(32)));
            push_line(document, stack, pending_blank);
        }
        (_, "img") => {
            let alt = attribute(attributes, "alt").unwrap_or_default();
            let src = attribute(attributes, "src").unwrap_or_default();
            let src = src.trim();
            if is_remote_source(src) {
                document.images.push(ImageRef {
                    src: src.to_string(),
                    alt: alt.trim().to_string(),
                });
            }
            let label = if alt.trim().is_empty() {
                "[Bild]".to_string()
            } else {
                format!("[Bild: {}]", alt.trim())
            };
            document.fragments.push(style_of(stack).fragment(label));
        }
        (_, "li") => {
            push_line(document, stack, pending_blank);
            let mut style = inherit(stack, tag_color);
            style.bold = false;
            document.fragments.push(style.fragment("• ".to_string()));
            stack.push(inherit(stack, tag_color));
        }
        (false, "td") | (false, "th") => {
            let mut style = inherit(stack, tag_color);
            style.bold = style.bold || name == "th";
            document.fragments.push(style.fragment("\t".to_string()));
            stack.push(inherit(stack, tag_color));
        }
        (true, "td") | (true, "th") => {
            if stack.len() > 1 {
                stack.pop();
            }
        }
        (false, "span") | (false, "font") => {
            stack.push(inherit(stack, tag_color));
        }
        (false, "b") | (false, "strong") => {
            stack.push(mutate(stack, tag_color, |style| style.bold = true))
        }
        (false, "i") | (false, "em") | (false, "cite") | (false, "var") => {
            stack.push(mutate(stack, tag_color, |style| style.italic = true));
        }
        (false, "u") | (false, "ins") => {
            stack.push(mutate(stack, tag_color, |style| style.underline = true));
        }
        (false, "code") | (false, "kbd") | (false, "samp") => {
            stack.push(mutate(stack, tag_color, |style| style.monospace = true));
        }
        (false, "pre") => {
            *preformatted += 1;
            stack.push(mutate(stack, tag_color, |style| style.monospace = true));
        }
        (true, "pre") => {
            *preformatted = preformatted.saturating_sub(1);
            stack.pop();
        }
        (false, "a") => {
            let href = attribute(attributes, "href").unwrap_or_default();
            let href = href.trim().to_string();
            let link = if href.is_empty() { None } else { Some(href) };
            stack.push(mutate(stack, tag_color, |style| {
                style.underline = true;
                style.link = link;
            }));
        }
        (true, "a") => {
            stack.pop();
        }
        (false, "h1") => stack.push(mutate(stack, tag_color, |style| heading(style, 1.55))),
        (false, "h2") => stack.push(mutate(stack, tag_color, |style| heading(style, 1.35))),
        (false, "h3") => stack.push(mutate(stack, tag_color, |style| heading(style, 1.18))),
        (false, "h4") | (false, "h5") | (false, "h6") => {
            stack.push(mutate(stack, tag_color, |style| heading(style, 1.05)));
        }
        (true, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
            stack.pop();
            *pending_blank = true;
        }
        (
            true,
            "b" | "strong" | "i" | "em" | "cite" | "var" | "u" | "ins" | "code" | "kbd" | "samp"
            | "span" | "font" | "small" | "big" | "sub" | "sup",
        ) => {
            if stack.len() > 1 {
                stack.pop();
            }
        }
        (true, _) if matches!(block, Block::Blank | Block::Line) => {
            if stack.len() > 1 {
                stack.pop();
            }
            if block == Block::Blank {
                *pending_blank = true;
            } else {
                push_line(document, stack, pending_blank);
            }
        }
        (false, _) if matches!(block, Block::Blank | Block::Line) => {
            if block == Block::Blank {
                *pending_blank = true;
            } else if name == "tr" {
                push_line(document, stack, pending_blank);
            }
            stack.push(inherit(stack, tag_color));
        }
        _ => {}
    }
}

/// The block behaviour of a tag.
fn block_of(name: &str) -> Block {
    match name {
        "p" | "div" | "blockquote" | "section" | "article" | "header" | "footer" | "table"
        | "ul" | "ol" | "dl" => Block::Blank,
        "tr" | "li" | "dd" | "dt" => Block::Line,
        _ => Block::None,
    }
}

/// The style currently in effect.
fn style_of(stack: &[Style]) -> Style {
    stack.last().cloned().unwrap_or_default()
}

/// A copy of the current style, with a newly declared colour applied.
fn inherit(stack: &[Style], color: Option<[u8; 3]>) -> Style {
    let mut style = style_of(stack);
    if let Some(color) = color {
        style.color = Some(color);
    }
    style
}

/// Pushes a copy of the current style after changing it.
fn mutate(stack: &[Style], color: Option<[u8; 3]>, change: impl FnOnce(&mut Style)) -> Style {
    let mut style = inherit(stack, color);
    change(&mut style);
    style
}

/// Applies the size and weight of a heading element.
fn heading(style: &mut Style, scale: f32) {
    style.bold = true;
    style.scale = scale;
}

/// Starts a new line, without creating empty ones.
fn push_line(document: &mut Document, stack: &[Style], pending_blank: &mut bool) {
    *pending_blank = false;
    let last_is_break = document
        .fragments
        .last()
        .is_some_and(|fragment| fragment.text.ends_with('\n'));
    if last_is_break || document.fragments.is_empty() {
        return;
    }
    document
        .fragments
        .push(style_of(stack).fragment("\n".to_string()));
}

/// Collapses every whitespace run into a single space.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(character);
    }
    if in_space && !out.is_empty() {
        out.push(' ');
    }
    out
}

/// `true` when an image `src` is a remote URL the client may fetch (`FR-READ-2`).
pub(crate) fn is_remote_source(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//")
}

/// The text colour an element declares (`FR-READ-7`).
///
/// Read from the `color` attribute (`<font color=…>`) or from a `color`
/// declaration in the `style` attribute; a `bgcolor` never counts.
fn colour_of(attributes: &str) -> Option<[u8; 3]> {
    if let Some(value) = attribute(attributes, "color") {
        if let Some(rgb) = parse_colour(&value) {
            return Some(rgb);
        }
    }
    let style = attribute(attributes, "style")?;
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        if property.trim().eq_ignore_ascii_case("color") {
            if let Some(rgb) = parse_colour(value) {
                return Some(rgb);
            }
        }
    }
    None
}

/// Parses `#rgb`, `#rrggbb`, `rgb(r,g,b)` and a handful of colour names.
fn parse_colour(value: &str) -> Option<[u8; 3]> {
    let trimmed = value.trim().trim_matches('"').trim();
    let lowered = trimmed.to_ascii_lowercase();
    match lowered.as_str() {
        "black" => return Some([0, 0, 0]),
        "white" => return Some([255, 255, 255]),
        "red" => return Some([255, 0, 0]),
        "green" => return Some([0, 128, 0]),
        "blue" => return Some([0, 0, 255]),
        "gray" | "grey" => return Some([128, 128, 128]),
        _ => {}
    }
    if let Some(hex) = lowered.strip_prefix('#') {
        return match hex.len() {
            3 => Some([
                u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
                u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
            ]),
            6 => Some([
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ]),
            _ => None,
        };
    }
    if let Some(rest) = lowered
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut parts = rest.split(',').map(|part| part.trim().parse::<u8>().ok());
        let red = parts.next()??;
        let green = parts.next()??;
        let blue = parts.next()??;
        return Some([red, green, blue]);
    }
    None
}

/// Reads an attribute value from the raw attribute text.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let haystack = attributes.to_ascii_lowercase();
    let mut search = 0;
    while let Some(index) = haystack[search..].find(name).map(|offset| search + offset) {
        let before_ok = index == 0
            || haystack[..index]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        let after = &attributes[index + name.len()..];
        let after_ok = after.trim_start().starts_with('=');
        if before_ok && after_ok {
            let value = after.trim_start().trim_start_matches('=').trim_start();
            let (quote, value) = match value.chars().next() {
                Some(quote @ ('"' | '\'')) => (Some(quote), &value[quote.len_utf8()..]),
                _ => (None, value),
            };
            let end = match quote {
                Some(quote) => value.find(quote).unwrap_or(value.len()),
                None => value
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(value.len()),
            };
            return Some(decode_entities(&value[..end]));
        }
        search = index + name.len();
    }
    None
}

/// Decodes the entities `ammonia` leaves in the sanitised markup.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let end = tail.find(';').filter(|end| *end <= 12);
        let Some(end) = end else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        match named_entity(entity) {
            Some(decoded) => {
                out.push_str(decoded);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The named entities worth decoding, plus the numeric forms.
fn named_entity(entity: &str) -> Option<&'static str> {
    let named = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        "nbsp" => " ",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "laquo" => "«",
        "raquo" => "»",
        "szlig" => "ß",
        "auml" => "ä",
        "ouml" => "ö",
        "uuml" => "ü",
        "Auml" => "Ä",
        "Ouml" => "Ö",
        "Uuml" => "Ü",
        "euro" => "€",
        "deg" => "°",
        "middot" => "·",
        "bull" => "•",
        _ => return numeric_entity(entity),
    };
    Some(named)
}

/// Decodes `&#228;` and `&#xE4;` into a `&'static str` is impossible, so only
/// the handful of numeric forms that occur in real mail are mapped.
fn numeric_entity(entity: &str) -> Option<&'static str> {
    let value = entity.strip_prefix('#')?;
    let code = if let Some(hex) = value.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        value.parse::<u32>().ok()?
    };
    Some(match code {
        0x20 | 0xA0 => " ",
        0x3C => "<",
        0x3E => ">",
        0xE4 => "ä",
        0xF6 => "ö",
        0xFC => "ü",
        0xC4 => "Ä",
        0xD6 => "Ö",
        0xDC => "Ü",
        0xDF => "ß",
        0x20AC => "€",
        0x2026 => "…",
        0x2013 => "–",
        0x2014 => "—",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_becomes_one_fragment() {
        let document = render("<p>Hallo Welt</p>");
        assert_eq!(document.plain_text(), "Hallo Welt");
    }

    #[test]
    fn emphasis_survives_the_scan() {
        let document = render("a <b>fett</b> b <i>kursiv</i> c");
        let bold = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("fett"));
        assert!(bold.is_some_and(|fragment| fragment.bold));
        let italic = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("kursiv"));
        assert!(italic.is_some_and(|fragment| fragment.italic));
    }

    #[test]
    fn headings_are_larger_and_bold() {
        let document = render("<h1>Titel</h1><p>Text</p>");
        let heading = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("Titel"));
        assert!(heading.is_some_and(|fragment| fragment.bold && fragment.scale > 1.2));
        let body = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("Text"));
        assert!(body.is_some_and(|fragment| fragment.scale == 1.0));
    }

    #[test]
    fn nested_emphasis_keeps_both_attributes() {
        let document = render("<b><i>beides</i></b>");
        let fragment = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("beides"));
        assert!(fragment.is_some_and(|fragment| fragment.bold && fragment.italic));
    }

    #[test]
    fn closing_a_tag_restores_the_previous_style() {
        let document = render("<b>fett</b>normal");
        let normal = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("normal"));
        assert!(normal.is_some_and(|fragment| !fragment.bold));
    }

    #[test]
    fn every_inline_span_is_closed_again() {
        let document = render("<b>a</b><i>b</i><u>c</u><code>d</code>e");
        let last = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains('e'));
        assert!(last.is_some_and(|fragment| !fragment.bold
            && !fragment.italic
            && !fragment.underline
            && !fragment.monospace));
    }

    #[test]
    fn links_are_collected_with_their_label() {
        let document = render(r#"<a href="https://example.org/x">Beispiel</a>"#);
        assert_eq!(document.links.len(), 1);
        assert_eq!(document.links[0].href, "https://example.org/x");
        assert_eq!(document.links[0].label, "Beispiel");
        let fragment = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("Beispiel"));
        assert!(fragment.is_some_and(|fragment| fragment.link.is_some()));
    }

    #[test]
    fn an_attribute_may_be_unquoted() {
        assert_eq!(
            attribute(" href=https://example.org/a title=\"x\"", "href").as_deref(),
            Some("https://example.org/a")
        );
        assert_eq!(attribute(" href=\"a b\"", "href").as_deref(), Some("a b"));
        assert_eq!(attribute(" class=\"x\"", "href"), None);
    }

    #[test]
    fn list_items_get_a_bullet() {
        let document = render("<ul><li>eins</li><li>zwei</li></ul>");
        let text = document.plain_text();
        assert!(text.contains("• eins"), "{text}");
        assert!(text.contains("• zwei"), "{text}");
    }

    #[test]
    fn an_image_without_alt_is_marked() {
        let document = render(r#"<img src="cid:x">"#);
        assert!(document.plain_text().contains("[Bild]"));
        let document = render(r#"<img src="cid:x" alt="Logo">"#);
        assert!(document.plain_text().contains("[Bild: Logo]"));
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&auml;&ouml;&uuml;"), "äöü");
        assert_eq!(decode_entities("&#228;&#xDF;"), "äß");
        assert_eq!(decode_entities("100 &euro;"), "100 €");
        assert_eq!(decode_entities("a & b"), "a & b");
        assert_eq!(decode_entities("&unbekannt;"), "&unbekannt;");
    }

    #[test]
    fn whitespace_is_collapsed_outside_pre() {
        let document = render("<p>a\n   b\t\tc</p>");
        assert_eq!(document.plain_text(), "a b c");
        let pre = render("<pre>a\nb</pre>");
        assert_eq!(pre.fragments[0].text, "a\nb");
    }

    #[test]
    fn a_preformatted_block_is_monospace() {
        let document = render("<pre>code</pre>");
        assert!(document.fragments[0].monospace);
    }

    #[test]
    fn trailing_empty_fragments_are_removed() {
        let document = render("<p>a</p><p></p><br>");
        assert!(!document
            .fragments
            .last()
            .is_some_and(|fragment| fragment.text.trim().is_empty()));
    }

    #[test]
    fn an_unterminated_tag_does_not_lose_text() {
        let document = render("vorher <b");
        assert!(document.plain_text().contains("vorher"));
        assert!(document.plain_text().contains("<b"));
    }

    #[test]
    fn the_layout_job_carries_the_links_colour() {
        let palette = *crate::theme::palette();
        let document = render(r#"<a href="https://example.org">x</a>"#);
        let job = document.layout(&palette, 13.0, false);
        assert!(job
            .sections
            .iter()
            .any(|section| section.format.underline.width > 0.0));
    }

    #[test]
    fn an_empty_body_is_detected() {
        assert!(render("").is_empty());
        assert!(render("<p>   </p>").is_empty());
        assert!(!render("<p>x</p>").is_empty());
    }

    #[test]
    fn declared_colours_are_parsed() {
        let document = render(r##"<p><font color="#ff0000">rot</font></p>"##);
        let fragment = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("rot"))
            .unwrap();
        assert_eq!(fragment.color, Some([255, 0, 0]));

        let document = render(r#"<div style="color: rgb(0, 128, 0)">grün</div>"#);
        let fragment = document
            .fragments
            .iter()
            .find(|fragment| fragment.text.contains("grün"))
            .unwrap();
        assert_eq!(fragment.color, Some([0, 128, 0]));

        // `bgcolor` is a background, never the text colour.
        let document = render(r##"<td bgcolor="#ffffff">x</td>"##);
        assert!(document
            .fragments
            .iter()
            .all(|fragment| fragment.color.is_none()));
    }

    #[test]
    fn a_newsletter_keeps_its_colour_but_a_simple_message_does_not() {
        let document = render(r##"<font color="#ff0000">rot</font>"##);
        let palette = *crate::theme::palette();
        let red = egui::Color32::from_rgb(255, 0, 0);

        let newsletter = document.layout(&palette, 13.0, true);
        assert!(newsletter
            .sections
            .iter()
            .any(|section| section.format.color == red));
        // The dark-adapted job uses the palette, not the declared red.
        let adapted = document.layout(&palette, 13.0, false);
        assert!(adapted
            .sections
            .iter()
            .all(|section| section.format.color != red));
    }
}
