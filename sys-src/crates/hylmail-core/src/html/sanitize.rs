//! The HTML pipeline: allow-list sanitising, image policy, plain text.
//!
//! Mail HTML is hostile input.  A message may carry `<script>`, an `onclick`
//! handler, a `javascript:` link, a CSS `expression()` or a one-pixel tracking
//! image, and the reading pane has to render the *layout* while none of the
//! *behaviour* can ever run.
//!
//! The pipeline is therefore built around [`ammonia`], which parses with
//! `html5ever` (the parser a browser uses, so a malformed message cannot
//! confuse it) and then keeps only what an allow-list permits:
//!
//! ```text
//!   raw HTML ──► ammonia (tags, attributes, URL schemes, style properties)
//!            ──► attribute filter (image policy, tracking pixels)
//!            ──► sanitised HTML  +  ImageStats
//! ```
//!
//! The image policy is a *product decision* that lives in the settings
//! ([`crate::models::ExternalImagePolicy`]), so it is passed in as
//! [`HtmlPolicy`] rather than read from a global.
//!
//! Nothing here can fail: a sanitiser that returned an error would mean the
//! reading pane shows a half-rendered message.  Unparsable input simply ends up
//! as escaped text.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use ammonia::{Builder, UrlRelative};
use parking_lot::Mutex;
use scraper::{Html, Selector};

use crate::models::{ExternalImagePolicy, Settings};

/// URL schemes that address content *inside* the message itself.
const INLINE_SCHEMES: &[&str] = &["cid", "data"];

/// Presentational attributes that carry a newsletter's layout.
///
/// None of them can execute anything; without them a large share of legitimate
/// mail renders as an unstyled column of text.
const GENERIC_ATTRIBUTES: &[&str] = &[
    "align",
    "valign",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "colspan",
    "dir",
    "height",
    "lang",
    "role",
    "rowspan",
    "style",
    "valign",
    "width",
];

/// The only CSS properties that survive inside a `style` attribute.
///
/// The list is deliberately conservative: every entry takes a colour, a length
/// or a keyword, so no `url(...)`, no `expression(...)` and no vendor specific
/// behaviour can be smuggled in.  Layout properties are included because
/// newsletters position their content with them.
const STYLE_PROPERTIES: &[&str] = &[
    "background-color",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-style",
    "border-top-width",
    "border-width",
    "color",
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-width",
    "min-width",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "text-align",
    "text-decoration",
    "text-decoration-color",
    "text-decoration-line",
    "text-indent",
    "text-transform",
    "vertical-align",
    "white-space",
    "width",
    "word-break",
    "word-wrap",
];

/// Tags that are never rendered, not even without their content.
const REMOVED_TAGS: &[&str] = &[
    "area", "audio", "base", "button", "canvas", "embed", "form", "iframe", "input", "link", "map",
    "math", "meta", "noscript", "object", "option", "param", "select", "source", "svg", "textarea",
    "track", "video",
];

/// Tags that are allowed because mail HTML uses them for layout.
const ADDED_TAGS: &[&str] = &["font", "tfoot"];

/// How many images of each kind a message carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageStats {
    /// External images that are kept (the policy allows them).
    pub external: usize,
    /// External images whose `src` was removed.
    pub blocked: usize,
    /// Images that address content inside the message (`cid:`, `data:`).
    pub inline: usize,
    /// A subset of [`ImageStats::blocked`]: images that look like trackers.
    pub tracking: usize,
}

impl ImageStats {
    /// Total number of `<img>` elements that were classified.
    #[must_use]
    pub const fn total(self) -> usize {
        self.external + self.blocked + self.inline
    }

    /// Whether any image was blocked.
    #[must_use]
    pub const fn any_blocked(self) -> bool {
        self.blocked > 0
    }
}

/// Everything the sanitiser needs to know about the user's preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlPolicy {
    /// Run the allow-list at all; disabling it is a diagnostics switch.
    pub sanitize: bool,
    /// Load `http(s)` images immediately.
    pub allow_external_images: bool,
    /// Hosts that are always treated as trackers, even when images are allowed.
    pub blocked_tracking_domains: Vec<String>,
    /// From this number of images on, a message counts as a newsletter.
    pub newsletter_image_threshold: usize,
}

impl Default for HtmlPolicy {
    fn default() -> Self {
        Self {
            sanitize: true,
            allow_external_images: false,
            blocked_tracking_domains: Vec::new(),
            newsletter_image_threshold: 5,
        }
    }
}

impl HtmlPolicy {
    /// Resolves the policy for one message.
    ///
    /// `sender` is the `From` address; when it appears in
    /// [`Settings::trusted_image_senders`] the user has already decided for
    /// this sender and the images are loaded even under
    /// [`ExternalImagePolicy::Ask`].
    #[must_use]
    pub fn from_settings(settings: &Settings, sender: Option<&str>) -> Self {
        let trusted = sender.is_some_and(|sender| Self::trusts(settings, sender));
        Self {
            sanitize: settings.sanitize_html,
            allow_external_images: trusted
                || settings.external_images == ExternalImagePolicy::Allow,
            blocked_tracking_domains: settings
                .blocked_tracking_domains
                .iter()
                .map(|domain| domain.trim().to_ascii_lowercase())
                .filter(|domain| !domain.is_empty())
                .collect(),
            newsletter_image_threshold: settings.newsletter_image_threshold,
        }
    }

    /// Whether `sender` is trusted enough to load remote content.
    #[must_use]
    pub fn trusts(settings: &Settings, sender: &str) -> bool {
        settings
            .trusted_image_senders
            .iter()
            .any(|known| known.eq_ignore_ascii_case(sender.trim()))
    }
}

/// The outcome of the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitisedHtml {
    /// The HTML to render.
    pub html: String,
    /// What the pipeline saw while rewriting.
    pub images: ImageStats,
    /// `true` when the message carries enough images to be a newsletter.
    pub is_newsletter: bool,
}

impl SanitisedHtml {
    /// The HTML of a message that had no HTML body at all.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            html: String::new(),
            images: ImageStats::default(),
            is_newsletter: false,
        }
    }

    /// `true` when nothing needs to be pointed out to the user.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        !self.images.any_blocked()
    }

    /// The German banner text for the reading pane, if a banner is needed.
    #[must_use]
    pub fn banner(&self) -> Option<String> {
        match (self.images.blocked, self.images.tracking) {
            (0, _) => None,
            (count, 0) => Some(format!(
                "{count} externes Bild wurde blockiert. Bilder für diesen Absender erlauben?"
            )),
            (count, tracking) => Some(format!(
                "{count} externes Bild wurde blockiert, davon {tracking} Zählpixel. \
                 Bilder für diesen Absender erlauben?"
            )),
        }
    }
}

/// Runs the pipeline over one HTML body.
#[must_use]
pub fn sanitize(html: &str, policy: &HtmlPolicy) -> SanitisedHtml {
    if html.trim().is_empty() {
        return SanitisedHtml::empty();
    }
    if !policy.sanitize {
        // Diagnostics only: hand back what arrived, but still classify the
        // images so the caller knows what it is looking at.
        let images = scan_images(html, policy);
        return SanitisedHtml {
            html: html.to_string(),
            images,
            is_newsletter: is_newsletter(images, policy),
        };
    }

    let stats = Arc::new(Mutex::new(ImageStats::default()));
    let collected = Arc::clone(&stats);
    let policy_for_filter = policy.clone();
    let mut builder = Builder::default();
    builder
        .add_tags(ADDED_TAGS)
        .rm_tags(REMOVED_TAGS)
        .add_generic_attributes(GENERIC_ATTRIBUTES)
        .add_tag_attributes("img", &["alt", "height", "src", "width"])
        .add_tag_attributes("table", &["border", "cellpadding", "cellspacing", "width"])
        .add_tag_attributes("td", &["colspan", "rowspan", "height", "width"])
        .add_tag_attributes("a", &["href", "name", "title"])
        .filter_style_properties(STYLE_PROPERTIES.iter().copied().collect::<HashSet<_>>())
        .url_schemes(
            ["cid", "data", "http", "https", "mailto", "tel"]
                .into_iter()
                .collect::<HashSet<_>>(),
        )
        .url_relative(UrlRelative::PassThrough)
        .link_rel(Some("noopener noreferrer nofollow"))
        .strip_comments(true)
        .attribute_filter(move |element, attribute, value| {
            filter_attribute(element, attribute, value, &policy_for_filter, &collected)
        });

    let cleaned = builder.clean(html).to_string();
    let cleaned = rasterise_inline_svgs(&cleaned);
    let images = *stats.lock();
    SanitisedHtml {
        html: cleaned,
        images,
        is_newsletter: is_newsletter(images, policy),
    }
}

/// Replaces every inline SVG data URI with a rasterised PNG data URI.
///
/// An SVG is a document that can carry scripts and external references, so it
/// must never be handed to the reading pane.  [`crate::image::rasterise_data_uri`]
/// turns it into a bitmap that cannot execute anything; a document that cannot
/// be rasterised is dropped rather than passed through.  This is the only place
/// the sanitised HTML is allowed to change after `ammonia` has run.
fn rasterise_inline_svgs(html: &str) -> String {
    const NEEDLE: &str = "data:image/svg+xml";
    if !html.contains(NEEDLE) {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(index) = rest.find(NEEDLE) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        // The URI runs to the closing quote of the attribute; the sanitised
        // markup always quotes attribute values.
        let end = tail
            .find('"')
            .or_else(|| tail.find('\''))
            .or_else(|| tail.find(|c: char| c.is_whitespace() || c == '>'))
            .unwrap_or(tail.len());
        if let Some(png) = crate::image::rasterise_data_uri(&tail[..end]) {
            out.push_str(&png);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Classification of a single URL seen in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlKind {
    /// `cid:` or `data:` - the bytes travel with the message.
    Inline,
    /// An absolute `http(s)` (or scheme-relative) URL.
    External,
    /// A relative URL; mail has no base, so it cannot be resolved.
    Relative,
}

/// The attribute filter: drop tracking pixels, enforce the image policy.
fn filter_attribute<'u>(
    element: &str,
    attribute: &str,
    value: &'u str,
    policy: &HtmlPolicy,
    stats: &Arc<Mutex<ImageStats>>,
) -> Option<Cow<'u, str>> {
    match (element, attribute) {
        ("img", "src") => {
            let mut stats = stats.lock();
            match classify_url(value) {
                UrlKind::Inline => {
                    stats.inline += 1;
                    Some(Cow::Borrowed(value))
                }
                kind => {
                    let tracking = kind == UrlKind::External && is_tracker(value, policy);
                    if tracking || !policy.allow_external_images {
                        stats.blocked += 1;
                        if tracking {
                            stats.tracking += 1;
                        }
                        None
                    } else {
                        stats.external += 1;
                        Some(Cow::Borrowed(value))
                    }
                }
            }
        }
        // `srcset` selects between variants; the policy has to see every one
        // of them to stay honest, and no mail client needs the attribute.
        ("img", "srcset") | ("source", "srcset") => None,
        // `data:` documents are never a link target.
        ("a", "href") if value.trim_start().to_ascii_lowercase().starts_with("data:") => None,
        // Everything else was already allow-listed by ammonia.
        _ => Some(Cow::Borrowed(value)),
    }
}

/// Which kind of URL this is, without allocating more than necessary.
fn classify_url(value: &str) -> UrlKind {
    let trimmed = value.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if INLINE_SCHEMES
        .iter()
        .any(|scheme| lowered.starts_with(&format!("{scheme}:")))
    {
        return UrlKind::Inline;
    }
    if lowered.starts_with("http:")
        || lowered.starts_with("https:")
        || lowered.starts_with("//")
        || lowered.starts_with("ftp:")
    {
        return UrlKind::External;
    }
    // Everything else - relative paths, `about:`, unknown schemes - is not
    // something this client can fetch.
    UrlKind::Relative
}

/// Whether an external image is a tracking pixel.
fn is_tracker(value: &str, policy: &HtmlPolicy) -> bool {
    if policy.blocked_tracking_domains.is_empty() {
        return false;
    }
    let host = url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .or_else(|| {
            // `//host/path` and malformed URLs still reveal their host.
            let rest = value.trim_start_matches('/');
            let candidate = rest.split(['/', '?']).next().unwrap_or_default();
            (!candidate.is_empty()).then(|| candidate.to_ascii_lowercase())
        });
    let Some(host) = host else {
        return false;
    };
    policy
        .blocked_tracking_domains
        .iter()
        .any(|blocked| host == *blocked || host.ends_with(&format!(".{blocked}")))
}

fn is_newsletter(images: ImageStats, policy: &HtmlPolicy) -> bool {
    policy.newsletter_image_threshold > 0 && images.total() >= policy.newsletter_image_threshold
}

/// Classifies every `<img>` of a document that is *not* being sanitised.
///
/// Only used when the allow-list is switched off for diagnostics; the numbers
/// it produces are the same the attribute filter would have produced.
fn scan_images(html: &str, policy: &HtmlPolicy) -> ImageStats {
    let document = Html::parse_document(html);
    let Ok(selector) = Selector::parse("img") else {
        return ImageStats::default();
    };
    let mut stats = ImageStats::default();
    for element in document.select(&selector) {
        let source = element.value().attr("src").unwrap_or_default();
        if classify_url(source) == UrlKind::Inline {
            stats.inline += 1;
            continue;
        }
        let tracking = is_tracking_pixel_element(element) || is_tracker(source, policy);
        if tracking || !policy.allow_external_images {
            stats.blocked += 1;
            if tracking {
                stats.tracking += 1;
            }
        } else {
            stats.external += 1;
        }
    }
    stats
}

/// Whether an image element declares itself as one pixel.
fn is_tracking_pixel_element(element: scraper::ElementRef<'_>) -> bool {
    let dimension = |name: &str| {
        element
            .value()
            .attr(name)
            .map(str::trim)
            .and_then(|value| value.parse::<f64>().ok())
    };
    let one_pixel = |value: Option<f64>| value.is_some_and(|value| value <= 1.0);
    let style = element.value().attr("style").unwrap_or_default();
    (one_pixel(dimension("width")) && one_pixel(dimension("height")))
        || style_declares_one_pixel(style)
}

/// Looks for `width: 1px` / `height: 1px` inside a `style` attribute.
///
/// Both dimensions have to be declared; a `height: 1px` on a wide banner is a
/// divider, not a tracker.
fn style_declares_one_pixel(style: &str) -> bool {
    let mut width = false;
    let mut height = false;
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        if !(value == "1" || value == "1px") {
            continue;
        }
        match property.trim().to_ascii_lowercase().as_str() {
            "width" => width = true,
            "height" => height = true,
            _ => {}
        }
    }
    width && height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowing() -> HtmlPolicy {
        HtmlPolicy {
            allow_external_images: true,
            ..HtmlPolicy::default()
        }
    }

    #[test]
    fn scripts_and_handlers_are_removed() {
        let result = sanitize(
            "<p onclick=\"steal()\">Hallo</p><script>evil()</script>\
             <iframe src=\"https://x.example\"></iframe><style>p{color:red}</style>",
            &HtmlPolicy::default(),
        );
        assert!(result.html.contains("Hallo"));
        for forbidden in ["script", "onclick", "iframe", "style"] {
            assert!(
                !result.html.contains(forbidden),
                "`{forbidden}` survived: {}",
                result.html
            );
        }
    }

    #[test]
    fn javascript_urls_are_dropped() {
        let result = sanitize("<a href=\"JavaScript:alert(1)\">klick</a>", &allowing());
        assert!(result.html.contains("klick"));
        assert!(!result.html.to_ascii_lowercase().contains("javascript"));
    }

    #[test]
    fn data_url_links_are_dropped() {
        let result = sanitize(
            "<a href=\"data:text/html,<script>x</script>\">böse</a>",
            &allowing(),
        );
        assert!(!result.html.contains("data:"));
    }

    #[test]
    fn newsletter_layout_survives() {
        let html = "<table width=\"600\" cellpadding=\"0\"><tr><td align=\"center\" \
                    style=\"color:#333;font-size:14px;margin:0\">Rechnung</td></tr></table>";
        let result = sanitize(html, &HtmlPolicy::default());
        assert!(result.html.contains("<table"));
        assert!(result.html.contains("cellpadding"));
        assert!(result.html.contains("align=\"center\""));
        assert!(result.html.contains("font-size:14px"));
        assert!(result.html.contains("Rechnung"));
    }

    #[test]
    fn dangerous_style_declarations_are_filtered() {
        let result = sanitize(
            "<p style=\"color:red;background-image:url(javascript:x);behavior:url(#x)\">x</p>",
            &HtmlPolicy::default(),
        );
        assert!(result.html.contains("color:red"));
        assert!(!result.html.contains("background-image"));
        assert!(!result.html.contains("behavior"));
    }

    #[test]
    fn external_images_are_blocked_by_default() {
        let result = sanitize(
            "<img src=\"https://track.example/pixel.gif\">",
            &HtmlPolicy::default(),
        );
        assert!(!result.html.contains("track.example"));
        assert_eq!(result.images.blocked, 1);
        assert_eq!(result.images.external, 0);
        assert!(result.images.any_blocked());
        assert!(result
            .banner()
            .is_some_and(|text| text.contains("blockiert")));
    }

    #[test]
    fn external_images_survive_when_allowed() {
        let result = sanitize("<img src=\"https://cdn.example/logo.png\">", &allowing());
        assert!(result.html.contains("https://cdn.example/logo.png"));
        assert_eq!(result.images.external, 1);
        assert!(result.is_clean());
        assert!(result.banner().is_none());
    }

    #[test]
    fn inline_images_are_kept_even_when_blocking() {
        let result = sanitize("<img src=\"cid:logo@example\">", &HtmlPolicy::default());
        assert!(result.html.contains("cid:logo@example"));
        assert_eq!(result.images.inline, 1);
        assert_eq!(result.images.blocked, 0);
    }

    #[test]
    fn tracking_pixels_are_blocked_even_when_images_are_allowed() {
        let mut policy = allowing();
        policy.blocked_tracking_domains = vec!["tracker.example".to_string()];
        let result = sanitize(
            "<img src=\"https://tracker.example/open.gif\" width=\"1\" height=\"1\">",
            &policy,
        );
        assert!(!result.html.contains("open.gif"));
        assert_eq!(result.images.tracking, 1);
        assert_eq!(result.images.blocked, 1);

        // A subdomain counts too.
        let result = sanitize("<img src=\"https://a.tracker.example/p.gif\">", &policy);
        assert_eq!(result.images.tracking, 1);
    }

    #[test]
    fn one_pixel_images_without_a_listed_domain_are_blocked_when_blocking() {
        let result = sanitize(
            "<img src=\"https://cdn.example/p.gif\" width=\"1\" height=\"1\">",
            &HtmlPolicy::default(),
        );
        assert_eq!(result.images.blocked, 1);
    }

    #[test]
    fn newsletter_threshold_marks_image_heavy_messages() {
        let mut policy = allowing();
        policy.newsletter_image_threshold = 2;
        let result = sanitize(
            "<img src=\"https://cdn.example/a.png\"><img src=\"https://cdn.example/b.png\">",
            &policy,
        );
        assert!(result.is_newsletter);
        assert_eq!(result.images.total(), 2);
    }

    #[test]
    fn sanitising_can_be_switched_off_for_diagnostics() {
        let policy = HtmlPolicy {
            sanitize: false,
            ..HtmlPolicy::default()
        };
        let result = sanitize("<p onclick=\"x\">hi</p>", &policy);
        assert!(result.html.contains("onclick"));
    }

    #[test]
    fn empty_input_yields_an_empty_result() {
        let result = sanitize("   ", &HtmlPolicy::default());
        assert!(result.html.is_empty());
        assert_eq!(result.images, ImageStats::default());
        assert!(!result.is_newsletter);
    }

    #[test]
    fn malformed_html_does_not_escape_the_sanitiser() {
        let result = sanitize(
            "<p>a<script>b</p></script><img src=x onerror=alert(1)>",
            &HtmlPolicy::default(),
        );
        assert!(!result.html.contains("onerror"));
        assert!(!result.html.to_ascii_lowercase().contains("<script"));
    }

    #[test]
    fn policy_follows_the_settings() {
        let mut settings = Settings::sanitized_default();
        settings.external_images = ExternalImagePolicy::Block;
        let policy = HtmlPolicy::from_settings(&settings, Some("ada@example.org"));
        assert!(!policy.allow_external_images);

        settings.external_images = ExternalImagePolicy::Allow;
        let policy = HtmlPolicy::from_settings(&settings, None);
        assert!(policy.allow_external_images);

        // `Ask` means "not until the user said so for this sender".
        settings.external_images = ExternalImagePolicy::Ask;
        let policy = HtmlPolicy::from_settings(&settings, Some("ada@example.org"));
        assert!(!policy.allow_external_images);

        settings.trusted_image_senders = vec!["ADA@example.org".to_string()];
        let policy = HtmlPolicy::from_settings(&settings, Some("ada@example.org"));
        assert!(policy.allow_external_images);
        assert!(HtmlPolicy::trusts(&settings, "ada@example.org"));
    }

    #[test]
    fn inline_svg_is_rasterised_and_never_handed_through() {
        use base64::prelude::{Engine as _, BASE64_STANDARD};
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"></svg>"#;
        let encoded = BASE64_STANDARD.encode(svg.as_bytes());
        let html =
            format!("<p>Logo</p><img src=\"data:image/svg+xml;base64,{encoded}\" alt=\"x\">");
        let result = sanitize(&html, &HtmlPolicy::default());
        assert!(
            result.html.contains("data:image/png;base64,"),
            "{}",
            result.html
        );
        assert!(
            !result.html.contains("image/svg+xml"),
            "the SVG document reached the reader: {}",
            result.html
        );

        // A broken SVG data URI is dropped, not passed through and never panics.
        let broken = sanitize(
            "<img src=\"data:image/svg+xml;base64,bm90IHN2Zw==\">",
            &HtmlPolicy::default(),
        );
        assert!(!broken.html.contains("image/svg+xml"), "{}", broken.html);
        assert!(!broken.html.contains("image/png"));
    }

    #[test]
    fn url_classification_is_case_insensitive() {
        assert_eq!(classify_url("CID:logo"), UrlKind::Inline);
        assert_eq!(classify_url("HTTPS://example.org/a.png"), UrlKind::External);
        assert_eq!(classify_url("//example.org/a.png"), UrlKind::External);
        assert_eq!(classify_url("/images/logo.png"), UrlKind::Relative);
        assert_eq!(classify_url(""), UrlKind::Relative);
    }
}
