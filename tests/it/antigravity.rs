// SPDX-License-Identifier: GPL-3.0-or-later
//! Gemini through Google Antigravity: the `agy -p /usage --output-format json` reply, the agy run
//! itself (a fake agy on Unix), and what the Gemini block, its tooltips and Stats make of it.

use crate::common;
use aitrack::antigravity::{self, parse_usage, reply_json};
use aitrack::collector::Status;
use aitrack::live::sample_record;
use aitrack::reading::{Kind, Reading};
use aitrack::settings::Values;
use aitrack::sources::{Env, Os};
use aitrack::state::{build_antigravity_state, build_state};
use aitrack::vendor::VendorId;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 1, 30, 0).unwrap()
}

fn t(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The reply agy 1.4.12 printed on the author's PC (2026-11-01).
fn real_reply() -> Value {
    json!({
        "conversation_id": "", "status": "SUCCESS",
        "response": "Gemini Models\tWeekly Limit Remaining\t83%\t2026-11-08T01:12:50Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-11-08T01:23:24Z\n",
        "duration_seconds": 0, "num_turns": 0,
        "usage": {"input_tokens": 0, "output_tokens": 0, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 0},
        "command": {"name": "usage", "data": {
            "description": "Within each group, models share a weekly limit.",
            "groups": [
                {"name": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro",
                 "buckets": [{"id": "gemini-weekly", "name": "Weekly Limit Remaining",
                   "description": "You have used some of your weekly limit, it will fully refresh in 6 days, 23 hours.",
                   "window": "weekly", "remaining_fraction": 0.8292807936668396, "reset_time": "2026-11-08T01:12:50Z"}]},
                {"name": "Claude and GPT models", "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
                 "buckets": [{"id": "3p-weekly", "name": "Weekly Limit Remaining", "window": "weekly",
                   "remaining_fraction": 1, "reset_time": "2026-11-08T01:23:24Z"}]}
            ]}}
    })
}

#[test]
fn the_real_reply_gives_the_gemini_weekly_bar_and_the_other_group() {
    let s = parse_usage(&real_reply(), now()).unwrap();
    assert!(s.session.is_none());
    let w = s.weekly.as_ref().unwrap();
    assert_eq!(w.pct, 17.1, "used = 1 - remaining_fraction");
    // reset_time is when the bucket is full again, not a window end: kept apart, never a reset
    assert_eq!(w.resets_at, None);
    let rec = sample_record(&s, antigravity::SOURCE_USAGE, false);
    let r = Reading::from_value(&rec);
    assert_eq!(
        r.full_at.as_deref().and_then(aitrack::util::parse_ts),
        aitrack::util::parse_ts("2026-11-08T01:12:50Z")
    );
    assert_eq!(r.limits[0].resets_at, None);
    assert_eq!(r.limits.len(), 1);
    assert_eq!(r.limits[0].name, "Claude and GPT models");
    assert_eq!(r.limits[0].kind, "weekly");
    assert_eq!(r.limits[0].pct, 0.0);
    assert_eq!(r.source.as_deref(), Some("agy-usage"));
}

#[test]
fn a_five_hour_bucket_becomes_the_session_bar_in_either_spelling() {
    let reset5 = now() + Duration::hours(3);
    let body = json!({"status": "SUCCESS", "command": {"data": {"groups": [
        {"name": "Gemini Models", "buckets": [
            {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.85, "resetTime": t(reset5)},
            {"bucketId": "gemini-weekly", "window": "7d", "remainingFraction": 0.62, "resetTime": t(now() + Duration::days(3))}
        ]},
        {"name": "Claude and GPT models", "buckets": [
            {"bucketId": "3p-weekly", "window": "7d", "remainingFraction": 0.4}
        ]}
    ]}}});
    let s = parse_usage(&body, now()).unwrap();
    assert_eq!(s.session.as_ref().unwrap().pct, 15.0);
    assert_eq!(s.session.as_ref().unwrap().resets_at, None);
    assert_eq!(
        s.raw.as_ref().unwrap()["full_at"]["session"]
            .as_str()
            .and_then(aitrack::util::parse_ts),
        Some(reset5)
    );
    assert_eq!(s.weekly.as_ref().unwrap().pct, 38.0);
    let limits = &s.raw.as_ref().unwrap()["limits"];
    assert_eq!(limits[0]["percent"], json!(60.0));
    assert_eq!(limits[0]["kind"], json!("weekly"));
}

#[test]
fn failures_say_what_to_do_and_never_echo_much() {
    let e = parse_usage(
        &json!({"status": "ERROR", "error": "You are not logged into Antigravity."}),
        now(),
    )
    .unwrap_err();
    assert!(e.contains("not signed in"), "{e}");
    let long = "x".repeat(500);
    let e = parse_usage(&json!({"status": "FAILED", "response": long}), now()).unwrap_err();
    assert!(e.len() < 220, "{e}");
    assert!(parse_usage(&json!({"status": "SUCCESS"}), now()).is_err());
    let no_gemini = json!({"status": "SUCCESS", "command": {"data": {"groups": [
        {"name": "Claude and GPT models", "buckets": [{"window": "weekly", "remaining_fraction": 1}]}
    ]}}});
    assert!(parse_usage(&no_gemini, now()).is_err());
}

#[test]
fn the_reply_is_found_after_any_notice_lines() {
    let text = format!(
        "A new version is available.\n{}\n",
        serde_json::to_string(&real_reply()).unwrap()
    );
    let v = reply_json(text.as_bytes()).unwrap();
    assert_eq!(v["status"], json!("SUCCESS"));
    assert!(reply_json(b"no json here").is_none());
}

#[test]
fn agy_is_found_where_the_installer_puts_it() {
    let d = common::TempDir::new();
    let mut env = Env::new(Os::Windows);
    env.localappdata = Some(d.path().to_path_buf());
    env.home = Some(d.path().to_path_buf());
    assert_eq!(
        antigravity::install_dirs(&env),
        vec![d.path().join("agy").join("bin")]
    );
    let mut env = Env::new(Os::Linux);
    env.home = Some(d.path().to_path_buf());
    assert_eq!(
        antigravity::install_dirs(&env)[0],
        d.path().join(".local").join("bin")
    );
    let text = antigravity::diagnose(&env, now());
    assert!(text.contains("Gemini (Antigravity"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_usage_run_reads_agys_output_and_ends_a_hung_agy() {
    use std::os::unix::fs::PermissionsExt;
    let d = common::TempDir::new();
    let ok = d.path().join("agy-ok");
    let body = serde_json::to_string(&real_reply())
        .unwrap()
        .replace('\'', "");
    std::fs::write(
        &ok,
        format!("#!/bin/sh\n[ \"$1 $2 $3 $4\" = \"-p /usage --output-format json\" ] || exit 3\nprintf '%s\\n' '{body}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out =
        antigravity::run_usage(&ok, Some(d.path()), std::time::Duration::from_secs(20)).unwrap();
    let s = parse_usage(&reply_json(&out).unwrap(), now()).unwrap();
    assert_eq!(s.weekly.unwrap().pct, 17.1);

    let hung = d.path().join("agy-hung");
    std::fs::write(&hung, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&hung, std::fs::Permissions::from_mode(0o755)).unwrap();
    let start = std::time::Instant::now();
    let e = antigravity::run_usage(&hung, None, std::time::Duration::from_millis(500)).unwrap_err();
    assert!(e.contains("did not answer"), "{e}");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
}

fn status() -> Status {
    Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        sample: None,
        live: true,
    }
}

fn readings(at: DateTime<Utc>) -> Vec<Reading> {
    let reset = at + Duration::days(6);
    (0..4)
        .map(|i| {
            let ts = t(at - Duration::minutes(15 * (3 - i)));
            Reading::from_value(&json!({
                "v": 1, "ts": ts, "observed_at": ts, "ok": true, "vendor": "antigravity", "source": "agy-usage",
                "weekly": {"pct": 10.0 + i as f64 * 2.0, "resets_at": t(reset)},
                "limits": [{"name": "Claude and GPT models", "kind": "weekly", "percent": 5.0, "resets_at": t(reset)}],
            }))
        })
        .collect()
}

fn gemini_view(rs: &[Reading], at: DateTime<Utc>) -> aitrack::view::VendorView {
    let mut s = build_state(&[], Values::default(), status(), at);
    s.claude_on = false;
    s.antigravity = Some(build_antigravity_state(rs, status(), true, at));
    aitrack::view::build_home(&s, at)
        .vendors
        .into_iter()
        .find(|v| v.id == VendorId::Antigravity)
        .expect("a Gemini block")
}

fn tip_text(row: &aitrack::view::Row) -> String {
    row.tip
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_gemini_block_shows_a_weekly_bar_with_the_other_group_in_the_tooltip() {
    let at = Utc::now();
    let v = gemini_view(&readings(at), at);
    assert_eq!(v.name, "Gemini");
    assert_eq!(
        v.rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
        [Kind::Weekly]
    );
    assert_eq!(v.rows[0].value, "16%");
    let tip = tip_text(&v.rows[0]);
    for want in [
        "resets in",
        "Live from Antigravity (agy)",
        "Other models",
        "Claude and GPT models  5%",
    ] {
        assert!(tip.contains(want), "{want} missing from\n{tip}");
    }
    let st = build_antigravity_state(&readings(at), status(), true, at);
    assert!(
        st.diagnostics[0].text.starts_with("Gemini: weekly"),
        "{}",
        st.diagnostics[0].text
    );
}

#[test]
fn gemini_without_a_reading_is_a_placeholder_and_follows_its_setting() {
    let at = Utc::now();
    let mut s = build_state(&[], Values::default(), status(), at);
    s.claude_on = false;
    let mut st = status();
    st.error = Some("Antigravity is not signed in: run agy once and sign in".into());
    s.antigravity = Some(build_antigravity_state(&[], st, true, at));
    let home = aitrack::view::build_home(&s, at);
    let v = &home.vendors[0];
    assert_eq!(v.id, VendorId::Antigravity);
    assert!(v.rows.iter().all(|r| r.value == "--"));
    assert!(tip_text(&v.rows[0]).contains("run agy once"));
    s.antigravity.as_mut().unwrap().enabled = false;
    assert!(s.nothing_to_track());

    let v = Values::default();
    assert!(v.antigravity_on(true) && !v.antigravity_on(false));
    let dir = common::TempDir::new();
    let set = aitrack::settings::Settings::load(dir.join("s.json"));
    set.set_antigravity_track(aitrack::settings::Track::Off);
    assert!(!set.get().antigravity_on(true));
    let again = aitrack::settings::Settings::load(dir.join("s.json"));
    assert_eq!(again.get().antigravity_enabled, Some(false));
}

#[test]
fn an_empty_reply_a_model_turn_or_another_command_is_never_a_reading() {
    // Gene's PC right after switching agy to an Enterprise account (2026-11-01)
    let empty = json!({"conversation_id": "", "status": "SUCCESS", "response": "", "duration_seconds": 0,
        "num_turns": 0, "usage": {"input_tokens": 0, "output_tokens": 0, "thinking_tokens": 0,
        "cache_read_tokens": 0, "total_tokens": 0}, "command": {"name": "usage", "data": {"groups": []}}});
    assert_eq!(
        parse_usage(&empty, now()).unwrap_err(),
        antigravity::EMPTY_REPLY
    );
    let mut turned = real_reply();
    turned["num_turns"] = json!(1);
    assert_eq!(
        parse_usage(&turned, now()).unwrap_err(),
        antigravity::USED_MODEL
    );
    let mut tokens = real_reply();
    tokens["usage"]["total_tokens"] = json!(120);
    assert_eq!(
        parse_usage(&tokens, now()).unwrap_err(),
        antigravity::USED_MODEL
    );
    let mut other = real_reply();
    other["command"]["name"] = json!("model");
    assert!(parse_usage(&other, now()).is_err());
}

#[test]
fn a_daily_or_monthly_bucket_is_not_the_weekly_bar() {
    let body = json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "buckets": [
            {"id": "gemini-daily", "window": "daily", "remaining_fraction": 0.5},
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.9}
        ]}
    ]}}});
    let s = parse_usage(&body, now()).unwrap();
    assert_eq!(s.weekly.as_ref().unwrap().pct, 10.0);
    assert!(s.session.is_none());
    let limits = &s.raw.as_ref().unwrap()["limits"];
    assert_eq!(limits[0]["percent"], json!(50.0));
    assert_eq!(limits[0]["name"], json!("Gemini Models (daily)"));
    // a seven-day id is the weekly one, not "a day"
    let body = json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "buckets": [{"id": "gemini_7_days", "remaining_fraction": 0.7}]}
    ]}}});
    assert_eq!(parse_usage(&body, now()).unwrap().weekly.unwrap().pct, 30.0);
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<aitrack::alerts::Notification>>);

impl aitrack::alerts::Notifier for Recorder {
    fn notify(&self, n: &aitrack::alerts::Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

/// Antigravity's reset time moves with every poll (it is "full again at"), and usage refills a
/// little between polls: still one window, each alert once, no "back to" toast, no false pace.
#[test]
fn a_moving_reset_and_gradual_refill_do_not_repeat_alerts() {
    let dir = common::TempDir::new();
    let alerts = aitrack::alerts::Alerts::load(dir.join("alerts-antigravity.json"));
    let rec = Recorder::default();
    let start = now();
    let used = [92.0, 92.5, 91.0, 89.0, 90.5, 93.0, 91.5, 89.5, 90.0];
    let mut readings = Vec::new();
    let mut per_poll = Vec::new();
    for (i, pct) in used.iter().enumerate() {
        let at = start + Duration::minutes(15 * i as i64);
        let body = json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage", "data": {"groups": [
            {"name": "Gemini Models", "buckets": [{"id": "gemini-weekly", "window": "weekly",
              "remaining_fraction": 1.0 - pct / 100.0,
              "reset_time": t(at + Duration::days(7) - Duration::minutes(3))}]}
        ]}}});
        let s = parse_usage(&body, at).unwrap();
        let mut r = sample_record(&s, antigravity::SOURCE_USAGE, false);
        r["vendor"] = json!("antigravity");
        readings.push(Reading::from_value(&r));
        let plan = aitrack::analysis::analyze(&readings, at).unwrap();
        let w = plan.weekly.as_ref().unwrap();
        assert!(
            w.hours_left.is_none(),
            "no reset, so no 'empty before reset' warning"
        );
        per_poll.push(alerts.evaluate("Gemini", &plan, at, &rec).len());
    }
    assert!(per_poll[1..].iter().all(|n| *n == 0), "{per_poll:?}");
    let sent = rec.0.lock().unwrap();
    assert!(!sent.is_empty(), "the first poll at 92% alerts");
    assert!(
        sent.iter()
            .all(|n| !n.title.contains("Back to") && !n.body.contains("Back to")),
        "{:?}",
        sent.iter().map(|n| n.title.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn the_weekly_tooltip_says_when_gemini_is_full_again() {
    let at = Utc::now();
    let mut rs = readings(at);
    for r in rs.iter_mut() {
        r.weekly.as_mut().unwrap().resets_at = None;
        r.full_at = Some(t(at + Duration::days(6) + Duration::hours(23)));
    }
    let v = gemini_view(&rs, at);
    let text: Vec<String> = v.rows.iter().map(tip_text).collect();
    assert!(
        text.iter()
            .any(|l| l.contains("refills gradually: full again in")),
        "{text:?}"
    );
    assert!(
        !text.iter().any(|l| l.contains("reset time unknown")),
        "{text:?}"
    );
}

/// A refilled Gemini limit warns again when usage climbs back, and never says it "reset".
#[test]
fn a_refilled_gemini_limit_warns_again_without_a_reset_toast() {
    let dir = common::TempDir::new();
    let alerts = aitrack::alerts::Alerts::load(dir.join("alerts-antigravity.json"));
    let rec = Recorder::default();
    let start = now();
    // up to 92, a slow refill to 40 over three days (no 10-point step), back up to 92
    let mut used: Vec<f64> = vec![80.0, 92.0];
    used.extend((1..=26).map(|i| 92.0 - 2.0 * i as f64));
    used.extend([50.0, 60.0, 70.0, 80.0, 92.0]);
    let mut readings = Vec::new();
    let mut warnings = 0;
    for (i, pct) in used.iter().enumerate() {
        let at = start + Duration::hours(3 * i as i64);
        let body = json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": [
            {"name": "Gemini Models", "buckets": [{"id": "gemini-weekly", "window": "weekly",
              "remaining_fraction": 1.0 - pct / 100.0, "reset_time": t(at + Duration::days(7))}]}
        ]}}});
        let mut r = sample_record(
            &parse_usage(&body, at).unwrap(),
            antigravity::SOURCE_USAGE,
            false,
        );
        r["vendor"] = json!("antigravity");
        readings.push(Reading::from_value(&r));
        let plan = aitrack::analysis::analyze(&readings, at).unwrap();
        for n in alerts.evaluate("Gemini", &plan, at, &rec) {
            assert!(!n.title.contains("reset"), "{}", n.title);
            if n.title.contains("left") {
                warnings += 1;
                assert_eq!(n.body, "Refills gradually");
            }
        }
    }
    assert_eq!(warnings, 2, "the 90% warning fires again after the refill");
}

#[test]
fn readings_saved_with_a_moving_reset_do_not_keep_it() {
    let r = Reading::from_value(&json!({"ok": true, "ts": t(now()), "source": "agy-usage",
        "weekly": {"pct": 20.0, "resets_at": t(now() + Duration::days(7)), "resets_carried": true}}));
    let w = r.weekly.unwrap();
    assert_eq!((w.resets_at, w.carried), (None, false));
}

#[cfg(unix)]
#[test]
fn a_helper_holding_agys_output_open_does_not_hold_the_poll() {
    use std::os::unix::fs::PermissionsExt;
    let d = common::TempDir::new();
    let p = d.path().join("agy-helper");
    let body = serde_json::to_string(&real_reply())
        .unwrap()
        .replace('\'', "");
    // agy prints its reply and exits, leaving a child that keeps its output open
    std::fs::write(
        &p,
        format!("#!/bin/sh\nprintf '%s\\n' '{body}'\n(sleep 20) &\nexit 0\n"),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let start = std::time::Instant::now();
    let out = antigravity::run_usage(&p, None, std::time::Duration::from_secs(20)).unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(reply_json(&out).is_some());
}
