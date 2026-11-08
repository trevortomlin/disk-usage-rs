//! Colors. A calm, dark palette with saturated, evenly spaced branch hues.

use gpui::{Hsla, hsla, rgb};

pub fn bg() -> Hsla {
    rgb(0x0c0e13).into()
}
pub fn panel() -> Hsla {
    rgb(0x12151c).into()
}
pub fn raised() -> Hsla {
    rgb(0x181c25).into()
}
pub fn hover() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.045)
}
pub fn border() -> Hsla {
    rgb(0x222733).into()
}
pub fn border_strong() -> Hsla {
    rgb(0x2e3443).into()
}
pub fn text() -> Hsla {
    rgb(0xe6e9ef).into()
}
pub fn muted() -> Hsla {
    rgb(0x8b93a5).into()
}
pub fn faint() -> Hsla {
    rgb(0x5a6276).into()
}
pub fn accent() -> Hsla {
    rgb(0x7c9cff).into()
}
pub fn accent_soft() -> Hsla {
    hsla(226.0 / 360.0, 1.0, 0.74, 0.16)
}
pub fn danger() -> Hsla {
    rgb(0xff7a85).into()
}

/// Hues (degrees) spaced so neighbours in size order contrast well.
const HUES: [f32; 12] = [
    222.0, 162.0, 34.0, 328.0, 264.0, 96.0, 8.0, 190.0, 50.0, 292.0, 134.0, 246.0,
];

pub fn hue(branch: u32) -> f32 {
    HUES[branch as usize % HUES.len()] / 360.0
}

/// Solid swatch color for a top-level branch (list swatches, legend).
pub fn branch(branch: u32) -> Hsla {
    hsla(hue(branch), 0.68, 0.62, 1.0)
}

/// Fill for a leaf tile; `jitter` separates adjacent siblings slightly.
pub fn tile(branch: u32, depth: u8, jitter: u32) -> (Hsla, Hsla) {
    let h = hue(branch);
    let j = ((jitter % 5) as f32 - 2.0) * 0.022;
    let l = (0.60 - depth as f32 * 0.018 + j).clamp(0.42, 0.72);
    let s = (0.64 - depth as f32 * 0.02).max(0.45);
    (hsla(h, s, l + 0.07, 1.0), hsla(h, s, l - 0.07, 1.0))
}

/// Background of a directory frame.
pub fn frame(branch: u32, depth: u8) -> Hsla {
    hsla(
        hue(branch),
        0.30,
        (0.13 + depth as f32 * 0.02).min(0.26),
        1.0,
    )
}

pub fn frame_text(branch: u32) -> Hsla {
    hsla(hue(branch), 0.80, 0.84, 1.0)
}

pub fn tile_text(branch: u32) -> Hsla {
    hsla(hue(branch), 0.55, 0.10, 0.92)
}
