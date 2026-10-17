// SPDX-License-Identifier: GPL-3.0-or-later
//! Regression tests for the release-1 code review (one section per finding). Every test takes an
//! explicit `now`; nothing here depends on the wall clock.
use aitrack::analysis::analyze;
use aitrack::reading::{Kind, Reading};
use aitrack::stats::{compute, StatsRange};
use aitrack::view::{chart_data, Range};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use std::time::Instant;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn rd(
    t: DateTime<Utc>,
    session: Option<(f64, Option<DateTime<Utc>>)>,
    weekly: Option<(f64, Option<DateTime<Utc>>)>,
) -> Reading {
    let b = |x: Option<(f64, Option<DateTime<Utc>>)>| match x {
        Some((pct, r)) => json!({"pct": pct, "resets_at": r.map(|r| r.to_rfc3339())}),
        None => serde_json::Value::Null,
    };
    Reading::from_value(&json!({
        "ok": true, "ts": t.to_rfc3339(), "session": b(session), "weekly": b(weekly),
    }))
}

// ---- F1: window grouping scales (no O(n x r) scan) ---------------------------------------------

/// Two months of readings every three minutes, alternating between a source that reports the
/// reset time and one that does not (the real mix of cache and history samples).
fn two_months() -> Vec<Reading> {
    let start = now() - Duration::days(60);
    let mut v = Vec::new();
    for i in 0..(60 * 24 * 20) {
        let t = start + Duration::minutes(3 * i);
        // session: 5 h windows, weekly: 7 d windows
        let s_end = start + Duration::hours(5 * (t - start).num_hours().div_euclid(5) + 5);
        let w_end = start + Duration::days(7 * (t - start).num_days().div_euclid(7) + 7);
        let (s_pct, w_pct) = (
            ((t - start).num_minutes().rem_euclid(300)) as f64 / 3.5,
            ((t - start).num_hours().rem_euclid(168)) as f64 / 2.0,
        );
        let stamped = i % 2 == 0;
        v.push(rd(
            t,
            Some((s_pct, stamped.then_some(s_end))),
            Some((w_pct, stamped.then_some(w_end))),
        ));
    }
    v
}

#[test]
fn tens_of_thousands_of_readings_go_through_analysis_chart_and_stats_quickly() {
    let rs = two_months();
    assert!(rs.len() > 25_000);
    // Only a guard against going quadratic again (that took minutes on this input), so the bound is
    // far above what a loaded CI machine needs (about 0.1 s here); correctness is asserted below.
    let budget = std::time::Duration::from_secs(30);
    let t0 = Instant::now();
    let plan = analyze(&rs, now()).unwrap();
    let a = t0.elapsed();
    let t0 = Instant::now();
    let chart = chart_data(&rs, Kind::Session, Range::All, now());
    let c = t0.elapsed();
    let t0 = Instant::now();
    let st = compute(&rs, StatsRange::All, now(), 0);
    let s = t0.elapsed();
    // the grouping is right, not just fast: one window per 5 h and per 7 days
    assert!(
        (285..=290).contains(&st.session.peaks.len()),
        "{}",
        st.session.peaks.len()
    );
    assert!(plan.session_stats.windows >= 285);
    assert!(!chart.segments.is_empty());
    assert!(
        a < budget && c < budget && s < budget,
        "analyze {a:?}, chart {c:?}, stats {s:?}"
    );
}

// ---- F2: a fall across a window boundary is not an early end ------------------------------------

#[test]
fn a_new_window_that_starts_unstamped_after_a_high_one_stays_one_window() {
    use aitrack::alerts::{AlertKind, Alerts, Notification, Notifier};
    use std::sync::Mutex;
    struct Rec(Mutex<Vec<(AlertKind, String)>>);
    impl Notifier for Rec {
        fn notify(&self, n: &Notification) -> Result<(), String> {
            self.0.lock().unwrap().push((n.kind, n.title.clone()));
            Ok(())
        }
    }
    let day = Utc.with_ymd_and_hms(2026, 10, 14, 0, 0, 0).unwrap();
    let at = |h: i64, m: i64| day + Duration::minutes(h * 60 + m);
    let (r1, r2) = (Some(at(12, 0)), Some(at(19, 0)));
    // 10:00 93% (reset 12:00), then the next window: first sample unstamped (history), then
    // cache and history samples alternating
    let rs = vec![
        rd(at(10, 0), Some((93.0, r1)), None),
        rd(at(14, 5), Some((2.0, None)), None),
        rd(at(14, 10), Some((3.0, r2)), None),
        rd(at(14, 30), Some((5.0, None)), None),
        rd(at(15, 0), Some((9.0, r2)), None),
        rd(at(15, 20), Some((12.0, None)), None),
    ];
    let end = at(15, 21);
    let plan = analyze(&rs, end).unwrap();
    assert_eq!(plan.session_stats.windows, 2, "{:?}", plan.session_stats);
    // feed the readings one by one to the alert evaluator, as the app does after each poll
    let dir = crate::common::TempDir::new();
    let alerts = Alerts::load(dir.join("alerts.json"));
    let rec = Rec(Mutex::new(vec![]));
    let mut ids = Vec::new();
    let mut first_window = 0;
    for n in 1..=rs.len() {
        let now = rs[n - 1].t.unwrap() + Duration::minutes(1);
        let plan = analyze(&rs[..n], now).unwrap();
        ids.push(
            plan.session
                .as_ref()
                .map(|g| g.window_id.clone())
                .unwrap_or_default(),
        );
        alerts.evaluate("Claude", &plan, now, &rec);
        if n == 1 {
            first_window = rec.0.lock().unwrap().len();
        }
    }
    // from the first sample of the new window on, every reading is the same window
    let tail: Vec<&String> = ids[2..].iter().collect();
    assert!(tail.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    // after the first window's own alerts the new window brings exactly one "reset" notice and
    // nothing else (no repeated threshold toasts, no second recovery)
    let sent = rec.0.lock().unwrap().clone();
    let later: Vec<AlertKind> = sent[first_window..].iter().map(|s| s.0).collect();
    assert_eq!(later, [AlertKind::Recovery], "{sent:?}");
}

// ---- F3: a reported reset time that has passed ends the window ----------------------------------

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

/// The home view of one fresh Claude reading (5 minutes old) whose resets are `session_in` and
/// `weekly_in` minutes from `now` (negative: already passed).
fn home_for(
    session_in: i64,
    weekly_in: i64,
) -> (
    aitrack::view::HomeView,
    aitrack::vendor::Vendor,
    aitrack::state::State,
) {
    use aitrack::settings::Values;
    let t = now() - Duration::minutes(5);
    let rs = vec![
        rd(
            t - Duration::minutes(30),
            Some((88.0, Some(now() + Duration::minutes(session_in)))),
            Some((20.0, Some(now() + Duration::minutes(weekly_in)))),
        ),
        rd(
            t,
            Some((93.0, Some(now() + Duration::minutes(session_in)))),
            Some((20.0, Some(now() + Duration::minutes(weekly_in)))),
        ),
    ];
    let st = aitrack::state::build_state(&rs, Values::default(), status(), now());
    let vs = aitrack::vendor::from_state_at(&st, now());
    let home = aitrack::view::build_home_for(&st, &vs, now());
    (home, vs[0].clone(), st)
}

#[test]
fn a_passed_reported_reset_is_not_shown_as_resets_in_one_minute() {
    use aitrack::reading::Kind;
    use aitrack::view::build_detail_at;
    for passed_by in [1, 30] {
        let (home, v, _) = home_for(-passed_by, 5000);
        let row = &home.vendors[0].rows[0];
        let tip: Vec<&str> = row.tip.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            (row.value.as_str(), row.stale),
            ("--", true),
            "the old percentage must not stay lit: {row:?}"
        );
        assert!(
            tip[0].contains("reset time has passed")
                && tip[0].contains("waiting for a new reading"),
            "{tip:?}"
        );
        assert!(
            !tip.iter()
                .any(|l| l.contains("resets in") || l.contains("Limit in") || l.contains("Pace")),
            "no countdown, clock time or pace: {tip:?}"
        );
        assert_eq!(home.vendors[0].warn, None, "no Empty in warning");
        let d = build_detail_at(&v, Kind::Session, false, now());
        assert_eq!(
            d.resets.as_deref(),
            Some("Reset passed, waiting for a new reading")
        );
        assert_eq!(d.value, "--");
        assert!(d.rows.iter().all(|r| r.0 != "Pace" && r.0 != "At reset"));
        // the weekly row (reset far ahead) is unaffected
        assert_ne!(home.vendors[0].rows[1].value, "--");
    }
    // the weekly row alone
    let (home, _, _) = home_for(120, -3);
    assert_eq!(home.vendors[0].rows[1].value, "--");
    assert!(home.vendors[0].rows[1].tip[0]
        .text
        .contains("reset time has passed"));
    assert_ne!(home.vendors[0].rows[0].value, "--");
    // a reset still ahead is shown as before
    let (home, _, _) = home_for(60, 5000);
    assert!(home.vendors[0].rows[0].tip[0]
        .text
        .starts_with("Session resets in 1h"));
}

// ---- F5: a live reading says why the local sources were skipped -------------------------------

mod fake_sources {
    use aitrack::live::CollectError;
    use aitrack::reading::Bucket;
    use aitrack::sources::{Observation, Sample, UsageSource};
    use chrono::{DateTime, Duration, Utc};

    pub struct Local(pub DateTime<Utc>);
    impl UsageSource for Local {
        fn name(&self) -> &'static str {
            "desktop-cache"
        }
        fn max_age(&self) -> Option<Duration> {
            Some(Duration::minutes(120))
        }
        fn stale_message(&self, age_min: i64) -> String {
            format!("desktop-cache is stale (last sample {age_min} min ago)")
        }
        fn read(&self, _: DateTime<Utc>) -> Result<Observation, CollectError> {
            let s = Sample {
                at: self.0,
                session: None,
                weekly: None,
                raw: None,
            };
            Ok(Observation {
                latest: s.clone(),
                history: vec![s],
                raw: None,
            })
        }
    }

    pub struct Broken;
    impl UsageSource for Broken {
        fn name(&self) -> &'static str {
            "claude-json"
        }
        fn read(&self, _: DateTime<Utc>) -> Result<Observation, CollectError> {
            Err(CollectError::new("no usage entry (the file is missing)"))
        }
    }

    /// A live source that always fails (a login that expired).
    pub struct LiveFailing;
    impl UsageSource for LiveFailing {
        fn name(&self) -> &'static str {
            "oauth"
        }
        fn live(&self) -> bool {
            true
        }
        fn read(&self, _: DateTime<Utc>) -> Result<Observation, CollectError> {
            Err(CollectError::new("login expired: run grok once"))
        }
    }

    pub struct Live;
    impl UsageSource for Live {
        fn name(&self) -> &'static str {
            "oauth"
        }
        fn live(&self) -> bool {
            true
        }
        fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
            let s = Sample {
                at: now,
                session: Some(Bucket {
                    carried: false,
                    pct: 12.0,
                    resets_at: None,
                }),
                weekly: None,
                raw: None,
            };
            Ok(Observation {
                latest: s,
                history: vec![],
                raw: None,
            })
        }
    }
}

#[test]
fn a_live_reading_carries_the_note_why_local_sources_were_skipped() {
    use aitrack::live::Poller;
    use aitrack::store::{JsonlStore, Store};
    let t = crate::common::TempDir::new();
    let store = JsonlStore::new(t.join("u.jsonl"));
    let poller = Poller::with_chain(vec![
        Box::new(fake_sources::Local(now() - Duration::hours(5))),
        Box::new(fake_sources::Broken),
        Box::new(fake_sources::Live),
    ]);
    let rec = poller.poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "oauth");
    let note = rec["note"].as_str().expect("a note on the live record");
    assert!(
        note.contains("desktop-cache skipped: desktop-cache is stale")
            && note.contains("claude-json skipped: no usage entry"),
        "{note}"
    );
    // the stored record has it too, and Diagnostics shows it
    let stored = store.read();
    assert_eq!(stored.last().unwrap()["note"], rec["note"]);
    let readings: Vec<aitrack::reading::Reading> = stored
        .iter()
        .map(aitrack::reading::Reading::from_value)
        .collect();
    let st = aitrack::state::build_state(
        &readings,
        aitrack::settings::Values::default(),
        status(),
        now(),
    );
    assert!(
        st.diagnostics[0]
            .text
            .contains("oauth (desktop-cache skipped"),
        "{}",
        st.diagnostics[0].text
    );
    // nothing skipped: no note
    let rec = Poller::with_chain(vec![Box::new(fake_sources::Live)])
        .poll_at(&store, now())
        .unwrap();
    assert!(rec.get("note").is_none());
}

// ---- F6: the status line speaks about the vendors that are shown -------------------------------

#[test]
fn with_claude_hidden_the_status_line_is_about_grok_not_old_claude_data() {
    use aitrack::settings::Values;
    use aitrack::state::{build_grok_state, build_state};
    let old = vec![rd(
        now() - Duration::days(3),
        Some((40.0, Some(now() - Duration::days(3) + Duration::hours(2)))),
        None,
    )];
    let grok = vec![Reading::from_value(&json!({
        "ok": true, "source": "grok-billing", "vendor": "grok",
        "ts": (now() - Duration::minutes(2)).to_rfc3339(),
        "weekly": {"pct": 8.0, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
    }))];
    let mut st = build_state(&old, Values::default(), status(), now());
    st.grok = Some(build_grok_state(&grok, status(), true, now()));
    // Claude shown: its old data is reported
    st.claude_on = true;
    assert!(aitrack::view::status_line(&st, now())
        .text
        .starts_with("No current data"));
    // Claude hidden (Show Claude = Off): the line is about Grok
    st.claude_on = false;
    let line = aitrack::view::status_line(&st, now());
    assert!(
        !line.text.contains("No current data") && !line.text.contains("72.0 h"),
        "{line:?}"
    );
    assert!(line.text.starts_with("Updated 2 min ago"), "{line:?}");
    assert_eq!(line.dot, aitrack::view::Dot::Ok);
    // and an old Grok value that is still shown (dimmed) is not called "no current data" either
    st.grok = Some(build_grok_state(
        &[Reading::from_value(&json!({
            "ok": true, "source": "grok-log", "vendor": "grok",
            "ts": (now() - Duration::hours(9)).to_rfc3339(),
            "weekly": {"pct": 8.0, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
        }))],
        status(),
        true,
        now(),
    ));
    let line = aitrack::view::status_line(&st, now());
    assert!(!line.text.contains("No current data"), "{line:?}");
}

// ---- F7: the By-model block survives a newer reading without limits ----------------------------

#[test]
fn per_model_limits_come_from_the_newest_reading_that_has_any() {
    use aitrack::settings::Values;
    let with_limits = Reading::from_value(&json!({
        "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(40)).to_rfc3339(),
        "session": {"pct": 10.0, "resets_at": null}, "weekly": {"pct": 20.0, "resets_at": null},
        "limits": [{"kind": "weekly_scoped", "percent": 43, "scope": {"model": {"display_name": "Fable"}}}],
    }));
    let history_only = Reading::from_value(&json!({
        "ok": true, "source": "desktop-history", "ts": (now() - Duration::minutes(2)).to_rfc3339(),
        "session": {"pct": 12.0, "resets_at": null}, "weekly": {"pct": 21.0, "resets_at": null},
        "limits": null,
    }));
    assert!(!with_limits.limits.is_empty() && history_only.limits.is_empty());
    let st = aitrack::state::build_state(
        &[with_limits.clone(), history_only],
        Values::default(),
        status(),
        now(),
    );
    assert_eq!(st.models, with_limits.limits, "the block must not vanish");
    // a newer reading that has limits wins
    let newer = Reading::from_value(&json!({
        "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(1)).to_rfc3339(),
        "session": {"pct": 12.0, "resets_at": null}, "weekly": {"pct": 21.0, "resets_at": null},
        "limits": [{"kind": "weekly_scoped", "percent": 50, "scope": {"model": {"display_name": "Opus"}}}],
    }));
    let st = aitrack::state::build_state(
        &[with_limits, newer.clone()],
        Values::default(),
        status(),
        now(),
    );
    assert_eq!(st.models, newer.limits);
}

// ---- F4: nothing assumes reset times fall on the hour ------------------------------------------

#[test]
fn unstamped_samples_find_their_window_for_resets_at_half_past_and_41_minutes() {
    // the owner's real tooltips: session reset 6:30 PM, weekly reset Fri 2:41 AM
    let day = Utc.with_ymd_and_hms(2026, 10, 14, 0, 0, 0).unwrap();
    let s_reset = day + Duration::hours(18) + Duration::minutes(30);
    let w_reset = day + Duration::days(2) + Duration::hours(2) + Duration::minutes(41);
    let at = |h: i64, m: i64| day + Duration::minutes(h * 60 + m);
    let rs = vec![
        // unstamped history sample at the very start of the 5 h window (13:30 + a minute)
        rd(at(13, 31), Some((10.0, None)), Some((30.0, None))),
        // stamped cache samples
        rd(
            at(15, 0),
            Some((20.0, Some(s_reset))),
            Some((31.0, Some(w_reset))),
        ),
        // unstamped again, near the end of both windows
        rd(at(18, 29), Some((30.0, None)), Some((32.0, None))),
    ];
    let plan = analyze(&rs, at(18, 31)).unwrap();
    assert_eq!(plan.session_stats.windows, 1, "{:?}", plan.session_stats);
    assert_eq!(plan.weekly_stats.windows, 1, "{:?}", plan.weekly_stats);
    // the minute after the reset belongs to the old window, the one before the 5 h start does not
    let before = vec![
        rd(at(13, 20), Some((10.0, None)), None),
        rd(at(15, 0), Some((20.0, Some(s_reset))), None),
    ];
    assert_eq!(
        analyze(&before, at(15, 1)).unwrap().session_stats.windows,
        2
    );
}

// ---- F8: the minimum Rust version is declared --------------------------------------------------

#[test]
fn cargo_toml_declares_the_minimum_rust_version_the_file_lock_needs() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("rust-version"))
        .expect("rust-version in Cargo.toml");
    let v = line.split('"').nth(1).unwrap();
    let minor: u32 = v.split('.').nth(1).unwrap().parse().unwrap();
    assert!(
        v.starts_with("1.") && minor >= 89,
        "File::try_lock is stable since 1.89: {v}"
    );
}

// ---- O2: By-model limits belong to the current weekly window -----------------------------------

#[test]
fn by_model_limits_from_the_previous_weekly_window_or_a_past_model_reset_are_not_shown() {
    use aitrack::reading::ModelLimit;
    use aitrack::settings::Values;
    let w_reset = now() + Duration::days(3); // the weekly window started 4 days ago
    let rd2 = |ago: Duration, limits: serde_json::Value| {
        Reading::from_value(&json!({
            "ok": true, "source": "oauth", "ts": (now() - ago).to_rfc3339(),
            "session": {"pct": 10.0, "resets_at": (now() + Duration::hours(2)).to_rfc3339()},
            "weekly": {"pct": 20.0, "resets_at": w_reset.to_rfc3339()},
            "limits": limits,
        }))
    };
    let fable = |pct: u32| json!([{"kind": "weekly_scoped", "percent": pct, "scope": {"model": {"display_name": "Fable"}}}]);
    let state =
        |rs: &[Reading]| aitrack::state::build_state(rs, Values::default(), status(), now());
    // limits of last week's window (6 days old, before this window began) are not this week's
    let st = state(&[
        rd2(Duration::days(6), fable(90)),
        rd2(Duration::minutes(1), json!(null)),
    ]);
    assert!(st.models.is_empty(), "{:?}", st.models);
    // limits from inside the current window still show
    let st = state(&[
        rd2(Duration::days(2), fable(40)),
        rd2(Duration::minutes(1), json!(null)),
    ]);
    assert_eq!(st.models.len(), 1);
    assert_eq!(st.models[0].pct, 40.0);
    // and a model whose own reset time has passed is dropped from the tooltip instead of 'resets in 1m'
    let mut st = state(&[rd2(Duration::minutes(1), json!(null))]);
    st.models = vec![
        ModelLimit {
            name: "Fable".into(),
            kind: "weekly_scoped".into(),
            pct: 43.0,
            resets_at: Some((now() - Duration::minutes(1)).to_rfc3339()),
        },
        ModelLimit {
            name: "Opus".into(),
            kind: "weekly_scoped".into(),
            pct: 12.0,
            resets_at: Some((now() + Duration::days(2)).to_rfc3339()),
        },
    ];
    let vs = aitrack::vendor::from_state_at(&st, now());
    let home = aitrack::view::build_home_for(&st, &vs, now());
    let tip: Vec<&str> = home.vendors[0].rows[1]
        .tip
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(!tip.iter().any(|l| l.contains("Fable")), "{tip:?}");
    assert!(
        tip.iter()
            .any(|l| l.starts_with("Opus") && l.contains("resets in 2d")),
        "{tip:?}"
    );
}

// ---- O3: a reading dated in the future is ignored ----------------------------------------------

#[test]
fn a_future_dated_reading_never_becomes_the_latest() {
    use aitrack::settings::Values;
    let real = |ago: i64, pct: f64| {
        rd(
            now() - Duration::minutes(ago),
            Some((pct, Some(now() + Duration::hours(2)))),
            None,
        )
    };
    // a wrong clock (or a hand-edited date) stamped one reading three days ahead
    let skewed = rd(
        now() + Duration::days(3),
        Some((77.0, Some(now() + Duration::days(3) + Duration::hours(2)))),
        None,
    );
    let rs = vec![real(20, 10.0), skewed.clone(), real(2, 12.0)];
    let plan = analyze(&rs, now()).unwrap();
    assert_eq!(
        plan.session.as_ref().unwrap().pct,
        12.0,
        "the real, latest reading is chosen"
    );
    assert_eq!(plan.last, rs[2].ts);
    assert_eq!(plan.stale_minutes, 2);
    // the view says the data is 2 minutes old, not "just now"
    let st = aitrack::state::build_state(&rs, Values::default(), status(), now());
    assert!(aitrack::view::status_line(&st, now())
        .text
        .contains("2 min ago"));
    // the chart and the stats do not draw or count it
    let chart = chart_data(&rs, Kind::Session, Range::All, now());
    assert!(
        chart
            .segments
            .iter()
            .flatten()
            .all(|p| p.0 <= now().timestamp() as f64 + 300.0),
        "{chart:?}"
    );
    assert!(chart.segments.iter().flatten().all(|p| p.1 != 77.0));
    let stats = compute(&rs, StatsRange::All, now(), 0);
    assert!(stats.session.peaks.iter().all(|w| w.peak != 77.0));
    // a few minutes of clock skew is tolerated (clamped to "now")
    let slightly = rd(now() + Duration::minutes(3), Some((15.0, None)), None);
    assert_eq!(
        analyze(&[real(20, 10.0), slightly], now())
            .unwrap()
            .session
            .unwrap()
            .pct,
        15.0
    );
}

// ---- age of the data: an unchanged value from a fresh poll is fresh ----------------------------

fn grok_rows(source: &str, count: i64, every_min: i64, newest_ago_min: i64) -> Vec<Reading> {
    (0..count)
        .map(|i| {
            let t = now() - Duration::minutes(newest_ago_min + (count - 1 - i) * every_min);
            Reading::from_value(&json!({
                "ok": true, "source": source, "vendor": "grok", "ts": t.to_rfc3339(), "observed_at": t.to_rfc3339(),
                "weekly": {"pct": 12.0, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
            }))
        })
        .collect()
}

pub fn grok_chip(
    rs: &[Reading],
    interval_min: f64,
) -> (Option<aitrack::view::AgeChip>, aitrack::vendor::Vendor) {
    use aitrack::settings::Values;
    let mut st = aitrack::state::build_state(&[], Values::default(), status(), now());
    st.settings.interval_min = interval_min;
    st.grok = Some(aitrack::state::build_grok_state(rs, status(), true, now()));
    let vs = aitrack::vendor::from_state_at(&st, now());
    let v = vs
        .into_iter()
        .find(|v| v.id == aitrack::vendor::VendorId::Grok)
        .unwrap();
    (aitrack::view::age_chip(&v, interval_min, now()), v)
}

#[test]
fn identical_fresh_grok_readings_have_no_age_chip_and_old_ones_still_do() {
    // five identical readings 5 minutes apart, the newest 1 minute ago (live billing, same value)
    for interval in [5.0, 15.0] {
        let (chip, v) = grok_chip(&grok_rows("grok-billing", 5, 5, 1), interval);
        let age = (now() - v.observed_at.unwrap()).num_minutes();
        assert!(age <= 5, "the age is that of the newest poll: {age} min");
        assert!(chip.is_none(), "interval {interval}: {chip:?}");
    }
    // the same five readings, all old: the chip is there
    let (chip, v) = grok_chip(&grok_rows("grok-billing", 5, 5, 50), 5.0);
    assert_eq!((now() - v.observed_at.unwrap()).num_minutes(), 50);
    assert_eq!(chip.expect("old data").compact, "50 min");
    // a stale log sample next to fresh billing rows: the fresh polls decide the age
    let mut mixed = grok_rows("grok-log", 1, 5, 50);
    mixed.extend(grok_rows("grok-billing", 3, 5, 1));
    assert!(grok_chip(&mixed, 5.0).0.is_none());
}

#[test]
fn a_failing_live_source_behind_a_working_local_one_leaves_a_diagnostics_row() {
    use aitrack::live::Poller;
    use aitrack::store::{JsonlStore, Store};
    let t = crate::common::TempDir::new();
    let store = JsonlStore::new(t.join("u.jsonl"));
    let poller = Poller::with_chain(vec![
        Box::new(fake_sources::LiveFailing),
        Box::new(fake_sources::Local(now() - Duration::minutes(1))),
    ]);
    // the local tick never asks the live source: no failure row
    poller.poll_local_at(&store, now()).unwrap();
    assert!(store.read().iter().all(|r| r["ok"] != false));
    // a full poll asks it, it fails, the local reading still serves ...
    poller.poll_at(&store, now()).unwrap();
    let failed: Vec<_> = store
        .read()
        .into_iter()
        .filter(|r| r["ok"] == false)
        .collect();
    // ... and the failure is on record (so Diagnostics shows why the live data is not updating)
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["error"], "oauth: login expired: run grok once");
    poller
        .poll_at(&store, now() + Duration::minutes(5))
        .unwrap();
    assert_eq!(store.read().iter().filter(|r| r["ok"] == false).count(), 2);
}

#[test]
fn every_screenshot_the_readme_shows_exists_so_the_pdf_and_the_zips_have_them() {
    // scripts/readme-pdf.ps1 inlines docs/img/* into README.pdf and the zips carry docs/img next to
    // README.md: a missing picture would fail the release build, so check it here on every platform.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    let mut seen = 0;
    for part in readme.split("](docs/img/").skip(1) {
        let name = part.split(')').next().unwrap();
        assert!(
            root.join("docs/img").join(name).is_file(),
            "README shows docs/img/{name}, which does not exist"
        );
        seen += 1;
    }
    assert!(seen >= 5, "{seen} screenshots found");
    // the picture the zip check requires
    assert!(root.join("docs/img/home-dark.png").is_file());
}
