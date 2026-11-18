//! The shipped icon set.
//!
//! `packaging/icons/hylmail.svg` is the source.  The PNG scales, the Windows
//! `.ico` and the macOS `.icns` lie committed next to it, so a release build
//! needs no rasteriser at all.  The generator lives here because it uses
//! exactly the `resvg`/`tiny-skia` stack the client already ships for inline
//! SVG images:
//!
//! ```text
//! cargo test -p hylmail-core --test icon_assets -- --ignored regenerate_the_icon_set
//! ```
//!
//! The test that runs on every `cargo test` reads the committed rasters back
//! and checks them against the sizes the packaging templates ask for, so an
//! edited SVG that was never re-rendered cannot pass unnoticed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use resvg::{tiny_skia, usvg};

/// Every PNG scale that ships.  16 px is the Windows file-association size,
/// 1024 px feeds the macOS `ic10` entry.
const PNG_SCALES: [u32; 9] = [16, 24, 32, 48, 64, 128, 256, 512, 1024];

/// The sizes inside the Windows `.ico`.
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// The `.icns` entries: the four character type code and the pixel size.
///
/// `icp4`-`icp6` are the small PNG types, `ic07`-`ic10` the classic ones and
/// `ic11`-`ic14` their `@2x` counterparts.
const ICNS_ENTRIES: [(&str, u32); 11] = [
    ("icp4", 16),
    ("icp5", 32),
    ("icp6", 64),
    ("ic07", 128),
    ("ic08", 256),
    ("ic09", 512),
    ("ic10", 1024),
    ("ic11", 32),
    ("ic12", 64),
    ("ic13", 256),
    ("ic14", 512),
];

/// `packaging/icons`, resolved from this crate's manifest directory.
fn icons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons")
}

/// File name of one PNG scale.
fn png_name(size: u32) -> String {
    format!("hylmail-{size}.png")
}

/// Rasterises the icon source into a square bitmap of `size` pixels.
fn render(svg: &[u8], size: u32) -> tiny_skia::Pixmap {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).expect("the icon SVG parses");
    let declared = tree.size().width().max(tree.size().height());
    assert!(declared > 0.0, "the icon SVG declares a drawable size");
    let scale = size as f32 / declared;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("the bitmap is allocated");
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// Appends a little endian `u16`.
fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a little endian `u32`.
fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a big endian `u32`; `.icns` is an Apple container and stores its
/// lengths the other way round than `.ico`.
fn push_u32_be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Encodes a bitmap as the bottom-up 32 bit DIB an `.ico` entry stores.
///
/// That is a `BITMAPINFOHEADER` with pixel data in BGRA order and a 1 bit AND
/// mask behind it.  The mask is all zeroes because the alpha channel is the
/// only transparency Windows 10 and 11 look at, but the field has to be
/// present and row padded for the icon to stay valid.
fn dib(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    let (width, height) = (pixmap.width(), pixmap.height());
    let mask_row = (width.div_ceil(32)) * 4;
    let pixels = (width * height * 4) as usize;
    let mask = (mask_row * height) as usize;

    let mut out = Vec::with_capacity(40 + pixels + mask);
    push_u32(&mut out, 40);
    push_u32(&mut out, width);
    push_u32(&mut out, height * 2);
    push_u16(&mut out, 1);
    push_u16(&mut out, 32);
    push_u32(&mut out, 0);
    push_u32(&mut out, (pixels + mask) as u32);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);

    for y in (0..height).rev() {
        for x in 0..width {
            let pixel = pixmap.pixel(x, y).expect("a pixel inside the bitmap");
            out.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
        }
    }
    out.resize(out.len() + mask, 0);
    out
}

/// Wraps DIB payloads into an `.ico` container.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, 0);
    push_u16(&mut out, 1);
    push_u16(&mut out, images.len() as u16);

    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in images {
        // 256 px is stored as 0; every entry is 32 bit with an alpha channel.
        let side = u8::try_from(*size % 256).expect("an icon side below 256");
        out.extend_from_slice(&[side, side, 0, 0]);
        push_u16(&mut out, 1);
        push_u16(&mut out, 32);
        push_u32(&mut out, data.len() as u32);
        push_u32(&mut out, offset);
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(data);
    }
    out
}

/// Wraps PNG payloads into an `.icns` container.
fn icns(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (kind, data) in entries {
        body.extend_from_slice(kind.as_bytes());
        push_u32_be(&mut body, (data.len() + 8) as u32);
        body.extend_from_slice(data);
    }

    let mut out = b"icns".to_vec();
    push_u32_be(&mut out, (body.len() + 8) as u32);
    out.extend_from_slice(&body);
    out
}

/// Reads a `u32` out of a byte slice, little endian.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Rewrites every raster in `packaging/icons` from `hylmail.svg`.
#[test]
#[ignore = "writes the committed icon set; run it after editing the SVG"]
fn regenerate_the_icon_set() {
    let dir = icons_dir();
    let svg = std::fs::read(dir.join("hylmail.svg")).expect("the icon source");

    for size in PNG_SCALES {
        let png = render(&svg, size).encode_png().expect("the PNG encodes");
        std::fs::write(dir.join(png_name(size)), png).expect("the PNG is written");
    }

    let icons = ICO_SIZES
        .iter()
        .map(|size| (*size, dib(&render(&svg, *size))))
        .collect::<Vec<_>>();
    std::fs::write(dir.join("hylmail.ico"), ico(&icons)).expect("the ICO is written");

    let pictures = ICNS_ENTRIES
        .iter()
        .map(|(kind, size)| {
            (
                *kind,
                render(&svg, *size).encode_png().expect("the PNG encodes"),
            )
        })
        .collect::<Vec<_>>();
    std::fs::write(dir.join("HylMail.icns"), icns(&pictures)).expect("the ICNS is written");

    the_committed_icon_set_is_complete();
}

/// The committed rasters exist, have the promised sizes and are not blank.
#[test]
fn the_committed_icon_set_is_complete() {
    let dir = icons_dir();
    assert!(dir.join("hylmail.svg").is_file(), "the source is committed");

    for size in PNG_SCALES {
        let name = png_name(size);
        let bytes = std::fs::read(dir.join(&name))
            .unwrap_or_else(|error| panic!("{name} is committed: {error}"));
        let pixmap = tiny_skia::Pixmap::decode_png(&bytes)
            .unwrap_or_else(|error| panic!("{name} is a PNG: {error}"));
        assert_eq!((pixmap.width(), pixmap.height()), (size, size), "{name}");

        if size == 64 {
            // A blank render (a missing font, a broken transform) would still
            // be a valid PNG of the right size.
            let mut opaque = 0;
            for y in 0..size {
                for x in 0..size {
                    if pixmap.pixel(x, y).expect("a pixel").alpha() > 0 {
                        opaque += 1;
                    }
                }
            }
            assert!(opaque > size * size / 2, "the icon is not mostly empty");
        }

        if size == 256 {
            // The middle of the tile is inside the envelope, so it has to be
            // the accent colour: a broken transform would move it elsewhere.
            let middle = pixmap.pixel(128, 128).expect("the centre pixel");
            assert_eq!(
                (middle.red(), middle.green(), middle.blue()),
                (0x4C, 0x8D, 0xFF)
            );
            assert_eq!(middle.alpha(), 0xFF);
        }
    }

    let ico = std::fs::read(dir.join("hylmail.ico")).expect("the Windows icon is committed");
    assert_eq!(&ico[0..4], &[0, 0, 1, 0], "hylmail.ico is an ICO");
    let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
    assert_eq!(count, ICO_SIZES.len(), "every scale is in the ICO");

    let mut declared = Vec::new();
    for index in 0..count {
        let entry = &ico[6 + 16 * index..6 + 16 * index + 16];
        let side = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        declared.push(side);

        let length = u32_at(entry, 8) as usize;
        let offset = u32_at(entry, 12) as usize;
        assert!(
            offset + length <= ico.len(),
            "entry {index} is inside the file"
        );
        assert_eq!(u32_at(&ico, offset), 40, "entry {index} is a DIB");
        assert_eq!(u32_at(&ico, offset + 4), side, "entry {index} width");
        assert_eq!(
            u32_at(&ico, offset + 8),
            side * 2,
            "entry {index} height + mask"
        );
        let planes = u16::from_le_bytes([ico[offset + 12], ico[offset + 13]]);
        let bits = u16::from_le_bytes([ico[offset + 14], ico[offset + 15]]);
        assert_eq!(planes, 1, "entry {index} has one plane");
        assert_eq!(bits, 32, "entry {index} is 32 bit");
    }
    assert_eq!(declared, ICO_SIZES.to_vec());

    let icns = std::fs::read(dir.join("HylMail.icns")).expect("the macOS icon is committed");
    assert_eq!(&icns[0..4], b"icns");
    assert_eq!(
        u32::from_be_bytes([icns[4], icns[5], icns[6], icns[7]]) as usize,
        icns.len(),
        "the ICNS length covers the whole file"
    );

    let mut found = Vec::new();
    let mut at = 8;
    while at + 8 <= icns.len() {
        let kind = std::str::from_utf8(&icns[at..at + 4]).expect("a type code");
        let length =
            u32::from_be_bytes([icns[at + 4], icns[at + 5], icns[at + 6], icns[at + 7]]) as usize;
        assert!(
            length >= 8 && at + length <= icns.len(),
            "{kind} stays inside the file"
        );
        let payload = &icns[at + 8..at + length];
        assert_eq!(&payload[1..4], b"PNG", "{kind} carries a PNG");
        found.push(kind.to_string());
        at += length;
    }
    assert_eq!(at, icns.len(), "the entries tile the file exactly");
    assert_eq!(
        found,
        ICNS_ENTRIES
            .iter()
            .map(|(kind, _)| (*kind).to_string())
            .collect::<Vec<_>>()
    );
}
