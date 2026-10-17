// SPDX-License-Identifier: GPL-3.0-or-later
//! Usage alerts: once per window and event, persisted, pruned; and wired end to end (default source
//! chain, real collector thread, real App) with a recording notifier instead of a real toast.
use crate::common;
use aitrack::alerts::{AlertKind, Alerts, Notification, Notifier};
use aitrack::analysis::{analyze, Plan};
use aitrack::app::App;
use aitrack::live::Poller;
use aitrack::reading::Reading;
use aitrack::settings::Settings;
use aitrack::sources::{Env, Os};
use aitrack::store::JsonlStore;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Notification>>);

impl Recorder {
    fn titles(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.title.clone())
            .collect()
    }
}

impl Notifier for Recorder {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

struct Failing;
impl Notifier for Failing {
    fn notify(&self, _: &Notification) -> Result<(), String> {
        Err("toast service unavailable".into())
    }
}

/// One reading per `(minutes_ago, session pct)` of a 5h window that resets `resets_in_min` from now.
fn plan(points: &[(i64, f64)], resets_in_min: i64, weekly: f64) -> Plan {
    let rs: Vec<Reading> = points
        .iter()
        .map(|(m, s)| {
            Reading::from_value(&json!({
                "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(*m)).to_rfc3339(),
                "session": {"pct": s, "resets_at": (now() + Duration::minutes(resets_in_min)).to_rfc3339()},
                "weekly": {"pct": weekly, "resets_at": (now() + Duration::hours(100)).to_rfc3339()},
            }))
        })
        .collect();
    analyze(&rs, now()).unwrap()
}

/// A flat window (no pace) at `pct`: two readings an hour apart. (A lone reading gets the average
/// pace since the window opened, which can legitimately trigger the predictive alert.)
fn flat(pct: f64) -> Plan {
    plan(&[(60, pct), (0, pct)], 200, 5.0)
}

fn alerts(t: &TempDir) -> Alerts {
    Alerts::load(t.join("alerts.json"))
}

// ------------------------------------------------------------ thresholds

#[test]
fn each_threshold_fires_once_per_window_with_readable_text() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    // 78% with 3h20m left: informational
    let sent = a.evaluate("Claude", &flat(78.0), now(), &rec);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].title.as_str(), sent[0].body.as_str()),
        ("Claude session: 78% used", "Resets in 3h 20m")
    );
    assert_eq!(sent[0].kind, AlertKind::Info);
    // the same reading again, and a slightly higher one: nothing new
    assert!(a.evaluate("Claude", &flat(78.0), now(), &rec).is_empty());
    assert!(a.evaluate("Claude", &flat(85.0), now(), &rec).is_empty());
    // 90%: "10% left"
    let sent = a.evaluate("Claude", &flat(90.0), now(), &rec);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].title.as_str(), sent[0].kind),
        ("Claude session: 10% left", AlertKind::Warning)
    );
    assert!(a.evaluate("Claude", &flat(93.0), now(), &rec).is_empty());
    // 100%: limit hit
    let sent = a.evaluate("Claude", &flat(100.0), now(), &rec);
    assert_eq!(
        (sent[0].title.as_str(), sent[0].kind),
        ("Claude session: limit reached", AlertKind::Limit)
    );
    assert!(a.evaluate("Claude", &flat(100.0), now(), &rec).is_empty());
    assert_eq!(rec.titles().len(), 3);
}

#[test]
fn jumping_straight_past_a_threshold_sends_one_alert_not_a_pile() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    let sent = a.evaluate("Claude", &flat(95.0), now(), &rec);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "Claude session: 5% left");
    // the lower thresholds were marked done: dropping and re-crossing 75 stays quiet
    assert!(a.evaluate("Claude", &flat(80.0), now(), &rec).is_empty());
}

#[test]
fn weekly_and_session_are_separate_windows() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    let sent = a.evaluate(
        "Claude",
        &plan(&[(60, 80.0), (0, 80.0)], 200, 91.0),
        now(),
        &rec,
    );
    let mut titles: Vec<String> = sent.iter().map(|n| n.title.clone()).collect();
    titles.sort();
    assert_eq!(
        titles,
        ["Claude session: 80% used", "Claude weekly: 9% left"]
    );
    assert_eq!(
        sent.iter()
            .find(|n| n.title.contains("weekly"))
            .unwrap()
            .body,
        "Resets in 4d 4h"
    );
}

#[test]
fn nothing_below_the_first_threshold() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    assert!(a
        .evaluate(
            "Claude",
            &plan(&[(60, 20.0), (0, 30.0)], 200, 5.0),
            now(),
            &rec
        )
        .is_empty());
    assert!(
        !t.join("alerts.json").exists()
            || !std::fs::read_to_string(t.join("alerts.json"))
                .unwrap()
                .contains("p75")
    );
}

// ------------------------------------------------------------ predictive

#[test]
fn predictive_alert_needs_a_pace_that_empties_the_window_within_30_minutes_before_the_reset() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    // 80% rising 70 pts/h: empty in 17 min, reset in 3 h
    let p = plan(&[(60, 10.0), (30, 45.0), (0, 80.0)], 180, 5.0);
    let sent = a.evaluate("Claude", &p, now(), &rec);
    let pred: Vec<_> = sent
        .iter()
        .filter(|n| n.kind == AlertKind::Predictive)
        .collect();
    assert_eq!(pred.len(), 1, "{sent:?}");
    assert_eq!(
        (pred[0].title.as_str(), pred[0].body.as_str()),
        ("Claude session: empty in ~17m", "Resets in 3h")
    );
    assert!(a
        .evaluate("Claude", &p, now(), &rec)
        .iter()
        .all(|n| n.kind != AlertKind::Predictive));
}

#[test]
fn predictive_alert_stays_quiet_when_it_is_not_urgent() {
    let t = TempDir::new();
    let rec = Recorder::default();
    // 40% at 20 pts/h: empty in 3 h, but not within 30 min
    let slow = plan(&[(60, 20.0), (30, 30.0), (0, 40.0)], 400, 5.0);
    assert!(alerts(&t).evaluate("Claude", &slow, now(), &rec).is_empty());
    // fast, but the window resets first (empty in 25 min, reset in 10 min)
    let resets_first = plan(&[(60, 10.0), (30, 40.0), (0, 70.0)], 10, 5.0);
    assert!(alerts(&TempDir::new())
        .evaluate("Claude", &resets_first, now(), &rec)
        .iter()
        .all(|n| n.kind != AlertKind::Predictive));
}

// ------------------------------------------------------------ recovery

#[test]
fn recovery_fires_once_after_a_red_window_resets() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    a.evaluate("Claude", &plan(&[(0, 92.0)], 20, 5.0), now(), &rec); // window A in the red
                                                                     // 30 minutes later window A is over and a new one (different reset time) has started
    let later = now() + Duration::minutes(30);
    let rs = [Reading::from_value(
        &json!({"ok": true, "source": "oauth", "ts": later.to_rfc3339(),
        "session": {"pct": 3.0, "resets_at": (later + Duration::minutes(290)).to_rfc3339()},
        "weekly": {"pct": 5.0, "resets_at": (now() + Duration::hours(100)).to_rfc3339()}}),
    )];
    let p = analyze(&rs, later).unwrap();
    let sent = a.evaluate("Claude", &p, later, &rec);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].title.as_str(), sent[0].body.as_str(), sent[0].kind),
        (
            "Claude session: reset",
            "Back to 3% used",
            AlertKind::Recovery
        )
    );
    assert!(a.evaluate("Claude", &p, later, &rec).is_empty());
}

#[test]
fn no_recovery_when_the_old_window_never_got_that_high() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    a.evaluate("Claude", &plan(&[(0, 80.0)], 20, 5.0), now(), &rec);
    let later = now() + Duration::minutes(30);
    let rs = [Reading::from_value(
        &json!({"ok": true, "source": "oauth", "ts": later.to_rfc3339(),
        "session": {"pct": 3.0, "resets_at": (later + Duration::minutes(290)).to_rfc3339()}, "weekly": null}),
    )];
    let sent = a.evaluate("Claude", &analyze(&rs, later).unwrap(), later, &rec);
    assert!(sent.is_empty(), "{sent:?}");
}

// ------------------------------------------------------------ persistence

#[test]
fn fired_events_survive_a_restart_and_old_windows_are_pruned() {
    let t = TempDir::new();
    let rec = Recorder::default();
    alerts(&t).evaluate("Claude", &flat(93.0), now(), &rec);
    assert_eq!(rec.titles().len(), 1);
    // a "new process": same file, nothing is sent again
    assert!(alerts(&t)
        .evaluate("Claude", &flat(93.0), now(), &rec)
        .is_empty());
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("alerts.json")).unwrap()).unwrap();
    let keys: Vec<&String> = saved["windows"].as_object().unwrap().keys().collect();
    assert!(keys.iter().any(|k| k.starts_with("session:r")), "{keys:?}");
    // 20 days later the old window's record is gone
    let later = now() + Duration::days(20);
    let rs = [Reading::from_value(
        &json!({"ok": true, "source": "oauth", "ts": later.to_rfc3339(),
        "session": {"pct": 1.0, "resets_at": (later + Duration::hours(3)).to_rfc3339()}, "weekly": null}),
    )];
    alerts(&t).evaluate("Claude", &analyze(&rs, later).unwrap(), later, &rec);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("alerts.json")).unwrap()).unwrap();
    assert_eq!(saved["windows"].as_object().unwrap().len(), 1);
}

// ------------------------------------------------------------ end to end: real chain + collector + App

fn app_over_history(t: &TempDir, fh: f64) -> (App, Arc<Recorder>, std::path::PathBuf) {
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.xdg_config_home = Some(t.join("xdg"));
    let data = t.join("xdg/Claude");
    std::fs::create_dir_all(&data).unwrap();
    let hist = data.join("plan-usage-history.json");
    let s = |m: i64, fh: f64| json!({"t": (Utc::now() - Duration::minutes(m)).timestamp_millis(), "org": "o", "u": {"fh": fh, "sd": 10.0}});
    std::fs::write(
        &hist,
        json!({"version": 2, "samples": [s(40, 50.0), s(2, fh)]}).to_string(),
    )
    .unwrap();
    let app = App::new(
        Box::new(JsonlStore::new(t.join("usage.jsonl"))),
        Arc::new(Settings::load(t.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None),
    );
    let rec = Arc::new(Recorder::default());
    app.set_notifier(rec.clone());
    (app, rec, hist)
}

fn wait(what: &str, f: &dyn Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < StdDuration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    }
}

#[test]
fn the_real_collector_sends_one_toast_and_never_repeats_it_across_ticks_or_restarts() {
    let t = TempDir::new();
    let (app, rec, hist) = app_over_history(&t, 92.0);
    app.collector
        .set_local_interval_override(Some(StdDuration::from_millis(60)));
    app.start();
    wait("the first alert", &|| !rec.titles().is_empty());
    wait("several more polls", &|| app.collector.status().polls >= 6);
    assert_eq!(rec.titles(), ["Claude session: 8% left"]);
    app.stop();
    app.collector.join();

    // a restart (new App, same files): still nothing new
    let (app2, rec2, _) = app_over_history(&t, 92.0);
    app2.start();
    wait("poll after restart", &|| app2.collector.status().polls >= 1);
    std::thread::sleep(StdDuration::from_millis(300));
    assert!(rec2.titles().is_empty(), "{:?}", rec2.titles());
    app2.stop();
    app2.collector.join();

    // the limit is hit later: exactly one more alert
    let s = |m: i64, fh: f64| json!({"t": (Utc::now() - Duration::minutes(m)).timestamp_millis(), "org": "o", "u": {"fh": fh, "sd": 10.0}});
    std::fs::write(
        &hist,
        json!({"version": 2, "samples": [s(40, 50.0), s(2, 92.0), s(0, 100.0)]}).to_string(),
    )
    .unwrap();
    let (app3, rec3, _) = app_over_history_keep(&t, &hist);
    app3.start();
    wait("limit alert", &|| !rec3.titles().is_empty());
    std::thread::sleep(StdDuration::from_millis(200));
    assert_eq!(rec3.titles(), ["Claude session: limit reached"]);
    app3.stop();
    app3.collector.join();
}

fn app_over_history_keep(t: &TempDir, _hist: &std::path::Path) -> (App, Arc<Recorder>, ()) {
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.xdg_config_home = Some(t.join("xdg"));
    let app = App::new(
        Box::new(JsonlStore::new(t.join("usage.jsonl"))),
        Arc::new(Settings::load(t.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None),
    );
    let rec = Arc::new(Recorder::default());
    app.set_notifier(rec.clone());
    (app, rec, ())
}

#[test]
fn notifications_can_be_switched_off_and_old_readings_never_alert() {
    // off in settings: nothing is sent
    let t = TempDir::new();
    std::fs::write(t.join("s.json"), "{\"notifications\": false}").unwrap();
    let (app, rec, _) = app_over_history(&t, 95.0);
    app.start();
    wait("a poll", &|| app.collector.status().polls >= 1);
    std::thread::sleep(StdDuration::from_millis(300));
    assert!(rec.titles().is_empty());
    app.stop();
    app.collector.join();

    // a stale sample (13 h old, 95%) is history, not news
    let t2 = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(t2.join("home"));
    env.xdg_config_home = Some(t2.join("xdg"));
    let data = t2.join("xdg/Claude");
    std::fs::create_dir_all(&data).unwrap();
    let old = json!({"t": (Utc::now() - Duration::hours(13)).timestamp_millis(), "org": "o", "u": {"fh": 95.0, "sd": 10.0}});
    std::fs::write(
        data.join("plan-usage-history.json"),
        json!({"version": 2, "samples": [old]}).to_string(),
    )
    .unwrap();
    let app = App::new(
        Box::new(JsonlStore::new(t2.join("usage.jsonl"))),
        Arc::new(Settings::load(t2.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None),
    );
    let rec = Arc::new(Recorder::default());
    app.set_notifier(rec.clone());
    app.start();
    wait("a poll", &|| app.collector.status().polls >= 1);
    std::thread::sleep(StdDuration::from_millis(300));
    assert!(rec.titles().is_empty());
    app.stop();
    app.collector.join();
}

#[test]
fn the_test_button_goes_through_the_notifier_and_reports_failures() {
    let t = TempDir::new();
    let (app, rec, _) = app_over_history(&t, 10.0);
    app.send_test_notification(AlertKind::Info).unwrap();
    assert_eq!(rec.titles(), ["Claude session: 75% used"]);
    app.set_notifier(Arc::new(Failing));
    assert_eq!(
        app.send_test_notification(AlertKind::Info).unwrap_err(),
        "toast service unavailable"
    );
}

#[test]
fn toast_xml_is_escaped() {
    let x = aitrack::toast::toast_xml("Claude <session> & co", "Resets in 2h");
    assert!(x.contains("Claude &lt;session&gt; &amp; co"));
    assert!(x.starts_with("<toast>") && x.contains("ToastGeneric"));
}

/// Readings of a session window: `(minutes_ago, pct, resets_in_min)`; `None` = a source that
/// reports no reset time.
fn session_plan(points: &[(i64, f64, Option<i64>)]) -> Plan {
    let rs: Vec<Reading> = points
        .iter()
        .map(|(m, pct, reset)| {
            Reading::from_value(&json!({
                "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(*m)).to_rfc3339(),
                "session": {"pct": pct, "resets_at": reset.map(|r| (now() + Duration::minutes(r)).to_rfc3339())},
            }))
        })
        .collect();
    analyze(&rs, now()).unwrap()
}

#[test]
fn a_window_that_gets_its_reset_time_later_does_not_announce_its_thresholds_again() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    // only a source without reset times so far: the window is known by its first reading (i<ts>)
    let unstamped = session_plan(&[(60, 92.0, None), (30, 92.0, None)]);
    assert!(unstamped
        .session
        .as_ref()
        .unwrap()
        .window_id
        .starts_with('i'));
    assert_eq!(a.evaluate("Claude", &unstamped, now(), &rec).len(), 1);
    // then a reading carrying the reset time arrives: the same window is now keyed r<minute>
    let stamped = session_plan(&[(60, 92.0, None), (30, 92.0, None), (0, 92.0, Some(200))]);
    assert!(stamped.session.as_ref().unwrap().window_id.starts_with('r'));
    assert!(
        a.evaluate("Claude", &stamped, now(), &rec).is_empty(),
        "{:?}",
        rec.titles()
    );
    assert_eq!(rec.titles().len(), 1, "{:?}", rec.titles());
    // and it stays quiet across a restart (the carried list is persisted)
    let again = alerts(&t);
    assert!(again.evaluate("Claude", &stamped, now(), &rec).is_empty());
}

#[test]
fn an_unstamped_window_that_ended_long_ago_is_not_mistaken_for_the_new_stamped_one() {
    let t = TempDir::new();
    let (a, rec) = (alerts(&t), Recorder::default());
    // an inferred window that began 10 h ago, long before the 5 h window that now reports its reset
    let old = session_plan(&[(600, 92.0, None), (570, 92.0, None)]);
    assert_eq!(a.evaluate("Claude", &old, now(), &rec).len(), 1);
    let current = session_plan(&[(60, 92.0, Some(200)), (0, 92.0, Some(200))]);
    assert_eq!(
        a.evaluate("Claude", &current, now(), &rec).len(),
        1,
        "a different window still announces: {:?}",
        rec.titles()
    );
}

#[test]
fn the_render_example_never_sends_real_toasts() {
    // examples/render.rs builds Apps with scenes above the alert thresholds; it must swap the
    // platform notifier for a silent one before anything is polled.
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/render.rs"))
        .unwrap();
    // (the Grok variant builds its Claude App through build_app first and shares its notifier)
    let build = src.find("fn build_app(").expect("render.rs builds apps");
    let set = src
        .find("app.set_notifier(")
        .expect("render.rs sets a notifier");
    let poll = build
        + src[build..]
            .find("poll_blocking()")
            .expect("build_app polls");
    assert!(
        set > build && set < poll,
        "replace the notifier before the first poll"
    );
    assert!(
        !src.contains("default_notifier"),
        "no platform notifier in render.rs"
    );
}
