// SPDX-License-Identifier: GPL-3.0-or-later
//! The small window: its saved setting and the time column beside each meter.
use crate::common::TempDir;
use aitrack::analysis::Gauge;
use aitrack::settings::Settings;
use aitrack::view::{self, RowTime};
use serde_json::{json, Value};

fn gauge(pct: f64, hours_left: Option<f64>, rate: Option<f64>, to_limit: Option<f64>) -> Gauge {
    Gauge {
        pct,
        resets_at: None,
        hours_left,
        rate_per_hour: rate,
        projected_pct: None,
        hours_to_limit: to_limit,
        window_hours: 5.0,
        window_id: String::new(),
        resets_carried: false,
        reset_passed: false,
        rate_method: None,
    }
}

fn warn(text: &str) -> Option<RowTime> {
    Some(RowTime {
        text: text.into(),
        warn: true,
    })
}

#[test]
fn the_setting_defaults_to_off_and_persists() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert!(!s.get().small);
    s.set_small(true);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["small"], true);
    assert!(Settings::load(&path).get().small);
    // a wrong type leaves it as it was
    s.update(&json!({"small": "yes"}), true);
    assert!(s.get().small);
    s.set_small(false);
    assert!(!Settings::load(&path).get().small);
}

#[test]
fn runs_out_before_the_reset_shows_hours_and_minutes() {
    // 55% used, 3h20m to the reset, empty in 1.4h at this pace
    let g = gauge(55.0, Some(3.0 + 20.0 / 60.0), Some(32.0), Some(1.4));
    assert_eq!(view::row_time(&g, false), warn("1h 24m"));
    // a weekly window that runs out in over a day still reads in hours and minutes
    let g = gauge(60.0, Some(80.0), Some(1.2), Some(33.2));
    assert_eq!(view::row_time(&g, false), warn("33h 12m"));
    let g = gauge(90.0, Some(2.0), Some(30.0), Some(0.3));
    assert_eq!(view::row_time(&g, false), warn("18m"));
}

#[test]
fn lasting_past_the_reset_shows_infinity() {
    let inf = Some(RowTime {
        text: "\u{221e}".into(),
        warn: false,
    });
    assert_eq!(
        view::row_time(&gauge(20.0, Some(3.0), Some(5.0), Some(16.0)), false),
        inf
    );
    // no recent use: it cannot run out before the reset either
    assert_eq!(
        view::row_time(&gauge(20.0, Some(3.0), Some(0.0), None), false),
        inf
    );
}

#[test]
fn at_the_limit_and_unknown_pace() {
    assert_eq!(
        view::row_time(&gauge(100.0, Some(1.0), Some(0.0), None), false),
        warn("limit")
    );
    // no pace, or no reset time to compare with: no time
    assert_eq!(
        view::row_time(&gauge(20.0, Some(3.0), None, None), false),
        None
    );
    assert_eq!(
        view::row_time(&gauge(20.0, None, Some(5.0), Some(16.0)), false),
        None
    );
    // an old reading gets no pace-based time
    assert_eq!(
        view::row_time(&gauge(55.0, Some(3.0), Some(32.0), Some(1.4)), true),
        None
    );
}

#[test]
fn hours_and_minutes_format() {
    assert_eq!(view::hours_minutes(0.0), "1m");
    assert_eq!(view::hours_minutes(0.75), "45m");
    assert_eq!(view::hours_minutes(3.0 + 5.0 / 60.0), "3h 05m");
    assert_eq!(view::hours_minutes(49.5), "49h 30m");
}
