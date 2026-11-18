//! SVG rasterisation for attachments and inline images.
//!
//! `<img src="data:image/svg+xml;base64,…">` and an `.svg` attachment both end
//! up here: the reading pane shows a bitmap, the SVG itself is never handed to
//! the UI.  That is not a cosmetic decision - an SVG is a document that can
//! contain scripts and external references, so the rasteriser is the only
//! component that ever looks at one.
//!
//! Two limits keep a hostile file from taking the process down: the input is
//! capped at [`MAX_SVG_BYTES`] and the raster at [`MAX_RASTER_SIDE`] pixels per
//! side, so a document that declares a 100000×100000 canvas becomes a small
//! image instead of a failed allocation.

use base64::prelude::{Engine as _, BASE64_STANDARD};
use resvg::tiny_skia;
use resvg::usvg;

use crate::error::{Error, Result};

/// Largest SVG document that is still rendered.
pub const MAX_SVG_BYTES: usize = 8 * 1024 * 1024;

/// Largest edge of the produced bitmap, in pixels.
pub const MAX_RASTER_SIDE: u32 = 2048;

/// Whether a media type is an SVG document.
#[must_use]
pub fn is_svg(mime_type: &str) -> bool {
    mime_type.trim().eq_ignore_ascii_case("image/svg+xml")
}

/// Rasterises an SVG document with the default size limit.
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the document is empty, larger than
/// [`MAX_SVG_BYTES`] or not valid SVG, and [`Error::Unsupported`] when the
/// document has no drawable size.
pub fn rasterise_svg(data: &[u8]) -> Result<Vec<u8>> {
    rasterise_svg_limited(data, MAX_RASTER_SIDE)
}

/// Rasterises an SVG document, scaling it down to `max_side` pixels.
///
/// The image is never upscaled: a 16×16 icon stays 16×16.
///
/// # Errors
///
/// See [`rasterise_svg`].
pub fn rasterise_svg_limited(data: &[u8], max_side: u32) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Err(malformed("the SVG document is empty"));
    }
    if data.len() > MAX_SVG_BYTES {
        return Err(malformed(format!(
            "the SVG document is {} bytes, the limit is {MAX_SVG_BYTES}",
            data.len()
        )));
    }
    if max_side == 0 {
        return Err(Error::Unsupported(
            "an SVG cannot be rasterised to zero pixels".to_string(),
        ));
    }

    let tree = usvg::Tree::from_data(data, &usvg::Options::default())
        .map_err(|error| malformed(format!("the SVG could not be parsed: {error}")))?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
        return Err(Error::Unsupported(format!(
            "the SVG declares no drawable size ({width}x{height})"
        )));
    }

    let scale = (max_side as f32 / width.max(height)).min(1.0);
    let target_width = ((width * scale).ceil() as u32).max(1);
    let target_height = ((height * scale).ceil() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(target_width, target_height).ok_or_else(|| {
        Error::Unsupported(format!(
            "a {target_width}x{target_height} bitmap could not be allocated"
        ))
    })?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| malformed(format!("the bitmap could not be encoded: {error}")))
}

/// Encodes bytes as an RFC 2397 `data:` URI.
///
/// Used for inline images the client already holds, so that the reading pane
/// can render them without the UI ever touching the filesystem.
#[must_use]
pub fn data_uri(mime_type: &str, bytes: &[u8]) -> String {
    format!(
        "data:{};base64,{}",
        mime_type.trim(),
        BASE64_STANDARD.encode(bytes)
    )
}

/// The media type of a `data:` URI, if it has one.
#[must_use]
pub fn data_uri_mime(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("data:")?;
    let head = rest.split(',').next()?;
    let mime = head.split(';').next()?;
    (!mime.is_empty()).then_some(mime)
}

/// Rasterises an inline `data:image/svg+xml;base64,…` URI into a PNG data URI.
///
/// The reading pane must never receive an SVG document, because an SVG can
/// carry scripts and external references.  This is the single translation step:
/// the SVG is decoded, rendered to a bitmap by [`rasterise_svg`], and handed
/// back as `data:image/png;base64,…`.
///
/// Returns `None` when the URI is not a base64 SVG, when it is malformed or
/// when the document cannot be rasterised; the caller drops the image in that
/// case rather than passing the document through.
#[must_use]
pub fn rasterise_data_uri(uri: &str) -> Option<String> {
    if !is_svg(data_uri_mime(uri)?) {
        return None;
    }
    let (meta, payload) = uri.split_once(',')?;
    if !meta.to_ascii_lowercase().contains("base64") {
        return None;
    }
    let bytes = BASE64_STANDARD.decode(payload.trim()).ok()?;
    let png = rasterise_svg(&bytes).ok()?;
    Some(data_uri("image/png", &png))
}

fn malformed(message: impl Into<String>) -> Error {
    Error::MalformedResponse {
        protocol: "SVG",
        message: message.into(),
    }
}

/// A decoded raster image, ready to be uploaded as a texture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Eight-bit RGBA samples, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    /// Size of the pixel buffer in bytes, for the session cache accounting.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// Limits applied when fetching remote images (`FR-READ-3`).
#[derive(Debug, Clone, Copy)]
pub struct ImageLimits {
    /// Deadline for one download.
    pub timeout: std::time::Duration,
    /// Largest accepted body, in bytes.
    pub per_image_bytes: u64,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(30),
            per_image_bytes: 15 * 1024 * 1024,
        }
    }
}

/// Recognises an image by its **magic bytes**, not its declared type (`FR-READ-4`).
///
/// Returns the media type the bytes really are, or `None` when they are not a
/// raster image this build can show.  The declared `Content-Type` and the file
/// name are deliberately ignored: both are attacker controlled.
#[must_use]
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if data.starts_with(PNG) {
        return Some("image/png");
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if data.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Decodes a raster image into RGBA samples.
///
/// The format is taken from the bytes themselves; an image whose format this
/// build does not carry a codec for is reported as [`Error::Unsupported`].
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the bytes are not a decodable image and
/// [`Error::Unsupported`] when the format has no codec.
pub fn decode(data: &[u8]) -> Result<DecodedImage> {
    if data.is_empty() {
        return Err(malformed("the image is empty"));
    }
    let format = sniff(data).ok_or_else(|| {
        Error::Unsupported("the bytes are not a raster image this build can show".to_string())
    })?;
    if !matches!(format, "image/png" | "image/jpeg") {
        return Err(Error::Unsupported(format!(
            "no codec for {format} is built in"
        )));
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| malformed(format!("the image could not be read: {error}")))?;
    let decoded = reader
        .decode()
        .map_err(|error| malformed(format!("the image could not be decoded: {error}")))?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedImage {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// Downloads and decodes a set of remote images in parallel (`FR-READ-2/3`).
///
/// Only `http(s)` URLs are followed, every download is bounded by
/// [`ImageLimits::timeout`], a body larger than [`ImageLimits::per_image_bytes`]
/// is dropped, and each body is decoded from its own magic bytes.  A URL that
/// fails for any reason is simply absent from the result: one broken image must
/// not stop the others.
pub async fn fetch_all(
    urls: &[String],
    tls: &crate::net::TlsPolicy,
    limits: ImageLimits,
) -> Vec<(String, DecodedImage)> {
    if urls.is_empty() {
        return Vec::new();
    }
    let Ok(client) = crate::soap::http::build_client(tls, &crate::net::Timeouts::default()) else {
        return Vec::new();
    };
    let futures = urls.iter().cloned().map(|url| {
        let client = client.clone();
        async move {
            let bytes = fetch_one(&client, &url, limits).await?;
            let decoded = decode(&bytes).ok()?;
            Some((url, decoded))
        }
    });
    futures_util::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// One bounded download; `None` on any failure.
async fn fetch_one(client: &reqwest::Client, url: &str, limits: ImageLimits) -> Option<Vec<u8>> {
    let absolute = if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        return None;
    };
    let mut response = tokio::time::timeout(limits.timeout, client.get(&absolute).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout(limits.timeout, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() as u64 > limits.per_image_bytes {
                    return None;
                }
            }
            Ok(Ok(None)) => break,
            _ => return None,
        }
    }
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16">
        <rect width="32" height="16" fill="#c83232"/></svg>"##;

    /// Parses the IHDR header of a PNG to check the produced size.
    fn png_dimensions(png: &[u8]) -> (u32, u32) {
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (width, height)
    }

    #[test]
    fn an_svg_becomes_a_png_of_its_own_size() {
        let png = rasterise_svg(SQUARE.as_bytes()).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(png_dimensions(&png), (32, 16));
    }

    #[test]
    fn large_svgs_are_scaled_down_but_not_upscaled() {
        let big = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="2000">
            <rect width="4000" height="2000" fill="blue"/></svg>"#;
        let png = rasterise_svg(big.as_bytes()).unwrap();
        assert_eq!(png_dimensions(&png).0, MAX_RASTER_SIDE);
        assert_eq!(png_dimensions(&png).1, MAX_RASTER_SIDE / 2);

        let small = rasterise_svg_limited(SQUARE.as_bytes(), 1024).unwrap();
        assert_eq!(png_dimensions(&small), (32, 16));
    }

    #[test]
    fn svg_text_is_rendered_when_the_fonts_are_available() {
        let with_text = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="20">
            <text x="2" y="15" font-family="sans-serif" font-size="14">HylMail</text></svg>"#;
        // Whether a system font is found depends on the machine; what matters
        // is that rendering text never fails.
        assert!(rasterise_svg(with_text.as_bytes()).is_ok());
    }

    #[test]
    fn broken_input_is_reported_not_panicked() {
        assert!(rasterise_svg(b"").is_err());
        assert!(rasterise_svg(b"not svg at all").is_err());
        assert!(rasterise_svg(b"<svg>geheim</svg").is_err());
        let huge = vec![b'x'; MAX_SVG_BYTES + 1];
        assert!(rasterise_svg(&huge).is_err());
        assert!(rasterise_svg_limited(SQUARE.as_bytes(), 0).is_err());
    }

    #[test]
    fn an_svg_without_a_declared_size_gets_the_default_canvas() {
        // usvg falls back to 100x100 when the root declares neither width nor
        // height; that is a rendered image, not an error.
        let png = rasterise_svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>").unwrap();
        assert_eq!(png_dimensions(&png), (100, 100));
    }

    #[test]
    fn errors_never_contain_the_document() {
        let error = rasterise_svg(b"<svg>geheim</svg").unwrap_err();
        assert!(
            !error.to_string().contains("geheim"),
            "the error quoted the document: {error}"
        );
        let mut huge = b"<svg>".to_vec();
        huge.extend(std::iter::repeat_n(b'x', MAX_SVG_BYTES));
        let error = rasterise_svg(&huge).unwrap_err();
        assert!(!error.to_string().contains("xxxx"));
    }

    #[test]
    fn data_uris_round_trip_their_media_type() {
        let uri = data_uri("image/png", b"abc");
        assert_eq!(uri, "data:image/png;base64,YWJj");
        assert_eq!(data_uri_mime(&uri), Some("image/png"));
        assert_eq!(data_uri_mime("https://example.org/x.png"), None);
        assert_eq!(data_uri_mime("data:,"), None);
    }

    #[test]
    fn svg_media_types_are_recognised() {
        assert!(is_svg("image/svg+xml"));
        assert!(is_svg("IMAGE/SVG+XML"));
        assert!(!is_svg("image/png"));
    }

    #[test]
    fn an_inline_svg_data_uri_becomes_a_png_data_uri() {
        let encoded = BASE64_STANDARD.encode(SQUARE.as_bytes());
        let uri = format!("data:image/svg+xml;base64,{encoded}");
        let png_uri = rasterise_data_uri(&uri).unwrap();
        assert!(png_uri.starts_with("data:image/png;base64,"));
        let payload = png_uri.strip_prefix("data:image/png;base64,").unwrap();
        let png = BASE64_STANDARD.decode(payload).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(png_dimensions(&png), (32, 16));
    }

    #[test]
    fn only_base64_svg_uris_are_rasterised() {
        assert!(rasterise_data_uri("data:image/png;base64,AAAA").is_none());
        assert!(rasterise_data_uri("data:image/svg+xml,<svg/>").is_none());
        assert!(rasterise_data_uri("https://example.org/a.svg").is_none());
        assert!(rasterise_data_uri("data:image/svg+xml;base64,not*base64").is_none());
        // A base64 payload that is not valid SVG is dropped, not panicked.
        let bogus = BASE64_STANDARD.encode(b"nope");
        assert!(rasterise_data_uri(&format!("data:image/svg+xml;base64,{bogus}")).is_none());
    }

    #[test]
    fn magic_bytes_are_recognised_not_the_declared_type() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0rest"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"BM"), Some("image/bmp"));
        assert_eq!(sniff(b"not an image at all"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn a_png_is_decoded_to_rgba() {
        let mut png = Vec::new();
        let source = image::RgbaImage::from_pixel(2, 3, image::Rgba([10, 20, 30, 255]));
        source
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let decoded = decode(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 3));
        assert_eq!(decoded.rgba.len(), 2 * 3 * 4);
        assert_eq!(&decoded.rgba[..4], &[10, 20, 30, 255]);
        assert_eq!(decoded.byte_len(), 24);
    }

    #[test]
    fn a_non_image_is_reported_not_panicked() {
        assert!(decode(b"").is_err());
        assert!(decode(b"definitely not an image").is_err());
        // A GIF is recognised by magic bytes but has no codec in this build.
        assert!(matches!(
            decode(b"GIF89a...."),
            Err(crate::error::Error::Unsupported(_))
        ));
    }
}
