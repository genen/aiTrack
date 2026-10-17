// SPDX-License-Identifier: GPL-3.0-or-later
use aitrack::analysis::{analyze, series};
use aitrack::reading::{model_limits, Reading};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

fn rec(
    t: DateTime<Utc>,
    s: f64,
    w: f64,
    reset_s: DateTime<Utc>,
    reset_w: DateTime<Utc>,
) -> Reading {
    Reading::from_value(&json!({
        "ok": true, "ts": t.to_rfc3339(),
        "session": {"pct": s, "resets_at": reset_s.to_rfc3339()},
        "weekly": {"pct": w, "resets_at": reset_w.to_rfc3339()},
    }))
}

fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap()
}

#[test]
fn rate_projection_and_window_stats() {
    let base = base();
    let reset_w = base + Duration::days(3);
    let mut recs = vec![];
    // window 1 hit the limit, window 2 is in progress at +10%/h
    for i in 0..5 {
        let t = base - Duration::hours(10) + Duration::hours(i);
        recs.push(rec(
            t,
            25.0 * i as f64,
            5.0,
            base - Duration::hours(5),
            reset_w,
        ));
    }
    for i in 0..3 {
        let t = base + Duration::hours(i);
        recs.push(rec(
            t,
            10.0 * i as f64,
            6.0,
            base + Duration::hours(5),
            reset_w,
        ));
    }
    let now = base + Duration::hours(2);
    let out = analyze(&recs, now).unwrap();
    let cur = out.session.as_ref().unwrap();
    assert!((cur.rate_per_hour.unwrap() - 10.0).abs() < 0.05);
    assert_eq!(cur.pct, 20.0);
    assert!((cur.hours_left.unwrap() - 3.0).abs() < 0.05);
    assert_eq!(cur.projected_pct, Some(50.0));
    assert_eq!(cur.hours_to_limit, Some(8.0));
    assert_eq!(cur.window_hours, 5.0);
    assert_eq!(out.session_stats.windows, 2);
    assert_eq!(out.session_stats.hit_rate, Some(50));
    assert_eq!(out.session_stats.max_peak, Some(100.0));
    assert_eq!(out.session_stats.avg_peak, Some(60.0));
    assert_eq!(out.weekly_stats.windows, 1);
    assert_eq!(out.snapshots, 8);
    assert_eq!(out.errors, 0);
}

#[test]
fn empty_and_errors_only() {
    assert!(analyze(&[], base()).is_none());
    let bad =
        Reading::from_value(&json!({"ok": false, "ts": "2026-01-01T00:00:00+00:00", "error": "x"}));
    assert!(analyze(&[bad], base()).is_none());
}

#[test]
fn errors_are_counted_and_stale_minutes_computed() {
    let b = base();
    let good = rec(b, 1.0, 1.0, b + Duration::hours(5), b + Duration::days(3));
    let bad = Reading::from_value(
        &json!({"ok": false, "ts": (b + Duration::minutes(15)).to_rfc3339(), "error": "x"}),
    );
    let out = analyze(&[good, bad], b + Duration::minutes(30)).unwrap();
    assert_eq!((out.snapshots, out.errors, out.stale_minutes), (1, 1, 30));
    // a single reading has no regression; the 5h window opened 30 min ago, so the pace is the
    // average since then: 1% / 0.5 h = 2 pts/h, projected over the 4.5 h left = 10%
    let g = out.session.as_ref().unwrap();
    assert_eq!((g.rate_per_hour, g.projected_pct), (Some(2.0), Some(10.0)));
    // ... but not when the window opened less than 15 minutes ago
    let good = rec(b, 1.0, 1.0, b + Duration::hours(5), b + Duration::days(3));
    let out = analyze(&[good], b + Duration::minutes(10)).unwrap();
    assert_eq!(out.session.as_ref().unwrap().rate_per_hour, None);
    assert_eq!(out.session.as_ref().unwrap().projected_pct, None);
}

#[test]
fn elapsed_time_pace_matches_the_other_tool_example() {
    use aitrack::analysis::elapsed_pace;
    use aitrack::reading::Kind;
    // 55% used with 3h20m left of the 5h window: 1h40m elapsed -> 33 pts/h -> empty in ~1h22m
    let rate = elapsed_pace(Kind::Session, 55.0, Some(3.0 + 20.0 / 60.0)).unwrap();
    assert!((rate - 33.0).abs() < 1e-9);
    assert!(((100.0 - 55.0) / rate - 1.3636).abs() < 1e-3);
    // weekly: 8% with 6d 5h left of 7d -> 19h elapsed
    let w = elapsed_pace(Kind::Weekly, 8.0, Some(6.0 * 24.0 + 5.0)).unwrap();
    assert!((w - 8.0 / 19.0).abs() < 1e-9);
    // unknown reset, reset already passed, or window barely started
    assert_eq!(elapsed_pace(Kind::Session, 5.0, None), None);
    assert_eq!(elapsed_pace(Kind::Session, 5.0, Some(0.0)), None);
    assert_eq!(elapsed_pace(Kind::Session, 5.0, Some(4.9)), None);
}

#[test]
fn resets_jitter_within_a_minute_is_one_window() {
    let b = base();
    let mk = |i: i64, jitter_ms: i64| {
        rec(
            b + Duration::minutes(15 * i),
            i as f64,
            1.0,
            b + Duration::hours(5) + Duration::milliseconds(jitter_ms),
            b + Duration::days(3),
        )
    };
    let recs: Vec<Reading> = (0..4).map(|i| mk(i, i * 7)).collect();
    assert_eq!(
        analyze(&recs, b + Duration::hours(1))
            .unwrap()
            .session_stats
            .windows,
        1
    );
}

#[test]
fn flat_usage_is_zero_rate_with_no_limit_eta() {
    let b = base();
    let recs: Vec<Reading> = (0..4)
        .map(|i| {
            rec(
                b + Duration::hours(i),
                30.0,
                3.0,
                b + Duration::hours(5),
                b + Duration::days(3),
            )
        })
        .collect();
    let g = analyze(&recs, b + Duration::hours(3))
        .unwrap()
        .session
        .unwrap();
    assert_eq!(g.rate_per_hour, Some(0.0));
    assert_eq!(g.hours_to_limit, None);
    assert_eq!(g.projected_pct, Some(30.0));
}

#[test]
fn series_is_downsampled_and_keeps_latest() {
    let b = base();
    let recs: Vec<Reading> = (0..7000)
        .map(|i| {
            rec(
                b + Duration::minutes(i),
                1.0,
                2.0,
                b + Duration::days(9),
                b + Duration::days(9),
            )
        })
        .collect();
    let s = series(&recs);
    assert!(s.len() <= 3001 && s.len() > 2000, "{}", s.len());
    assert_eq!(s.last().unwrap().t, recs.last().unwrap().ts);
}

#[test]
fn per_model_limit_extraction() {
    let limits: Value = json!([
        {"kind": "weekly_scoped", "percent": 43, "resets_at": "2026-10-20T00:00:00Z", "scope": {"model": {"display_name": "Fable"}}},
        {"kind": "weekly_scoped", "utilization": 7.5, "scope": {"model": {"id": "model-x"}}},
        {"kind": "no_model", "percent": 9},
        {"kind": "bad_pct", "percent": "x", "scope": {"model": {"display_name": "Nope"}}},
        "junk"
    ]);
    let m = model_limits(&limits);
    assert_eq!(m.len(), 2);
    assert_eq!(
        (m[0].name.as_str(), m[0].kind.as_str(), m[0].pct),
        ("Fable", "weekly_scoped", 43.0)
    );
    assert_eq!(m[0].resets_at.as_deref(), Some("2026-10-20T00:00:00Z"));
    assert_eq!(
        (m[1].name.as_str(), m[1].pct, m[1].resets_at.clone()),
        ("model-x", 7.5, None)
    );
    assert!(model_limits(&Value::Null).is_empty());
}

#[test]
fn timestamps_parse_leniently() {
    use aitrack::util::parse_ts;
    assert!(parse_ts("2026-10-14T15:00:00.123456789+00:00").is_some());
    assert!(parse_ts("2026-10-14T15:00:00Z").is_some());
    assert_eq!(
        parse_ts("2026-10-14T15:00:00"),
        parse_ts("2026-10-14T15:00:00+00:00")
    );
    assert!(parse_ts("garbage").is_none());
}
