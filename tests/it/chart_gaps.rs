// SPDX-License-Identifier: GPL-3.0-or-later
//! History chart data: normal sample spacing stays one connected line, a long gap or a window
//! reset cuts it, and the y axis zooms in for low data. Every test takes an explicit `now`.
use aitrack::reading::{Kind, Reading};
use aitrack::view::{chart_data, chart_y_max, Range, CHART_GAP_SECS};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// A weekly reading `mins_ago` minutes before `now`, with a reset time when `reset` is given.
fn weekly(mins_ago: i64, pct: f64, reset: Option<DateTime<Utc>>) -> Reading {
    Reading::from_value(&json!({
        "ok": true, "ts": (now() - Duration::minutes(mins_ago)).to_rfc3339(),
        "session": null,
        "weekly": {"pct": pct, "resets_at": reset.map(|r| r.to_rfc3339())},
    }))
}

fn day(rs: &[Reading]) -> aitrack::view::ChartData {
    chart_data(rs, Kind::Weekly, Range::Day, now())
}

#[test]
fn uneven_sample_spacing_with_mixed_reset_stamps_is_one_polyline() {
    let reset = now() + Duration::days(5);
    // every 5-15 minutes for 24 h, alternating between a source that reports the reset time (the
    // cache) and one that does not (the history): used to be a run of one-point segments
    let mut rs = Vec::new();
    let (mut m, mut i) = (24 * 60 - 3, 0usize);
    while m > 0 {
        rs.push(weekly(
            m,
            8.0 + (i % 2) as f64,
            (i % 2 == 0).then_some(reset),
        ));
        m -= [5, 15, 10, 7, 12][i % 5];
        i += 1;
    }
    let d = day(&rs);
    assert_eq!(
        d.segments.len(),
        1,
        "{:?}",
        d.segments.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(d.segments[0].len(), rs.len());
    assert!(d.segments[0].windows(2).all(|w| w[0].0 < w[1].0));
}

#[test]
fn only_a_long_gap_or_a_window_reset_cuts_the_line() {
    let reset = now() + Duration::days(5);
    // 1 h 45 min between samples: still connected; over 2 h: cut
    let near = vec![
        weekly(300, 8.0, Some(reset)),
        weekly(195, 8.0, Some(reset)),
        weekly(90, 9.0, Some(reset)),
    ];
    assert_eq!(day(&near).segments.len(), 1);
    let far = vec![
        weekly(400, 8.0, Some(reset)),
        weekly(380, 8.0, Some(reset)),
        weekly(30, 9.0, Some(reset)),
    ];
    let d = day(&far);
    assert_eq!(d.segments.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
    assert!((CHART_GAP_SECS - 7200.0).abs() < 1e-9);
    // a new limit window (another reset time) starts a new segment, however close
    let next = reset + Duration::days(7);
    let reset_cut = vec![
        weekly(30, 80.0, Some(reset)),
        weekly(20, 2.0, Some(next)),
        weekly(10, 3.0, Some(next)),
    ];
    assert_eq!(
        day(&reset_cut)
            .segments
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    // a fall of 10+ points without any reset time is a reset too
    let dropped = vec![
        weekly(30, 80.0, None),
        weekly(20, 2.0, None),
        weekly(10, 3.0, None),
    ];
    assert_eq!(day(&dropped).segments.len(), 2);
    // a lone sample is a one-point segment (drawn as a dot)
    let lone = day(&[weekly(10, 8.0, None)]);
    assert_eq!(
        lone.segments,
        vec![vec![(now().timestamp() as f64 - 600.0, 8.0)]]
    );
    // nothing in range: no segments
    assert!(day(&[weekly(48 * 60, 8.0, None)]).segments.is_empty());
}

#[test]
fn the_y_axis_zooms_for_low_data_and_stays_at_100_for_high_data() {
    assert_eq!(chart_y_max(0.0), 10.0);
    assert_eq!(chart_y_max(4.0), 10.0);
    assert_eq!(chart_y_max(9.0), 20.0);
    assert_eq!(chart_y_max(16.0), 20.0);
    assert_eq!(chart_y_max(17.0), 25.0);
    assert_eq!(chart_y_max(21.0), 50.0);
    assert_eq!(chart_y_max(25.0), 50.0);
    assert_eq!(chart_y_max(25.5), 100.0);
    assert_eq!(chart_y_max(60.0), 100.0);
    // through chart_data: flat 8-9 % over 24 h, a rising line, and "no data"
    let flat: Vec<Reading> = (0..48)
        .map(|i| weekly(24 * 60 - 30 * i - 5, 8.0 + (i % 2) as f64, None))
        .collect();
    assert_eq!(day(&flat).y_max, 20.0);
    let rising: Vec<Reading> = (0..48)
        .map(|i| weekly(24 * 60 - 30 * i - 5, i as f64 * 1.3, None))
        .collect();
    assert_eq!(day(&rising).y_max, 100.0);
    assert_eq!(day(&[]).y_max, 100.0);
    // data above the zoom limit anywhere in the range keeps the 100 % scale
    let mut mixed = flat;
    mixed.push(weekly(60, 40.0, None));
    assert_eq!(day(&mixed).y_max, 100.0);
}

// ---- the range the chart opens with ------------------------------------------------------------

#[test]
fn the_default_range_is_the_shortest_one_that_covers_all_the_data() {
    use aitrack::view::default_range;
    let span = |hours: i64| vec![weekly(hours * 60, 5.0, None), weekly(10, 6.0, None)];
    let range = |rs: &[Reading]| default_range(rs, Kind::Weekly, now());
    assert_eq!(range(&[]), Range::Day, "no data");
    assert_eq!(range(&span(2)), Range::Day);
    assert_eq!(range(&span(23)), Range::Day);
    assert_eq!(
        range(&span(24)),
        Range::Week,
        "exactly a day is not covered by 24h"
    );
    assert_eq!(range(&span(3 * 24)), Range::Week);
    assert_eq!(range(&span(20 * 24)), Range::Month);
    assert_eq!(range(&span(29 * 24)), Range::Month);
    assert_eq!(range(&span(90 * 24)), Range::All);
    // only readings that have this kind of window (and are ok) count
    let session_only = Reading::from_value(&json!({
        "ok": true, "ts": (now() - Duration::days(90)).to_rfc3339(),
        "session": {"pct": 5.0, "resets_at": null}, "weekly": null}));
    let failed = Reading::from_value(&json!({
        "ok": false, "ts": (now() - Duration::days(90)).to_rfc3339(), "error": "x"}));
    assert_eq!(
        range(&[session_only, failed, weekly(60, 5.0, None)]),
        Range::Day
    );
}

#[cfg(feature = "gui")]
mod ui_default {
    use super::*;
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use aitrack::store::{JsonlStore, Store};
    use aitrack::ui::AitrackUi;
    use aitrack::vendor::VendorId;
    use eframe::egui::{self, Context, Pos2, RawInput, Rect};
    use std::sync::Arc;

    fn frame(ctx: &Context, ui: &mut AitrackUi, t: f64) {
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 900.0))),
            time: Some(t),
            ..Default::default()
        };
        let _ = ctx.run(raw, |c| ui.draw(c));
    }

    /// A Claude store with readings spread over `days` (real time: the window uses the real clock).
    fn app_with_span(days: i64) -> (Arc<App>, crate::common::TempDir) {
        let t = crate::common::TempDir::new();
        let store = JsonlStore::new(t.join("u.jsonl"));
        let real = Utc::now();
        for d in [days, 0] {
            let ts = (real - Duration::days(d) - Duration::minutes(1)).to_rfc3339();
            store
                .append(
                    &json!({"v":1,"ts":ts,"ok":true,"source":"oauth","observed_at":ts,
                    "session":{"pct":5.0,"resets_at":(real + Duration::hours(3)).to_rfc3339()},
                    "weekly":{"pct":6.0,"resets_at":(real + Duration::days(3)).to_rfc3339()},
                    "spend":null,"limits":null,"raw":{}}),
                )
                .unwrap();
        }
        let app = Arc::new(App::with_poll_fn(
            Box::new(store),
            Arc::new(Settings::load(t.join("s.json"))),
            Box::new(|_| Ok(serde_json::Value::Null)),
        ));
        app.history.load();
        (app, t)
    }

    #[test]
    fn the_chart_opens_on_the_default_range_and_a_users_choice_is_never_overridden() {
        let (app, _t) = app_with_span(20);
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app.clone());
        ui.sandbox();
        ui.expanded = Some((VendorId::Claude, Kind::Weekly));
        for i in 0..4 {
            frame(&ctx, &mut ui, 0.2 * i as f64);
        }
        assert_eq!(ui.range, Range::Month, "20 days of data");
        // the user picks 24h: later frames, a re-expanded row and new data keep it
        ui.set_range(Range::Day);
        for i in 0..4 {
            frame(&ctx, &mut ui, 1.0 + 0.2 * i as f64);
        }
        assert_eq!(ui.range, Range::Day);
        ui.expanded = None;
        frame(&ctx, &mut ui, 3.0);
        ui.expanded = Some((VendorId::Claude, Kind::Session));
        for i in 0..4 {
            frame(&ctx, &mut ui, 4.0 + 0.2 * i as f64);
        }
        assert_eq!(ui.range, Range::Day);
    }

    #[test]
    fn short_data_opens_on_24h_and_old_data_on_all() {
        for (days, want) in [(0, Range::Day), (3, Range::Week), (90, Range::All)] {
            let (app, _t) = app_with_span(days);
            let ctx = Context::default();
            let mut ui = AitrackUi::new(&ctx, app);
            ui.sandbox();
            ui.expanded = Some((VendorId::Claude, Kind::Weekly));
            for i in 0..4 {
                frame(&ctx, &mut ui, 0.2 * i as f64);
            }
            assert_eq!(ui.range, want, "{days} days");
        }
    }
}
