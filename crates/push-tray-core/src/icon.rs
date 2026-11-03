//! The icons' pixels: the tray glyph (two speech bubbles) in the taskbar's text colour with
//! a status badge, the menu's status dots, the app icon, and their PNG and ICO files.

use std::time::Duration;

use crate::glyph::{Bounds, Canvas, VIEW_BOX, glyph};
use crate::state::IconState;

/// The taskbar's theme: the glyph is dark on a light taskbar, white on a dark one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Theme {
    Light,
    Dark,
}

/// A frame of the pulsing badge: full or dimmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frame {
    Full,
    Dim,
}

/// How long a pulse frame shows: two frames make a one-second period.
pub const PULSE_FRAME: Duration = Duration::from_millis(500);

/// The dimmed frame's badge opacity.
const DIM_OPACITY: f64 = 0.4;

/// The badge's centre in view box units, its radius, and the transparent ring's.
const BADGE_CENTRE: (f64, f64) = (25.5, 25.5);
const BADGE_RADIUS: f64 = 6.0;
const KNOCK_OUT_RADIUS: f64 = 8.0;

/// The app icon's tile colour and corner radius.
const TILE: [u8; 3] = [0x2b, 0x5f, 0xd9];
const TILE_RADIUS: f64 = 7.0;
const WHITE: [u8; 3] = [0xff, 0xff, 0xff];

/// The glyph's colour on a `theme` taskbar, RGB.
pub fn glyph_color(theme: Theme) -> [u8; 3] {
    match theme {
        Theme::Light => [0x1b, 0x1b, 0x1b],
        Theme::Dark => WHITE,
    }
}

/// The badge's colour for `state`, RGB.
pub fn badge_color(state: IconState) -> [u8; 3] {
    match state {
        IconState::Normal => [0x2e, 0x9d, 0x4f],
        IconState::Busy | IconState::Transition => [0x3b, 0x7b, 0xe8],
        IconState::Paused => [0x8a, 0x5c, 0xd1],
        IconState::Waiting => [0xe8, 0xa3, 0x17],
        IconState::Disconnected => [0x8c, 0x8c, 0x8c],
        IconState::Error => [0xd1, 0x34, 0x38],
    }
}

/// Whether the badge pulses: in transitions, unless the owner asks for less motion.
pub fn animated(state: IconState, reduce_motion: bool) -> bool {
    state == IconState::Transition && !reduce_motion
}

/// The tray icon, `size`×`size` straight RGBA: the glyph in the `theme`'s colour and, with
/// a `badge`, a dot in its colour inside a transparent ring at the bottom right.
pub fn tray_rgba(theme: Theme, badge: Option<IconState>, frame: Frame, size: u32) -> Vec<u8> {
    let mut canvas = Canvas::new(size);
    glyph().draw(&mut canvas, glyph_color(theme), 1.0);
    if let Some(state) = badge {
        canvas.clear_circle(BADGE_CENTRE, KNOCK_OUT_RADIUS);
        let opacity = match frame {
            Frame::Full => 1.0,
            Frame::Dim => DIM_OPACITY,
        };
        canvas.fill_circle(BADGE_CENTRE, BADGE_RADIUS, badge_color(state), opacity);
    }
    canvas.into_rgba()
}

/// A menu item's status dot, `size`×`size` straight RGBA: a circle in the badge colour with
/// a 1 px transparent margin.
pub fn dot_rgba(state: IconState, size: u32) -> Vec<u8> {
    let mut canvas = Canvas::new(size);
    if size > 0 {
        let margin = VIEW_BOX / f64::from(size);
        let centre = VIEW_BOX / 2.0;
        canvas.fill_circle((centre, centre), centre - margin, badge_color(state), 1.0);
    }
    canvas.into_rgba()
}

/// The app and window icon, `size`×`size` straight RGBA: a rounded blue tile with the glyph
/// in white at 75 %, centred.
pub fn app_rgba(size: u32) -> Vec<u8> {
    let mut canvas = Canvas::new(size);
    let tile = Bounds {
        min_x: 0.0,
        min_y: 0.0,
        max_x: VIEW_BOX,
        max_y: VIEW_BOX,
    };
    canvas.fill_rounded_rect(tile, TILE_RADIUS, TILE, 1.0);
    let source = glyph();
    if let Some(bounds) = source.bounds() {
        let scale = 0.75;
        let centre = VIEW_BOX / 2.0;
        let dx = centre - (bounds.min_x + bounds.max_x) / 2.0 * scale;
        let dy = centre - (bounds.min_y + bounds.max_y) / 2.0 * scale;
        source
            .transformed(scale, dx, dy)
            .draw(&mut canvas, WHITE, 1.0);
    }
    canvas.into_rgba()
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 == 1 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &byte| {
        CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(usize::from(u16::MAX)).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let length = block.len() as u16;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A PNG file of the `size`×`size` straight RGBA `rgba`.
pub fn png(rgba: &[u8], size: u32) -> Vec<u8> {
    let row = size as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * size as usize);
    for line in rgba.chunks(row.max(1)).take(size as usize) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    png_chunk(&mut out, b"IHDR", &header);
    png_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// An icon image as an ICO entry: a 32-bit BMP with an empty AND mask.
fn bmp(rgba: &[u8], size: u32) -> Vec<u8> {
    let side = size as usize;
    let mask_row = side.div_ceil(32) * 4;
    let image = side * side * 4 + mask_row * side;
    let mut out = Vec::with_capacity(40 + image);
    for value in [40, size, size * 2] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for value in [0, image as u32, 0, 0, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for line in rgba.chunks(side * 4).rev() {
        for pixel in line.chunks(4) {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    out.resize(out.len() + mask_row * side, 0);
    out
}

/// An ICO file of the app icon ([`app_rgba`]) at each of `sizes` (1..=256): BMP images,
/// and a PNG at 256.
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&size| {
            let rgba = app_rgba(size);
            if size >= 256 {
                png(&rgba, size)
            } else {
                bmp(&rgba, size)
            }
        })
        .collect();
    let mut out = Vec::new();
    for value in [0u16, 1, sizes.len() as u16] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut offset = 6 + 16 * sizes.len();
    for (&size, image) in sizes.iter().zip(&images) {
        let side = if size >= 256 { 0 } else { size as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += image.len();
    }
    for image in images {
        out.extend_from_slice(&image);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [u32; 4] = [16, 20, 24, 32];

    fn pixel(pixels: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * size + x) * 4) as usize;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    /// The pixel at view box (`x`, `y`).
    fn at(pixels: &[u8], size: u32, x: f64, y: f64) -> [u8; 4] {
        let scale = f64::from(size) / VIEW_BOX;
        pixel(
            pixels,
            size,
            (x * scale).floor() as u32,
            (y * scale).floor() as u32,
        )
    }

    fn rgb(pixel: [u8; 4]) -> [u8; 3] {
        [pixel[0], pixel[1], pixel[2]]
    }

    fn opaque(color: [u8; 3]) -> [u8; 4] {
        [color[0], color[1], color[2], 0xff]
    }

    fn u32_le(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u16_le(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    #[test]
    fn the_tray_icon_shows_the_glyph_and_the_badge() {
        for size in SIZES {
            for theme in [Theme::Light, Theme::Dark] {
                let ink = glyph_color(theme);
                for state in IconState::ALL {
                    let badge = badge_color(state);
                    let full = tray_rgba(theme, Some(state), Frame::Full, size);
                    assert_eq!(full.len(), (size * size * 4) as usize);
                    assert_eq!(at(&full, size, 17.0, 18.0), opaque(ink), "{size} {state:?}");
                    assert_eq!(at(&full, size, 25.5, 25.5), opaque(badge));
                    assert_eq!(pixel(&full, size, 0, 0)[3], 0);
                    // At 32 px the ring clears every pixel the dot touches; smaller, edges blend.
                    for chunk in full.chunks(4).filter(|p| size == 32 && p[3] > 0) {
                        let colour = [chunk[0], chunk[1], chunk[2]];
                        assert!(colour == ink || colour == badge, "{colour:?}");
                    }
                    let dim = tray_rgba(theme, Some(state), Frame::Dim, size);
                    let centre = at(&dim, size, 25.5, 25.5);
                    assert_eq!(rgb(centre), badge);
                    assert!((99..=105).contains(&centre[3]), "{}", centre[3]);
                    if size == 32 {
                        assert_eq!(at(&full, size, 25.5, 18.5)[3], 0);
                    }
                }
                let plain = tray_rgba(theme, None, Frame::Full, size);
                // The badge's place is in the solid bubble's tail; small sizes blend its edge.
                let under = at(&plain, size, 25.5, 25.5);
                assert_eq!(rgb(under), ink);
                assert!(under[3] >= 250, "{size}: {}", under[3]);
                assert_eq!(pixel(&plain, size, 0, 0)[3], 0);
                assert!(
                    plain
                        .chunks(4)
                        .filter(|p| p[3] > 0)
                        .all(|p| [p[0], p[1], p[2]] == ink)
                );
                assert_eq!(plain, tray_rgba(theme, None, Frame::Dim, size));
            }
        }
    }

    #[test]
    fn the_themes_differ_only_in_the_glyph_colour() {
        for badge in [None, Some(IconState::Waiting)] {
            let light = tray_rgba(Theme::Light, badge, Frame::Full, 24);
            let dark = tray_rgba(Theme::Dark, badge, Frame::Full, 24);
            for (l, d) in light.chunks(4).zip(dark.chunks(4)) {
                assert_eq!(l[3], d[3]);
                if l[3] > 0 && [l[0], l[1], l[2]] == glyph_color(Theme::Light) {
                    assert_eq!([d[0], d[1], d[2]], glyph_color(Theme::Dark));
                } else {
                    assert_eq!(l, d);
                }
            }
        }
    }

    #[test]
    fn only_transitions_pulse() {
        for state in IconState::ALL {
            assert_eq!(animated(state, false), state == IconState::Transition);
            assert!(!animated(state, true));
        }
        assert_eq!(PULSE_FRAME * 2, Duration::from_secs(1));
    }

    #[test]
    fn badge_colours_are_distinct_except_transitions() {
        let mut colours: Vec<_> = IconState::ALL
            .iter()
            .filter(|state| **state != IconState::Transition)
            .map(|state| badge_color(*state))
            .collect();
        colours.sort();
        colours.dedup();
        assert_eq!(colours.len(), 6);
        assert_eq!(
            badge_color(IconState::Transition),
            badge_color(IconState::Busy)
        );
        assert!(!colours.contains(&glyph_color(Theme::Light)));
        assert!(!colours.contains(&glyph_color(Theme::Dark)));
    }

    #[test]
    fn a_dot_fills_the_image_with_the_badge_colour() {
        for size in [16, 20, 32] {
            let dot = dot_rgba(IconState::Error, size);
            assert_eq!(dot.len(), (size * size * 4) as usize);
            let middle = size / 2;
            assert_eq!(
                pixel(&dot, size, middle, middle),
                opaque(badge_color(IconState::Error))
            );
            assert_eq!(
                pixel(&dot, size, 2, middle),
                opaque(badge_color(IconState::Error))
            );
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert_eq!(pixel(&dot, size, x, y)[3], 0);
            }
            assert_eq!(pixel(&dot, size, 0, middle)[3], 0);
        }
    }

    #[test]
    fn the_app_icon_is_a_rounded_tile_with_a_white_glyph() {
        let app = app_rgba(32);
        assert_eq!(pixel(&app, 32, 0, 0)[3], 0);
        assert_eq!(pixel(&app, 32, 2, 16), opaque(TILE));
        assert!(app.chunks(4).any(|p| p == opaque(WHITE)));
        assert_eq!(app_rgba(256).len(), 256 * 256 * 4);
    }

    #[test]
    fn crc32_matches_the_png_reference() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_png_has_its_header_data_and_end() {
        let size = 20;
        let file = png(&dot_rgba(IconState::Normal, size), size);
        assert_eq!(&file[..8], &PNG_SIGNATURE);
        assert_eq!(&file[8..16], &[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        assert_eq!(&file[16..20], &size.to_be_bytes());
        assert_eq!(&file[20..24], &size.to_be_bytes());
        assert_eq!(&file[24..29], &[8, 6, 0, 0, 0]);
        let crc = u32::from_be_bytes(file[29..33].try_into().unwrap());
        assert_eq!(crc, crc32(&file[12..29]));
        assert!(file.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]));
        assert_eq!(&file[37..41], b"IDAT");
    }

    #[test]
    fn zlib_splits_into_stored_blocks() {
        let data = vec![7u8; 70_000];
        let stream = zlib_stored(&data);
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        assert_eq!(stream[2], 0);
        assert_eq!(u16_le(&stream, 3), 65535);
        assert_eq!(u16_le(&stream, 5), 0);
        let second = 7 + 65535;
        assert_eq!(stream[second], 1);
        assert_eq!(u16_le(&stream, second + 1), (70_000 - 65535) as u16);
        assert_eq!(stream.len(), 2 + 5 + 65535 + 5 + (70_000 - 65535) + 4);
        assert_eq!(
            zlib_stored(&[]),
            [0x78, 0x01, 1, 0, 0, 0xff, 0xff, 0, 0, 0, 1]
        );
    }

    #[test]
    fn an_ico_holds_bmps_and_a_png() {
        let file = ico(&[16, 32, 256]);
        assert_eq!(u16_le(&file, 0), 0);
        assert_eq!(u16_le(&file, 2), 1);
        assert_eq!(u16_le(&file, 4), 3);
        let mut images = Vec::new();
        for (index, side) in [16u8, 32, 0].into_iter().enumerate() {
            let entry = 6 + 16 * index;
            assert_eq!(file[entry], side);
            assert_eq!(file[entry + 1], side);
            assert_eq!(u16_le(&file, entry + 6), 32);
            let length = u32_le(&file, entry + 8) as usize;
            let offset = u32_le(&file, entry + 12) as usize;
            assert!(offset + length <= file.len());
            images.push(&file[offset..offset + length]);
        }
        assert_eq!(images[2][..8], PNG_SIGNATURE);
        let bmp = images[0];
        assert_eq!(u32_le(bmp, 0), 40);
        assert_eq!(u32_le(bmp, 4), 16);
        assert_eq!(u32_le(bmp, 8), 32);
        assert_eq!(u16_le(bmp, 14), 32);
        assert_eq!(bmp.len(), 40 + 16 * 16 * 4 + 4 * 16);
        // The bottom-left pixel comes first, BGRA: the tile's rounded corner is transparent.
        assert_eq!(bmp[43], 0);
        let rgba = app_rgba(16);
        let top_row_middle = 40 + (15 * 16 + 4) * 4;
        let source = pixel(&rgba, 16, 4, 0);
        assert_eq!(
            &bmp[top_row_middle..top_row_middle + 4],
            &[source[2], source[1], source[0], source[3]]
        );
    }
}
