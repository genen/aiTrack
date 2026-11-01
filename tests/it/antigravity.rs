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
    assert_eq!(
        w.resets_at.as_deref().and_then(aitrack::util::parse_ts),
        aitrack::util::parse_ts("2026-11-08T01:12:50Z")
    );
    let rec = sample_record(&s, antigravity::SOURCE_USAGE, false);
    let r = Reading::from_value(&rec);
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
    assert_eq!(
        s.session
            .as_ref()
            .unwrap()
            .resets_at
            .as_deref()
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
