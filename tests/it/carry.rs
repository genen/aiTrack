// SPDX-License-Identifier: GPL-3.0-or-later
//! A sample with percentages but no reset time borrows the newest reset time of the same window:
//! the rules (analysis), the tooltip wording, and the diagnostics text.

use aitrack::analysis::analyze;
use aitrack::reading::Reading;
use aitrack::state::build_state;
use aitrack::view;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn rd(
    minutes_ago: i64,
    session: (f64, Option<i64>),
    weekly: (f64, Option<i64>),
    extra: Value,
) -> Reading {
    let at = now() - Duration::minutes(minutes_ago);
    let b = |(pct, reset): (f64, Option<i64>)| json!({"pct": pct, "resets_at": reset.map(|m| (now() + Duration::minutes(m)).to_rfc3339())});
    let mut v = json!({"ok": true, "ts": at.to_rfc3339(), "source": "desktop-history",
        "session": b(session), "weekly": b(weekly)});
    if let Value::Object(o) = extra {
        v.as_object_mut().unwrap().extend(o);
    }
    Reading::from_value(&v)
}

fn gauge_of(rs: &[Reading], weekly: bool) -> aitrack::analysis::Gauge {
    let p = analyze(rs, now()).unwrap();
    if weekly { p.weekly } else { p.session }.expect("gauge")
}

#[test]
fn the_newest_reset_time_of_the_window_is_carried_to_a_sample_without_one() {
    // 30 min ago: 50 %, resets in 73 min; now: 55 %, no reset time
    let rs = [
        rd(
            30,
            (50.0, Some(73)),
            (10.0, Some(6 * 1440)),
            json!({"source": "desktop-cache"}),
        ),
        rd(0, (55.0, None), (10.0, None), json!({})),
    ];
    let s = gauge_of(&rs, false);
    assert!(s.resets_carried);
    assert_eq!(
        s.resets_at.as_deref().map(|t| &t[..16]),
        Some(&(now() + Duration::minutes(73)).to_rfc3339()[..16])
    );
    assert!((s.hours_left.unwrap() - 73.0 / 60.0).abs() < 0.02);
    let w = gauge_of(&rs, true);
    assert!(w.resets_carried && (w.hours_left.unwrap() - 6.0 * 24.0).abs() < 0.1);
}

#[test]
fn a_reported_reset_time_is_never_replaced() {
    let rs = [
        rd(30, (50.0, Some(73)), (10.0, Some(9000)), json!({})),
        rd(0, (55.0, Some(70)), (10.0, Some(8990)), json!({})),
    ];
    let s = gauge_of(&rs, false);
    assert!(!s.resets_carried);
    assert!((s.hours_left.unwrap() - 70.0 / 60.0).abs() < 0.02);
}

#[test]
fn nothing_is_invented() {
    // no earlier reset time at all
    let only = [rd(0, (55.0, None), (10.0, None), json!({}))];
    let s = gauge_of(&only, false);
    assert!(s.resets_at.is_none() && s.hours_left.is_none() && !s.resets_carried);
    // the percentage dropped: the window the old time describes is over
    let dropped = [
        rd(30, (50.0, Some(73)), (10.0, Some(9000)), json!({})),
        rd(0, (12.0, None), (10.0, None), json!({})),
    ];
    assert!(gauge_of(&dropped, false).resets_at.is_none());
    // the old reset time has already passed
    let past = [
        rd(300, (50.0, Some(-10)), (10.0, Some(9000)), json!({})),
        rd(0, (55.0, None), (10.0, None), json!({})),
    ];
    let s = gauge_of(&past, false);
    assert!(s.resets_at.is_none() && s.hours_left.is_none());
    // the sample lies before the window that time defines (6 h ahead: the 5 h window has not begun)
    let early = [
        rd(30, (50.0, Some(360)), (10.0, Some(9000)), json!({})),
        rd(0, (55.0, None), (10.0, None), json!({})),
    ];
    assert!(gauge_of(&early, false).resets_at.is_none());
}

#[test]
fn a_carried_time_that_has_passed_is_never_shown() {
    // stored as carried when it was still ahead; by now it is in the past
    let rs = [rd(10, (55.0, Some(-5)), (10.0, Some(9000)), json!({}))];
    let mut r = rs[0].clone();
    r.session.as_mut().unwrap().carried = true;
    let s = gauge_of(std::slice::from_ref(&r), false);
    assert!(s.resets_at.is_none() && s.hours_left.is_none() && !s.resets_carried);
    // and the old percentage must not stay lit as the current one: the window it belonged to is over
    assert!(s.reset_passed, "{s:?}");
    let st = build_state(
        std::slice::from_ref(&r),
        Default::default(),
        status(),
        now(),
    );
    let vs = aitrack::vendor::from_state_at(&st, now());
    let home = view::build_home_for(&st, &vs, now());
    let row = &home.vendors[0].rows[0];
    assert_eq!((row.value.as_str(), row.stale), ("--", true), "{row:?}");
    assert!(
        row.tip[0].text.contains("reset time has passed")
            && row
                .tip
                .iter()
                .all(|l| !l.text.contains("resets in") && !l.text.contains("Pace")),
        "{:?}",
        row.tip
    );
    assert_eq!(home.vendors[0].warn, None);
    let d = view::build_detail_at(&vs[0], aitrack::reading::Kind::Session, false, now());
    assert_eq!(
        d.resets.as_deref(),
        Some("Reset passed, waiting for a new reading")
    );
    assert_eq!(d.value, "--");
}

fn home_rows(rs: &[Reading]) -> Vec<String> {
    let st = build_state(rs, Default::default(), status(), now());
    let home = view::build_home(&st, now());
    home.vendors[0]
        .rows
        .iter()
        .map(|r| r.tip[0].text.clone())
        .collect()
}

fn status() -> aitrack::collector::Status {
    aitrack::collector::Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    }
}

#[test]
fn the_tooltip_says_carried_and_unknown_only_when_nothing_is_known() {
    let carried = [
        rd(30, (50.0, Some(73)), (10.0, Some(8640)), json!({})),
        rd(0, (55.0, None), (10.0, None), json!({})),
    ];
    let tips = home_rows(&carried);
    assert_eq!(
        tips[0], "Session resets in ~1h 13m (carried from earlier sample)",
        "{tips:?}"
    );
    assert!(
        tips[1].starts_with("Weekly resets in ~")
            && tips[1].contains("carried from earlier sample"),
        "{tips:?}"
    );
    // a reported time reads as before, without the marker
    let real = [rd(0, (55.0, Some(73)), (10.0, Some(8640)), json!({}))];
    let tips = home_rows(&real);
    assert!(
        tips[0].starts_with("Session resets in 1h 13m") && !tips[0].contains("carried"),
        "{tips:?}"
    );
    // nothing known
    let none = [rd(0, (55.0, None), (10.0, None), json!({}))];
    let tips = home_rows(&none);
    assert_eq!(tips[0], "Session reset time unknown");
    assert_eq!(tips[1], "Weekly reset time unknown");
}

#[test]
fn the_diagnostics_say_which_source_supplied_the_reading_and_why() {
    let note =
        "desktop-cache skipped: no usage entry in the Claude desktop cache (scanned 4 files)";
    let rs = [
        rd(
            5,
            (50.0, Some(73)),
            (10.0, Some(8640)),
            json!({"source": "desktop-cache"}),
        ),
        rd(
            0,
            (55.0, None),
            (10.0, None),
            json!({"source": "desktop-history", "note": note}),
        ),
    ];
    let st = build_state(&rs, Default::default(), status(), now());
    let newest = &st.diagnostics[0].text;
    assert!(
        newest.contains("session 55%") && newest.contains("desktop-history"),
        "{newest}"
    );
    assert!(newest.contains("desktop-cache skipped"), "{newest}");
    assert_eq!(
        st.diagnostics[1].text,
        "session 50%, weekly 10% \u{b7} desktop-cache"
    );
    // nothing sensitive: no paths, ids or tokens are added by this
    assert!(!newest.contains('/') && !newest.contains("token"));
}
