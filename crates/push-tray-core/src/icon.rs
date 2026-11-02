//! The tray icon's pixels: a filled circle in the state's colour with a darker 1 px ring,
//! transparent outside.

use crate::state::IconState;

/// The state's colour, RGB.
pub fn color(state: IconState) -> [u8; 3] {
    match state {
        IconState::Normal => [0x2e, 0x9d, 0x4f],
        IconState::Busy => [0x2f, 0x6f, 0xdf],
        IconState::Waiting => [0xe8, 0xa3, 0x17],
        IconState::Paused => [0x8a, 0x5c, 0xd1],
        IconState::Disconnected => [0x8c, 0x8c, 0x8c],
        IconState::Error => [0xd1, 0x34, 0x38],
    }
}

/// The ring's colour: the state's colour, darker.
pub fn ring_color(state: IconState) -> [u8; 3] {
    color(state).map(|channel| (u16::from(channel) * 7 / 10) as u8)
}

/// A `size`×`size` RGBA image, rows top to bottom, 4 bytes per pixel.
pub fn rgba(state: IconState, size: u32) -> Vec<u8> {
    let side = size as usize;
    let mut pixels = vec![0u8; side * side * 4];
    let radius = f64::from(size) / 2.0;
    let fill = color(state);
    let ring = ring_color(state);
    for y in 0..side {
        for x in 0..side {
            let dx = x as f64 + 0.5 - radius;
            let dy = y as f64 + 0.5 - radius;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance > radius {
                continue;
            }
            let rgb = if distance > radius - 1.0 { ring } else { fill };
            let at = (y * side + x) * 4;
            pixels[at..at + 3].copy_from_slice(&rgb);
            pixels[at + 3] = 0xff;
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * size + x) * 4) as usize;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    #[test]
    fn the_image_has_four_bytes_per_pixel() {
        for size in [0, 1, 16, 32, 33] {
            assert_eq!(
                rgba(IconState::Normal, size).len(),
                (size * size * 4) as usize
            );
        }
    }

    #[test]
    fn the_centre_has_the_state_colour_and_the_corners_are_transparent() {
        for state in IconState::ALL {
            for size in [16, 32] {
                let pixels = rgba(state, size);
                let [r, g, b] = color(state);
                assert_eq!(pixel(&pixels, size, size / 2, size / 2), [r, g, b, 0xff]);
                for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                    assert_eq!(pixel(&pixels, size, x, y)[3], 0, "{state:?} {x},{y}");
                }
                let [r, g, b] = ring_color(state);
                assert_eq!(pixel(&pixels, size, size / 2, 0), [r, g, b, 0xff]);
            }
        }
    }

    #[test]
    fn the_six_colours_are_distinct() {
        let mut colours: Vec<_> = IconState::ALL.iter().map(|state| color(*state)).collect();
        colours.sort();
        colours.dedup();
        assert_eq!(colours.len(), 6);
        assert_eq!(color(IconState::Normal), [0x2e, 0x9d, 0x4f]);
        assert_eq!(color(IconState::Error), [0xd1, 0x34, 0x38]);
    }
}
