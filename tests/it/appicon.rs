// SPDX-License-Identifier: GPL-3.0-or-later
//! The embedded app icon files: they decode to the expected sizes, the dial picks the right size
//! for a scale factor, a damaged image fails soft, the header still renders (dark and light), and
//! the build script's Windows resource path works on this host.
use aitrack::appicon::*;

#[test]
fn the_embedded_pngs_decode_to_the_expected_sizes() {
    let w = decode(WINDOW_256).expect("window icon");
    assert_eq!(
        (w.width, w.height, w.pixels.len()),
        (256, 256, 256 * 256 * 4)
    );
    assert_eq!(
        w.pixels[3], 0,
        "transparent corner (the OS icon has its own rounded square)"
    );
    assert!(w.pixels[(128 * 256 + 128) * 4 + 3] == 255);
    let t = decode(TOAST_192).expect("toast icon");
    assert_eq!((t.width, t.height), (192, 192));
    for set in [&DIAL_DARK, &DIAL_LIGHT] {
        for (n, bytes) in set.iter() {
            let d = decode(bytes).unwrap_or_else(|| panic!("dial {n}"));
            assert_eq!((d.width, d.height), (*n, *n));
            // a dial only: transparent background, some opaque pixels
            assert_eq!(d.pixels[3], 0, "dial {n} corner");
            assert!(
                d.pixels.as_chunks::<4>().0.iter().any(|p| p[3] > 200),
                "dial {n} has a drawn part"
            );
        }
    }
}

#[test]
fn dark_and_light_dials_differ_in_colour_not_shape() {
    let (_, dark) = dial_for(false, 48.0);
    let (_, light) = dial_for(true, 48.0);
    let (d, l) = (decode(dark).unwrap(), decode(light).unwrap());
    assert_eq!((d.width, d.height), (l.width, l.height));
    let alpha = |i: &Rgba| {
        i.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3] as i32)
            .sum::<i32>()
    };
    assert!(
        (alpha(&d) - alpha(&l)).abs() < alpha(&d) / 10,
        "same silhouette"
    );
    assert_ne!(d.pixels, l.pixels);
}

#[test]
fn the_dial_size_follows_the_scale_factor() {
    // a 24 px logo: 1x -> 24, 1.25x -> 30 -> 32, 1.5x -> 36 -> 48, 2x -> 48, 3x -> 72 -> 96, beyond -> the largest
    for (ppp, want) in [
        (1.0, 24),
        (1.25, 32),
        (1.5, 48),
        (2.0, 48),
        (2.5, 64),
        (3.0, 96),
        (5.0, 96),
    ] {
        assert_eq!(dial_for(false, 24.0 * ppp).0, want, "ppp {ppp}");
        assert_eq!(dial_for(true, 24.0 * ppp).0, want, "ppp {ppp} light");
    }
}

#[test]
fn damaged_or_unsupported_images_fail_soft() {
    assert_eq!(decode(b""), None);
    assert_eq!(decode(b"not a png"), None);
    assert_eq!(decode(&WINDOW_256[..300]), None, "truncated");
    let mut bad = WINDOW_256.to_vec();
    let n = bad.len();
    bad[n / 2] ^= 0xFF;
    // corrupted data must not panic (it may or may not still decode)
    let _ = decode(&bad);
}

#[test]
fn the_exe_resource_files_exist_and_the_ico_holds_several_sizes() {
    let root = env!("CARGO_MANIFEST_DIR");
    let ico = std::fs::read(format!("{root}/assets/icon/aitrack.ico")).unwrap();
    assert_eq!(&ico[..4], &[0, 0, 1, 0], "ICO header");
    let count = u16::from_le_bytes([ico[4], ico[5]]);
    assert!(count >= 4, "a multi-size icon, got {count}");
    let rc = std::fs::read_to_string(format!("{root}/aitrack.rc")).unwrap();
    for needle in [
        "ICON",
        "ProductName",
        "aiTrack",
        "FileDescription",
        "VERSION_STR",
        "VERSION_NUM",
    ] {
        assert!(rc.contains(needle), "{needle}");
    }
}

#[cfg(feature = "gui")]
mod header {
    use crate::common::TempDir;
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use aitrack::store::JsonlStore;
    use aitrack::ui::AitrackUi;
    use eframe::egui::{self, epaint::Shape, Context, Pos2, RawInput, Rect};
    use serde_json::Value;
    use std::sync::Arc;

    fn images_drawn(light: bool) -> usize {
        let d = TempDir::new();
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            Arc::new(Settings::load(d.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        ));
        let ctx = Context::default();
        if light {
            ctx.set_theme(egui::ThemePreference::Light);
        }
        let mut ui = AitrackUi::new(&ctx, app);
        ui.sandbox();
        let mut count = 0;
        for i in 0..3 {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 400.0))),
                time: Some(i as f64 * 0.2),
                ..Default::default()
            };
            let out = ctx.run(raw, |c| ui.draw(c));
            count = 0;
            for cs in &out.shapes {
                if let Shape::Mesh(m) = &cs.shape {
                    // the logo is a textured quad that is not the font atlas
                    if m.texture_id != egui::TextureId::default() {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    #[test]
    fn the_header_draws_the_dial_image_in_both_themes() {
        assert!(
            images_drawn(false) >= 1,
            "dark theme: the dial is drawn as an image"
        );
        assert!(images_drawn(true) >= 1, "light theme");
    }
}
