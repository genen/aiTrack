// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::analysis::analyze;
use aitrack::collector::{SampleInfo, Status};
use aitrack::live::Poller;
use aitrack::reading::Reading;
use aitrack::settings::Values;
use aitrack::sources::{self, ClaudeJson, DesktopHistory, Env, Os, UsageSource};
use aitrack::state::build_state;
use aitrack::store::{JsonlStore, Store};
use aitrack::view::{self, Dot};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const ORG_A: &str = "org-aaaa-secret-1111";
const ORG_B: &str = "org-bbbb-secret-2222";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn sample(minutes_ago: i64, org: &str, u: Value) -> Value {
    json!({"t": ms(now() - Duration::minutes(minutes_ago)), "org": org, "u": u})
}

fn write(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, v.to_string()).unwrap();
}

/// A Linux-style environment rooted in a temp dir; returns the history file path.
fn linux_env(dir: &TempDir) -> (Env, PathBuf) {
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.xdg_config_home = Some(dir.join("xdg"));
    (env, dir.join("xdg/Claude/plan-usage-history.json"))
}

fn desktop_poller(env: &Env) -> Poller {
    Poller::with_chain(vec![Box::new(DesktopHistory { env: env.clone() })])
}

fn history(samples: Vec<Value>) -> Value {
    json!({"version": 2, "samples": samples})
}

// ------------------------------------------------------------ path discovery

#[test]
fn windows_paths_include_classic_and_every_store_package() {
    let dir = TempDir::new();
    let local = dir.join("Local");
    for d in [
        "Claude_abc123",
        "Claude_zzz",
        "claude_lower",
        "Other_1",
        "NotClaude_2",
    ] {
        std::fs::create_dir_all(local.join("Packages").join(d)).unwrap();
    }
    std::fs::write(
        local.join("Packages/Claude_file"),
        "a file, not a dir is still a candidate name",
    )
    .unwrap();
    let mut env = Env::new(Os::Windows);
    env.appdata = Some(dir.join("Roaming"));
    env.localappdata = Some(local.clone());
    let got = sources::desktop_history_candidates(&env);
    let tail = |p: &PathBuf| {
        p.strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let got: Vec<String> = got.iter().map(tail).collect();
    let pkg =
        |n: &str| format!("Local/Packages/{n}/LocalCache/Roaming/Claude/plan-usage-history.json");
    assert_eq!(
        got,
        vec![
            "Roaming/Claude/plan-usage-history.json".to_string(),
            pkg("Claude_abc123"),
            pkg("Claude_file"),
            pkg("Claude_zzz"),
            pkg("claude_lower"),
        ]
    );
}

#[test]
fn windows_without_env_falls_back_to_the_home_directory() {
    let mut env = Env::new(Os::Windows);
    env.home = Some(PathBuf::from("/u/me"));
    let got = sources::desktop_history_candidates(&env);
    assert_eq!(got.len(), 1);
    assert!(got[0]
        .to_string_lossy()
        .replace('\\', "/")
        .ends_with("/u/me/AppData/Roaming/Claude/plan-usage-history.json"));
}

#[test]
fn mac_cache_dir_sits_under_application_support() {
    let mut env = Env::new(Os::Mac);
    env.home = Some(PathBuf::from("/h"));
    let dirs = sources::desktop_data_dirs(&env);
    assert_eq!(dirs.len(), 1);
    assert_eq!(
        sources::cache_dir_of(&dirs[0])
            .to_string_lossy()
            .replace('\\', "/"),
        "/h/Library/Application Support/Claude/Cache/Cache_Data"
    );
}

#[test]
fn mac_linux_and_claude_json_candidates() {
    let mut env = Env::new(Os::Mac);
    env.home = Some(PathBuf::from("/h"));
    let norm = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
    assert_eq!(
        norm(&sources::desktop_history_candidates(&env)[0]),
        "/h/Library/Application Support/Claude/plan-usage-history.json"
    );
    env.os = Os::Linux;
    assert_eq!(
        norm(&sources::desktop_history_candidates(&env)[0]),
        "/h/.config/Claude/plan-usage-history.json"
    );
    env.xdg_config_home = Some(PathBuf::from("/x"));
    assert_eq!(
        norm(&sources::desktop_history_candidates(&env)[0]),
        "/x/Claude/plan-usage-history.json"
    );
    assert_eq!(
        sources::claude_json_candidates(&env)
            .iter()
            .map(norm)
            .collect::<Vec<_>>(),
        ["/h/.claude.json"]
    );
    env.claude_config_dir = Some(PathBuf::from("/cfg"));
    assert_eq!(
        sources::claude_json_candidates(&env)
            .iter()
            .map(norm)
            .collect::<Vec<_>>(),
        ["/cfg/.claude.json", "/h/.claude.json"]
    );
}

// ------------------------------------------------------------ parsing the history file

#[test]
fn parses_version_1_and_2() {
    for v in [1, 2] {
        let f = json!({"version": v, "samples": [
            sample(30, ORG_A, json!({"fh": 10, "sd": 20})),
            sample(10, ORG_A, json!({"fh": 12.5, "sd": 21})),
        ], "unknown_top_level": true});
        let h = sources::parse_desktop_history(&f.to_string(), now()).unwrap();
        assert_eq!(h.version, Some(v));
        assert_eq!((h.total, h.timed, h.orgs, h.samples.len()), (2, 2, 1, 2));
        let l = h.latest().unwrap();
        assert_eq!((l.fh, l.sd), (Some(12.5), Some(21.0)));
        assert_eq!(h.newest_u_fields, ["fh", "sd"]);
    }
}

#[test]
fn missing_or_invalid_u_fields_are_tolerated() {
    let f = history(vec![
        sample(50, ORG_A, json!({"fh": 5})),                // only 5h
        sample(40, ORG_A, json!({"sd": 7, "extra": "x"})),  // only 7d, unknown field
        sample(35, ORG_A, json!({})),                       // nothing usable: skipped
        sample(30, ORG_A, json!({"fh": 150, "sd": -3})),    // out of range: skipped
        sample(25, ORG_A, json!({"fh": null, "sd": "50"})), // wrong types: skipped
        json!({"t": ms(now() - Duration::minutes(20)), "org": ORG_A}), // no u at all
        json!("a stray string"),
        json!({"org": ORG_A, "u": {"fh": 1}}), // no t
    ]);
    let h = sources::parse_desktop_history(&f.to_string(), now()).unwrap();
    assert_eq!(h.total, 8);
    assert_eq!(
        h.samples.iter().map(|s| (s.fh, s.sd)).collect::<Vec<_>>(),
        [(Some(5.0), None), (None, Some(7.0))]
    );
    // the newest entry (20 min ago) has no `u`, so there is no usable latest sample
    assert!(h.latest().is_none());
    assert_eq!(h.newest_fields, ["t", "org"]);
}

#[test]
fn newest_sample_decides_the_org_and_orgs_are_never_mixed() {
    let f = history(vec![
        sample(60, ORG_A, json!({"fh": 90, "sd": 90})),
        sample(30, ORG_B, json!({"fh": 11, "sd": 12})),
        sample(5, ORG_B, json!({"fh": 13})), // newest: org B, no sd
    ]);
    let h = sources::parse_desktop_history(&f.to_string(), now()).unwrap();
    assert_eq!(h.orgs, 2);
    assert_eq!(h.samples.len(), 2);
    let l = h.latest().unwrap();
    // sd is NOT filled in from org A's older sample (or from org B's older one)
    assert_eq!((l.fh, l.sd), (Some(13.0), None));
    assert!(h.samples.iter().all(|s| s.fh != Some(90.0)));
}

#[test]
fn order_duplicates_and_odd_timestamps() {
    let t = |m: i64| ms(now() - Duration::minutes(m));
    let f = history(vec![
        json!({"t": t(5), "org": ORG_A, "u": {"fh": 3}}),
        json!({"t": t(20), "org": ORG_A, "u": {"fh": 1}}),
        json!({"t": t(5), "org": ORG_A, "u": {"fh": 4}}), // same t again: the later entry wins
        json!({"t": ms(now() + Duration::hours(3)), "org": ORG_A, "u": {"fh": 99}}), // future: ignored
        // seconds instead of milliseconds
        json!({"t": (now() - Duration::minutes(40)).timestamp(), "org": ORG_A, "u": {"fh": 0}}),
    ]);
    let h = sources::parse_desktop_history(&f.to_string(), now()).unwrap();
    let fh: Vec<_> = h.samples.iter().map(|s| s.fh.unwrap()).collect();
    assert_eq!(fh, [0.0, 1.0, 4.0]);
    assert!(h.samples.windows(2).all(|w| w[0].t_ms < w[1].t_ms));
}

#[test]
fn malformed_input_gives_descriptive_errors_not_panics() {
    for (text, want) in [
        ("{not json", "not valid JSON"),
        ("", "not valid JSON"),
        ("[1,2,3]", "not an object"),
        ("{\"version\":2}", "no `samples`"),
        ("{\"samples\": 5}", "no `samples`"),
    ] {
        let e = sources::parse_desktop_history(text, now()).unwrap_err();
        assert!(e.contains(want), "{text:?} -> {e}");
    }
    // parseable but empty
    let h = sources::parse_desktop_history("{\"samples\": []}", now()).unwrap();
    assert!(h.newest_t_ms.is_none() && h.samples.is_empty());
}

#[test]
fn bom_prefixed_file_is_read() {
    let dir = TempDir::new();
    let (env, path) = linux_env(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = history(vec![sample(5, ORG_A, json!({"fh": 1, "sd": 2}))]).to_string();
    std::fs::write(&path, format!("\u{feff}{body}")).unwrap();
    let obs = DesktopHistory { env }.read(now()).unwrap();
    assert_eq!(obs.latest.session.unwrap().pct, 1.0);
}

// ------------------------------------------------------------ the desktop source through the poller

#[test]
fn first_read_backfills_all_samples_then_is_idempotent() {
    let dir = TempDir::new();
    let (env, path) = linux_env(&dir);
    write(
        &path,
        &history(
            (0..6)
                .map(|i| sample(80 - i * 10, ORG_A, json!({"fh": 10 + i, "sd": 20 + i})))
                .collect(),
        ),
    );
    let store = JsonlStore::new(dir.join("usage.jsonl"));

    let rec = desktop_poller(&env).poll_at(&store, now()).unwrap();
    let all = store.read();
    assert_eq!(all.len(), 6);
    // ordered by sample time, ts = the sample's own time, no reset times, tagged with the source
    let ts: Vec<&str> = all.iter().map(|r| r["ts"].as_str().unwrap()).collect();
    let mut sorted = ts.clone();
    sorted.sort();
    assert_eq!(ts, sorted);
    for r in &all {
        assert_eq!(
            (r["v"].as_i64(), r["ok"].as_bool(), r["source"].as_str()),
            (Some(1), Some(true), Some("desktop-history"))
        );
        assert_eq!(r["observed_at"], r["ts"]);
        assert!(r["session"]["resets_at"].is_null() && r["weekly"]["resets_at"].is_null());
    }
    assert_eq!(all[0]["session"]["pct"], 10.0);
    assert_eq!(all[5]["weekly"]["pct"], 25.0);
    // only the newest record is a "live" row; the rest are flagged as backfill
    assert_eq!(all.iter().filter(|r| r["backfill"] == true).count(), 5);
    assert!(all[5].get("backfill").is_none());
    assert_eq!(rec["ts"], all[5]["ts"]);
    // no org id or other identifying content in the store
    let text = std::fs::read_to_string(dir.join("usage.jsonl")).unwrap();
    assert!(!text.contains("org-") && !text.contains("secret"));
    assert!(
        text.contains("\"fh\":15"),
        "the sample's u object is kept in raw"
    );

    // same poller again, a brand new poller (fresh process), and a longer time later: no duplicates
    assert_eq!(
        desktop_poller(&env).poll_at(&store, now()).unwrap()["stored"],
        false
    );
    let p = desktop_poller(&env);
    let r = p.poll_at(&store, now() + Duration::minutes(15)).unwrap();
    assert_eq!(r["stored"], false); // "checked, nothing new"
    assert_eq!(r["source"], "desktop-history");
    assert!(r["observed_at"].as_str().is_some());
    assert_eq!(store.read().len(), 6);
    assert_eq!(p.poll_at(&store, now()).unwrap()["stored"], false);
    assert_eq!(store.read().len(), 6);

    // the app writes two more samples: only those are appended
    let mut samples = (0..6)
        .map(|i| sample(80 - i * 10, ORG_A, json!({"fh": 10 + i, "sd": 20 + i})))
        .collect::<Vec<_>>();
    samples.push(sample(5, ORG_A, json!({"fh": 30, "sd": 26})));
    samples.push(sample(1, ORG_A, json!({"fh": 31, "sd": 26})));
    write(&path, &history(samples));
    let r = p.poll_at(&store, now()).unwrap();
    assert_eq!(r["session"]["pct"], 31.0);
    assert_eq!(store.read().len(), 8);
    assert_eq!(store.read()[6]["backfill"], true);
}

#[test]
fn backfill_skips_other_orgs_and_survives_other_records_in_the_store() {
    let dir = TempDir::new();
    let (env, path) = linux_env(&dir);
    write(
        &path,
        &history(vec![
            sample(90, ORG_A, json!({"fh": 99, "sd": 99})),
            sample(20, ORG_B, json!({"fh": 4, "sd": 5})),
            sample(10, ORG_B, json!({"fh": 6, "sd": 7})),
        ]),
    );
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    // an older record without a source and an oauth record are already there
    store.append(&json!({"v": 1, "ts": "2026-10-14T10:00:00+00:00", "ok": true, "session": {"pct": 1.0, "resets_at": "2026-10-14T15:00:00+00:00"}, "weekly": null})).unwrap();
    store.append(&json!({"v": 1, "ts": "2026-10-14T10:30:00+00:00", "ok": true, "source": "oauth", "observed_at": "2026-10-14T10:30:00+00:00", "session": null, "weekly": null})).unwrap();
    desktop_poller(&env).poll_at(&store, now()).unwrap();
    let all = store.read();
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|r| r["session"]["pct"] != 99.0));
}

#[test]
fn stale_desktop_history_is_backfilled_but_rejected_as_current() {
    let dir = TempDir::new();
    let (env, path) = linux_env(&dir);
    write(
        &path,
        &history(vec![
            sample(300, ORG_A, json!({"fh": 10, "sd": 20})),
            sample(190, ORG_A, json!({"fh": 12, "sd": 21})),
        ]),
    );
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let e = desktop_poller(&env).poll_at(&store, now()).unwrap_err();
    assert_eq!(
        e.message,
        "Claude desktop usage is stale (last sample 3 h ago): open the Claude app"
    );
    let all = store.read();
    assert_eq!(all.len(), 3); // 2 imported samples + 1 error record
    assert_eq!(all[2]["ok"], false);
    assert_eq!(all[2]["error"], e.message);
}

#[test]
fn missing_and_malformed_files_report_why() {
    let dir = TempDir::new();
    let (env, path) = linux_env(&dir);
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let e = desktop_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(e.message.contains("not found (checked 1 location)"), "{e}");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ definitely not json").unwrap();
    let e = desktop_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(
        e.message.contains("unreadable") && e.message.contains("not valid JSON"),
        "{e}"
    );
    write(&path, &json!({"samples": [sample(5, ORG_A, json!({}))]}));
    let e = desktop_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(e.message.contains("no usable"), "{e}");
    assert_eq!(store.read().len(), 3); // each failure was recorded once
}

#[test]
fn the_freshest_of_several_history_files_wins() {
    let dir = TempDir::new();
    let local = dir.join("Local");
    let mut env = Env::new(Os::Windows);
    env.appdata = Some(dir.join("Roaming"));
    env.localappdata = Some(local.clone());
    let store_pkg =
        local.join("Packages/Claude_x/LocalCache/Roaming/Claude/plan-usage-history.json");
    write(
        &dir.join("Roaming/Claude/plan-usage-history.json"),
        &history(vec![sample(80, ORG_A, json!({"fh": 1, "sd": 1}))]),
    );
    write(
        &store_pkg,
        &history(vec![sample(4, ORG_B, json!({"fh": 8, "sd": 9}))]),
    );
    let obs = DesktopHistory { env }.read(now()).unwrap();
    assert_eq!(obs.latest.session.unwrap().pct, 8.0);
}

// ------------------------------------------------------------ ~/.claude.json

fn claude_json(fetched_min_ago: i64, cache_acct: Option<&str>, own_acct: Option<&str>) -> Value {
    let mut cache = json!({
        "fetchedAtMs": ms(now() - Duration::minutes(fetched_min_ago)),
        "utilization": {
            "five_hour": {"utilization": 33.0, "resets_at": "2026-10-14T14:00:00+00:00"},
            "seven_day": {"utilization": 44.0, "resets_at": "2026-10-18T09:00:00.000000+00:00"},
        },
    });
    if let Some(a) = cache_acct {
        cache["accountUuid"] = json!(a);
    }
    let mut v = json!({"cachedUsageUtilization": cache, "numStartups": 12});
    if let Some(a) = own_acct {
        v["oauthAccount"] = json!({"accountUuid": a, "emailAddress": "someone@example.com"});
    }
    v
}

fn json_poller(env: &Env) -> Poller {
    Poller::with_chain(vec![Box::new(ClaudeJson { env: env.clone() })])
}

#[test]
fn claude_json_cache_is_used_with_its_reset_times() {
    let dir = TempDir::new();
    let (env, _) = linux_env(&dir);
    write(
        &dir.join("home/.claude.json"),
        &claude_json(10, Some("acct-1"), Some("acct-1")),
    );
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let rec = json_poller(&env).poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "claude-json");
    assert_eq!(rec["session"]["pct"], 33.0);
    assert_eq!(rec["session"]["resets_at"], "2026-10-14T14:00:00+00:00");
    assert_eq!(rec["weekly"]["pct"], 44.0);
    assert_eq!(store.read().len(), 1);
    // same fetch again: nothing new is stored
    assert_eq!(
        json_poller(&env).poll_at(&store, now()).unwrap()["stored"],
        false
    );
    assert_eq!(store.read().len(), 1);
}

#[test]
fn claude_json_account_mismatch_missing_and_stale() {
    let dir = TempDir::new();
    let (env, _) = linux_env(&dir);
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let path = dir.join("home/.claude.json");

    write(&path, &claude_json(10, Some("acct-1"), Some("acct-2")));
    let e = json_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(e.message.contains("different account"), "{e}");

    // no oauthAccount to compare with, or no accountUuid in the cache: accepted
    write(&path, &claude_json(10, Some("acct-1"), None));
    assert!(json_poller(&env).poll_at(&store, now()).is_ok());
    write(&path, &claude_json(10 + 1, None, Some("acct-2")));
    assert!(json_poller(&env).poll_at(&store, now()).is_ok());

    write(&path, &json!({"numStartups": 3}));
    let e = json_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(e.message.contains("no cachedUsageUtilization"), "{e}");

    write(&path, &claude_json(200, None, None));
    let e = json_poller(&env).poll_at(&store, now()).unwrap_err();
    assert!(
        e.message.contains("stale") && e.message.contains("3 h"),
        "{e}"
    );

    std::fs::write(&path, "{broken").unwrap();
    assert!(json_poller(&env)
        .poll_at(&store, now())
        .unwrap_err()
        .message
        .contains("not valid JSON"));
}

// ------------------------------------------------------------ the chain

#[test]
fn chain_falls_through_stale_desktop_to_claude_json_then_oauth() {
    let dir = TempDir::new();
    let (env, hist) = linux_env(&dir);
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    write_creds(&dir.join("cfg"), "tok-chain", now_ms() + 3_600_000.0);
    let mock = Mock::start();
    let mut env = env;
    env.claude_config_dir = Some(dir.join("cfg"));
    let chain = || Poller::with_env(&env, &mock.url, None);

    // desktop stale, claude.json fresh -> claude-json wins, oauth untouched
    write(
        &hist,
        &history(vec![sample(400, ORG_A, json!({"fh": 1, "sd": 1}))]),
    );
    write(&dir.join("cfg/.claude.json"), &claude_json(5, None, None));
    assert_eq!(
        chain().poll_at(&store, now()).unwrap()["source"],
        "claude-json"
    );
    assert!(mock.seen.lock().unwrap().is_empty());

    // everything stale/missing -> the endpoint is the last resort
    write(&dir.join("cfg/.claude.json"), &claude_json(500, None, None));
    let rec = chain().poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "oauth");
    assert_eq!(rec["session"]["pct"], 42.0);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);

    // fresh desktop beats everything and the endpoint is not called again
    write(
        &hist,
        &history(vec![sample(2, ORG_A, json!({"fh": 7, "sd": 8}))]),
    );
    let rec = chain().poll_at(&store, now()).unwrap();
    assert_eq!(
        (rec["source"].as_str(), rec["session"]["pct"].as_f64()),
        (Some("desktop-history"), Some(7.0))
    );
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
}

#[test]
fn one_error_lists_why_each_source_failed() {
    let dir = TempDir::new();
    let (mut env, hist) = linux_env(&dir);
    env.claude_config_dir = Some(dir.join("cfg"));
    write(
        &hist,
        &history(vec![sample(180, ORG_A, json!({"fh": 1, "sd": 1}))]),
    );
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let e = Poller::with_env(&env, "http://127.0.0.1:1/", None)
        .poll_at(&store, now())
        .unwrap_err();
    assert!(
        e.message
            .contains("Claude desktop usage is stale (last sample 3 h ago): open the Claude app"),
        "{e}"
    );
    assert!(
        e.message
            .contains("~/.claude.json has no usable usage cache"),
        "{e}"
    );
    assert!(
        e.message.contains("usage API: no Claude login found"),
        "{e}"
    );
    let errors: Vec<_> = store
        .read()
        .into_iter()
        .filter(|r| r["ok"] == false)
        .collect();
    assert_eq!(errors.len(), 1, "exactly one error record per failed poll");
    assert_eq!(errors[0]["error"], e.message);
}

#[test]
fn oauth_retry_after_survives_the_combined_error() {
    let dir = TempDir::new();
    let (mut env, _) = linux_env(&dir);
    env.claude_config_dir = Some(dir.join("cfg"));
    write_creds(&dir.join("cfg"), "t", now_ms() + 1e6);
    let mock = Mock::start();
    mock.set_status(429);
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    let e = Poller::with_env(&env, &mock.url, None)
        .poll_at(&store, now())
        .unwrap_err();
    assert_eq!(e.retry_after, Some(120.0));
    assert!(e.message.contains("HTTP 429"));
}

#[test]
fn source_list_can_be_restricted_by_name() {
    let env = Env::new(Os::Linux);
    let names = |only: Option<&str>| -> Vec<&'static str> {
        sources::build_chain(&env, "http://x", only)
            .iter()
            .map(|s| s.name())
            .collect()
    };
    assert_eq!(
        names(None),
        ["desktop-cache", "desktop-history", "claude-json", "oauth"]
    );
    assert_eq!(names(Some("oauth")), ["oauth"]);
    assert_eq!(names(Some("claude-json, oauth")), ["claude-json", "oauth"]);
    assert_eq!(
        names(Some("bogus")),
        ["desktop-cache", "desktop-history", "claude-json", "oauth"]
    );
}

// ------------------------------------------------------------ --sources diagnostic

#[test]
fn diagnose_prints_structure_only() {
    let dir = TempDir::new();
    let (mut env, hist) = linux_env(&dir);
    env.claude_config_dir = Some(dir.join("cfg"));
    write(
        &hist,
        &history(vec![
            sample(300, ORG_A, json!({"fh": 90, "sd": 91})),
            sample(7, ORG_B, json!({"fh": 12, "sd": 34, "extra_u_field": 1})),
        ]),
    );
    let mut cj = claude_json(15, Some("acct-secret-9"), Some("acct-secret-9"));
    cj["oauthAccount"]["emailAddress"] = json!("private@example.com");
    write(&dir.join("cfg/.claude.json"), &cj);
    write_creds(&dir.join("cfg"), "tok-very-secret", now_ms() + 1e6);

    let out = sources::diagnose(&env, now());
    for want in [
        "desktop-history",
        "[exists ]",
        "[missing]",
        "parsed: yes",
        "version: 2",
        "distinct orgs: 2",
        "newest sample fields: t, org, u",
        "newest sample `u` fields: fh, sd, extra_u_field",
        "newest sample age: 7 min (fh=12, sd=34)",
        "fresh enough: yes",
        "claude-json",
        "five_hour=33 seven_day=44",
        "account matches oauthAccount: yes",
        "token present, expired: no",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    for secret in [
        ORG_A,
        ORG_B,
        "org-",
        "acct-secret",
        "private@example.com",
        "tok-very-secret",
    ] {
        assert!(!out.contains(secret), "leaked {secret:?} in:\n{out}");
    }
}

#[test]
fn diagnose_reports_missing_and_broken_files() {
    let dir = TempDir::new();
    let (env, hist) = linux_env(&dir);
    let out = sources::diagnose(&env, now());
    assert!(
        out.contains("no history file found")
            && out.contains("no .claude.json found")
            && out.contains("no usable token"),
        "{out}"
    );
    write(&hist, &json!({"nope": 1}));
    let out = sources::diagnose(&env, now());
    assert!(
        out.contains("parsed: no (unexpected shape (no `samples` list))"),
        "{out}"
    );
}

// ------------------------------------------------------------ null reset times in analysis and view

fn rd(minutes_ago: i64, session: f64, weekly: f64) -> Reading {
    Reading::from_value(&json!({
        "v": 1, "ok": true, "source": "desktop-history",
        "ts": (now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        "session": {"pct": session, "resets_at": null},
        "weekly": {"pct": weekly, "resets_at": null},
    }))
}

#[test]
fn windows_without_reset_times_are_inferred_from_drops() {
    // 5h usage climbs, falls back (a reset), climbs again; weekly just climbs
    let pts = [
        (300, 5.0, 10.0),
        (240, 20.0, 11.0),
        (180, 55.0, 12.0),
        (120, 2.0, 13.0),
        (60, 10.0, 14.0),
        (0, 30.0, 15.0),
    ];
    let rs: Vec<Reading> = pts.iter().map(|(m, s, w)| rd(*m, *s, *w)).collect();
    let plan = analyze(&rs, now()).unwrap();
    assert_eq!(plan.session_stats.windows, 2);
    assert_eq!(plan.session_stats.max_peak, Some(55.0));
    assert_eq!(plan.weekly_stats.windows, 1);
    let g = plan.session.unwrap();
    assert_eq!(
        (g.pct, g.hours_left, g.resets_at.clone(), g.projected_pct),
        (30.0, None, None, None)
    );
    // pace comes from the readings of the current (inferred) window only: 2 -> 30 over 2 h
    assert!(
        (g.rate_per_hour.unwrap() - 14.0).abs() < 0.1,
        "{:?}",
        g.rate_per_hour
    );
    assert!(g.hours_to_limit.is_some());
    assert_eq!(plan.last_source.as_deref(), Some("desktop-history"));
}

#[test]
fn small_dips_do_not_start_a_new_window_and_known_resets_still_win() {
    let rs: Vec<Reading> = [(120, 40.0), (60, 35.0), (0, 50.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 1.0))
        .collect();
    assert_eq!(analyze(&rs, now()).unwrap().session_stats.windows, 1); // a 5-point dip is not a reset
                                                                       // records that do carry resets_at are grouped by it exactly as before
    let with_reset = |m: i64, s: f64, reset_h: i64| {
        Reading::from_value(
            &json!({"ok": true, "ts": (now() - Duration::minutes(m)).to_rfc3339(),
            "session": {"pct": s, "resets_at": (now() + Duration::hours(reset_h)).to_rfc3339()}, "weekly": null}),
        )
    };
    let rs = vec![
        with_reset(60, 10.0, 2),
        with_reset(30, 20.0, 2),
        with_reset(0, 30.0, 2),
    ];
    let g = analyze(&rs, now()).unwrap().session.unwrap();
    assert_eq!((g.hours_left, g.projected_pct), (Some(2.0), Some(70.0)));
}

#[test]
fn unknown_reset_shows_in_tooltip_detail_and_pace_note() {
    let rs: Vec<Reading> = [(120, 10.0), (60, 20.0), (0, 30.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 5.0))
        .collect();
    let settings = Values::default();
    let st = build_state(
        &rs,
        settings,
        Status {
            error: None,
            polling: false,
            next_in: Some(600.0),
            polls: 1,
            sample: None,
            live: false,
        },
        now(),
    );
    let home = view::build_home(&st, now());
    let tip: Vec<&str> = home.vendors[0].rows[0]
        .tip
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(tip[0], "Session reset time unknown");
    assert!(tip[1].starts_with("Rising ~10.0 pts/h"), "{tip:?}");
    assert!(
        tip.iter()
            .any(|l| l.starts_with("From Claude desktop app, sample just now")),
        "{tip:?}"
    );
    assert!(!tip.iter().any(|l| l.contains("resets in")));
    let v = &aitrack::vendor::from_state_at(&st, now())[0];
    let d = view::build_detail_at(v, aitrack::reading::Kind::Session, false, now());
    assert_eq!(d.resets.as_deref(), Some("Reset time unknown"));
    assert!(d.rows.iter().any(|r| r.0 == "At reset" && r.1 == "-"));
    // the chart still splits windows by drops
    let chart = view::chart_data(
        &rs,
        aitrack::reading::Kind::Session,
        view::Range::Day,
        now(),
    );
    assert_eq!(chart.segments.len(), 1);
    let rs2: Vec<Reading> = [(120, 60.0), (60, 5.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 5.0))
        .collect();
    assert_eq!(
        view::chart_data(
            &rs2,
            aitrack::reading::Kind::Session,
            view::Range::Day,
            now()
        )
        .segments
        .len(),
        2
    );
}

#[test]
fn status_line_reports_last_sample_age_and_app_not_running() {
    let at = |m: i64| Status {
        live: false,
        error: None,
        polling: false,
        next_in: Some(900.0),
        polls: 3,
        sample: Some(SampleInfo {
            source: "desktop-history".into(),
            observed_at: Some(now() - Duration::minutes(m)),
            stored: false,
        }),
    };
    let rs = vec![rd(47, 10.0, 5.0)];
    let fresh = view::status_line(&build_state(&rs, Values::default(), at(3), now()), now());
    assert_eq!(
        (fresh.dot, fresh.text.as_str()),
        (Dot::Ok, "Last sample 3 min ago \u{b7} next in 15 min")
    );
    let old = view::status_line(&build_state(&rs, Values::default(), at(47), now()), now());
    assert_eq!(old.dot, Dot::Stale);
    assert_eq!(old.text, "Last sample 47 min ago (app not running?)");
    // the live endpoint keeps the old wording
    let live = Reading::from_value(
        &json!({"ok": true, "source": "oauth", "ts": (now() - Duration::minutes(2)).to_rfc3339(),
        "session": {"pct": 1.0, "resets_at": null}, "weekly": null}),
    );
    let st = build_state(
        &[live],
        Values::default(),
        Status {
            sample: None,
            live: false,
            ..at(0)
        },
        now(),
    );
    assert!(view::status_line(&st, now())
        .text
        .starts_with("Updated 2 min ago"));
}

#[test]
fn sample_info_is_read_from_the_poll_record() {
    let rec = json!({"source": "desktop-history", "observed_at": "2026-10-14T11:00:00+00:00", "stored": false});
    let i = SampleInfo::from_record(&rec).unwrap();
    assert_eq!((i.source.as_str(), i.stored), ("desktop-history", false));
    assert_eq!(
        i.observed_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 14, 11, 0, 0).unwrap())
    );
    assert!(SampleInfo::from_record(&json!({"ts": "x"})).is_none());
    assert!(SampleInfo::from_record(&Value::Null).is_none());
}

#[test]
fn live_local_polling_shows_live_instead_of_a_countdown() {
    let st = |live: bool, src: &str| Status {
        error: None,
        polling: false,
        next_in: Some(900.0),
        polls: 3,
        live,
        sample: Some(SampleInfo {
            source: src.into(),
            observed_at: Some(now() - Duration::minutes(1)),
            stored: true,
        }),
    };
    let rs = vec![rd(1, 10.0, 5.0)];
    let s = view::status_line(
        &build_state(&rs, Values::default(), st(true, "desktop-cache"), now()),
        now(),
    );
    assert_eq!(s.text, "Last sample 1 min ago \u{b7} live");
    // the network source keeps its countdown
    let s = view::status_line(
        &build_state(&rs, Values::default(), st(true, "oauth"), now()),
        now(),
    );
    assert!(s.text.ends_with("next in 15 min"), "{}", s.text);
}

#[test]
fn claude_plan_name_comes_from_the_login_file_without_the_token() {
    use aitrack::live::parse_subscription;
    let creds = |t: &str| {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-secret","subscriptionType":{t},"rateLimitTier":"default_claude_max_20x"}}}}"#
        )
    };
    assert_eq!(
        parse_subscription(creds("\"max\"").as_bytes()).as_deref(),
        Some("Max")
    );
    assert_eq!(
        parse_subscription(creds("\"pro\"").as_bytes()).as_deref(),
        Some("Pro")
    );
    assert_eq!(
        parse_subscription(creds("\"team\"").as_bytes()).as_deref(),
        Some("Team")
    );
    assert_eq!(
        parse_subscription(creds("\"enterprise\"").as_bytes()).as_deref(),
        Some("Enterprise")
    );
    assert_eq!(
        parse_subscription(creds("\"newplan\"").as_bytes()).as_deref(),
        Some("Newplan")
    );
    for none in ["null", "\"\"", "\"unknown\"", "3"] {
        assert_eq!(parse_subscription(creds(none).as_bytes()), None, "{none}");
    }
    assert_eq!(parse_subscription(b"not json"), None);
    assert_eq!(
        parse_subscription(br#"{"claudeAiOauth":{"accessToken":"t"}}"#),
        None
    );

    // $CLAUDE_CONFIG_DIR first, then ~/.claude; a file without the field falls through
    let d = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(d.join("home"));
    assert_eq!(sources::claude_plan(&env), None, "no login file");
    std::fs::create_dir_all(d.join("home/.claude")).unwrap();
    std::fs::write(d.join("home/.claude/.credentials.json"), creds("\"pro\"")).unwrap();
    assert_eq!(sources::claude_plan(&env).as_deref(), Some("Pro"));
    std::fs::create_dir_all(d.join("cfg")).unwrap();
    env.claude_config_dir = Some(d.join("cfg"));
    std::fs::write(
        d.join("cfg/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"t"}}"#,
    )
    .unwrap();
    assert_eq!(sources::claude_plan(&env).as_deref(), Some("Pro"));
    std::fs::write(d.join("cfg/.credentials.json"), creds("\"max\"")).unwrap();
    assert_eq!(sources::claude_plan(&env).as_deref(), Some("Max"));
    let report = sources::diagnose(&env, now());
    assert!(
        report.contains("plan (subscriptionType in .credentials.json): Max"),
        "{report}"
    );
    assert!(!report.contains("sk-secret"));
}

#[test]
fn claude_tooltip_shows_the_plan_like_chatgpt() {
    let rs: Vec<Reading> = [(60, 20.0), (0, 30.0)]
        .iter()
        .map(|(m, s)| rd(*m, *s, 5.0))
        .collect();
    let mut st = build_state(
        &rs,
        Values::default(),
        Status {
            error: None,
            polling: false,
            next_in: Some(600.0),
            polls: 1,
            sample: None,
            live: false,
        },
        now(),
    );
    let tips = |st: &aitrack::state::State| -> Vec<String> {
        view::build_home(st, now()).vendors[0].rows[0]
            .tip
            .iter()
            .map(|l| l.text.clone())
            .collect()
    };
    assert!(!tips(&st).iter().any(|l| l.starts_with("Plan:")));
    st.tier = Some("Max".into());
    assert!(
        tips(&st).contains(&"Plan: Max".to_string()),
        "{:?}",
        tips(&st)
    );
}
