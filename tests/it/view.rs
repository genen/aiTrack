// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::app::App;
use aitrack::collector::PollFn;
use aitrack::reading::Kind;
use aitrack::settings::{Direction, Settings};
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::{self, VendorId};
use aitrack::view::*;
use chrono::Utc;
use common::TempDir;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn levels() {
    assert_eq!(level(74.9), Level::Normal);
    // no intermediate 75% step any more: only red from 90%
    assert_eq!(level(75.0), Level::Normal);
    assert_eq!(level(89.9), Level::Normal);
    assert_eq!(level(90.0), Level::Bad);
}

#[test]
fn left_and_ago_formatting() {
    assert_eq!(left(0.0), "1m");
    assert_eq!(left(0.5), "30m");
    assert_eq!(left(3.25), "3.2h");
    assert_eq!(left(9.96), "10.0h");
    assert_eq!(left(14.4), "14h");
    assert_eq!(left(47.9), "48h");
    assert_eq!(left(72.0), "3d");
    assert_eq!(ago(0.2), "just now");
    assert_eq!(ago(12.0), "12 min ago");
    assert_eq!(ago(90.0), "1.5 h ago");
}

#[test]
fn bar_segments_used_remaining_and_projection() {
    let lit = |c: &[Cell]| c.iter().filter(|c| **c == Cell::Lit).count();
    let proj = |c: &[Cell]| c.iter().filter(|c| **c == Cell::Projected).count();
    let c = bar_cells(42.0, Some(60.0), false);
    assert_eq!(c.len(), 20);
    assert_eq!((lit(&c), proj(&c)), (8, 4)); // segment midpoints 2.5, 7.5.. < 42 -> 8 lit; < 60 -> 4 more faint
    assert!(c[..8].iter().all(|x| *x == Cell::Lit));
    // remaining mode: shows what is left, no projection
    let r = bar_cells(42.0, Some(60.0), true);
    assert_eq!((lit(&r), proj(&r)), (12, 0));
    assert_eq!(lit(&bar_cells(0.0, None, false)), 0);
    assert_eq!(lit(&bar_cells(100.0, None, false)), 20);
}

fn gauge(
    htl: Option<f64>,
    hl: Option<f64>,
    proj: Option<f64>,
    rate: Option<f64>,
) -> aitrack::analysis::Gauge {
    aitrack::analysis::Gauge {
        pct: 20.0,
        resets_at: None,
        hours_left: hl,
        rate_per_hour: rate,
        projected_pct: proj,
        hours_to_limit: htl,
        window_hours: 5.0,
        window_id: String::new(),
        resets_carried: false,
        reset_passed: false,
        rate_method: None,
    }
}

#[test]
fn pace_notes() {
    let n = pace_note(&gauge(Some(2.0), Some(3.0), Some(100.0), Some(10.0))).unwrap();
    assert_eq!(
        (n.text.as_str(), n.warn),
        ("Limit in ~2.0h at this pace", true)
    );
    let n = pace_note(&gauge(Some(8.0), Some(3.0), Some(50.0), Some(10.0))).unwrap();
    assert_eq!(
        (n.text.as_str(), n.warn),
        ("On pace for ~50% by reset", false)
    );
    let n = pace_note(&gauge(None, Some(3.0), Some(20.0), Some(0.0))).unwrap();
    assert_eq!(n.text, "Steady, no recent use");
    assert!(pace_note(&gauge(None, Some(3.0), None, None)).is_none());
}

#[test]
fn presets_and_warning() {
    let p = presets(15.0);
    assert_eq!(
        p.iter().map(|p| p.label).collect::<Vec<_>>(),
        ["1 min", "5 min", "15 min", "1 hour"]
    );
    assert_eq!(
        p.iter()
            .filter(|p| p.selected)
            .map(|p| p.minutes)
            .collect::<Vec<_>>(),
        [15.0]
    );
    assert!(presets(7.0).iter().all(|p| !p.selected));
    assert!(fast_warning(1.0) && !fast_warning(5.0));
}

fn fake_poll() -> PollFn {
    Box::new(|store: &dyn Store| {
        let now = Utc::now();
        let rec = json!({"v": 1, "ts": aitrack::util::now_iso(), "ok": true,
            "session": {"pct": 30.0, "resets_at": (now + chrono::Duration::hours(3)).to_rfc3339()},
            "weekly": {"pct": 50.0, "resets_at": (now + chrono::Duration::days(2)).to_rfc3339()},
            "spend": {"enabled": true, "used": 13.59, "limit": 50.0}, "raw": {},
            "limits": [{"kind": "weekly_scoped", "percent": 43, "scope": {"model": {"display_name": "Fable"}}}]});
        store.append(&rec).unwrap();
        Ok(rec)
    })
}

fn make_app(dir: &TempDir, poll: PollFn) -> App {
    App::with_poll_fn(
        Box::new(JsonlStore::new(dir.join("u.jsonl"))),
        Arc::new(Settings::load(dir.join("s.json"))),
        poll,
    )
}

fn wait_state(app: &App, f: impl Fn(&aitrack::state::State) -> bool) -> aitrack::state::State {
    let t = Instant::now();
    loop {
        let s = app.state();
        if f(&s) {
            return s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn state_and_vendor_rows_from_a_live_app() {
    let dir = TempDir::new();
    let app = make_app(&dir, fake_poll());
    assert!(app.state().loading || app.state().plan.is_none()); // before the collector ran
    app.start();
    let s = wait_state(&app, |s| s.plan.is_some());
    assert_eq!(s.models[0].name, "Fable");
    assert_eq!(s.plan.as_ref().unwrap().session.as_ref().unwrap().pct, 30.0);
    assert_eq!(s.diagnostics.len(), 1);
    assert_eq!(s.diagnostics[0].text, "session 30%, weekly 50%");

    let home = build_home(&s, Utc::now());
    assert!(home.empty.is_none());
    assert_eq!(home.vendors.len(), 1); // only vendors with data are shown
    let claude = &home.vendors[0];
    assert_eq!(
        (claude.id, claude.name.as_str()),
        (VendorId::Claude, "Claude")
    );
    let labels: Vec<&str> = claude.rows.iter().map(|r| r.label).collect();
    assert_eq!(labels, ["Session", "Weekly"]);
    assert_eq!(
        (claude.rows[0].value.as_str(), claude.rows[1].value.as_str()),
        ("30%", "50%")
    );
    assert_eq!(claude.rows[0].level, Level::Normal);
    // reset line + pace go in tooltips, not inline
    let t0 = &claude.rows[0].tip;
    assert!(
        t0[0].text.starts_with("Session resets in ") && t0[0].text.ends_with(')'),
        "{}",
        t0[0].text
    );
    // per-model limits and credits live in the weekly row's tooltip
    let t1: Vec<&str> = claude.rows[1].tip.iter().map(|l| l.text.as_str()).collect();
    assert!(
        t1[0].starts_with("Weekly resets in 1d 23h") || t1[0].starts_with("Weekly resets in 2d"),
        "{t1:?}"
    );
    assert!(t1.iter().any(|l| l.starts_with("Fable  43%")), "{t1:?}");
    assert!(t1.contains(&"Credits  $13.59 of $50.00"), "{t1:?}");
    assert!(!t0.iter().any(|l| l.text.contains("Fable")));
    assert_eq!(home.status.dot, Dot::Ok);
    assert!(
        home.status.text.starts_with("Updated "),
        "{}",
        home.status.text
    );
    assert!(
        home.status.text.contains("next in 15 min"),
        "{}",
        home.status.text
    );

    // remaining mode
    app.settings.set_direction(Direction::Remaining);
    let s = app.state();
    let home = build_home(&s, Utc::now());
    assert_eq!(home.vendors[0].rows[0].value, "70% left");
    assert!(home.vendors[0].rows[1]
        .tip
        .iter()
        .any(|l| l.text.starts_with("Fable  57% left")));

    let v = &vendor::from_state(&s)[0];
    let d = build_detail(v, Kind::Session, true);
    assert_eq!((d.value.as_str(), d.unit), ("70", "% left"));
    assert!(d.rows.iter().any(|r| r.0 == "Hit rate"));
    assert_eq!(d.chart, Some(Kind::Session));
    app.stop();
    app.collector.join();
    assert_eq!(JsonlStore::new(dir.join("u.jsonl")).read().len(), 1);
}

#[test]
fn empty_state_shows_collector_error() {
    let dir = TempDir::new();
    let failing: PollFn = Box::new(|store: &dyn Store| {
        store
            .append(&aitrack::live::error_record(
                "2026-01-01T00:00:00+00:00",
                "no Claude login found",
            ))
            .unwrap();
        Err(aitrack::live::CollectError::new("no Claude login found"))
    });
    let app = make_app(&dir, failing);
    app.start();
    let s = wait_state(&app, |s| s.collector.error.is_some() && !s.loading);
    let home = build_home(&s, Utc::now());
    let e = home.empty.unwrap();
    assert_eq!(
        (e.title.as_str(), e.body.as_str()),
        ("No reading yet", "no Claude login found")
    );
    assert_eq!(home.status.dot, Dot::Err);
    assert!(home.vendors.is_empty());
    assert!(!s.diagnostics[0].ok);
    assert!(copy_log(&s.diagnostics).contains("FAIL no Claude login found"));
    app.stop();
}

#[test]
fn diagnostics_keep_only_the_last_25_newest_first() {
    let dir = TempDir::new();
    let store = JsonlStore::new(dir.join("u.jsonl"));
    for i in 0..40 {
        store.append(&json!({"v":1,"ts":format!("2026-10-14T10:{i:02}:00+00:00"),"ok":false,"error":format!("e{i}")})).unwrap();
    }
    let app = make_app(
        &dir,
        Box::new(|_| Err(aitrack::live::CollectError::new("x"))),
    );
    app.history.load();
    let s = app.state();
    assert_eq!(s.diagnostics.len(), 25);
    assert_eq!(s.diagnostics[0].text, "e39");
    assert_eq!(s.diagnostics[24].text, "e15");
}

#[test]
fn chart_data_splits_windows_and_respects_range() {
    use aitrack::reading::{Kind, Reading};
    use chrono::{Duration, TimeZone};
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 14, 12, 0, 0)
        .unwrap();
    let mut rs = Vec::new();
    // 30 hours of readings every 30 min; the 5h window resets at 05:00/10:00/15:00 ... boundaries
    for i in 0..60 {
        let t = now - Duration::minutes(30 * (59 - i));
        let reset = {
            let k = (now + Duration::hours(3) - t).num_minutes() / 300; // windows back from 15:00
            now + Duration::hours(3) - Duration::minutes(300 * k)
        };
        rs.push(Reading::from_value(
            &json!({"ok": true, "ts": t.to_rfc3339(),
            "session": {"pct": (i % 10) as f64, "resets_at": reset.to_rfc3339()}, "weekly": null}),
        ));
    }
    rs.push(Reading::from_value(
        &json!({"ok": false, "ts": now.to_rfc3339(), "error": "x"}),
    ));
    let all = chart_data(&rs, Kind::Session, Range::All, now);
    let day = chart_data(&rs, Kind::Session, Range::Day, now);
    assert!(all.segments.len() >= 6, "{}", all.segments.len());
    assert_eq!(all.segments.iter().map(Vec::len).sum::<usize>(), 60);
    assert_eq!(day.segments.iter().map(Vec::len).sum::<usize>(), 49); // last 24h inclusive of the edge
    assert_eq!((day.x1 - day.x0) as i64, 24 * 3600);
    assert!(chart_data(&rs, Kind::Weekly, Range::All, now)
        .segments
        .is_empty());
}

fn gauge_at(pct: f64, hours_left: f64) -> aitrack::analysis::Gauge {
    aitrack::analysis::Gauge {
        pct,
        resets_at: None,
        hours_left: Some(hours_left),
        rate_per_hour: Some(10.0),
        projected_pct: Some(50.0),
        hours_to_limit: Some(8.0),
        window_hours: 5.0,
        window_id: String::new(),
        resets_carried: false,
        reset_passed: false,
        rate_method: None,
    }
}

#[test]
fn vendors_without_data_are_hidden_and_rows_follow_the_setting() {
    let dir = TempDir::new();
    let app = make_app(&dir, fake_poll());
    app.history.load();
    let s = app.state();
    let mut claude = vendor::Vendor::new(VendorId::Claude);
    claude.session = Some(gauge_at(42.0, 2.2));
    claude.weekly = Some(gauge_at(95.0, 70.0));
    let silent = vendor::Vendor::new(VendorId::Codex); // no readings
    let home = build_home_for(&s, &[claude.clone(), silent], Utc::now());
    assert_eq!(home.vendors.len(), 1);
    let rows = &home.vendors[0].rows;
    // 20 segments, honest numbers, red only from 90%
    assert_eq!(rows[0].cells.iter().filter(|c| **c == Cell::Lit).count(), 8);
    assert_eq!((rows[0].level, rows[1].level), (Level::Normal, Level::Bad));
    assert_eq!(rows[1].value, "95%");
    // pace line: 8h to limit vs 70h left -> on-pace note, not the warning
    assert_eq!(rows[0].tip[1].text, "On pace for ~50% by reset");
    assert_eq!(rows[0].tip[1].tone, Tone::Muted);
    // a vendor missing a window still gets both rows
    claude.weekly = None;
    let home = build_home_for(&s, &[claude], Utc::now());
    assert_eq!(home.vendors[0].rows[1].value, "--");
    assert_eq!(home.vendors[0].rows[1].tip[0].text, "Weekly not reported");
}

#[test]
fn limit_warning_tone_and_detail_without_history() {
    let mut v = vendor::Vendor::new(VendorId::Codex);
    v.session = Some(aitrack::analysis::Gauge {
        hours_to_limit: Some(1.0),
        ..gauge_at(60.0, 3.0)
    });
    let dir = TempDir::new();
    let app = make_app(&dir, fake_poll());
    app.history.load();
    let home = build_home_for(&app.state(), std::slice::from_ref(&v), Utc::now());
    let tip = &home.vendors[0].rows[0].tip[1];
    assert_eq!(
        (tip.text.as_str(), tip.tone),
        ("Limit in ~1.0h at this pace", Tone::Warn)
    );
    let d = build_detail(&v, Kind::Session, false);
    assert_eq!(d.chart, None); // no stored history for this vendor
    assert_eq!(
        (d.value.as_str(), d.unit, d.vendor),
        ("60", "%", VendorId::Codex)
    );
    assert!(!d.rows.iter().any(|r| r.0 == "Hit rate")); // no stats either
}

#[test]
fn demo_vendors_cover_all_four() {
    let v = vendor::demo_vendors(Utc::now());
    let names: Vec<&str> = v.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Claude", "ChatGPT", "Antigravity", "Grok"]);
    assert!(v.iter().all(|v| v.has_data()));
}

#[test]
fn tooltip_time_formats() {
    use chrono::{FixedOffset, TimeZone};
    assert_eq!(left_long(2.0 + 14.0 / 60.0), "2h 14m");
    assert_eq!(left_long(2.0), "2h");
    assert_eq!(left_long(0.5), "30m");
    assert_eq!(left_long(0.0), "1m");
    assert_eq!(left_long(75.0), "3d 3h");
    assert_eq!(left_long(72.0), "3d");
    let tz = FixedOffset::west_opt(5 * 3600).unwrap();
    let t = tz.with_ymd_and_hms(2026, 10, 17, 15, 42, 0).unwrap(); // a Saturday
    assert_eq!(clock_text(t, false), "3:42 PM");
    assert_eq!(clock_text(t, true), "Sat 3:42 PM");
}

#[test]
fn messages_say_mac_on_a_mac_and_pc_elsewhere() {
    assert_eq!(aitrack::util::this_machine_for(true), "Mac");
    assert_eq!(aitrack::util::this_machine_for(false), "PC");
}
