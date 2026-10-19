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

#[test]
fn the_demo_vendors_show_both_kinds_of_time() {
    // AITRACK_DEMO=1 previews the small window with these: one window runs out first, others last
    use chrono::{TimeZone, Utc};
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
    let vs = aitrack::vendor::demo_vendors(now);
    let status = aitrack::collector::Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    };
    let st = aitrack::state::State::loading(Default::default(), status);
    let home = view::build_home_for(&st, &vs, now);
    let times: Vec<Option<RowTime>> = home
        .vendors
        .iter()
        .flat_map(|v| v.rows.iter().map(|r| r.time.clone()))
        .collect();
    assert!(times.contains(&warn("10h 00m")), "{times:?}");
    assert!(times
        .iter()
        .flatten()
        .any(|t| t.text == view::INFINITY && !t.warn));
}

// ------------------------------------------------------------ Grok Bot pace

fn bot_at(
    now: chrono::DateTime<chrono::Utc>,
    pct: f64,
    age_min: i64,
) -> aitrack::grok_bot::BotReading {
    aitrack::grok_bot::BotReading {
        pct,
        resets_at: now + chrono::Duration::hours(50),
        read_at: now - chrono::Duration::minutes(age_min),
        expires_at: None,
    }
}

fn bot_gauge(now: chrono::DateTime<chrono::Utc>, pct: f64, rate: f64) -> Gauge {
    let mut g = gauge(pct, Some(50.0), Some(rate), Some((100.0 - pct) / rate));
    g.resets_at = Some((now + chrono::Duration::hours(50)).to_rfc3339());
    g.projected_pct = Some(pct + rate * 50.0);
    g
}

#[test]
fn the_grok_bot_row_gets_the_pace_of_its_saved_readings() {
    use chrono::{TimeZone, Utc};
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
    // 60% and 2 pts/h: empty in 20h, before the reset in 50h
    let g = bot_gauge(now, 60.0, 2.0);
    let row = view::bot_row(&bot_at(now, 60.0, 5), Some(&g), false, now);
    assert_eq!(row.time, warn("20h 00m"));
    assert!(
        row.tip.iter().any(|t| t.text.starts_with("Pace: +2.0")),
        "{:?}",
        row.tip
    );
    // slow: lasts past the reset
    let g = bot_gauge(now, 10.0, 0.5);
    let row = view::bot_row(&bot_at(now, 10.0, 5), Some(&g), false, now);
    assert_eq!(row.time.map(|t| t.text), Some(view::INFINITY.to_string()));
    // an old reading gets no pace, and neither does a gauge for another weekly period
    let g = bot_gauge(now, 60.0, 2.0);
    assert_eq!(
        view::bot_row(&bot_at(now, 60.0, 120), Some(&g), false, now).time,
        None
    );
    let mut other = g.clone();
    other.resets_at = Some((now + chrono::Duration::hours(10)).to_rfc3339());
    assert_eq!(
        view::bot_row(&bot_at(now, 60.0, 5), Some(&other), false, now).time,
        None
    );
    assert_eq!(
        view::bot_row(&bot_at(now, 60.0, 5), None, false, now).time,
        None
    );
    // the detail pane shows the same pace
    let d = view::build_bot_detail_with(&bot_at(now, 60.0, 5), None, Some(&g), false, now);
    assert!(
        d.rows.iter().any(|(k, v)| k == "Pace" && v == "+2.0 pts/h"),
        "{:?}",
        d.rows
    );
}
