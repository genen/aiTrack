// SPDX-License-Identifier: GPL-3.0-or-later
//! The app icon files embedded in the binary (`assets/icon/`) and a small PNG decoder for them.
//!
//! * the full rounded-square icon (navy, orange / white / teal gauge) is for the window and taskbar
//!   icon and the generic toast logo; the Windows .exe icon is `aitrack.ico`, embedded by `build.rs`;
//! * the *dial* variants (the gauge alone, transparent background) are the logo in the window
//!   header: white parts for dark UI themes, navy for light ones, in several sizes so the header
//!   can pick the one closest to the screen's scale factor and stay crisp.
//!
//! Nothing here can fail loudly: a file that does not decode gives `None` and the caller falls back
//! to the old procedural logo.

/// The icon on its rounded-square background, 256 px (window / taskbar icon).
pub const WINDOW_256: &[u8] = include_bytes!("../assets/icon/aitrack-256.png");
/// The same, 192 px (the generic toast logo).
pub const TOAST_192: &[u8] = include_bytes!("../assets/icon/aitrack-192.png");

/// Header dials for dark UI themes, by pixel size.
pub const DIAL_DARK: [(u32, &[u8]); 5] = [
    (24, include_bytes!("../assets/icon/dial-24.png")),
    (32, include_bytes!("../assets/icon/dial-32.png")),
    (48, include_bytes!("../assets/icon/dial-48.png")),
    (64, include_bytes!("../assets/icon/dial-64.png")),
    (96, include_bytes!("../assets/icon/dial-96.png")),
];
/// Header dials for light UI themes (the white parts are navy).
pub const DIAL_LIGHT: [(u32, &[u8]); 5] = [
    (24, include_bytes!("../assets/icon/dial-light-24.png")),
    (32, include_bytes!("../assets/icon/dial-light-32.png")),
    (48, include_bytes!("../assets/icon/dial-light-48.png")),
    (64, include_bytes!("../assets/icon/dial-light-64.png")),
    (96, include_bytes!("../assets/icon/dial-light-96.png")),
];

/// A decoded image: straight (non-premultiplied) RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decode a PNG into RGBA8 (8-bit grey, grey+alpha, RGB and RGBA are understood). `None` for
/// anything else or for damaged data.
pub fn decode(bytes: &[u8]) -> Option<Rgba> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let data = &buf[..info.buffer_size()];
    let pixels: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (pixels.len() == info.width as usize * info.height as usize * 4).then_some(Rgba {
        width: info.width,
        height: info.height,
        pixels,
    })
}

/// The header dial for a UI theme that needs about `pixels` pixels: the smallest embedded size that
/// is at least that large (the largest when none is).
pub fn dial_for(light_theme: bool, pixels: f32) -> (u32, &'static [u8]) {
    let set = if light_theme { &DIAL_LIGHT } else { &DIAL_DARK };
    set.iter()
        .find(|(n, _)| *n as f32 >= pixels - 0.01)
        .copied()
        .unwrap_or(set[set.len() - 1])
}
