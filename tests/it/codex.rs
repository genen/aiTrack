// SPDX-License-Identifier: GPL-3.0-or-later
//! Codex plan usage: the live usage endpoint (mock server), the session logs, the login, and what
//! the Codex block, its tooltips, Stats and the transcripts make of it.

use crate::common;
use aitrack::codex::{self, kind_of, parse_auth, parse_log_line, parse_usage};
use aitrack::collector::Status;
use aitrack::live::Poller;
use aitrack::reading::{Kind, Reading};
use aitrack::settings::Values;
use aitrack::sources::{Env, Os};
use aitrack::state::{build_codex_state, build_state};
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::VendorId;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN_SECRET: &str = "SECRET-TOKEN-PART";
const ACCOUNT: &str = "acct-SECRET-99";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap()
}

fn t(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A JWT-shaped token whose payload carries `exp` (the signature part is junk).
fn jwt(exp: DateTime<Utc>) -> String {
    format!(
        "{}.{}.{TOKEN_SECRET}",
        b64url(br#"{"alg":"none"}"#),
        b64url(
            json!({"exp": exp.timestamp(), "sub": "user-x"})
                .to_string()
                .as_bytes()
        )
    )
}

struct Home {
    dir: TempDir,
    env: Env,
}

fn home() -> Home {
    let dir = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.codex_home = Some(dir.join("codexhome"));
    std::fs::create_dir_all(dir.join("codexhome")).unwrap();
    Home { dir, env }
}

fn write_auth(h: &Home, token: &str) {
    std::fs::write(
        h.dir.join("codexhome/auth.json"),
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": "id-SECRET", "access_token": token, "refresh_token": "rt-SECRET", "account_id": ACCOUNT},
            "last_refresh": t(now()),
        })
        .to_string(),
    )
    .unwrap();
}

fn rl_line(at: DateTime<Utc>, limit_id: &str, primary: Value, secondary: Value) -> String {
    json!({
        "timestamp": t(at),
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": null,
            "rate_limits": {
                "limit_id": limit_id, "limit_name": if limit_id == "codex" { Value::Null } else { json!("GPT-5.3-Codex-Spark") },
                "primary": primary, "secondary": secondary,
                "credits": {"has_credits": true, "unlimited": false, "balance": "120"},
                "individual_limit": null, "spend_control_reached": null,
                "plan_type": "plus", "rate_limit_reached_type": null,
            },
        },
    })
    .to_string()
}

fn win(pct: f64, minutes: u64, resets: DateTime<Utc>) -> Value {
    json!({"used_percent": pct, "window_minutes": minutes, "resets_at": resets.timestamp()})
}

fn write_session(h: &Home, day: &str, name: &str, lines: &[String]) -> std::path::PathBuf {
    let d = h.dir.join(&format!("codexhome/sessions/{day}"));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

fn log_poller(h: &Home) -> Poller {
    Poller::with_chain_for(
        vec![Box::new(codex::CodexLog { env: h.env.clone() })],
        "codex",
    )
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

// ------------------------------------------------------------ parsing

#[test]
fn windows_are_classified_by_length_not_slot() {
    assert_eq!(kind_of(300.0), Kind::Session);
    assert_eq!(kind_of(10080.0), Kind::Weekly);
    // a weekly-only plan puts its weekly window in `primary`
    let line = rl_line(
        now(),
        "codex",
        win(43.0, 10080, now() + Duration::days(3)),
        Value::Null,
    );
    let e = parse_log_line(line.as_bytes()).unwrap();
    assert_eq!(e.windows.len(), 1);
    assert_eq!(kind_of(e.windows[0].minutes), Kind::Weekly);
    let s = codex::log_samples(&[e]);
    assert!(s[0].session.is_none());
    assert_eq!(s[0].weekly.as_ref().unwrap().pct, 43.0);
}

#[test]
fn log_lines_are_parsed_strictly() {
    let r = now() + Duration::hours(3);
    // a full line: both windows, plan and credits
    let line = rl_line(
        now(),
        "codex",
        win(12.0, 300, r),
        win(30.0, 10080, now() + Duration::days(5)),
    );
    let e = parse_log_line(line.as_bytes()).unwrap();
    assert!(e.main());
    assert_eq!(e.plan.as_deref(), Some("plus"));
    assert_eq!(
        e.windows[0].resets_at,
        Some(Utc.timestamp_opt(r.timestamp(), 0).unwrap())
    );
    // older builds: resets_in_seconds relative to the line's time
    let old = rl_line(
        now(),
        "codex",
        json!({"used_percent": 5.0, "window_minutes": 300, "resets_in_seconds": 600}),
        Value::Null,
    );
    let e = parse_log_line(old.as_bytes()).unwrap();
    assert_eq!(e.windows[0].resets_at, Some(now() + Duration::seconds(600)));
    // not usage lines, or broken ones
    for junk in [
        "",
        "not json",
        r#"{"type":"response_item","payload":{"type":"message"}}"#,
        r#"{"timestamp":"2026-10-24T12:00:00Z","payload":{"rate_limits":{"primary":null,"secondary":null}}}"#,
        r#"{"payload":{"rate_limits":{"primary":{"used_percent":1,"window_minutes":300}}}}"#,
    ] {
        assert!(parse_log_line(junk.as_bytes()).is_none(), "{junk}");
    }
}

#[test]
fn the_live_reply_gives_both_windows_plan_credits_buckets_and_banked_resets() {
    let body = json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 2, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": (now() + Duration::hours(1)).timestamp()},
            "secondary_window": {"used_percent": 7.5, "limit_window_seconds": 604800, "reset_at": (now() + Duration::days(4)).timestamp()},
        },
        "credits": {"has_credits": true, "unlimited": false, "balance": "42.5"},
        "additional_rate_limits": [{
            "limit_name": "GPT-5.3-Codex-Spark",
            "rate_limit": {
                "primary_window": {"used_percent": 20, "limit_window_seconds": 18000, "reset_after_seconds": 600},
                "secondary_window": {"used_percent": 9, "limit_window_seconds": 604800, "reset_after_seconds": 86400},
            },
        }],
        "rate_limit_reset_credits": {"available_count": 2},
        "account_id": ACCOUNT, "email": "someone@example.com",
    });
    let s = parse_usage(&body, now()).unwrap();
    assert_eq!(s.session.as_ref().unwrap().pct, 2.0);
    assert_eq!(s.weekly.as_ref().unwrap().pct, 7.5);
    let raw = s.raw.unwrap();
    assert_eq!(raw["tier"], "Pro");
    assert_eq!(raw["credits"]["balance"], 42.5);
    assert_eq!(raw["banked_resets"], 2);
    let limits = raw["limits"].as_array().unwrap();
    assert_eq!(limits.len(), 2);
    assert_eq!(limits[0]["kind"], "session");
    assert_eq!(limits[1]["kind"], "weekly");
    // no ids or emails are carried along
    let text = raw.to_string();
    assert!(
        !text.contains(ACCOUNT) && !text.contains("example.com"),
        "{text}"
    );
    // no windows at all is an error, not a 0 %
    assert!(parse_usage(&json!({"rate_limit": {}}), now()).is_err());
    assert!(parse_usage(&json!({"detail": "nope"}), now()).is_err());
}

#[test]
fn the_login_is_read_without_ever_showing_the_token() {
    let token = jwt(now() + Duration::days(10));
    let text = json!({"tokens": {"access_token": token, "account_id": ACCOUNT}}).to_string();
    let (login, sum) = parse_auth(&text).map_err(|e| e.0).unwrap();
    assert_eq!(
        login.expires_at(),
        Some(
            Utc.timestamp_opt((now() + Duration::days(10)).timestamp(), 0)
                .unwrap()
        )
    );
    assert!(sum.access_token && sum.account_id);
    // an API-key sign-in has no plan limits
    let (why, sum) = parse_auth(r#"{"OPENAI_API_KEY":"sk-SECRET"}"#)
        .err()
        .unwrap();
    assert!(sum.api_key_only);
    assert!(
        why.contains("API key") && !why.contains("sk-SECRET"),
        "{why}"
    );
    assert!(parse_auth("{nope").is_err());
    assert!(parse_auth("{}").is_err());
}

// ------------------------------------------------------------ the session logs

#[test]
fn the_newest_session_file_wins_and_history_is_imported() {
    let h = home();
    let at = Utc::now();
    let reset5 = at + Duration::hours(2);
    let reset7 = at + Duration::days(4);
    write_session(
        &h,
        "2026/10/22",
        "rollout-2026-10-22T10-00-00-a.jsonl",
        &[rl_line(
            at - Duration::hours(30),
            "codex",
            win(80.0, 300, at - Duration::hours(26)),
            win(20.0, 10080, reset7),
        )],
    );
    write_session(
        &h,
        "2026/10/24",
        "rollout-2026-10-24T10-00-00-b.jsonl",
        &[
            r#"{"type":"session_meta","payload":{"id":"s1","cwd":"/home/me/proj"}}"#.to_string(),
            rl_line(
                at - Duration::minutes(20),
                "codex",
                win(5.0, 300, reset5),
                win(25.0, 10080, reset7),
            ),
            rl_line(
                at - Duration::minutes(10),
                "codex_spark",
                win(40.0, 300, reset5),
                win(11.0, 10080, reset7),
            ),
            rl_line(
                at - Duration::minutes(5),
                "codex",
                win(9.0, 300, reset5),
                win(26.0, 10080, reset7),
            ),
        ],
    );
    let store = JsonlStore::new(h.dir.join("usage-codex.jsonl"));
    let rec = log_poller(&h).poll_once(&store).unwrap();
    assert_eq!(rec["vendor"], "codex");
    assert_eq!(rec["source"], "codex-log");
    assert_eq!(rec["session"]["pct"], 9.0);
    assert_eq!(rec["weekly"]["pct"], 26.0);
    assert_eq!(rec["raw"]["tier"], "Plus");
    // the extra bucket is stored with the per-model limits
    let r = Reading::from_value(&rec);
    assert_eq!(r.limits.len(), 2);
    assert_eq!(r.limits[0].name, "GPT-5.3-Codex-Spark");
    assert_eq!(r.credits.as_ref().unwrap().balance, Some(120.0));
    // the older samples came in as history (backfill), the newest is the regular row
    let all = store.read();
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all.iter().filter(|v| v["backfill"] == true).count(), 2);
    // nothing new: the next poll stores nothing
    let again = log_poller(&h).poll_once(&store).unwrap();
    assert_eq!(again["stored"], false);
    assert_eq!(store.read().len(), 3);
}

#[test]
fn stale_ended_and_missing_logs_fail_with_clear_messages() {
    // no sessions folder at all
    let h = home();
    let store = JsonlStore::new(h.dir.join("u.jsonl"));
    let e = log_poller(&h).poll_once(&store).unwrap_err();
    assert!(e.message.contains("run a Codex task once"), "{}", e.message);
    // a folder without usage lines
    write_session(
        &h,
        "2026/10/24",
        "rollout-x.jsonl",
        &[r#"{"type":"session_meta"}"#.into()],
    );
    let e = log_poller(&h).poll_once(&store).unwrap_err();
    assert!(e.message.contains("no usage line"), "{}", e.message);
    // old: three hours
    let at = Utc::now() - Duration::hours(3);
    write_session(
        &h,
        "2026/10/24",
        "rollout-y.jsonl",
        &[rl_line(
            at,
            "codex",
            win(5.0, 300, at + Duration::hours(1)),
            win(25.0, 10080, Utc::now() + Duration::days(2)),
        )],
    );
    let e = log_poller(&h).poll_once(&store).unwrap_err();
    assert!(e.message.contains("stale"), "{}", e.message);
    // a weekly window that has already reset is last week's number
    let h = home();
    let at = Utc::now() - Duration::minutes(5);
    write_session(
        &h,
        "2026/10/24",
        "rollout-z.jsonl",
        &[rl_line(
            at,
            "codex",
            win(5.0, 300, at + Duration::hours(1)),
            win(25.0, 10080, at - Duration::minutes(1)),
        )],
    );
    let e = log_poller(&h)
        .poll_once(&JsonlStore::new(h.dir.join("v.jsonl")))
        .unwrap_err();
    assert!(e.message.contains("weekly window ended"), "{}", e.message);
}

#[test]
fn compressed_and_archived_sessions_are_read() {
    let h = home();
    let at = Utc::now() - Duration::minutes(3);
    let line = rl_line(
        at,
        "codex",
        win(15.0, 300, at + Duration::hours(1)),
        win(35.0, 10080, at + Duration::days(2)),
    );
    let bytes = ruzstd::encoding::compress_to_vec(
        (line + "\n").as_bytes(),
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    std::fs::create_dir_all(h.dir.join("codexhome/archived_sessions")).unwrap();
    std::fs::write(
        h.dir
            .join("codexhome/archived_sessions/rollout-old.jsonl.zst"),
        bytes,
    )
    .unwrap();
    let rec = log_poller(&h)
        .poll_once(&JsonlStore::new(h.dir.join("u.jsonl")))
        .unwrap();
    assert_eq!(rec["weekly"]["pct"], 35.0);
}

// ------------------------------------------------------------ the live endpoint (mock server)

type Seen = Arc<Mutex<Vec<Vec<(String, String)>>>>;

struct Mock {
    url: String,
    status: Arc<AtomicU16>,
    seen: Seen,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Mock {
    fn start(body: Value) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = Arc::new(AtomicU16::new(200));
        let body = Arc::new(body.to_string());
        let seen: Seen = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sn, sp) = (status.clone(), seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(c) = conn {
                    serve(c, &st, &body, &sn);
                }
            }
        });
        Mock {
            url: format!("http://{addr}/backend-api/wham/usage"),
            status,
            seen,
            stop,
            addr,
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(
    mut c: TcpStream,
    status: &AtomicU16,
    body: &str,
    seen: &Mutex<Vec<Vec<(String, String)>>>,
) {
    let mut reader = BufReader::new(c.try_clone().unwrap());
    let mut headers = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    seen.lock().unwrap().push(headers);
    let _ = write!(
        c,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        status.load(Ordering::SeqCst),
        body.len()
    );
}

fn usage_body() -> Value {
    json!({
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": {"used_percent": 2, "limit_window_seconds": 18000, "reset_at": (Utc::now() + Duration::hours(3)).timestamp()},
            "secondary_window": {"used_percent": 1, "limit_window_seconds": 604800, "reset_at": (Utc::now() + Duration::days(6)).timestamp()},
        },
        "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
        "account_id": ACCOUNT,
    })
}

fn live_chain(h: &Home, url: &str) -> Poller {
    let mut live = codex::CodexUsage::new(h.env.clone());
    live.url = url.to_string();
    Poller::with_chain_for(
        vec![
            Box::new(live),
            Box::new(codex::CodexLog { env: h.env.clone() }),
        ],
        "codex",
    )
}

#[test]
fn live_usage_sends_the_login_and_stores_a_token_free_record() {
    let h = home();
    let token = jwt(Utc::now() + Duration::days(9));
    write_auth(&h, &token);
    let mock = Mock::start(usage_body());
    let store = JsonlStore::new(h.dir.join("usage-codex.jsonl"));
    let rec = live_chain(&h, &mock.url).poll_once(&store).unwrap();
    assert_eq!(
        (rec["vendor"].as_str(), rec["source"].as_str()),
        (Some("codex"), Some("codex-usage"))
    );
    assert_eq!(rec["session"]["pct"], 2.0);
    assert_eq!(rec["weekly"]["pct"], 1.0);
    assert_eq!(rec["raw"]["tier"], "Plus");
    let seen = mock.seen.lock().unwrap();
    let h0: std::collections::HashMap<_, _> = seen[0].iter().cloned().collect();
    assert_eq!(h0["authorization"], format!("Bearer {token}"));
    assert_eq!(h0["chatgpt-account-id"], ACCOUNT);
    drop(seen);
    let text = std::fs::read_to_string(h.dir.join("usage-codex.jsonl")).unwrap();
    for secret in [TOKEN_SECRET, ACCOUNT, "rt-SECRET", "id-SECRET"] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
}

#[test]
fn an_expired_or_refused_login_says_open_codex_and_the_log_takes_over() {
    let h = home();
    // expired by its own claim: no request is made at all
    write_auth(&h, &jwt(Utc::now() - Duration::hours(1)));
    let mock = Mock::start(usage_body());
    let store = JsonlStore::new(h.dir.join("u.jsonl"));
    let e = live_chain(&h, &mock.url).poll_once(&store).unwrap_err();
    assert!(e.message.contains("open Codex once"), "{}", e.message);
    assert!(!e.message.contains(TOKEN_SECRET));
    assert!(mock.seen.lock().unwrap().is_empty());
    // refused by the server (401) while a fresh log line exists: the log answers, the failure is noted
    write_auth(&h, &jwt(Utc::now() + Duration::days(3)));
    mock.status.store(401, Ordering::SeqCst);
    let at = Utc::now() - Duration::minutes(2);
    write_session(
        &h,
        "2026/10/24",
        "rollout-a.jsonl",
        &[rl_line(
            at,
            "codex",
            win(33.0, 300, at + Duration::hours(1)),
            win(44.0, 10080, at + Duration::days(2)),
        )],
    );
    let rec = live_chain(&h, &mock.url).poll_once(&store).unwrap();
    assert_eq!(rec["source"], "codex-log");
    assert!(
        rec["note"].as_str().unwrap().contains("open Codex once"),
        "{rec}"
    );
}

// ------------------------------------------------------------ the Codex block

fn reading(v: Value) -> Reading {
    Reading::from_value(&v)
}

fn live_readings(at: DateTime<Utc>, now: DateTime<Utc>) -> Vec<Reading> {
    let reset5 = now + Duration::hours(2);
    let reset7 = now + Duration::days(3);
    (0..4)
        .map(|i| {
            let ts = t(at - Duration::minutes(15 * (3 - i)));
            reading(json!({
                "v": 1, "ts": ts, "observed_at": ts, "ok": true, "vendor": "codex", "source": "codex-usage",
                "session": {"pct": 10.0 + i as f64 * 5.0, "resets_at": t(reset5)},
                "weekly": {"pct": 40.0 + i as f64, "resets_at": t(reset7)},
                "limits": [
                    {"name": "Spark", "kind": "session", "percent": 12.0, "resets_at": t(reset5)},
                    {"name": "Spark", "kind": "weekly", "percent": 6.0, "resets_at": t(reset7)},
                ],
                "raw": {"tier": "Plus", "credits": {"unlimited": false, "has_credits": true, "balance": 120.0}, "banked_resets": 2},
            }))
        })
        .collect()
}

fn codex_home_view(readings: &[Reading], now: DateTime<Utc>) -> aitrack::view::VendorView {
    let mut s = build_state(&[], Values::default(), status(), now);
    s.claude_on = false;
    s.codex = Some(build_codex_state(readings, status(), true, now));
    let home = aitrack::view::build_home(&s, now);
    home.vendors
        .into_iter()
        .find(|v| v.id == VendorId::Codex)
        .expect("a Codex block")
}

fn tip_text(row: &aitrack::view::Row) -> String {
    row.tip
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_codex_block_shows_what_the_claude_block_shows() {
    let now = Utc::now();
    let v = codex_home_view(&live_readings(now, now), now);
    assert_eq!(v.name, "Codex");
    assert_eq!(
        v.rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
        [Kind::Session, Kind::Weekly]
    );
    assert_eq!(v.rows[0].value, "25%");
    assert_eq!(v.rows[1].value, "43%");
    let session = tip_text(&v.rows[0]);
    let weekly = tip_text(&v.rows[1]);
    for want in [
        "Session resets in",
        "Pace:",
        "Plan: Plus",
        "Live from Codex usage",
        "By model",
        "Spark  12%",
    ] {
        assert!(session.contains(want), "{want} missing from\n{session}");
    }
    for want in [
        "Weekly resets in",
        "Plan: Plus",
        "By model",
        "Spark  6%",
        "Credits  120 left",
        "Banked resets  2 available",
    ] {
        assert!(weekly.contains(want), "{want} missing from\n{weekly}");
    }
    // the pace (25 % in 45 min with 2 h left) runs the session out before it resets
    assert!(v.warn.is_some(), "header warning expected");
    // the detail pane has the pace rows and a chart
    let vendor = aitrack::vendor::codex(
        &build_codex_state(&live_readings(now, now), status(), true, now),
        now,
    );
    let d = aitrack::view::build_detail_at(&vendor, Kind::Session, false, now);
    assert_eq!(d.chart, Some(Kind::Session));
    let labels: Vec<&str> = d.rows.iter().map(|(l, _)| l.as_str()).collect();
    for want in ["Pace", "At reset", "To limit", "Windows"] {
        assert!(labels.contains(&want), "{want} missing from {labels:?}");
    }
}

#[test]
fn an_old_codex_reading_stays_dimmed_with_the_age_chip_and_no_pace() {
    let now = Utc::now();
    let mut rs = live_readings(now - Duration::hours(3), now);
    for r in rs.iter_mut() {
        r.source = Some("codex-log".into());
    }
    let v = codex_home_view(&rs, now);
    assert!(v.rows.iter().all(|r| r.dim), "old rows are dimmed");
    assert_eq!(v.rows[0].value, "25%", "the last value stays");
    let chip = v.age_chip.expect("age chip");
    assert!(chip.tooltip.contains("session logs"), "{}", chip.tooltip);
    let weekly = tip_text(&v.rows[1]);
    assert!(!weekly.contains("Pace:"), "{weekly}");
    assert!(
        weekly.contains("Codex figures come from its local session logs"),
        "{weekly}"
    );
}

#[test]
fn a_weekly_only_plan_shows_one_bar() {
    let now = Utc::now();
    let ts = t(now - Duration::minutes(1));
    let r = reading(json!({
        "v": 1, "ts": ts, "observed_at": ts, "ok": true, "source": "codex-usage",
        "session": null, "weekly": {"pct": 12.0, "resets_at": t(now + Duration::days(2))},
    }));
    let v = codex_home_view(&[r], now);
    assert_eq!(v.rows.len(), 1);
    assert_eq!(v.rows[0].kind, Kind::Weekly);
}

#[test]
fn codex_without_a_reading_is_a_placeholder_with_the_reason() {
    let now = Utc::now();
    let mut s = build_state(&[], Values::default(), status(), now);
    s.claude_on = false;
    let mut st = status();
    st.error = Some("Codex login not found: open Codex and sign in".into());
    s.codex = Some(build_codex_state(&[], st, true, now));
    let home = aitrack::view::build_home(&s, now);
    let v = &home.vendors[0];
    assert_eq!(v.id, VendorId::Codex);
    assert!(v.rows.iter().all(|r| r.value == "--"));
    assert!(tip_text(&v.rows[0]).contains("open Codex and sign in"));
    // the status line speaks about Codex when it is the only vendor
    assert!(
        home.status.text.contains("No current data"),
        "{}",
        home.status.text
    );
    // switched off: nothing to track
    s.codex.as_mut().unwrap().enabled = false;
    assert!(s.nothing_to_track());
}

#[test]
fn codex_polls_are_diagnostics_rows() {
    let now = Utc::now();
    let st = build_codex_state(&live_readings(now, now), status(), true, now);
    assert!(
        st.diagnostics[0].text.starts_with("Codex: session"),
        "{}",
        st.diagnostics[0].text
    );
    assert!(st.diagnostics[0].text.contains("codex-usage"));
}

// ------------------------------------------------------------ settings, detection, Stats

#[test]
fn show_codex_follows_detection_unless_set() {
    let v = Values::default();
    assert!(v.codex_on(true) && !v.codex_on(false));
    let dir = TempDir::new();
    let s = aitrack::settings::Settings::load(dir.join("s.json"));
    s.set_codex_track(aitrack::settings::Track::Off);
    assert!(!s.get().codex_on(true));
    let again = aitrack::settings::Settings::load(dir.join("s.json"));
    assert_eq!(again.get().codex_enabled, Some(false));
}

#[test]
fn codex_is_detected_by_its_folder() {
    let h = home();
    assert!(aitrack::detect::codex_present(&h.env));
    let mut e = h.env.clone();
    e.codex_home = Some(h.dir.join("missing"));
    assert!(!aitrack::detect::codex_present(&e));
}

#[test]
fn the_diagnostics_text_has_no_secrets() {
    let h = home();
    write_auth(&h, &jwt(now() + Duration::days(9)));
    let at = Utc::now() - Duration::minutes(2);
    write_session(
        &h,
        "2026/10/24",
        "rollout-a.jsonl",
        &[rl_line(
            at,
            "codex",
            win(3.0, 300, at + Duration::hours(1)),
            Value::Null,
        )],
    );
    let d = codex::diagnose(&h.env, Utc::now());
    assert!(d.contains("ChatGPT token: yes; account id: yes"), "{d}");
    assert!(d.contains("usage lines: 1"), "{d}");
    for secret in [TOKEN_SECRET, ACCOUNT, "rt-SECRET"] {
        assert!(!d.contains(secret), "{secret} in\n{d}");
    }
}

#[test]
fn codex_sessions_feed_the_projects_view_without_double_counting() {
    use aitrack::pricing::PriceTable;
    use aitrack::transcripts::{aggregate, ProjRange, Roots, Scanner};
    let h = home();
    let at = Utc::now() - Duration::hours(1);
    let tc = |i: i64, input: u64, cached: u64, output: u64| {
        json!({"timestamp": t(at + Duration::minutes(i)), "type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": {"input_tokens": input, "cached_input_tokens": cached, "output_tokens": output, "reasoning_output_tokens": 10, "total_tokens": input + output},
                     "last_token_usage": {}}, "rate_limits": null}}).to_string()
    };
    write_session(
        &h,
        "2026/10/24",
        "rollout-p.jsonl",
        &[
            json!({"timestamp": t(at), "type": "session_meta", "payload": {"id": "sess-1", "cwd": "C:\\Users\\me\\aiTrack"}}).to_string(),
            json!({"timestamp": t(at), "type": "turn_context", "payload": {"model": "gpt-5-codex", "cwd": "C:\\Users\\me\\aiTrack"}}).to_string(),
            tc(1, 1000, 200, 100),
            tc(2, 1000, 200, 100), // a repeated line adds nothing
            tc(3, 3000, 1200, 300),
        ],
    );
    let roots = Roots {
        claude_projects: h.dir.join("claude"),
        grok_home: h.dir.join("grok"),
        codex_home: h.dir.join("codexhome"),
        index: None,
    };
    let mut sc = Scanner::new(roots);
    sc.scan();
    let report = aggregate(sc.requests(), &PriceTable::fallback(), Utc::now(), 0);
    let r = report.range(ProjRange::Days7);
    assert_eq!(r.projects.len(), 1);
    let p = &r.projects[0];
    assert_eq!((p.name.as_str(), p.vendor), ("aiTrack", VendorId::Codex));
    assert_eq!(p.messages, 2);
    // 3000 input (1200 of it cached) and 300 output in all
    assert_eq!(p.tokens.input, 1800);
    assert_eq!(p.tokens.cache_read, 1200);
    assert_eq!(p.tokens.output, 300);
    // gpt-5 list prices: 1.25 / 10 / 0.125 per million
    let want = (1800.0 * 1.25 + 300.0 * 10.0 + 1200.0 * 0.125) / 1e6;
    assert!((p.cost - want).abs() < 1e-9, "{} vs {want}", p.cost);
    assert!(sc
        .sources()
        .iter()
        .any(|s| s.label == "Codex sessions" && s.files == 1));
}

#[test]
fn stats_has_codex_session_and_weekly_blocks() {
    use aitrack::stats::{compute, StatsRange};
    use aitrack::statsview::{build_page, PlanData, Snapshot};
    let now = Utc::now();
    let rs = live_readings(now, now);
    let plan = PlanData {
        claude: vec![],
        grok: None,
        grok_bot: None,
        codex: Some(
            StatsRange::ALL
                .into_iter()
                .map(|k| (k, compute(&rs, k, now, 0)))
                .collect(),
        ),
    };
    let snap = Snapshot {
        plan: Some(Arc::new(plan)),
        ..Default::default()
    };
    let page = build_page(
        &snap,
        aitrack::transcripts::ProjRange::Days7,
        StatsRange::ALL[0],
        now,
    );
    let titles: Vec<&str> = page.plan.iter().map(|b| b.title.as_str()).collect();
    assert_eq!(titles, ["Codex session (5 h)", "Codex weekly (7 d)"]);
    assert!(page.plan.iter().all(|b| b.vendor == VendorId::Codex));
}
