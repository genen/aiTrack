// SPDX-License-Identifier: GPL-3.0-or-later
//! Window position memory: the validate/clamp rules over monitor lists, the save tracker, and
//! round-trip persistence in settings.json.
use crate::common;
use aitrack::placement::*;
use aitrack::settings::{Settings, WindowPos};
use common::TempDir;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn mon(x: f64, y: f64, w: f64, h: f64, scale: f64, primary: bool) -> Monitor {
    Monitor {
        x,
        y,
        w,
        h,
        scale,
        primary,
    }
}

fn pos(x: f64, y: f64) -> WindowPos {
    WindowPos {
        x,
        y,
        width: Some(310.0),
        scale: Some(1.0),
    }
}

fn one() -> Vec<Monitor> {
    vec![mon(0.0, 0.0, 1920.0, 1040.0, 1.0, true)]
}

#[test]
fn a_position_on_screen_is_restored_as_saved() {
    let p = restore(&pos(1210.0, 96.0), &one()).unwrap();
    assert_eq!((p.x, p.y, p.width), (1210.0, 96.0, 310.0));
}

#[test]
fn sentinel_and_nonsense_positions_are_ignored() {
    for (x, y) in [
        (-32000.0, -32000.0),
        (-32000.0, 10.0),
        (10.0, -32000.0),
        (f64::NAN, 5.0),
        (99999.0, 5.0),
    ] {
        assert!(is_sentinel(x, y), "{x} {y}");
        assert_eq!(restore(&pos(x, y), &one()), None);
    }
    assert!(!is_sentinel(-1500.0, 20.0)); // a monitor to the left is legitimate
}

#[test]
fn off_screen_positions_are_rejected_single_and_multi_monitor() {
    assert_eq!(restore(&pos(2500.0, 100.0), &one()), None); // the second monitor was unplugged
    assert_eq!(restore(&pos(100.0, 1500.0), &one()), None);
    assert_eq!(restore(&pos(-1500.0, 100.0), &one()), None);
    let two = vec![
        mon(0.0, 0.0, 1920.0, 1040.0, 1.0, true),
        mon(1920.0, 0.0, 1920.0, 1040.0, 1.0, false),
    ];
    let p = restore(&pos(2500.0, 100.0), &two).unwrap();
    assert_eq!((p.x, p.y), (2500.0, 100.0));
    // a monitor to the left of the primary (negative coordinates)
    let left = vec![
        mon(0.0, 0.0, 1920.0, 1040.0, 1.0, true),
        mon(-1920.0, 0.0, 1920.0, 1040.0, 1.0, false),
    ];
    assert_eq!(restore(&pos(-1500.0, 100.0), &left).unwrap().x, -1500.0);
    // in the gap of an L-shaped layout: not on any monitor
    let l = vec![
        mon(0.0, 0.0, 1920.0, 1040.0, 1.0, true),
        mon(1920.0, 600.0, 1920.0, 480.0, 1.0, false),
    ];
    assert_eq!(restore(&pos(2500.0, 50.0), &l), None);
}

#[test]
fn a_partly_visible_window_is_nudged_back_inside() {
    // 120 px of the window still shows on the right edge: kept, moved fully inside
    let p = restore(&pos(1800.0, 100.0), &one()).unwrap();
    assert_eq!(p.x, 1920.0 - 310.0);
    // above the top edge but mostly visible: the title bar is brought back
    let p = restore(&pos(100.0, -60.0), &one()).unwrap();
    assert_eq!(p.y, 0.0);
    // only a sliver (30 px) visible: rejected
    assert_eq!(restore(&pos(1890.0, 100.0), &one()), None);
    // bottom: the title bar must be visible too
    assert_eq!(restore(&pos(100.0, 1030.0), &one()), None);
    let p = restore(&pos(100.0, 1000.0), &one()).unwrap();
    assert_eq!(p.y, 1040.0 - 240.0);
}

#[test]
fn width_is_clamped_and_missing_fields_use_defaults() {
    let w = |width: Option<f64>| WindowPos {
        x: 10.0,
        y: 10.0,
        width,
        scale: None,
    };
    assert_eq!(restore(&w(None), &one()).unwrap().width, 310.0);
    assert_eq!(restore(&w(Some(50.0)), &one()).unwrap().width, MIN_WIDTH);
    assert_eq!(restore(&w(Some(99999.0)), &one()).unwrap().width, MAX_WIDTH);
}

#[test]
fn logical_points_convert_with_the_saved_scale_factor() {
    // saved on a 200 % screen at (100, 100) points = (200, 200) pixels, width 310 points = 620 px
    let saved = WindowPos {
        x: 100.0,
        y: 100.0,
        width: Some(310.0),
        scale: Some(2.0),
    };
    let p = restore(&saved, &one()).unwrap();
    assert_eq!((p.x, p.y, p.width), (200.0, 200.0, 620.0));
    // the viewport builder multiplies by the primary monitor's scale: divide by it
    let hi = vec![mon(0.0, 0.0, 3840.0, 2080.0, 2.0, true)];
    let ((x, y), w) = builder_points(&p, &hi);
    assert_eq!((x, y, w), (100.0, 100.0, 310.0));
    let ((x, _), _) = builder_points(
        &p,
        &[
            mon(0.0, 0.0, 1920.0, 1040.0, 1.0, false),
            mon(1920.0, 0.0, 3840.0, 2080.0, 1.5, true),
        ],
    );
    assert!((x - 200.0 / 1.5).abs() < 0.01); // the primary one counts
                                             // a saved place that no longer fits a smaller screen is dropped
    let saved = WindowPos {
        x: 1500.0,
        y: 100.0,
        width: Some(310.0),
        scale: Some(2.0),
    };
    assert_eq!(restore(&saved, &one()), None);
}

#[test]
fn nothing_is_restored_when_the_monitors_are_unknown() {
    assert_eq!(restore(&pos(100.0, 100.0), &[]), None);
}

// ---- the tracker ---------------------------------------------------------------------------

fn sample(x: f64, y: f64) -> Sample {
    Sample {
        x,
        y,
        width: 310.0,
        scale: 1.0,
        minimized: false,
    }
}

#[test]
fn a_move_is_saved_once_after_it_settles_not_on_every_frame() {
    let t0 = Instant::now();
    let mut t = Tracker::with_saved(Some(&pos(100.0, 100.0)));
    // unchanged: nothing, ever
    for i in 0..10 {
        assert_eq!(
            t.observe(
                t0 + Duration::from_millis(100 * i),
                Some(sample(100.0, 100.0))
            ),
            None
        );
    }
    // dragging: a new place every frame, nothing saved yet
    for i in 0..20 {
        assert_eq!(
            t.observe(
                t0 + Duration::from_secs(2) + Duration::from_millis(16 * i),
                Some(sample(200.0 + i as f64, 100.0))
            ),
            None
        );
    }
    assert!(t.pending());
    // let go: saved after the settle time, once
    let rest = t0 + Duration::from_millis(2400);
    assert_eq!(t.observe(rest, Some(sample(219.0, 100.0))), None);
    assert_eq!(
        t.observe(rest + SETTLE / 2, Some(sample(219.0, 100.0))),
        None
    );
    let saved = t.observe(
        rest + SETTLE + Duration::from_millis(10),
        Some(sample(219.0, 100.0)),
    );
    assert_eq!(saved.map(|s| s.x), Some(219.0));
    assert_eq!(
        t.observe(rest + Duration::from_secs(5), Some(sample(219.0, 100.0))),
        None
    );
    assert!(!t.pending());
}

#[test]
fn a_minimised_window_and_the_sentinel_are_never_saved() {
    let t0 = Instant::now();
    let mut t = Tracker::default();
    let minimised = Sample {
        minimized: true,
        ..sample(300.0, 300.0)
    };
    for i in 0..5 {
        assert_eq!(
            t.observe(t0 + Duration::from_secs(2 * i), Some(minimised)),
            None
        );
        assert_eq!(
            t.observe(
                t0 + Duration::from_secs(2 * i),
                Some(sample(-32000.0, -32000.0))
            ),
            None
        );
        assert_eq!(t.observe(t0 + Duration::from_secs(2 * i), None), None);
    }
    assert!(!t.pending());
    assert_eq!(t.flush(), None);
    // a real place first, then minimising: the last real place is what exit would save
    assert_eq!(t.observe(t0, Some(sample(400.0, 50.0))), None);
    assert_eq!(
        t.observe(t0 + Duration::from_secs(1), Some(minimised)),
        None
    );
    assert_eq!(
        t.observe(
            t0 + Duration::from_secs(2),
            Some(sample(-32000.0, -32000.0))
        ),
        None
    );
    assert_eq!(t.flush().map(|s| (s.x, s.y)), Some((400.0, 50.0)));
    assert_eq!(t.flush(), None);
}

#[test]
fn resizing_the_width_counts_and_sub_point_jitter_does_not() {
    let t0 = Instant::now();
    let mut t = Tracker::with_saved(Some(&pos(100.0, 100.0)));
    assert_eq!(t.observe(t0, Some(sample(100.2, 99.8))), None);
    assert!(!t.pending());
    let wide = Sample {
        width: 420.0,
        ..sample(100.0, 100.0)
    };
    assert_eq!(t.observe(t0, Some(wide)), None);
    assert_eq!(
        t.observe(t0 + SETTLE + Duration::from_millis(1), Some(wide))
            .map(|s| s.width),
        Some(420.0)
    );
}

#[test]
fn the_reset_button_does_not_save_the_default_spot_again() {
    let t0 = Instant::now();
    let mut t = Tracker::with_saved(Some(&pos(900.0, 300.0)));
    t.mark_saved(Some(sample(80.0, 80.0)));
    assert_eq!(t.observe(t0, Some(sample(80.0, 80.0))), None);
    assert_eq!(
        t.observe(t0 + Duration::from_secs(5), Some(sample(80.0, 80.0))),
        None
    );
    assert!(!t.pending());
}

// ---- persistence ---------------------------------------------------------------------------

#[test]
fn the_place_round_trips_through_settings_json() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert_eq!(s.get().window, None);
    let w = WindowPos {
        x: 1210.5,
        y: 96.0,
        width: Some(333.0),
        scale: Some(1.5),
    };
    s.set_window(Some(w));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved["window"],
        json!({"x": 1210.5, "y": 96.0, "width": 333.0, "scale": 1.5})
    );
    assert_eq!(Settings::load(&path).get().window, Some(w));
    s.set_window(None);
    assert_eq!(Settings::load(&path).get().window, None);
}

#[test]
fn missing_or_corrupt_window_values_mean_the_default_placement() {
    let d = TempDir::new();
    let path = d.join("s.json");
    for bad in [
        r#"{"window": {"x": 5}}"#,
        r#"{"window": {"y": 5}}"#,
        r#"{"window": {"x": "left", "y": 5}}"#,
        r#"{"window": 7}"#,
        r#"{"window": null}"#,
        r#"{"window": {}}"#,
        r#"{}"#,
        "not json at all",
    ] {
        std::fs::write(&path, bad).unwrap();
        assert_eq!(Settings::load(&path).get().window, None, "{bad}");
    }
    // x and y are enough; width and scale are optional
    std::fs::write(&path, r#"{"window": {"x": 5, "y": 6}}"#).unwrap();
    let w = Settings::load(&path).get().window.unwrap();
    assert_eq!((w.x, w.y, w.width, w.scale), (5.0, 6.0, None, None));
    assert!(restore(&w, &one()).is_some());
}

// ---- macOS: points space, Cocoa screens flipped to a top-left origin ----

fn cocoa(frame_h: f64, v: (f64, f64, f64, f64), scale: f64) -> CocoaScreen {
    CocoaScreen {
        frame_h,
        visible: v,
        scale,
    }
}

/// 1440x900 Retina primary with a 25 pt menu bar, Dock hidden.
fn mac_primary() -> CocoaScreen {
    cocoa(900.0, (0.0, 0.0, 1440.0, 875.0), 2.0)
}

#[test]
fn cocoa_primary_is_flipped_below_the_menu_bar() {
    let m = cocoa_monitors(&[mac_primary()]);
    assert_eq!(m, vec![mon(0.0, 25.0, 1440.0, 875.0, 2.0, true)]);
}

#[test]
fn cocoa_second_display_left_has_negative_x_and_a_flipped_y() {
    // 1920x1080 at the left, bottom edges aligned: its visible top is 155 pt above the primary's
    let m = cocoa_monitors(&[
        mac_primary(),
        cocoa(1080.0, (-1920.0, 0.0, 1920.0, 1055.0), 1.0),
    ]);
    assert_eq!(m[1], mon(-1920.0, -155.0, 1920.0, 1055.0, 1.0, false));
    assert!(m[0].primary && !m[1].primary);
}

#[test]
fn cocoa_second_display_above_has_negative_y() {
    let m = cocoa_monitors(&[
        mac_primary(),
        cocoa(1080.0, (0.0, 900.0, 1920.0, 1080.0), 1.0),
    ]);
    assert_eq!((m[1].x, m[1].y), (0.0, -1080.0));
}

#[test]
fn cocoa_empty_or_degenerate_screens_give_nothing() {
    assert!(cocoa_monitors(&[]).is_empty());
    let m = cocoa_monitors(&[mac_primary(), cocoa(0.0, (0.0, 0.0, 0.0, 0.0), 2.0)]);
    assert_eq!(m.len(), 1);
}

fn retina_pos(x: f64, y: f64) -> WindowPos {
    WindowPos {
        x,
        y,
        width: Some(310.0),
        scale: Some(2.0),
    }
}

#[test]
fn points_are_restored_unscaled_on_a_retina_screen() {
    // the Windows rule would multiply by the saved scale of 2 and push this off the screen
    let monitors = cocoa_monitors(&[mac_primary()]);
    let p = restore_points(&retina_pos(1000.0, 100.0), &monitors).unwrap();
    assert_eq!((p.x, p.y, p.width), (1000.0, 100.0, 310.0));
    let ((x, y), w) = try_place(&retina_pos(1000.0, 100.0), &monitors, true).unwrap();
    assert_eq!((x, y, w), (1000.0, 100.0, 310.0));
}

#[test]
fn a_window_on_the_display_left_of_the_primary_is_restored() {
    let monitors = cocoa_monitors(&[
        mac_primary(),
        cocoa(1080.0, (-1920.0, 0.0, 1920.0, 1055.0), 1.0),
    ]);
    let ((x, y), _) = try_place(&retina_pos(-1500.0, -100.0), &monitors, true).unwrap();
    assert_eq!((x, y), (-1500.0, -100.0));
}

#[test]
fn a_window_on_the_display_above_is_restored_and_unplugging_it_discards() {
    let both = cocoa_monitors(&[
        mac_primary(),
        cocoa(1080.0, (0.0, 900.0, 1920.0, 1080.0), 1.0),
    ]);
    let saved = retina_pos(300.0, -700.0);
    assert!(try_place(&saved, &both, true).is_ok());
    let only = cocoa_monitors(&[mac_primary()]);
    assert_eq!(try_place(&saved, &only, true), Err(Discard::OffScreen));
}

#[test]
fn points_above_the_menu_bar_or_past_the_edge_are_nudged_inside() {
    let monitors = cocoa_monitors(&[mac_primary()]);
    // top-left partly under the menu bar and partly past the right edge
    let p = restore_points(&retina_pos(1300.0, 5.0), &monitors).unwrap();
    assert_eq!(p.x, 1440.0 - 310.0);
    assert_eq!(p.y, 25.0);
    // nearly off the bottom: pulled up so the whole assumed height shows
    let p = restore_points(&retina_pos(100.0, 850.0), &monitors).unwrap();
    assert_eq!(p.y, 900.0 - ASSUMED_HEIGHT);
}

#[test]
fn try_place_says_why_it_gave_up() {
    let monitors = cocoa_monitors(&[mac_primary()]);
    assert_eq!(
        try_place(&retina_pos(-32000.0, -32000.0), &monitors, true),
        Err(Discard::Sentinel)
    );
    assert_eq!(
        try_place(&retina_pos(100.0, 100.0), &[], true),
        Err(Discard::NoMonitors)
    );
    assert_eq!(
        try_place(&retina_pos(5000.0, 100.0), &monitors, true),
        Err(Discard::OffScreen)
    );
}

#[test]
fn try_place_keeps_the_windows_rule_in_pixel_space() {
    let saved = WindowPos {
        x: 600.0,
        y: 80.0,
        width: Some(310.0),
        scale: Some(1.5),
    };
    let m = vec![mon(0.0, 0.0, 2560.0, 1400.0, 1.5, true)];
    let want = builder_points(&restore(&saved, &m).unwrap(), &m);
    assert_eq!(try_place(&saved, &m, false), Ok(want));
}

#[test]
fn the_log_line_is_short_and_has_only_numbers() {
    let saved = retina_pos(1000.0, 100.0);
    let ok = try_place(&saved, &cocoa_monitors(&[mac_primary()]), true);
    let t = describe(Some(&saved), &ok);
    assert!(
        t.contains("restored") && t.contains("1000") && t.len() < 100,
        "{t}"
    );
    let t = describe(Some(&saved), &Err(Discard::NoMonitors));
    assert!(
        t.contains("discarded") && t.contains("no monitor list"),
        "{t}"
    );
    assert!(describe(None, &ok).contains("no saved position"));
}
