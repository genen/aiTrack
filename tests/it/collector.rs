// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::collector::{Collector, PollFn};
use aitrack::live::CollectError;
use aitrack::settings::{Direction, Settings};
use aitrack::store::{JsonlStore, Store};
use common::TempDir;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fake_ok(polls: Arc<AtomicUsize>) -> PollFn {
    Box::new(move |store: &dyn Store| {
        polls.fetch_add(1, Ordering::SeqCst);
        let rec = json!({"v": 1, "ts": aitrack::util::now_iso(), "ok": true,
            "session": {"pct": 30.0, "resets_at": null}, "weekly": null, "spend": null, "raw": {}});
        store.append(&rec).unwrap();
        Ok(rec)
    })
}

fn setup() -> (TempDir, Arc<dyn Store>, Arc<Settings>) {
    let dir = TempDir::new();
    let store: Arc<dyn Store> = Arc::new(JsonlStore::new(dir.join("u.jsonl")));
    let settings = Arc::new(Settings::load(dir.join("s.json")));
    (dir, store, settings)
}

#[test]
fn collector_polls_immediately_then_waits_and_refresh_polls_now() {
    let (_d, store, settings) = setup();
    let polls = Arc::new(AtomicUsize::new(0));
    let c = Collector::new(store.clone(), settings, fake_ok(polls.clone()));
    c.start();
    wait_for("first poll", || polls.load(Ordering::SeqCst) == 1);
    wait_for("status", || c.status().polls == 1);
    assert!(c.seconds_until_next() > 800.0); // waits ~15 min
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    c.refresh();
    wait_for("refresh poll", || polls.load(Ordering::SeqCst) == 2);
    c.stop();
    c.join();
    assert_eq!(store.read().len(), 2);
}

#[test]
fn interval_change_reschedules_without_polling() {
    let (_d, store, settings) = setup();
    let polls = Arc::new(AtomicUsize::new(0));
    let c = Collector::new(store, settings.clone(), fake_ok(polls.clone()));
    c.start();
    wait_for("first poll", || c.status().polls == 1);
    settings.set_interval(60.0);
    c.reschedule();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(
        c.seconds_until_next() > 3500.0,
        "{}",
        c.seconds_until_next()
    );
    settings.set_interval(5.0);
    c.reschedule();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    let n = c.seconds_until_next();
    assert!(n < 300.0 && n > 250.0, "{n}");
    c.stop();
    c.join();
}

#[test]
fn collector_records_error_and_honours_retry_after() {
    let (_d, store, settings) = setup();
    let failing: PollFn = Box::new(|_| {
        Err(CollectError {
            message: "HTTP 429".into(),
            retry_after: Some(1800.0),
        })
    });
    let c = Collector::new(store, settings, failing);
    assert_eq!(c.status().next_in, None);
    c.poll_blocking();
    let st = c.status();
    assert_eq!(st.error.as_deref(), Some("HTTP 429"));
    assert!(c.seconds_until_next() > 1700.0);
    assert!(!st.polling);
}

#[test]
fn error_clears_after_a_good_poll() {
    let (_d, store, settings) = setup();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let f: PollFn = Box::new(move |_| {
        if n2.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(CollectError {
                message: "boom".into(),
                retry_after: Some(1800.0),
            })
        } else {
            Ok(Value::Null)
        }
    });
    let c = Collector::new(store, settings, f);
    c.poll_blocking();
    assert!(c.status().error.is_some());
    c.poll_blocking();
    let st = c.status();
    assert!(st.error.is_none());
    assert!(c.seconds_until_next() < 901.0); // backoff reset
}

#[test]
fn waker_is_called() {
    let (_d, store, settings) = setup();
    let polls = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let c = Collector::new(store, settings, fake_ok(polls));
    let w = wakes.clone();
    c.set_waker(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    c.poll_blocking();
    assert!(wakes.load(Ordering::SeqCst) >= 2); // polling started + finished
}

#[test]
fn settings_validate_and_persist() {
    let dir = TempDir::new();
    let s = Settings::load(dir.join("s.json"));
    let v = s.update(
        &json!({"interval_min": 0.1, "direction": "bogus", "startup": "yes", "on_top": false}),
        true,
    );
    assert_eq!(
        (v.interval_min, v.direction, v.startup, v.on_top),
        (1.0, Direction::Used, false, false)
    );
    s.update(&json!({"interval_min": 5, "direction": "remaining"}), true);
    let again = Settings::load(dir.join("s.json"));
    assert_eq!(
        (again.get().interval_min, again.get().direction),
        (5.0, Direction::Remaining)
    );
    assert!(!again.get().on_top);
    assert_eq!(
        s.update(&json!({"interval_min": 99999}), false)
            .interval_min,
        1440.0
    );
    // not persisted when save = false
    assert_eq!(Settings::load(dir.join("s.json")).get().interval_min, 5.0);
}

#[test]
fn settings_defaults_and_an_early_format_file() {
    let dir = TempDir::new();
    let d = Settings::load(dir.join("none.json")).get();
    assert_eq!(
        (d.interval_min, d.direction, d.startup, d.on_top),
        (15.0, Direction::Used, false, true)
    );
    // a file with only the first four keys (an early format)
    std::fs::write(dir.join("py.json"), "{\n  \"interval_min\": 5.0,\n  \"direction\": \"remaining\",\n  \"startup\": true,\n  \"on_top\": false\n}").unwrap();
    let p = Settings::load(dir.join("py.json")).get();
    assert_eq!(
        (p.interval_min, p.direction, p.startup, p.on_top),
        (5.0, Direction::Remaining, true, false)
    );
    // garbage file falls back to defaults
    std::fs::write(dir.join("bad.json"), "{{{").unwrap();
    assert_eq!(
        Settings::load(dir.join("bad.json")).get().interval_min,
        15.0
    );
    // what we write has the same keys
    let s = Settings::load(dir.join("w.json"));
    s.set_interval(15.0);
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("w.json")).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({"interval_min": 15.0, "local_interval_sec": 60.0, "direction": "used", "startup": false, "on_top": true, "notifications": true, "grok_enabled": null, "claude_enabled": null, "alert_sounds": true, "badge_style": "auto", "theme": "dark", "ui": {"sections": {"general": true, "appearance": false, "display": false, "notifications": false, "diagnostics": false, "about": false}}, "window": null})
    );
}

#[test]
fn local_interval_is_clamped_and_persisted() {
    let dir = TempDir::new();
    let s = Settings::load(dir.join("s.json"));
    assert_eq!(s.get().local_interval_sec, 60.0);
    assert_eq!(s.set_local_interval(1.0).local_interval_sec, 15.0);
    assert_eq!(s.set_local_interval(5000.0).local_interval_sec, 600.0);
    s.set_local_interval(90.0);
    assert_eq!(
        Settings::load(dir.join("s.json")).get().local_interval_sec,
        90.0
    );
    // old settings files without the key keep the default
    std::fs::write(dir.join("old.json"), "{\"interval_min\": 5}").unwrap();
    assert_eq!(
        Settings::load(dir.join("old.json"))
            .get()
            .local_interval_sec,
        60.0
    );
}

#[test]
fn local_polls_run_on_their_own_timer_and_full_polls_stay_rare() {
    let (_d, store, settings) = setup();
    let full = Arc::new(AtomicUsize::new(0));
    let local = Arc::new(AtomicUsize::new(0));
    let (f2, l2) = (full.clone(), local.clone());
    let c = Collector::with_local(
        store,
        settings,
        Box::new(move |_| {
            f2.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"source": "desktop-cache", "observed_at": "2026-10-14T11:00:00+00:00"}))
        }),
        Some(Box::new(move |_| {
            l2.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"source": "desktop-cache", "observed_at": "2026-10-14T11:01:00+00:00"}))
        })),
    );
    c.set_local_interval_override(Some(Duration::from_millis(100)));
    c.start();
    wait_for("first full poll", || full.load(Ordering::SeqCst) == 1);
    wait_for("several local polls", || local.load(Ordering::SeqCst) >= 3);
    // the 15 min network interval has not elapsed: still exactly one full poll
    assert_eq!(full.load(Ordering::SeqCst), 1);
    assert!(c.status().live);
    assert!(c.status().sample.unwrap().observed_at.is_some());
    // "refresh now" runs the full poll immediately
    c.refresh();
    wait_for("refresh full poll", || full.load(Ordering::SeqCst) == 2);
    c.stop();
    c.join();
}

#[test]
fn a_failing_local_poll_is_silent_and_never_overwrites_a_good_state() {
    let (_d, store, settings) = setup();
    let after = Arc::new(AtomicUsize::new(0));
    let c = Collector::with_local(
        store,
        settings,
        Box::new(|_| Ok(json!({"source": "oauth", "observed_at": "2026-10-14T11:00:00+00:00"}))),
        Some(Box::new(|_| {
            Err(CollectError::new("local sources are stale"))
        })),
    );
    let a2 = after.clone();
    c.set_after_poll(move || {
        a2.fetch_add(1, Ordering::SeqCst);
    });
    c.set_local_interval_override(Some(Duration::from_millis(50)));
    c.start();
    wait_for("first poll", || c.status().polls == 1);
    std::thread::sleep(Duration::from_millis(400));
    let st = c.status();
    assert_eq!((st.error, st.polls), (None, 1)); // local failures left no trace
    assert_eq!(st.sample.unwrap().source, "oauth");
    assert_eq!(after.load(Ordering::SeqCst), 1); // after-poll hook only ran for the good full poll
    c.stop();
    c.join();
}
