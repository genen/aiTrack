// SPDX-License-Identifier: GPL-3.0-or-later
//! Stats > Plan window counts from realistic readings: ChatGPT's unused windows (a reset that moves
//! with the clock), reset times that jitter by seconds, Gemini's refilling limit, Grok Bot's week.

use aitrack::analysis::{self, window_keys, WindowKey};
use aitrack::antigravity;
use aitrack::codex;
use aitrack::grok_bot::{self, BotReading};
use aitrack::live::sample_record;
use aitrack::reading::{Kind, Reading};
use aitrack::stats::{compute, StatsRange};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

/// Sunday 2026-11-01 12:00 UTC.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap()
}

const SESSION_S: i64 = 5 * 3600;
const WEEK_S: i64 = 7 * 24 * 3600;

/// One `wham/usage` window. `reset` is the window's fixed end; `None` = unused, so the reply says
/// "a whole window from now", as ChatGPT does for a window nobody has started.
fn wham_window(pct: f64, len_s: i64, at: DateTime<Utc>, reset: Option<DateTime<Utc>>) -> Value {
    let end = reset.unwrap_or(at + Duration::seconds(len_s));
    json!({
        "used_percent": pct,
        "limit_window_seconds": len_s,
        "reset_after_seconds": (end - at).num_seconds(),
        "reset_at": end.timestamp(),
    })
}

fn wham_reading(
    at: DateTime<Utc>,
    session: (f64, Option<DateTime<Utc>>),
    weekly: (f64, Option<DateTime<Utc>>),
) -> Reading {
    let body = json!({
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": wham_window(session.0, SESSION_S, at, session.1),
            "secondary_window": wham_window(weekly.0, WEEK_S, at, weekly.1),
        }
    });
    let s = codex::parse_usage(&body, at).unwrap();
    Reading::from_value(&sample_record(&s, codex::SOURCE_USAGE, false))
}

/// A month of ChatGPT polls every 10 minutes by someone who used it in three sessions: 20, 12 and 3
/// days ago (2 hours of use each, up to 29 %), each starting a week too. Every other reading is
/// 0 % with a reset a whole window from the poll.
fn chatgpt_month() -> (Vec<Reading>, Vec<DateTime<Utc>>) {
    let starts: Vec<DateTime<Utc>> = [20, 12, 3]
        .iter()
        .map(|d| now() - Duration::days(*d) - Duration::hours(2))
        .collect();
    let mut out = Vec::new();
    let mut t = now() - Duration::days(31);
    while t <= now() {
        let session = starts
            .iter()
            .find(|s| t >= **s && t < **s + Duration::seconds(SESSION_S))
            .map(|s| {
                let used_min = (t - *s).num_minutes().min(120) as f64;
                (
                    (used_min / 120.0 * 29.0).round(),
                    Some(*s + Duration::seconds(SESSION_S)),
                )
            })
            .unwrap_or((0.0, None));
        let weekly = starts
            .iter()
            .rev()
            .find(|s| t >= **s && t < **s + Duration::seconds(WEEK_S))
            .map(|s| {
                let pct = if t >= *s + Duration::hours(1) {
                    5.0
                } else {
                    1.0
                };
                (pct, Some(*s + Duration::seconds(WEEK_S)))
            })
            .unwrap_or((0.0, None));
        out.push(wham_reading(t, session, weekly));
        t += Duration::minutes(10);
    }
    (out, starts)
}

#[test]
fn an_unused_chatgpt_window_reports_a_reset_that_moves_with_the_clock() {
    // the cause: two polls ten minutes apart, nothing used, give two reset times ten minutes apart
    let a = wham_reading(now(), (0.0, None), (0.0, None));
    let b = wham_reading(now() + Duration::minutes(10), (0.0, None), (0.0, None));
    let reset = |r: &Reading, k: Kind| r.bucket(k).unwrap().resets_at.clone().unwrap();
    for k in Kind::ALL {
        assert_ne!(reset(&a, k), reset(&b, k), "{k:?}");
        let rs = aitrack::util::parse_ts(&reset(&a, k)).unwrap();
        assert!(analysis::unstarted(now(), 0.0, rs, k), "{k:?}");
    }
    // a started window at 0 % (a few minutes in) has a fixed reset and is a window
    let rs = now() + Duration::seconds(SESSION_S) - Duration::minutes(20);
    assert!(!analysis::unstarted(now(), 0.0, rs, Kind::Session));
    assert!(!analysis::unstarted(
        now(),
        3.0,
        now() + Duration::seconds(SESSION_S),
        Kind::Session
    ));
}

#[test]
fn unused_chatgpt_windows_are_not_counted_as_windows() {
    let (readings, starts) = chatgpt_month();
    assert!(readings.len() > 4000);
    let st = compute(&readings, StatsRange::Last30d, now(), 0);
    // the three sessions, nothing else (was one "window" per idle poll: 1571 on Gene's PC)
    assert_eq!(st.session.windows, 3, "{:?}", st.session.peaks.len());
    assert_eq!(st.session.max_peak, Some(29.0));
    assert_eq!(st.session.avg_peak, Some(29.0));
    assert_eq!(st.session.hits, 0);
    assert!(st.session.current.is_none(), "idle now: none in progress");
    // weeks: two finished (20 and 12 days ago), the one from 3 days ago is in progress
    assert_eq!(st.weekly.windows, 2);
    assert_eq!(st.weekly.max_peak, Some(5.0));
    let cur = st.weekly.current.as_ref().unwrap();
    assert_eq!(cur.pct, 5.0);
    assert_eq!(cur.resets_at, Some(starts[2] + Duration::seconds(WEEK_S)));
    assert!(st.resets.is_empty(), "no early resets: {:?}", st.resets);

    // the window gauge agrees, and an idle stretch keeps one id while its reset moves
    let plan = analysis::analyze(&readings, now()).unwrap();
    assert_eq!(plan.session_stats.windows, 3);
    assert_eq!(plan.weekly_stats.windows, 3);
    let g = plan.session.as_ref().unwrap();
    assert!(g.window_id.starts_with('u'), "{}", g.window_id);
    assert_eq!(g.rate_per_hour, None);
    let earlier = analysis::analyze(&readings[..readings.len() - 6], now()).unwrap();
    assert_eq!(earlier.session.unwrap().window_id, g.window_id);
}

#[test]
fn reset_times_that_jitter_by_seconds_across_a_minute_are_one_window() {
    // a reset on the full minute, reported as "seconds left" from polls at odd milliseconds:
    // 11:59:59.6, 12:00:00.3, ... (the minute alone split this into two windows)
    let end = now() + Duration::hours(3);
    let start = end - Duration::seconds(SESSION_S);
    let mut pts = Vec::new();
    for i in 0..12i64 {
        let t = start + Duration::minutes(10 * i + 5) + Duration::milliseconds(137 * i);
        let jitter = Duration::milliseconds(if i % 2 == 0 { -400 } else { 300 });
        pts.push((t, (2 * i) as f64, Some(end + jitter)));
    }
    let keys = window_keys(&pts, Kind::Session);
    assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
    assert!(matches!(keys[0], WindowKey::Reset(_)));
    // a Codex log from an older build: resets_in_seconds counted from each line's own time
    let entries: Vec<_> = (0..12i64)
        .map(|i| {
            let t = start + Duration::minutes(10 * i + 5) + Duration::milliseconds(250 * i);
            let left = (end - t).num_milliseconds() as f64 / 1000.0;
            let line = json!({
                "timestamp": t.to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "rate_limits": {
                    "primary": {"used_percent": (2 * i) as f64, "window_minutes": 300, "resets_in_seconds": left.round()},
                }}
            })
            .to_string();
            codex::parse_log_line(line.as_bytes()).unwrap()
        })
        .collect();
    let readings: Vec<Reading> = codex::log_samples(&entries)
        .iter()
        .map(|s| Reading::from_value(&sample_record(s, codex::SOURCE_LOG, true)))
        .collect();
    let st = compute(&readings, StatsRange::Last30d, now(), 0);
    assert_eq!(st.session.windows, 0, "the one window is still in progress");
    assert_eq!(st.session.current.as_ref().unwrap().readings, 12);
    // two real windows back to back stay two
    let next = end + Duration::seconds(SESSION_S);
    pts.push((end + Duration::minutes(3), 1.0, Some(next)));
    let keys = window_keys(&pts, Kind::Session);
    assert_ne!(keys[0], keys[12]);
}

#[test]
fn claude_and_grok_windows_count_as_before() {
    // Claude: three days of 5-hour windows used back to back (fixed resets, sub-second jitter)
    let mut readings = Vec::new();
    let first = now() - Duration::days(3);
    for w in 0..14i64 {
        let end = first + Duration::seconds(SESSION_S * (w + 1));
        for i in 0..10i64 {
            let t = end - Duration::seconds(SESSION_S) + Duration::minutes(30 * i + 1);
            let v = json!({"ok": true, "ts": t.to_rfc3339(), "source": "oauth",
                "session": {"pct": (i * 8) as f64, "resets_at": (end + Duration::milliseconds(if i % 2 == 0 { 412 } else { -77 })).to_rfc3339()}});
            readings.push(Reading::from_value(&v));
        }
    }
    let st = compute(&readings, StatsRange::Last30d, now(), 0);
    assert_eq!(st.session.windows, 14);
    assert_eq!(st.session.max_peak, Some(72.0));
    // Grok's 0 % stays a window when its reset is a fixed period end well ahead
    let end = now() + Duration::days(2);
    let v = |t: DateTime<Utc>, p: f64| {
        Reading::from_value(
            &json!({"ok": true, "ts": t.to_rfc3339(), "source": "grok-billing",
            "weekly": {"pct": p, "resets_at": end.to_rfc3339()}}),
        )
    };
    let g = vec![v(now() - Duration::hours(2), 0.0), v(now(), 0.0)];
    let st = compute(&g, StatsRange::Last30d, now(), 0);
    assert_eq!(st.weekly.current.as_ref().unwrap().readings, 2);
}

/// One Gemini reading through the real path: agy's reply, parsed, stored, read back.
fn agy_reading(at: DateTime<Utc>, weekly_left: f64, session_left: Option<f64>) -> Reading {
    let mut buckets = vec![json!({"id": "gemini-weekly", "window": "weekly",
        "remaining_fraction": weekly_left,
        "reset_time": (at + Duration::days(7)).to_rfc3339()})];
    if let Some(l) = session_left {
        buckets.push(
            json!({"id": "gemini-5h", "window": "5h", "remaining_fraction": l,
            "reset_time": (at + Duration::hours(5)).to_rfc3339()}),
        );
    }
    let body = json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage",
        "data": {"groups": [{"name": "Gemini Models", "buckets": buckets}]}}});
    let s = antigravity::parse_usage(&body, at).unwrap();
    Reading::from_value(&sample_record(&s, antigravity::SOURCE_USAGE, false))
}

#[test]
fn gemini_counts_used_not_remaining_and_failed_polls_are_no_reading() {
    // a fresh paid plan: everything left is 0 % used
    let r = agy_reading(now(), 1.0, Some(1.0));
    assert_eq!(r.weekly.as_ref().unwrap().pct, 0.0);
    assert_eq!(r.session.as_ref().unwrap().pct, 0.0);
    assert!(
        r.weekly.as_ref().unwrap().resets_at.is_none(),
        "full-again time is no reset"
    );
    // the only way to 100 %: agy saying nothing is left
    assert_eq!(agy_reading(now(), 0.0, None).weekly.unwrap().pct, 100.0);
    // a business account's empty reply is an error, never a reading
    let empty = json!({"status": "SUCCESS", "num_turns": 0,
        "command": {"name": "usage", "data": {"groups": []}}});
    assert!(antigravity::parse_usage(&empty, now()).is_err());
    // ... stored as a failed poll, which Stats leaves out
    let failed = Reading::from_value(&json!({"ok": false, "ts": now().to_rfc3339(),
        "source": "agy-usage", "error": "No usage data from Antigravity"}));
    // a reading saved by 0.5.14-beta still had the full-again time as its reset
    let old = Reading::from_value(
        &json!({"ok": true, "ts": (now() - Duration::hours(3)).to_rfc3339(),
        "source": "agy-usage", "weekly": {"pct": 17.1, "resets_at": (now() + Duration::days(7)).to_rfc3339()}}),
    );
    assert!(old.weekly.as_ref().unwrap().resets_at.is_none());
    let mut readings = vec![old, failed.clone(), failed];
    for m in [9, 5, 0] {
        readings.push(agy_reading(now() - Duration::minutes(m), 1.0, Some(1.0)));
    }
    let st = compute(&readings, StatsRange::Last30d, now(), 0);
    // the old plan's stretch is one finished window at its own peak, never a hit
    assert_eq!(st.weekly.windows, 1);
    assert_eq!(st.weekly.max_peak, Some(17.1));
    assert_eq!(st.weekly.hits, 0);
    let cur = st.weekly.current.as_ref().unwrap();
    assert_eq!((cur.pct, cur.readings), (0.0, 3));
    assert!(
        (cur.elapsed_h * 60.0 - 9.0).abs() < 1e-6,
        "9m in: since its first reading"
    );
    // the new 5-hour bucket: no finished window yet, one in progress
    assert_eq!(st.session.windows, 0);
    assert_eq!(st.session.current.as_ref().unwrap().readings, 3);
}

#[test]
fn grok_bot_weeks_in_and_left_add_up_to_seven_days() {
    let reset = now() + Duration::hours(12) + Duration::minutes(35);
    let readings: Vec<Reading> = (0..40i64)
        .map(|i| {
            let read_at = now() - Duration::hours(6 * 26) + Duration::hours(4 * i);
            BotReading {
                pct: (i / 8) as f64,
                resets_at: reset,
                read_at,
                expires_at: None,
            }
        })
        .map(|r| Reading::from_value(&grok_bot::record(&r)))
        .collect();
    let st = compute(&readings, StatsRange::Last30d, now(), 0);
    let cur = st.weekly.current.as_ref().unwrap();
    let left_h = (reset - now()).num_minutes() as f64 / 60.0;
    assert!((cur.elapsed_h + left_h - 168.0).abs() < 1e-6);
    // shown as "6d 11h in" (days and hours only) and "Resets in 12h 35m"
    assert_eq!(aitrack::view::left_long(cur.elapsed_h), "6d 11h");
    assert_eq!(aitrack::view::left_long(left_h), "12h 35m");
}
