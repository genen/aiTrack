// SPDX-License-Identifier: GPL-3.0-or-later
//! An alert the notifier could not show yet (macOS permission still undecided) is offered again. This
//! file is its own test process because it sets `AITRACK_NOTIFY_LOG`, which is process-wide.
mod common;

use aitrack::alerts::{self, Alerts, Notification, Notifier};
use aitrack::analysis::{analyze, Plan};
use aitrack::reading::Reading;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::json;
use std::sync::Mutex;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Notification>>);

impl Notifier for Recorder {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

struct Later;
impl Notifier for Later {
    fn notify(&self, _: &Notification) -> Result<(), String> {
        Err(alerts::retry_later("permission not decided"))
    }
}

/// A flat session window at `pct` (two readings an hour apart, no pace).
fn flat(pct: f64) -> Plan {
    let rs: Vec<Reading> = [60, 0]
        .iter()
        .map(|m| {
            Reading::from_value(&json!({
                "ok": true, "source": "oauth", "ts": (now() - Duration::minutes(*m)).to_rfc3339(),
                "session": {"pct": pct, "resets_at": (now() + Duration::minutes(200)).to_rfc3339()},
                "weekly": {"pct": 5.0, "resets_at": (now() + Duration::hours(100)).to_rfc3339()},
            }))
        })
        .collect();
    analyze(&rs, now()).unwrap()
}

fn alerts_in(t: &TempDir) -> Alerts {
    Alerts::load(t.join("alerts.json"))
}

#[test]
fn an_alert_that_could_not_be_shown_yet_is_offered_again() {
    let t = TempDir::new();
    std::env::set_var("AITRACK_NOTIFY_LOG", t.join("notify.log"));
    let a = alerts_in(&t);
    assert!(a.evaluate("Claude", &flat(93.0), now(), &Later).is_empty());
    let rec = Recorder::default();
    assert_eq!(a.evaluate("Claude", &flat(93.0), now(), &rec).len(), 1);
    // once shown it is not repeated, also after a reload from alerts.json
    assert!(alerts_in(&t)
        .evaluate("Claude", &flat(93.0), now(), &rec)
        .is_empty());
}
