//! The HTML pipeline of the reading pane.
//!
//! [`sanitize`] is the entry point: it takes the raw `text/html` body and the
//! user's [`HtmlPolicy`] and returns HTML that is safe to render, together with
//! a count of the images it saw.  [`html_to_text`] produces the plain text
//! alternative, [`make_snippet`] the message list preview.

mod sanitize;
mod text;

pub use sanitize::{sanitize, HtmlPolicy, ImageStats, SanitisedHtml};
pub use text::{
    escape, html_to_text, image_count, link_count, looks_like_html, make_snippet, plain_to_html,
    quote_reply, truncate, SNIPPET_LENGTH,
};
