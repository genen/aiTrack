// SPDX-License-Identifier: GPL-3.0-or-later
//! Pace and the "empty soon" warning must not be fooled by integer percentages, one quantised step,
//! or readings from before a reset / plan change. Scenarios are the real-world ones that went wrong.
use aitrack::analysis::{analyze, empty_before_reset, pace_for, Gauge, PaceMethod};
use aitrack::collector::Status;
use aitrack::reading::{Kind, Reading};
use aitrack::settings::Values;
use aitrack::state::build_state;
use aitrack::view;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// One stored-style reading: `session`/`weekly` percentages and optional reset offsets (minutes).
fn rd(
    min_ago: i64,
    session: f64,
    weekly: f64,
    s_reset_min: Option<i64>,
    w_reset_min: Option<i64>,
) -> Reading {
    let reset = |m: Option<i64>| m.map(|m| (now() + Duration::minutes(m)).to_rfc3339());
    Reading::from_value(&json!({
        "v": 1, "ok": true, "source": "desktop-cache", "ts": (now() - Duration::minutes(min_ago)).to_rfc3339(),
        "session": {"pct": session, "resets_at": reset(s_reset_min)},
        "weekly": {"pct": weekly, "resets_at": reset(w_reset_min)},
    }))
}

fn home(rs: &[Reading]) -> view::HomeView {
    let st = build_state(
        rs,
        Values::default(),
        Status {
            error: None,
            polling: false,
            next_in: None,
            polls: 1,
            live: false,
            sample: None,
        },
        now(),
    );
    view::build_home(&st, now())
}

const WEEK_LEFT: i64 = 6 * 24 * 60 + 4 * 60; // 6d 4h

// ------------------------------------------------------------ the reported bug

#[test]
fn weekly_one_percent_with_6d4h_left_and_a_0_to_1_step_has_no_warning() {
    // weekly went 0 -> 1 within minutes; the window opened about 20 h ago
    let rs = [
        rd(20, 3.0, 0.0, Some(200), Some(WEEK_LEFT)),
        rd(10, 3.0, 0.0, Some(200), Some(WEEK_LEFT)),
        rd(2, 3.0, 1.0, Some(200), Some(WEEK_LEFT)),
    ];
    let plan = analyze(&rs, now()).unwrap();
    let w = plan.weekly.as_ref().unwrap();
    // the pace is the average since the window opened: 1 pt / 20 h, not the one-step trend
    assert_eq!(w.rate_method, Some(PaceMethod::Average));
    assert!(
        (w.rate_per_hour.unwrap() - 0.05).abs() < 0.006,
        "{:?}",
        w.rate_per_hour
    );
    let h = home(&rs);
    assert_eq!(h.vendors[0].warn, None, "{:?}", h.vendors[0].warn);
    assert_eq!(empty_before_reset(w), None);
}

#[test]
fn quantisation_steps_never_make_a_warning() {
    // several identical readings, then +1
    let rs: Vec<Reading> = [
        (50, 0.0),
        (40, 0.0),
        (30, 0.0),
        (20, 0.0),
        (10, 0.0),
        (1, 1.0),
    ]
    .iter()
    .map(|(m, w)| rd(*m, 3.0, *w, Some(200), Some(WEEK_LEFT)))
    .collect();
    assert_eq!(home(&rs).vendors[0].warn, None);
    // same on a source without reset times: no average either, so no pace and nothing to warn about
    let rs: Vec<Reading> = [(50, 20.0), (40, 20.0), (30, 20.0), (20, 20.0), (1, 21.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 2.0, None, None))
        .collect();
    let plan = analyze(&rs, now()).unwrap();
    assert_eq!(plan.session.as_ref().unwrap().rate_per_hour, None);
    assert_eq!(home(&rs).vendors[0].warn, None);
}

#[test]
fn weekly_plan_change_12_to_0_then_1_has_no_warning() {
    // 12% -> 0% (plan change or early reset, the reset time itself unchanged), then 1% a while later
    let rs = [
        rd(300, 20.0, 12.0, Some(200), Some(WEEK_LEFT)),
        rd(200, 25.0, 12.0, Some(200), Some(WEEK_LEFT)),
        rd(120, 0.0, 0.0, Some(300), Some(WEEK_LEFT)),
        rd(60, 0.0, 0.0, Some(300), Some(WEEK_LEFT)),
        rd(3, 1.0, 1.0, Some(300), Some(WEEK_LEFT)),
    ];
    let h = home(&rs);
    assert_eq!(h.vendors[0].warn, None, "{:?}", h.vendors[0].warn);
    let w = analyze(&rs, now()).unwrap().weekly.unwrap();
    assert!(w.rate_per_hour.unwrap() < 0.2, "{:?}", w.rate_per_hour);
}

// ------------------------------------------------------------ warnings that must still appear

#[test]
fn session_55_percent_with_3h20m_left_and_no_history_still_warns() {
    let rs = [rd(1, 55.0, 8.0, Some(200), Some(WEEK_LEFT))];
    let h = home(&rs);
    let w = h.vendors[0].warn.as_ref().expect("warning");
    assert_eq!(w.kind, Kind::Session);
    assert_eq!(w.full, "Empty in ~1h 22m \u{b7} resets in 3h 20m");
}

#[test]
fn a_real_surge_76_to_90_in_six_minutes_warns_promptly() {
    // dense readings of a heavy burst, ~2h58m left
    let rs: Vec<Reading> = [(8, 70.0), (6, 76.0), (4, 80.0), (2, 85.0), (0, 90.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 10.0, Some(178), Some(WEEK_LEFT)))
        .collect();
    let plan = analyze(&rs, now()).unwrap();
    let s = plan.session.as_ref().unwrap();
    assert_eq!(s.rate_method, Some(PaceMethod::Recent));
    let rate = s.rate_per_hour.unwrap();
    // the surge is ~120 pts/h; the average since the window opened is ~44; the blend caps at 3 x avg
    assert!((100.0..=135.0).contains(&rate), "{rate}");
    let w = home(&rs).vendors[0].warn.clone().expect("warning");
    assert_eq!(w.kind, Kind::Session);
    assert!(w.compact.starts_with("Empty in ~"), "{w:?}");
    assert!(w.compact.ends_with('m'), "{w:?}"); // minutes, not hours
}

#[test]
fn even_three_sparse_readings_of_the_burst_warn_through_the_average() {
    let rs: Vec<Reading> = [(6, 76.0), (3, 83.0), (0, 90.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 10.0, Some(178), Some(WEEK_LEFT)))
        .collect();
    let w = home(&rs).vendors[0].warn.clone().expect("warning");
    assert!(
        w.full.starts_with("Empty in ~") && w.full.contains("resets in 2h 58m"),
        "{w:?}"
    );
}

// ------------------------------------------------------------ the pace rules themselves

fn pts(v: &[(i64, f64)]) -> Vec<(DateTime<Utc>, f64)> {
    v.iter()
        .map(|(m, p)| (now() - Duration::minutes(*m), *p))
        .collect()
}

#[test]
fn pace_rules() {
    // one-point gain: insufficient, falls back to the average (55 min into... 20 h of a week)
    let (r, m) = pace_for(Kind::Weekly, 1.0, Some(148.0), &pts(&[(10, 0.0), (2, 1.0)])).unwrap();
    assert_eq!(m, PaceMethod::Average);
    assert!((r - 0.05).abs() < 1e-9);
    // a real gain over a real span is trusted, damped to at most 3 x the average
    let (r, m) = pace_for(
        Kind::Session,
        30.0,
        Some(3.0),
        &pts(&[(120, 10.0), (60, 20.0), (0, 30.0)]),
    )
    .unwrap();
    assert_eq!((m, r), (PaceMethod::Average, 15.0)); // trend 10 < average 15: the average is the floor
    let (r, m) = pace_for(
        Kind::Session,
        90.0,
        Some(3.0),
        &pts(&[(6, 76.0), (4, 80.0), (2, 85.0), (0, 90.0)]),
    )
    .unwrap();
    assert_eq!(m, PaceMethod::Recent);
    assert_eq!(r, 3.0 * 45.0); // 140 pts/h raw, capped at 3 x (90 / 2 h)
                               // no reset time: the trend alone, undamped
    let (r, m) = pace_for(
        Kind::Session,
        30.0,
        None,
        &pts(&[(120, 10.0), (60, 20.0), (0, 30.0)]),
    )
    .unwrap();
    assert_eq!(m, PaceMethod::Recent);
    assert!((r - 10.0).abs() < 1e-9);
    // an hour of nothing is evidence of idleness, whatever the average says
    let (r, _) = pace_for(
        Kind::Session,
        30.0,
        Some(1.0),
        &pts(&[(60, 30.0), (30, 30.0), (0, 30.0)]),
    )
    .unwrap();
    assert_eq!(r, 0.0);
    // too little of everything: no pace
    assert_eq!(
        pace_for(Kind::Session, 30.0, None, &pts(&[(5, 30.0), (0, 31.0)])),
        None
    );
    assert_eq!(pace_for(Kind::Session, 30.0, None, &[]), None);
}

#[test]
fn readings_from_before_a_plan_change_do_not_feed_the_pace() {
    // no reset times (desktop-history style): both windows fall to 0 (8 -> 0 is under the 10-point
    // inferred-window rule), then usage climbs; the old 8% must not drag the slope down
    let rs = [
        rd(200, 5.0, 8.0, None, None),
        rd(150, 6.0, 8.0, None, None),
        rd(60, 0.0, 0.0, None, None),
        rd(40, 1.0, 0.0, None, None),
        rd(20, 4.0, 0.0, None, None),
        rd(0, 7.0, 0.0, None, None),
    ];
    let s = analyze(&rs, now()).unwrap().session.unwrap();
    // slope of 0,1,4,7 over one hour is ~7.2 pts/h (including the 5%/6% readings it would be ~0)
    assert!(
        (s.rate_per_hour.unwrap() - 7.2).abs() < 0.3,
        "{:?}",
        s.rate_per_hour
    );
}

// ------------------------------------------------------------ the usage guard

fn gauge(pct: f64, hours_left: f64, rate: f64, hours: f64) -> Gauge {
    Gauge {
        pct,
        resets_at: None,
        hours_left: Some(hours_left),
        rate_per_hour: Some(rate),
        rate_method: Some(PaceMethod::Recent),
        projected_pct: None,
        hours_to_limit: None,
        window_hours: hours,
        window_id: String::new(),
        resets_carried: false,
        reset_passed: false,
    }
}

#[test]
fn low_usage_needs_the_average_to_agree_before_it_warns() {
    // 8% of the session at a (noisy) 25 pts/h would be empty in 3.7 h < 4 h left, but the window
    // average is only 8 pts/h: no warning
    assert_eq!(empty_before_reset(&gauge(8.0, 4.0, 25.0, 5.0)), None);
    // the same pace at 12% is allowed
    assert!(empty_before_reset(&gauge(12.0, 4.0, 30.0, 5.0)).is_some());
    // 9% only 15 minutes into the window: the average itself (36 pts/h) exhausts it before the reset
    assert!(empty_before_reset(&gauge(9.0, 4.75, 36.0, 5.0)).is_some());
    // weekly threshold is 25%
    assert_eq!(empty_before_reset(&gauge(20.0, 100.0, 5.0, 168.0)), None);
    assert!(empty_before_reset(&gauge(30.0, 100.0, 5.0, 168.0)).is_some());
    // the limit itself always counts
    assert!(
        empty_before_reset(&gauge(100.0, 2.0, 0.0, 5.0))
            .unwrap()
            .at_limit
    );
}

// ------------------------------------------------------------ alerts use the same rule

#[test]
fn toast_alerts_share_the_guard() {
    use aitrack::alerts::{Alerts, LogNotifier};
    let dir = std::env::temp_dir().join(format!("aitrack-pace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // weekly 1% with a quantised step: nothing, not even "empty in ..."
    let rs = [
        rd(20, 3.0, 0.0, Some(200), Some(WEEK_LEFT)),
        rd(2, 3.0, 1.0, Some(200), Some(WEEK_LEFT)),
    ];
    let plan = analyze(&rs, now()).unwrap();
    let sent = Alerts::load(dir.join("alerts.json")).evaluate("Claude", &plan, now(), &LogNotifier);
    assert!(sent.is_empty(), "{sent:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
