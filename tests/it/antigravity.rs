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

/// The reply on the author's PC after moving to a paid personal plan (agy, 2026-11-01): 5-hour
/// limits in both groups and a third group, "Gemini 4 Argon", with its own weekly limit.
fn paid_reply() -> Value {
    json!({
        "conversation_id": "", "status": "SUCCESS", "duration_seconds": 0, "num_turns": 0,
        "usage": {"input_tokens": 0, "output_tokens": 0, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 0},
        "command": {"name": "usage", "data": {"groups": [
            {"name": "Gemini Models", "buckets": [
                {"id": "gemini-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 1, "reset_time": "2026-11-08T05:56:03Z"},
                {"id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 1, "reset_time": "2026-11-01T10:56:03Z"}]},
            {"name": "Claude and GPT models", "buckets": [
                {"id": "3p-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 1, "reset_time": "2026-11-08T05:56:03Z"},
                {"id": "3p-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 1, "reset_time": "2026-11-01T10:56:03Z"}]},
            {"name": "Gemini 4 Argon", "buckets": [
                {"id": "gemini-argon-wtu-weekly", "name": "Gemini 4 Argon Weekly Limit", "window": "weekly", "remaining_fraction": 0.9654550552368164, "reset_time": "2026-11-08T05:55:14Z"}]}
        ]}}
    })
}

#[test]
fn a_paid_plan_reply_gives_both_bars_and_lists_the_argon_limit_in_any_group_order() {
    let mut reordered = paid_reply();
    reordered["command"]["data"]["groups"]
        .as_array_mut()
        .unwrap()
        .reverse();
    for body in [paid_reply(), reordered] {
        let s = parse_usage(&body, now()).unwrap();
        assert_eq!(
            s.session.as_ref().unwrap().pct,
            0.0,
            "Gemini 5h is the session bar"
        );
        assert_eq!(
            s.weekly.as_ref().unwrap().pct,
            0.0,
            "Gemini weekly, never Argon's"
        );
        let r = Reading::from_value(&sample_record(&s, antigravity::SOURCE_USAGE, false));
        let mut got: Vec<(String, String, f64)> = r
            .limits
            .iter()
            .map(|l| {
                (
                    l.name.clone(),
                    l.kind.clone(),
                    (l.pct * 10.0).round() / 10.0,
                )
            })
            .collect();
        got.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            got,
            vec![
                ("Claude and GPT models".into(), "session".into(), 0.0),
                ("Claude and GPT models".into(), "weekly".into(), 0.0),
                ("Gemini 4 Argon".into(), "weekly".into(), 3.5),
            ]
        );
    }
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

/// Records saved before limits carried their window length and full-again time still load: the
/// other group's weekly limit is a row of its own, not a tooltip line.
#[test]
fn the_gemini_block_shows_a_weekly_bar_and_a_row_for_the_other_group() {
    let at = Utc::now();
    let v = gemini_view(&readings(at), at);
    assert_eq!(v.name, "Gemini");
    assert_eq!(
        v.rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect::<Vec<_>>(),
        [("Weekly", "16%"), ("Claude/GPT", "5%")]
    );
    let tip = tip_text(&v.rows[0]);
    for want in ["Weekly refills gradually", "Live from Antigravity (agy)"] {
        assert!(tip.contains(want), "{want} missing from\n{tip}");
    }
    assert!(!tip.contains("Other models"), "{tip}");
    assert!(!tip.contains("Claude and GPT"), "{tip}");
    let row = &v.rows[1];
    assert!(row.secondary && !row.stale && !row.dim && row.time.is_none());
    assert_eq!(row.group.as_deref(), Some("Claude and GPT models"));
    let tip = tip_text(row);
    assert!(
        tip.starts_with("Claude and GPT models\nWeekly limit refills gradually\n"),
        "no full-again time in an old record: {tip}"
    );
    assert!(tip.contains("Live from Antigravity (agy)"), "{tip}");
    assert!(!tip.contains("hour limit"), "{tip}");
    let st = build_antigravity_state(&readings(at), status(), true, at);
    assert!(
        st.diagnostics[0].text.starts_with("Gemini: weekly"),
        "{}",
        st.diagnostics[0].text
    );
}

/// `paid_reply()` (or `real_reply()`) stored at `at` minus `age_min`, as the poller saves it.
fn stored(body: &Value, at: DateTime<Utc>, age_min: i64) -> Reading {
    let s = parse_usage(body, at - Duration::minutes(age_min)).unwrap();
    let mut r = sample_record(&s, antigravity::SOURCE_USAGE, false);
    r["vendor"] = json!("antigravity");
    Reading::from_value(&r)
}

/// `paid_reply()` with its full-again times moved to `at` plus a few days (the fixture's are fixed).
fn paid_reply_at(at: DateTime<Utc>) -> Value {
    let mut b = paid_reply();
    for g in b["command"]["data"]["groups"].as_array_mut().unwrap() {
        for k in g["buckets"].as_array_mut().unwrap() {
            k["reset_time"] = json!(t(at + Duration::days(6) + Duration::hours(23)));
        }
    }
    b
}

#[test]
fn a_paid_plan_gets_a_row_per_other_group_with_its_five_hour_limit_in_the_tooltip() {
    let at = Utc::now();
    let v = gemini_view(&[stored(&paid_reply_at(at), at, 1)], at);
    // the extra groups in the reply's order, under the block's own rows
    assert_eq!(
        v.rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str(), r.secondary))
            .collect::<Vec<_>>(),
        [
            ("Session", "0%", false),
            ("Weekly", "0%", false),
            ("Claude/GPT", "0%", true),
            ("Argon", "4%", true),
        ]
    );
    assert_eq!(v.rows[3].group.as_deref(), Some("Gemini 4 Argon"));
    // 3.5% used lights the first cells like the Weekly row would
    assert_eq!(
        v.rows[3].cells,
        aitrack::view::bar_cells(3.5, None, false),
        "Argon's 3.5%"
    );
    let cg = tip_text(&v.rows[2]);
    for want in [
        "Claude and GPT models",
        "Weekly limit refills gradually: full again in 6d 2",
        "5-hour limit 0%",
        "Live from Antigravity (agy)",
    ] {
        assert!(cg.contains(want), "{want} missing from\n{cg}");
    }
    let argon = tip_text(&v.rows[3]);
    assert!(argon.starts_with("Gemini 4 Argon\nWeekly limit refills gradually: full again in"));
    assert!(
        !argon.contains("hour limit"),
        "Argon has no 5-hour limit: {argon}"
    );
    // no group twice: neither in the Session nor the Weekly tooltip any more
    for r in &v.rows[..2] {
        let tip = tip_text(r);
        assert!(!tip.contains("Other models"), "{tip}");
        assert!(!tip.contains("Claude and GPT"), "{tip}");
        assert!(!tip.contains("Argon"), "{tip}");
    }
    // the alerts and Stats only ever see the Gemini Models bars
    let st = build_antigravity_state(&[stored(&paid_reply_at(at), at, 1)], status(), true, at);
    let plan = st.plan.as_ref().unwrap();
    assert_eq!(plan.weekly.as_ref().unwrap().pct, 0.0);
    assert_eq!(plan.session.as_ref().unwrap().pct, 0.0);
}

#[test]
fn a_free_reply_gets_one_extra_row() {
    let at = Utc::now();
    let v = gemini_view(&[stored(&real_reply(), at, 1)], at);
    assert_eq!(
        v.rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect::<Vec<_>>(),
        [("Weekly", "17%"), ("Claude/GPT", "0%")]
    );
    assert!(!tip_text(&v.rows[0]).contains("Other models"));
    let tip = tip_text(&v.rows[1]);
    assert!(
        tip.starts_with("Claude and GPT models\nWeekly limit refills gradually"),
        "{tip}"
    );
    assert!(
        !tip.contains("hour limit"),
        "a free plan has no 5-hour limit: {tip}"
    );
}

#[test]
fn group_rows_follow_the_staleness_rules_of_the_main_rows() {
    let at = Utc::now();
    // over an hour old: dimmed, the same caution line, still no pace or time
    let v = gemini_view(&[stored(&paid_reply_at(at), at, 90)], at);
    for r in &v.rows[2..] {
        assert!(r.dim && !r.stale && r.time.is_none(), "{r:?}");
        assert!(
            r.tip.iter().any(|l| l.tone == aitrack::view::Tone::Caution
                && l.text
                    .contains("Antigravity has not reported usage recently")),
            "{r:?}"
        );
    }
    assert!(v.age_chip.is_some());
    // remaining mode reads like the Weekly row
    let mut s = build_state(&[], Values::default(), status(), at);
    s.claude_on = false;
    s.settings.direction = aitrack::settings::Direction::Remaining;
    s.antigravity = Some(build_antigravity_state(
        &[stored(&paid_reply_at(at), at, 1)],
        status(),
        true,
        at,
    ));
    let home = aitrack::view::build_home(&s, at);
    let v = &home.vendors[0];
    assert_eq!(v.rows[3].value, "97% left");
    assert!(tip_text(&v.rows[2]).contains("5-hour limit 100% left"));
}

#[test]
fn a_group_row_label_is_short() {
    use antigravity::row_label;
    assert_eq!(row_label("Gemini 4 Argon"), "Argon");
    assert_eq!(row_label("gemini 2.5 Flash Lite"), "Flash Lite");
    assert_eq!(row_label("Claude and GPT models"), "Claude/GPT");
    assert_eq!(row_label("Claude and GPT Models"), "Claude/GPT");
    // no version number: kept, cut to the label column
    assert_eq!(row_label("Gemini Nano"), "Gemini Na\u{2026}");
    assert_eq!(row_label("Imagen"), "Imagen");
    assert_eq!(row_label("Open weights models"), "Open weig\u{2026}");
    assert_eq!(row_label("Gemini 4 "), "Gemini 4");
    assert_eq!(row_label("Mod\u{e8}les models"), "Mod\u{e8}les");
    for n in [
        "Some very long group name",
        "\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}",
    ] {
        assert!(
            row_label(n).chars().count() <= antigravity::ROW_LABEL_MAX,
            "{n}"
        );
    }
}

#[test]
fn only_a_groups_weekly_limit_is_a_row() {
    use aitrack::reading::model_limits;
    let ls = model_limits(&json!([
        {"name": "Gemini Models (daily)", "kind": "weekly", "percent": 5.0, "hours": 24.0},
        {"name": "Gemini Models (daily)", "kind": "weekly", "percent": 5.0},
        {"name": "Claude and GPT models", "kind": "session", "percent": 5.0, "hours": 5.0},
        {"name": "Claude and GPT models", "kind": "weekly", "percent": 5.0, "hours": 168.0, "full_at": "2026-11-08T05:56:03Z"},
        {"name": "Claude and GPT models", "kind": "weekly", "percent": 5.0},
        {"name": "Monthly pool", "kind": "weekly", "percent": 5.0, "hours": 720.0},
    ]));
    let rows: Vec<bool> = ls.iter().map(antigravity::is_row_limit).collect();
    assert_eq!(rows, [false, false, false, true, true, false]);
    assert_eq!(ls[3].full_at.as_deref(), Some("2026-11-08T05:56:03Z"));
    assert_eq!(ls[4].full_at, None);
    assert_eq!(ls[4].hours, None);
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

#[test]
fn the_sign_in_type_comes_from_agys_newest_log_only() {
    let d = common::TempDir::new();
    let mut env = Env::new(Os::Windows);
    env.home = Some(d.path().to_path_buf());
    assert_eq!(antigravity::auth_method(&env), None);
    let log = antigravity::cli_dir(&env).join("log");
    std::fs::create_dir_all(&log).unwrap();
    // the lines as agy 1.4.12 writes them (email and project made up)
    std::fs::write(log.join("cli-20261031_202322.log"),
        "I1031 20:23:23.077651       1 server_oauth.go:224] applyAuthResult: email=a@example.com, authMethod=consumer, quotaProject=\n").unwrap();
    assert_eq!(antigravity::auth_method(&env).as_deref(), Some("consumer"));
    std::fs::write(log.join("cli-20261031_204617.log"),
        "I1031 20:46:17.734413       1 auth_provider.go:954] [AuthProvider] SetEnableBusinessLogin called with enable: true\nI1031 20:46:17.898128       1 server_oauth.go:224] applyAuthResult: email=a@example.com, authMethod=gcp, quotaProject=some-project-1\n").unwrap();
    assert_eq!(antigravity::auth_method(&env).as_deref(), Some("gcp"));
}

#[test]
fn only_a_business_sign_in_says_likely_enterprise() {
    let at = Utc::now();
    let note = |err: &str| {
        let mut s = build_state(&[], Values::default(), status(), at);
        s.claude_on = false;
        let mut st = status();
        st.error = Some(err.into());
        s.antigravity = Some(build_antigravity_state(&readings(at), st, true, at));
        aitrack::view::build_home(&s, at)
            .vendors
            .into_iter()
            .find(|v| v.id == VendorId::Antigravity)
            .unwrap()
            .warn
            .map(|w| w.full)
    };
    assert_eq!(
        note(antigravity::BUSINESS_REPLY).as_deref(),
        Some("Likely enterprise account")
    );
    assert_ne!(
        note(antigravity::EMPTY_REPLY).as_deref(),
        Some("Likely enterprise account")
    );
}

/// The Gemini group rows in the window: under the block's own rows in every layout, hover only.
#[cfg(feature = "gui")]
mod window {
    use super::*;
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use aitrack::store::JsonlStore;
    use aitrack::ui::AitrackUi;
    use aitrack::vendor;
    use eframe::egui::{
        self, Context, CursorIcon, Event, PointerButton, Pos2, RawInput, Rect, ViewportCommand,
        ViewportId,
    };
    use std::sync::Arc;

    struct Win {
        ctx: Context,
        ui: AitrackUi,
        width: f32,
        height: f32,
        t: f64,
        _d: common::TempDir,
    }

    impl Win {
        fn new(horizontal: bool, small: bool) -> Win {
            let d = common::TempDir::new();
            let settings = Arc::new(Settings::load(d.join("s.json")));
            settings.set_horizontal(horizontal);
            settings.set_small(small);
            let app = Arc::new(App::with_poll_fn(
                Box::new(JsonlStore::new(d.join("u.jsonl"))),
                settings,
                Box::new(|_| Ok(Value::Null)),
            ));
            let ctx = Context::default();
            let mut ui = AitrackUi::new(&ctx, app);
            ui.sandbox();
            ui.mock = Some(vendor::demo_vendors(Utc::now()));
            let mut w = Win {
                ctx,
                ui,
                width: 310.0,
                height: 900.0,
                t: 0.0,
                _d: d,
            };
            for _ in 0..6 {
                w.frame(vec![]);
            }
            w
        }

        fn frame(&mut self, events: Vec<Event>) -> CursorIcon {
            self.t += 0.2;
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(self.width, self.height),
                )),
                time: Some(self.t),
                events,
                ..Default::default()
            };
            let ui = &mut self.ui;
            let out = self.ctx.run(raw, |c| ui.draw(c));
            if let Some(v) = out.viewport_output.get(&ViewportId::ROOT) {
                for c in &v.commands {
                    if let ViewportCommand::InnerSize(s) = c {
                        // like the OS: the last request of the frame wins
                        self.width = s.x;
                        self.height = s.y;
                    }
                }
            }
            out.platform_output.cursor_icon
        }

        fn groups(&self) -> Vec<(String, Rect)> {
            self.ui
                .group_row_rects
                .iter()
                .filter(|(id, _, _)| *id == VendorId::Antigravity)
                .map(|(_, g, r)| (g.clone(), *r))
                .collect()
        }

        fn row(&self, id: VendorId, kind: Kind) -> Rect {
            self.ui
                .row_rects
                .iter()
                .find(|(v, k, _)| *v == id && *k == kind)
                .map(|(_, _, r)| *r)
                .unwrap()
        }
    }

    fn names(g: &[(String, Rect)]) -> Vec<&str> {
        g.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn the_group_rows_grow_the_gemini_block_and_open_nothing() {
        let mut w = Win::new(false, false);
        let g = w.groups();
        assert_eq!(names(&g), ["Claude and GPT models", "Gemini 4 Argon"]);
        let weekly = w.row(VendorId::Antigravity, Kind::Weekly);
        let grok = w.row(VendorId::Grok, Kind::Weekly);
        // stacked under Gemini's Weekly row, one row each, and the Grok block starts below them
        assert!(g[0].1.top() >= weekly.bottom() - 0.5, "{g:?} {weekly:?}");
        assert!(g[1].1.top() >= g[0].1.bottom() - 0.5, "{g:?}");
        assert!(grok.top() > g[1].1.bottom() + 20.0, "{g:?} {grok:?}");
        assert!(w.height >= grok.bottom(), "the window fits every row");
        // hover only: no pointing hand, and a click opens no detail pane
        let p = g[0].1.center();
        w.frame(vec![Event::PointerMoved(p)]);
        assert_ne!(w.frame(vec![]), CursorIcon::PointingHand);
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        w.frame(vec![btn(true)]);
        w.frame(vec![btn(false)]);
        w.frame(vec![]);
        assert!(w.ui.expanded.is_none() && !w.ui.bot_expanded);
        // Gemini's Weekly row still opens its pane, and the group rows do not look open with it
        let p = w.row(VendorId::Antigravity, Kind::Weekly).center();
        w.frame(vec![Event::PointerMoved(p)]);
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        w.frame(vec![btn(true)]);
        w.frame(vec![btn(false)]);
        w.frame(vec![]);
        assert_eq!(w.ui.expanded, Some((VendorId::Antigravity, Kind::Weekly)));
    }

    #[test]
    fn the_group_rows_show_side_by_side_and_in_the_small_window() {
        for (horizontal, small) in [(true, false), (false, true), (true, true)] {
            let w = Win::new(horizontal, small);
            let g = w.groups();
            assert_eq!(
                names(&g),
                ["Claude and GPT models", "Gemini 4 Argon"],
                "horizontal {horizontal}, small {small}"
            );
            for (_, r) in &g {
                assert!(
                    r.bottom() <= w.height + 0.5 && r.right() <= w.width + 0.5,
                    "horizontal {horizontal}, small {small}: {r:?} in {}x{}",
                    w.width,
                    w.height
                );
            }
        }
    }
}
