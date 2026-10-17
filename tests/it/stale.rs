// SPDX-License-Identifier: GPL-3.0-or-later
//! "No current data" must never look like a reading: stale history is dashes and `--`, the status
//! line is a short summary with the full text on hover, and the pace tooltip names its method.
use crate::common;
use aitrack::app::App;
use aitrack::collector::Status;
use aitrack::live::Poller;
use aitrack::reading::{Kind, Reading};
use aitrack::settings::{Settings, Values};
use aitrack::sources::{Env, Os};
use aitrack::state::{build_state, reading_is_current};
use aitrack::store::JsonlStore;
use aitrack::view::{self, Cell, Dot};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn status() -> Status {
    Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    }
}

fn reading(minutes_ago: i64, source: &str, session: f64, resets_in_min: Option<i64>) -> Reading {
    Reading::from_value(&json!({
        "ok": true, "source": source, "ts": (now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        "session": {"pct": session, "resets_at": resets_in_min.map(|m| (now() + Duration::minutes(m)).to_rfc3339())},
        "weekly": {"pct": 5.0, "resets_at": resets_in_min.map(|_| (now() + Duration::hours(100)).to_rfc3339())},
    }))
}

// ------------------------------------------------------------ the real chain, collector and state

#[test]
fn stale_zero_percent_history_is_never_shown_as_the_current_reading() {
    let t = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.xdg_config_home = Some(t.join("xdg"));
    let data = t.join("xdg/Claude");
    std::fs::create_dir_all(&data).unwrap();
    // yesterday's samples: the 5h window had just reset and read 0%
    let real_now = Utc::now();
    let sample = |m: i64| json!({"t": (real_now - Duration::minutes(m)).timestamp_millis(), "org": "org-x", "u": {"fh": 0, "sd": 0}});
    std::fs::write(
        data.join("plan-usage-history.json"),
        json!({"version": 2, "samples": [sample(800), sample(790)]}).to_string(),
    )
    .unwrap();

    let usage = t.join("usage.jsonl");
    let app = App::new(
        Box::new(JsonlStore::new(&usage)),
        Arc::new(Settings::load(t.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None), // the real default chain
    );
    app.start();
    let t0 = Instant::now();
    while app.collector.status().error.is_none() {
        assert!(t0.elapsed() < StdDuration::from_secs(10), "no poll result");
        std::thread::sleep(StdDuration::from_millis(20));
    }
    let st = loop {
        let st = app.state();
        if st.plan.is_some() {
            break st;
        }
        assert!(
            t0.elapsed() < StdDuration::from_secs(10),
            "history never loaded"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    };
    // the 0% samples are in the store as history ...
    assert_eq!(st.plan.as_ref().unwrap().session.as_ref().unwrap().pct, 0.0);
    // ... but the view says there is no current data
    let home = view::build_home(&st, Utc::now());
    let claude = &home.vendors[0];
    for row in &claude.rows {
        assert!(row.stale, "{row:?}");
        assert_eq!(row.value, "--");
        assert!(row.cells.iter().all(|c| *c == Cell::Off));
        assert!(row.tip[0].text.contains("no current data"), "{:?}", row.tip);
    }
    assert_eq!(claude.warn, None);
    assert_eq!(home.status.dot, Dot::Err);
    assert_eq!(
        home.status.text,
        "No current data \u{b7} Claude app closed?"
    );
    assert!(home
        .status
        .full
        .as_deref()
        .unwrap()
        .contains("Claude desktop usage is stale"));
    // the detail pane agrees
    let v = &aitrack::vendor::from_state(&st)[0];
    let d = view::build_detail(v, Kind::Session, false);
    assert_eq!(
        (d.value.as_str(), d.resets.as_deref()),
        ("--", Some("No current data"))
    );
    app.stop();
    app.collector.join();
}

// ------------------------------------------------------------ current or not

#[test]
fn freshness_limits_per_source() {
    let plan = |m: i64, src: &str| {
        let st = build_state(
            &[reading(m, src, 10.0, Some(100))],
            Values::default(),
            status(),
            now(),
        );
        st.plan.unwrap()
    };
    let v = Values::default(); // 15 min network interval
    assert!(reading_is_current(&plan(10, "desktop-cache"), &v, now()));
    assert!(reading_is_current(&plan(119, "desktop-history"), &v, now()));
    assert!(!reading_is_current(
        &plan(121, "desktop-history"),
        &v,
        now()
    ));
    assert!(!reading_is_current(&plan(121, "claude-json"), &v, now()));
    // the network source is allowed 2.5 polling intervals (but at least 2 h)
    assert!(reading_is_current(&plan(100, "oauth"), &v, now()));
    assert!(!reading_is_current(&plan(125, "oauth"), &v, now()));
    let slow = Values {
        interval_min: 120.0,
        ..Values::default()
    };
    assert!(reading_is_current(&plan(250, "oauth"), &slow, now()));
    assert!(!reading_is_current(&plan(310, "oauth"), &slow, now()));
}

#[test]
fn stale_reading_renders_dashes_and_current_one_renders_normally() {
    let mk = |m: i64| {
        build_state(
            &[reading(m, "desktop-cache", 55.0, Some(200))],
            Values::default(),
            status(),
            now(),
        )
    };
    let fresh = view::build_home(&mk(3), now());
    assert!(!fresh.vendors[0].rows[0].stale);
    assert_eq!(fresh.vendors[0].rows[0].value, "55%");
    let old = view::build_home(&mk(300), now());
    assert!(old.vendors[0]
        .rows
        .iter()
        .all(|r| r.stale && r.value == "--"));
    assert_eq!(
        old.status.text,
        "No current data \u{b7} last reading 5.0 h ago"
    );
    assert_eq!(old.status.dot, Dot::Stale);
}

// ------------------------------------------------------------ the status line

#[test]
fn long_errors_become_short_summaries_with_the_full_text_kept() {
    let long = "Claude desktop usage is stale (last sample 13 h ago): open the Claude app; ~/.claude.json has no usable usage cache (file not found); usage API: no Claude login found (looked in: /home/someone/.claude/.credentials.json, /home/someone/other/.credentials.json). Sign in with Claude Code (run `claude`, then /login); a login that only exists in the Claude desktop app is not supported yet";
    assert_eq!(view::short_reason(long), "Claude app closed?");
    for (e, want) in [
        ("HTTP 429", "rate limited"),
        ("login rejected (HTTP 401); sign in to Claude Code again", "login rejected"),
        ("login token expired; open Claude Code once to refresh it", "login expired"),
        ("network error: dns error", "offline?"),
        ("no Claude login found (looked in: /x)", "no Claude login"),
        ("Claude desktop history not found (checked 1 location); ~/.claude.json has no usable usage cache (file not found); usage API: no Claude login found (looked in: /x)", "no Claude data found"),
        ("something rather unexpected happened in the parser today", "something rather unexpected happene\u{2026}"),
    ] {
        assert_eq!(view::short_reason(e), want, "{e}");
    }
    let mut c = status();
    c.error = Some(long.into());
    let st = build_state(&[], Values::default(), c, now());
    let home = view::build_home(&st, now());
    assert_eq!(
        home.status.text,
        "No current data \u{b7} Claude app closed?"
    );
    assert!(home.status.text.chars().count() <= 40);
    assert_eq!(home.status.full.as_deref(), Some(long));
    // the empty-state card keeps the full wrapped message
    assert_eq!(home.empty.unwrap().body, long);
}

#[test]
fn an_error_next_to_current_data_adds_only_the_short_reason() {
    let mut c = status();
    c.error = Some("HTTP 429".into());
    let st = build_state(
        &[reading(5, "oauth", 30.0, Some(100))],
        Values::default(),
        c,
        now(),
    );
    let s = view::build_home(&st, now()).status;
    assert_eq!(
        (s.dot, s.text.as_str()),
        (Dot::Err, "Updated 5 min ago \u{b7} rate limited")
    );
    assert_eq!(s.full.as_deref(), Some("HTTP 429"));
}

// ------------------------------------------------------------ pace method in the tooltip

#[test]
fn tooltip_names_the_pace_method() {
    // three steady readings (10 pts/h) but the window has averaged 15 pts/h since it opened: the
    // average is the floor, so that is what is shown and named
    let rs = [
        reading(120, "oauth", 10.0, Some(180)),
        reading(60, "oauth", 20.0, Some(180)),
        reading(0, "oauth", 30.0, Some(180)),
    ];
    let st = build_state(&rs, Values::default(), status(), now());
    let tip = &view::build_home(&st, now()).vendors[0].rows[0].tip;
    assert!(
        tip.iter()
            .any(|l| l.text == "Pace: +15.0 pts/h (avg since window start)"),
        "{tip:?}"
    );
    // a recent surge above the average: the recent trend is used and named (20 vs 15 pts/h)
    let rs = [
        reading(120, "oauth", 10.0, Some(100)),
        reading(60, "oauth", 20.0, Some(100)),
        reading(0, "oauth", 50.0, Some(100)),
    ];
    let st = build_state(&rs, Values::default(), status(), now());
    let tip = &view::build_home(&st, now()).vendors[0].rows[0].tip;
    assert!(
        tip.iter().any(|l| l.text == "Pace: +20.0 pts/h (recent)"),
        "{tip:?}"
    );
    // a single reading with a known reset: average since the window opened (55% in 1h40m)
    let st = build_state(
        &[reading(1, "desktop-cache", 55.0, Some(200))],
        Values::default(),
        status(),
        now(),
    );
    let tip = &view::build_home(&st, now()).vendors[0].rows[0].tip;
    assert!(
        tip.iter()
            .any(|l| l.text == "Pace: +33.0 pts/h (avg since window start)"),
        "{tip:?}"
    );
    assert!(
        tip[0].text.starts_with("Session resets in 3h 20m ("),
        "{tip:?}"
    );
    // no reset time and one reading: no pace line at all
    let st = build_state(
        &[reading(1, "desktop-history", 55.0, None)],
        Values::default(),
        status(),
        now(),
    );
    let tip = &view::build_home(&st, now()).vendors[0].rows[0].tip;
    assert!(!tip.iter().any(|l| l.text.starts_with("Pace")), "{tip:?}");
}
