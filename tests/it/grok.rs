// SPDX-License-Identifier: GPL-3.0-or-later
//! Grok weekly usage: the CLI log, the auth.json login, the live billing endpoint (mock server) and
//! the chain between them, all through `Poller` / the real binary. Tokens and ids must never leak.
use crate::common;
use aitrack::grok::{self, parse_billing, parse_snapshot, select_session};
use aitrack::live::Poller;
use aitrack::sources::{Env, Os};
use aitrack::store::{JsonlStore, Store};
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "xai-TOKEN-SECRET-abc123";
const USER: &str = "user-SECRET-42";
const OTHER_TOKEN: &str = "corp-OIDC-TOKEN-SECRET";
const API_KEY: &str = "xai-APIKEY-SECRET-777";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn t(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn billing_line(
    fetched: DateTime<Utc>,
    pct: Option<f64>,
    period_end: DateTime<Utc>,
    tier: Option<&str>,
) -> String {
    let mut config =
        json!({"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "end": t(period_end)}});
    if let Some(p) = pct {
        config["creditUsagePercent"] = json!(p);
    }
    let mut ctx = json!({"config": config});
    if let Some(tier) = tier {
        ctx["subscriptionTier"] = json!(tier);
    }
    json!({"ts": t(fetched), "level": "info", "msg": "billing: fetched credits config", "ctx": ctx})
        .to_string()
}

fn noise(i: usize) -> String {
    json!({"ts": t(now()), "level": "debug", "msg": "something else entirely", "ctx": {"i": i, "pad": "x".repeat(200)}}).to_string()
}

struct Home {
    dir: TempDir,
    env: Env,
}

fn home() -> Home {
    let dir = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.grok_home = Some(dir.join("grokhome"));
    std::fs::create_dir_all(dir.join("grokhome/logs")).unwrap();
    Home { dir, env }
}

fn write_log(h: &Home, lines: &[String]) {
    std::fs::write(
        h.dir.join("grokhome/logs/unified.jsonl"),
        lines.join("\n") + "\n",
    )
    .unwrap();
}

fn write_auth(h: &Home, v: &Value) {
    std::fs::write(h.dir.join("grokhome/auth.json"), v.to_string()).unwrap();
}

fn auth_ok(expires: DateTime<Utc>) -> Value {
    json!({
        "https://auth.x.ai::client-1": {"key": TOKEN, "user_id": USER, "refresh_token": "rt-SECRET", "expires_at": t(expires)},
    })
}

fn log_poller(h: &Home) -> Poller {
    Poller::with_chain_for(vec![Box::new(grok::GrokLog { env: h.env.clone() })], "grok")
}

// ------------------------------------------------------------ the log

#[test]
fn snapshot_lines_are_parsed_strictly() {
    let end = now() + Duration::days(4);
    let s =
        parse_snapshot(billing_line(now(), Some(7.0), end, Some("SuperGrok")).as_bytes()).unwrap();
    assert_eq!((s.pct, s.tier.as_deref()), (7.0, Some("SuperGrok")));
    assert_eq!(s.period_end, end);
    // exactly zero usage: the field is absent
    let z = parse_snapshot(billing_line(now(), None, end, None).as_bytes()).unwrap();
    assert_eq!((z.pct, z.tier), (0.0, None));
    for bad in [
        "not json at all".to_string(),
        "{\"msg\": \"billing: fetched credits config\"}".to_string(),
        json!({"ts": t(now()), "msg": "other", "ctx": {}}).to_string(),
        json!({"ts": "yesterday", "msg": "billing: fetched credits config", "ctx": {"config": {"creditUsagePercent": 3, "currentPeriod": {"end": t(end)}}}}).to_string(),
        billing_line(now(), Some(140.0), end, None),
        billing_line(now(), Some(-1.0), end, None),
        json!({"ts": t(now()), "msg": "billing: fetched credits config", "ctx": {"config": {"creditUsagePercent": "7", "currentPeriod": {"end": t(end)}}}}).to_string(),
        json!({"ts": t(now()), "msg": "billing: fetched credits config", "ctx": {"config": {"creditUsagePercent": 7}}}).to_string(),
        "{\"msg\":".to_string(),
    ] {
        assert!(parse_snapshot(bad.as_bytes()).is_none(), "{bad}");
    }
    assert!(parse_snapshot(&[0xff, 0xfe, 0x00]).is_none());
}

#[test]
fn the_newest_snapshot_wins_and_garbage_lines_are_skipped() {
    let h = home();
    let end = now() + Duration::days(5);
    write_log(
        &h,
        &[
            billing_line(now() - Duration::hours(30), Some(3.0), end, None),
            "this line is not json".into(),
            noise(1),
            billing_line(now() - Duration::minutes(10), Some(7.0), end, Some("Heavy")),
            "{\"ts\": \"broken".into(),
            noise(2),
        ],
    );
    let snaps = grok::read_snapshots(&grok::log_path(&h.env)).unwrap();
    assert_eq!(snaps.iter().map(|s| s.pct).collect::<Vec<_>>(), [7.0, 3.0]);
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let rec = log_poller(&h).poll_at(&store, now()).unwrap();
    assert_eq!(
        (rec["vendor"].as_str(), rec["source"].as_str()),
        (Some("grok"), Some("grok-log"))
    );
    assert_eq!(rec["weekly"]["pct"], 7.0);
    assert!(rec["session"].is_null());
    assert_eq!(
        rec["weekly"]["resets_at"],
        end.to_rfc3339_opts(SecondsFormat::Secs, false)
    );
    assert_eq!(rec["raw"]["tier"], "Heavy");
    // the older snapshot was backfilled as history; nothing is duplicated on the next poll
    assert_eq!(store.read().len(), 2);
    assert_eq!(
        log_poller(&h).poll_at(&store, now()).unwrap()["stored"],
        false
    );
    assert_eq!(store.read().len(), 2);
}

#[test]
fn a_huge_log_is_only_read_from_its_end() {
    let h = home();
    let end = now() + Duration::days(3);
    let mut lines = vec![billing_line(
        now() - Duration::days(2),
        Some(1.0),
        end,
        None,
    )]; // far beyond the 4 MB window
    lines.extend((0..30_000).map(noise)); // ~7 MB of other lines
    lines.push(billing_line(
        now() - Duration::minutes(5),
        Some(9.0),
        end,
        None,
    ));
    lines.push(noise(99));
    write_log(&h, &lines);
    assert!(std::fs::metadata(grok::log_path(&h.env)).unwrap().len() > 6_000_000);
    let start = std::time::Instant::now();
    let snaps = grok::read_snapshots(&grok::log_path(&h.env)).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(snaps.iter().map(|s| s.pct).collect::<Vec<_>>(), [9.0]); // the old one is out of reach
}

#[test]
fn a_log_without_a_trailing_newline_and_a_tiny_log_work() {
    let h = home();
    let end = now() + Duration::days(3);
    std::fs::write(
        grok::log_path(&h.env),
        billing_line(now(), Some(4.0), end, None),
    )
    .unwrap(); // no newline at all
    assert_eq!(
        grok::read_snapshots(&grok::log_path(&h.env)).unwrap().len(),
        1
    );
    std::fs::write(grok::log_path(&h.env), "").unwrap();
    assert!(grok::read_snapshots(&grok::log_path(&h.env))
        .unwrap()
        .is_empty());
}

#[test]
fn zero_percent_line_is_a_real_zero_reading() {
    let h = home();
    write_log(
        &h,
        &[billing_line(
            now() - Duration::minutes(2),
            None,
            now() + Duration::days(7),
            None,
        )],
    );
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let rec = log_poller(&h).poll_at(&store, now()).unwrap();
    assert_eq!(rec["weekly"]["pct"], 0.0);
}

#[test]
fn stale_ended_missing_and_empty_logs_fail_soft_with_clear_messages() {
    let h = home();
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let end = now() + Duration::days(3);
    // snapshot from 3 hours ago: too old to be current (but kept as history)
    write_log(
        &h,
        &[billing_line(
            now() - Duration::hours(3),
            Some(7.0),
            end,
            None,
        )],
    );
    let e = log_poller(&h).poll_at(&store, now()).unwrap_err();
    assert!(
        e.message
            .contains("Grok CLI log is stale (last billing fetch 3 h ago): run `grok`"),
        "{e}"
    );
    // fresh snapshot of a period that has ended
    write_log(
        &h,
        &[billing_line(
            now() - Duration::minutes(5),
            Some(7.0),
            now() - Duration::minutes(1),
            None,
        )],
    );
    let e = log_poller(&h).poll_at(&store, now()).unwrap_err();
    assert!(
        e.message.contains("billing period ended") && e.message.contains("7%"),
        "{e}"
    );
    // no snapshot at all, then no file
    write_log(&h, &[noise(1)]);
    assert!(log_poller(&h)
        .poll_at(&store, now())
        .unwrap_err()
        .message
        .contains("no billing snapshot"));
    std::fs::remove_file(grok::log_path(&h.env)).unwrap();
    assert!(log_poller(&h)
        .poll_at(&store, now())
        .unwrap_err()
        .message
        .contains("log not found"));
    // every failure left one error row (vendor-tagged) next to the kept history
    assert!(store
        .read()
        .iter()
        .filter(|r| r["ok"] == false)
        .all(|r| r["vendor"] == "grok"));
}

// ------------------------------------------------------------ auth.json

#[test]
fn only_xai_sign_ins_are_used_never_corporate_tokens_or_api_keys() {
    let soon = now() + Duration::hours(2);
    let later = now() + Duration::hours(9);
    let auth = json!({
        "https://corp.example.com::oidc-client": {"key": OTHER_TOKEN, "expires_at": t(later + Duration::days(30))},
        "https://api.x.ai/api-key": {"key": API_KEY},
        "https://auth.x.ai::client-a": {"key": "xai-a", "user_id": "u-a", "expires_at": t(soon)},
        "https://auth.x.ai::client-b": {"key": "xai-b", "user_id": "u-b", "refresh_token": "r", "expires_at": t(later)},
        "https://accounts.x.ai/sign-in": {"key": "xai-legacy", "expires_at": t(now() - Duration::days(1))},
        "https://auth.x.ai::": {"key": "xai-empty-client"},
    })
    .to_string();
    let (_, sum) = select_session(&auth, now()).ok().unwrap();
    assert_eq!(
        (sum.entries, sum.xai_signins, sum.other, sum.expired_xai),
        (6, 3, 3, 1)
    );
    // no xAI entry at all: an error that names no secret
    let only_other =
        json!({"https://corp.example.com::c": {"key": OTHER_TOKEN}, "k": {"key": API_KEY}})
            .to_string();
    let (why, sum) = select_session(&only_other, now()).err().unwrap();
    assert!(
        why.contains("no xAI sign-in") && !why.contains("SECRET"),
        "{why}"
    );
    assert_eq!((sum.entries, sum.xai_signins), (2, 0));
    assert!(select_session("[1,2]", now()).is_err());
    assert!(select_session("{broken", now()).is_err());
}

// ------------------------------------------------------------ the live endpoint (mock server)

type Seen = Arc<Mutex<Vec<Vec<(String, String)>>>>;

struct Mock {
    url: String,
    status: Arc<AtomicU16>,
    body: Arc<Mutex<String>>,
    seen: Seen,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Mock {
    fn start(body: Value) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = Arc::new(AtomicU16::new(200));
        let body = Arc::new(Mutex::new(body.to_string()));
        let seen: Seen = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, bd, sn, sp) = (status.clone(), body.clone(), seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(c) = conn {
                    serve(c, &st, &bd, &sn);
                }
            }
        });
        Mock {
            url: format!("http://{addr}/v1/billing?format=credits"),
            status,
            body,
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
    body: &Mutex<String>,
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
    let body = body.lock().unwrap().clone();
    let _ = write!(
        c,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        status.load(Ordering::SeqCst),
        body.len()
    );
}

/// The billing response shape of the CLI proxy (protobuf JSON: zero values are omitted).
fn billing_body(pct: Option<f64>, end: DateTime<Utc>) -> Value {
    let mut config = json!({
        "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "end": t(end)},
        "prepaidBalance": {"val": "1250"}, "onDemandCap": {"val": 5000},
        "productUsage": [{"product": "Grok Build", "usagePercent": 5.5}, {"product": "Chat", "usagePercent": 1.5}],
    });
    if let Some(p) = pct {
        config["creditUsagePercent"] = json!(p);
    }
    json!({"config": config, "onDemandEnabled": false, "subscriptionTier": "SuperGrok Heavy"})
}

fn live_chain(h: &Home, mock_url: &str) -> Poller {
    let mut billing = grok::GrokBilling::new(h.env.clone());
    billing.url = mock_url.to_string();
    Poller::with_chain_for(
        vec![
            Box::new(billing),
            Box::new(grok::GrokLog { env: h.env.clone() }),
        ],
        "grok",
    )
}

#[test]
fn live_billing_sends_the_cli_headers_and_stores_a_token_free_record() {
    let h = home();
    let end = Utc::now() + Duration::days(5);
    write_auth(&h, &auth_ok(Utc::now() + Duration::hours(6)));
    let mock = Mock::start(billing_body(Some(12.5), end));
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let rec = live_chain(&h, &mock.url).poll_once(&store).unwrap();
    assert_eq!(
        (rec["vendor"].as_str(), rec["source"].as_str()),
        (Some("grok"), Some("grok-billing"))
    );
    assert_eq!(rec["weekly"]["pct"], 12.5);
    assert_eq!(rec["raw"]["tier"], "SuperGrok Heavy");
    assert!(rec["weekly"]["resets_at"].as_str().is_some());
    // the headers the CLI proxy expects
    let seen = mock.seen.lock().unwrap();
    let h0: std::collections::HashMap<_, _> = seen[0].iter().cloned().collect();
    assert_eq!(h0["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(h0["x-xai-token-auth"], "xai-grok-cli");
    assert_eq!(h0["x-grok-client-mode"], "cli");
    assert_eq!(h0["x-grok-client-version"], "1.0.0");
    assert_eq!(h0["x-userid"], USER);
    drop(seen);
    // nothing secret in the stored record (or the file)
    let text = std::fs::read_to_string(h.dir.join("usage-grok.jsonl")).unwrap();
    for secret in [TOKEN, USER, "rt-SECRET", "client-1", "prepaid", "1250"] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
    // a live source adds a row every poll (no de-duplication)
    live_chain(&h, &mock.url).poll_once(&store).unwrap();
    assert_eq!(store.read().len(), 2);
}

#[test]
fn billing_response_variants() {
    let end = now() + Duration::days(6);
    // zero usage: the percent is omitted but the period is complete
    let s = parse_billing(&billing_body(None, end), now()).unwrap();
    assert_eq!(s.weekly.as_ref().unwrap().pct, 0.0);
    // numeric (seconds or milliseconds) period end
    for n in [end.timestamp() as f64, end.timestamp_millis() as f64] {
        let b = json!({"config": {"creditUsagePercent": 4.0, "currentPeriod": {"end": n}}});
        let s = parse_billing(&b, now()).unwrap();
        assert_eq!(
            s.weekly.unwrap().resets_at.as_deref(),
            Some(end.to_rfc3339_opts(SecondsFormat::Secs, false).as_str())
        );
    }
    // clamped, tier optional
    let s = parse_billing(
        &json!({"config": {"creditUsagePercent": 140.0, "currentPeriod": {"end": t(end)}}}),
        now(),
    )
    .unwrap();
    assert_eq!(s.weekly.unwrap().pct, 100.0);
    // nothing usable
    for bad in [
        json!({}),
        json!({"config": 5}),
        json!({"config": {}}),
        json!({"config": {"creditUsagePercent": "x"}}),
    ] {
        assert!(parse_billing(&bad, now()).is_err(), "{bad}");
    }
}

#[test]
fn an_expired_login_makes_no_request_and_says_what_to_do() {
    let h = home();
    write_auth(&h, &auth_ok(Utc::now() - Duration::hours(1)));
    let mock = Mock::start(billing_body(Some(1.0), Utc::now() + Duration::days(1)));
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let e = live_chain(&h, &mock.url).poll_once(&store).unwrap_err();
    assert!(
        e.message.contains("Grok login expired: run `grok` once"),
        "{e}"
    );
    assert!(
        mock.seen.lock().unwrap().is_empty(),
        "an expired token must not be sent"
    );
    assert!(!e.message.contains(TOKEN));
}

#[test]
fn rejected_tokens_http_errors_and_garbage_fall_through_to_the_log() {
    let h = home();
    let end = Utc::now() + Duration::days(4);
    write_auth(&h, &auth_ok(Utc::now() + Duration::hours(6)));
    write_log(
        &h,
        &[billing_line(
            Utc::now() - Duration::minutes(20),
            Some(7.0),
            end,
            Some("Heavy"),
        )],
    );
    let mock = Mock::start(billing_body(Some(50.0), end));
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));

    mock.status.store(401, Ordering::SeqCst);
    let rec = live_chain(&h, &mock.url).poll_once(&store).unwrap();
    assert_eq!(
        (rec["source"].as_str(), rec["weekly"]["pct"].as_f64()),
        (Some("grok-log"), Some(7.0))
    );
    mock.status.store(500, Ordering::SeqCst);
    assert_eq!(
        live_chain(&h, &mock.url).poll_once(&store).unwrap()["source"],
        "grok-log"
    );
    mock.status.store(200, Ordering::SeqCst);
    *mock.body.lock().unwrap() = "<html>nope</html>".into();
    assert_eq!(
        live_chain(&h, &mock.url).poll_once(&store).unwrap()["source"],
        "grok-log"
    );
    *mock.body.lock().unwrap() = json!({"unexpected": true}).to_string();
    assert_eq!(
        live_chain(&h, &mock.url).poll_once(&store).unwrap()["source"],
        "grok-log"
    );
    // the log reading was stored once, not four times
    assert_eq!(
        store
            .read()
            .iter()
            .filter(|r| r["source"] == "grok-log")
            .count(),
        1
    );

    // both fail: one combined message, no secret in it
    std::fs::remove_file(grok::log_path(&h.env)).unwrap();
    mock.status.store(403, Ordering::SeqCst);
    let e = live_chain(&h, &mock.url).poll_once(&store).unwrap_err();
    assert!(
        e.message.contains("Grok login expired") && e.message.contains("log not found"),
        "{e}"
    );
    assert!(!e.message.contains(TOKEN) && !e.message.contains(USER));
    // every failed live poll left a row (the four that the log covered for, and this combined one),
    // so Diagnostics shows why the live data stopped updating
    let failed: Vec<String> = store
        .read()
        .iter()
        .filter(|r| r["ok"] == false)
        .map(|r| r["error"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(failed.len(), 5, "{failed:?}");
    assert!(
        failed[0].starts_with("grok-billing: Grok login expired"),
        "{failed:?}"
    );
    assert!(failed
        .iter()
        .all(|m| !m.contains(TOKEN) && !m.contains(USER)));
}

#[test]
fn the_local_tick_reads_only_the_log_and_never_the_network() {
    let h = home();
    write_auth(&h, &auth_ok(Utc::now() + Duration::hours(6)));
    write_log(
        &h,
        &[billing_line(
            Utc::now() - Duration::minutes(3),
            Some(7.0),
            Utc::now() + Duration::days(4),
            None,
        )],
    );
    let mock = Mock::start(billing_body(Some(50.0), Utc::now() + Duration::days(4)));
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let p = live_chain(&h, &mock.url);
    let rec = p.poll_local(&store).unwrap();
    assert_eq!(rec["source"], "grok-log");
    assert!(mock.seen.lock().unwrap().is_empty());
    // with nothing current in the log the local tick fails quietly: no error row
    write_log(&h, &[noise(1)]);
    assert!(p.poll_local(&store).is_err());
    assert_eq!(store.read().iter().filter(|r| r["ok"] == false).count(), 0);
}

#[test]
fn grok_home_resolution() {
    let mut env = Env::new(Os::Linux);
    env.home = Some("/u/me".into());
    assert!(grok::grok_home(&env)
        .to_string_lossy()
        .replace('\\', "/")
        .ends_with("/u/me/.grok"));
    env.grok_home = Some("/custom/grok".into());
    assert!(grok::log_path(&env)
        .to_string_lossy()
        .replace('\\', "/")
        .ends_with("/custom/grok/logs/unified.jsonl"));
    assert!(grok::auth_path(&env)
        .to_string_lossy()
        .replace('\\', "/")
        .ends_with("/custom/grok/auth.json"));
    assert!(!grok::detected(&env));
}

// ------------------------------------------------------------ privacy of --sources (library and real binary)

fn secret_free(out: &str) {
    for secret in [
        TOKEN,
        USER,
        OTHER_TOKEN,
        API_KEY,
        "rt-SECRET",
        "client-1",
        "client-a",
    ] {
        assert!(!out.contains(secret), "{secret} leaked into:\n{out}");
    }
}

fn fill_home(h: &Home) {
    let end = Utc::now() + Duration::days(4);
    write_auth(
        h,
        &json!({
            "https://auth.x.ai::client-1": {"key": TOKEN, "user_id": USER, "refresh_token": "rt-SECRET", "expires_at": t(Utc::now() + Duration::hours(5))},
            "https://corp.example.com::oidc": {"key": OTHER_TOKEN},
            "api": {"key": API_KEY},
        }),
    );
    write_log(
        h,
        &[
            noise(1),
            billing_line(
                Utc::now() - Duration::minutes(12),
                Some(7.0),
                end,
                Some("SuperGrok Heavy"),
            ),
        ],
    );
}

#[test]
fn diagnose_describes_structure_without_secrets() {
    let h = home();
    fill_home(&h);
    let out = grok::diagnose(&h.env, Utc::now());
    for want in [
        "grok home:",
        "[exists]",
        "auth.json: [exists ]",
        "entries: 3 (xAI sign-in: 1, other logins/keys ignored: 2, xAI expired: 0)",
        "unified.jsonl: [exists ]",
        "billing snapshots in the last 4 MB: 1",
        "newest snapshot: 12 min ago, 7% used",
        "period end: in 4.0 days",
        "subscription tier known: yes",
        "current: yes",
        "live billing endpoint: not called",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    secret_free(&out);
    assert!(!out.contains("SuperGrok")); // the tier itself is not printed either
                                         // a home that does not exist
    let mut env = Env::new(Os::Linux);
    env.grok_home = Some(h.dir.join("nothing-here"));
    let out = grok::diagnose(&env, Utc::now());
    assert!(
        out.contains("[missing]") && out.contains("auth.json: [missing]"),
        "{out}"
    );
}

#[test]
fn the_real_binary_prints_a_grok_section_and_no_secrets() {
    let h = home();
    fill_home(&h);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aitrack"))
        .arg("--sources")
        .env("GROK_HOME", h.dir.join("grokhome"))
        .env("HOME", h.dir.path())
        .env("USERPROFILE", h.dir.path())
        .env("XDG_CONFIG_HOME", h.dir.path())
        .env("APPDATA", h.dir.path())
        .env("LOCALAPPDATA", h.dir.path())
        .env("CLAUDE_CONFIG_DIR", h.dir.join("cfg"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Grok (weekly credits") && text.contains("newest snapshot: 1"),
        "{text}"
    );
    secret_free(&text);
    secret_free(&String::from_utf8_lossy(&out.stderr));
}

/// The exact shapes seen on the user's PC: `ts` with 3 fractional digits and a `Z`, the period end
/// with SIX fractional digits and an explicit `+00:00`, and a percent that is a JSON number with a
/// decimal point.
#[test]
fn the_users_real_log_line_shapes_parse_and_show_resets_in_1d_22h() {
    use aitrack::analysis::analyze;
    use aitrack::collector::Status;
    use aitrack::reading::Reading;
    use aitrack::settings::Values;
    use aitrack::state::build_grok_state;

    let line = r#"{"ts":"2026-10-14T09:15:31.334Z","level":"info","msg":"billing: fetched credits config","ctx":{"config":{"creditUsagePercent":7.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-16T07:41:38.713088+00:00"}}}}"#;
    let s = parse_snapshot(line.as_bytes()).expect("the real line parses");
    assert_eq!(s.pct, 7.0);
    assert_eq!(
        s.period_end,
        Utc.with_ymd_and_hms(2026, 10, 16, 7, 41, 38).unwrap() + Duration::microseconds(713_088)
    );
    assert_eq!(
        s.fetched_at,
        Utc.with_ymd_and_hms(2026, 10, 14, 9, 15, 31).unwrap() + Duration::milliseconds(334)
    );

    // through the real chain: log -> store -> analysis -> view, with "now" = 2026-10-14T09:30Z
    let h = home();
    std::fs::write(grok::log_path(&h.env), format!("{line}\n")).unwrap();
    let at = Utc.with_ymd_and_hms(2026, 10, 14, 9, 30, 0).unwrap();
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let rec = log_poller(&h)
        .poll_at(&store, at)
        .expect("fresh enough: 15 minutes old");
    assert_eq!(rec["weekly"]["pct"], 7.0);
    let readings: Vec<Reading> = store.read().iter().map(Reading::from_value).collect();
    let plan = analyze(&readings, at).unwrap();
    let g = plan.weekly.as_ref().unwrap();
    assert!(
        (g.hours_left.unwrap() - 46.185).abs() < 0.01,
        "{:?}",
        g.hours_left
    ); // 1d 22h 11m
    let st = build_grok_state(
        &readings,
        Status {
            error: None,
            polling: false,
            next_in: None,
            polls: 1,
            live: false,
            sample: None,
        },
        true,
        at,
    );
    let mut state = aitrack::state::build_state(&[], Values::default(), st.collector.clone(), at);
    state.grok = Some(st);
    let home = aitrack::view::build_home(&state, at);
    let row = &home.vendors[0].rows[0];
    assert_eq!(row.value, "7%");
    assert!(
        row.tip[0].text.starts_with("Weekly resets in 1d 22h"),
        "{:?}",
        row.tip
    );
}

#[test]
fn a_mac_looks_in_homebrew_and_the_per_user_folders_because_a_gui_app_has_a_minimal_path() {
    use aitrack::grok_cli;
    use std::path::PathBuf;
    let t = TempDir::new();
    let mut env = Env::new(Os::Mac);
    env.home = Some(t.join("home"));
    env.grok_home = Some(t.join("gh"));
    let dirs = grok_cli::extra_dirs(&env);
    for d in ["/opt/homebrew/bin", "/usr/local/bin"] {
        assert!(dirs.contains(&PathBuf::from(d)), "{d}");
    }
    assert!(dirs.contains(&t.join("home/.grok/bin")));
    assert!(dirs.contains(&t.join("home/.local/bin")));
    // Linux does not get the Homebrew folders
    env.os = Os::Linux;
    assert!(!grok_cli::extra_dirs(&env).contains(&PathBuf::from("/opt/homebrew/bin")));
    // the Mac executable name is plain `grok`
    let exe = t.join("home/.grok/bin/grok");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "").unwrap();
    env.os = Os::Mac;
    assert_eq!(grok_cli::find_cli(&env, None, None), Some(exe));
}

#[test]
fn an_expired_login_is_noticed_for_the_refresh_message() {
    let h = home();
    // no auth.json: not "expired" (that has its own message)
    assert!(!grok::login_expired(&h.env, now()));
    write_auth(&h, &auth_ok(now() + Duration::hours(2)));
    assert!(!grok::login_expired(&h.env, now()));
    write_auth(&h, &auth_ok(now() - Duration::minutes(1)));
    assert!(grok::login_expired(&h.env, now()));
    let m = aitrack::grok_cli::with_login_hint(aitrack::grok_cli::NO_NEW_DATA);
    assert!(m.starts_with("Grok login expired"), "{m}");
    assert!(
        m.contains("sign in") && m.contains(aitrack::grok_cli::NO_NEW_DATA),
        "{m}"
    );
}
