// SPDX-License-Identifier: GPL-3.0-or-later
//! The testing override of the informational alert threshold (`AITRACK_ALERT_INFO_PCT`), through the
//! real evaluator. Environment variables are process-wide, so these tests take a lock and set /
//! clear the variable themselves (this file is its own test process).
mod common;

use aitrack::alerts::{self, Alerts, Notification, Notifier};
use aitrack::analysis::{analyze, Plan};
use aitrack::reading::Reading;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

/// Holds the lock and starts from a clean environment.
fn env() -> MutexGuard<'static, ()> {
    let g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(alerts::INFO_PCT_ENV);
    g
}

fn set(v: &str) {
    std::env::set_var(alerts::INFO_PCT_ENV, v);
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

struct Rec(Mutex<Vec<Notification>>);
impl Notifier for Rec {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

/// Flat readings (no pace) of the session at `s` and the weekly window at `w`.
fn plan(s: f64, w: f64) -> Plan {
    let rs: Vec<Reading> = [60, 0]
        .iter()
        .map(|m| {
            Reading::from_value(&json!({
                "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(*m)).to_rfc3339(),
                "session": {"pct": s, "resets_at": (now() + Duration::minutes(200)).to_rfc3339()},
                "weekly": {"pct": w, "resets_at": (now() + Duration::hours(100)).to_rfc3339()},
            }))
        })
        .collect();
    analyze(&rs, now()).unwrap()
}

fn titles(a: &Alerts, vendor: &str, p: &Plan) -> Vec<String> {
    let rec = Rec(Mutex::new(vec![]));
    a.evaluate(vendor, p, now(), &rec)
        .into_iter()
        .map(|n| n.title)
        .collect()
}

#[test]
fn without_the_variable_nothing_fires_at_25_percent() {
    let _g = env();
    let t = TempDir::new();
    let a = Alerts::load(t.join("alerts.json"));
    assert!(titles(&a, "Claude", &plan(26.0, 5.0)).is_empty());
    assert_eq!(alerts::info_pct(), 75.0);
    assert_eq!(
        titles(&a, "Claude", &plan(76.0, 5.0)),
        ["Claude session: 76% used"]
    );
}

#[test]
fn a_reading_crossing_25_fires_once_with_the_real_number() {
    let _g = env();
    set("25");
    let t = TempDir::new();
    let a = Alerts::load(t.join("alerts.json"));
    assert_eq!(titles(&a, "Claude", &plan(24.0, 5.0)), Vec::<String>::new());
    assert_eq!(
        titles(&a, "Claude", &plan(26.0, 5.0)),
        ["Claude session: 26% used"]
    );
    // again, and higher: not twice
    assert!(titles(&a, "Claude", &plan(26.0, 5.0)).is_empty());
    assert!(titles(&a, "Claude", &plan(40.0, 5.0)).is_empty());
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("alerts.json")).unwrap()).unwrap();
    let text = saved.to_string();
    assert!(
        text.contains("\"p25\"") && !text.contains("\"p75\""),
        "{text}"
    );
    std::env::remove_var(alerts::INFO_PCT_ENV);
}

#[test]
fn weekly_and_grok_use_the_same_path() {
    let _g = env();
    set("25");
    let t = TempDir::new();
    let claude = Alerts::load(t.join("alerts.json"));
    assert_eq!(
        titles(&claude, "Claude", &plan(10.0, 30.0)),
        ["Claude weekly: 30% used"]
    );
    let grok = Alerts::load(t.join("alerts-grok.json"));
    assert_eq!(
        titles(&grok, "Grok", &plan(10.0, 30.0)),
        ["Grok weekly: 30% used"]
    );
    assert!(titles(&grok, "Grok", &plan(10.0, 31.0)).is_empty());
    std::env::remove_var(alerts::INFO_PCT_ENV);
}

#[test]
fn keys_follow_the_number_so_stale_state_never_suppresses() {
    let _g = env();
    assert_eq!(alerts::info_tag(75.0), "p75");
    assert_eq!(alerts::info_tag(25.0), "p25");
    assert_eq!(alerts::info_tag(12.5), "p12.5");
    let t = TempDir::new();
    let a = Alerts::load(t.join("alerts.json"));
    // a real p75 was already announced for this window ...
    assert_eq!(titles(&a, "Claude", &plan(80.0, 5.0)).len(), 1);
    // ... which does not hide the 25 % test alert
    set("25");
    assert_eq!(
        titles(&a, "Claude", &plan(80.0, 5.0)),
        ["Claude session: 80% used"]
    );
    assert!(titles(&a, "Claude", &plan(80.0, 5.0)).is_empty());
    // and with the override gone again, neither state fires twice
    std::env::remove_var(alerts::INFO_PCT_ENV);
    assert!(titles(&a, "Claude", &plan(80.0, 5.0)).is_empty());
    // the other way round: a stale p25 does not suppress a later real p75 in a fresh window
    set("25");
    let b = Alerts::load(t.join("b.json"));
    assert_eq!(titles(&b, "Claude", &plan(30.0, 5.0)).len(), 1);
    std::env::remove_var(alerts::INFO_PCT_ENV);
    assert_eq!(
        titles(&b, "Claude", &plan(78.0, 5.0)),
        ["Claude session: 78% used"]
    );
}

#[test]
fn out_of_range_values_clamp_and_junk_is_ignored() {
    let _g = env();
    for (v, want) in [
        ("25", Some(25.0)),
        (" 30 ", Some(30.0)),
        ("0", Some(1.0)),
        ("-5", Some(1.0)),
        ("95", Some(89.0)),
        ("100", Some(89.0)),
        ("12.5", Some(12.5)),
        ("abc", None),
        ("", None),
        ("NaN", None),
        ("inf", None),
    ] {
        set(v);
        assert_eq!(alerts::info_pct_override(), want, "{v:?}");
    }
    set("abc");
    assert_eq!(alerts::info_pct(), 75.0);
    std::env::remove_var(alerts::INFO_PCT_ENV);
}

#[test]
fn the_90_and_100_alerts_still_work_and_mark_the_lower_ones_done() {
    let _g = env();
    set("25");
    let t = TempDir::new();
    let a = Alerts::load(t.join("alerts.json"));
    assert_eq!(
        titles(&a, "Claude", &plan(91.0, 5.0)),
        ["Claude session: 9% left"]
    );
    // the 25 % alert was skipped over (only the highest crossed is announced) and is now done
    assert!(titles(&a, "Claude", &plan(91.0, 5.0)).is_empty());
    assert_eq!(
        titles(&a, "Claude", &plan(100.0, 5.0)),
        ["Claude session: limit reached"]
    );
    std::env::remove_var(alerts::INFO_PCT_ENV);
}

#[test]
fn the_active_override_is_logged_when_the_alerts_load() {
    let _g = env();
    let t = TempDir::new();
    let log = t.join("notify.log");
    std::env::set_var("AITRACK_NOTIFY_LOG", &log);
    set("25");
    let _a = Alerts::load(t.join("alerts.json"));
    std::env::remove_var(alerts::INFO_PCT_ENV);
    let _b = Alerts::load(t.join("alerts2.json"));
    std::env::remove_var("AITRACK_NOTIFY_LOG");
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.matches("override active").count(), 1, "{text}");
    assert!(text.contains("25%"));
}

// ---- review #6: a failed append of the error record is logged, not swallowed -----------------

struct FailingStore;
impl aitrack::store::Store for FailingStore {
    fn append(&self, _: &Value) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
    fn read_each(&self, _: &mut dyn FnMut(Value)) {}
    fn describe(&self) -> String {
        "test store".into()
    }
}

#[test]
fn a_failed_append_of_the_error_record_is_logged() {
    let _g = env();
    let t = TempDir::new();
    let log = t.join("notify.log");
    std::env::set_var("AITRACK_NOTIFY_LOG", &log);
    // no source at all: the poll fails, and so does recording it
    let poller = aitrack::live::Poller::with_chain(vec![]);
    let now = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
    assert!(poller.poll_at(&FailingStore, now).is_err());
    std::env::remove_var("AITRACK_NOTIFY_LOG");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("could not record a failed poll in test store: disk full"),
        "{text}"
    );
}

struct Failing;
impl Notifier for Failing {
    fn notify(&self, _: &Notification) -> Result<(), String> {
        Err("toast service unavailable".into())
    }
}

/// Lives here (not in the shared `tests/it` binary) because it points `AITRACK_NOTIFY_LOG` at a
/// temp file, and an environment variable is process-wide.
#[test]
fn a_corrupt_state_file_starts_empty_and_a_failing_notifier_is_not_fatal() {
    let _g = env();
    let t = TempDir::new();
    std::env::set_var("AITRACK_NOTIFY_LOG", t.join("notify.log"));
    std::fs::write(t.join("alerts.json"), "{ this is not json").unwrap();
    let a = Alerts::load(t.join("alerts.json"));
    let p = plan(93.0, 5.0);
    let sent = a.evaluate("Claude", &p, now(), &Failing);
    assert_eq!(sent.len(), 1); // the alert was decided and recorded ...
    let log = std::fs::read_to_string(t.join("notify.log")).unwrap();
    assert!(log.contains("toast service unavailable")); // ... and the failure logged
    std::env::remove_var("AITRACK_NOTIFY_LOG");
    // ... and a failed delivery is not retried every minute
    assert!(a
        .evaluate("Claude", &p, now(), &Rec(Mutex::new(vec![])))
        .is_empty());
}
