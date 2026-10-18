// SPDX-License-Identifier: GPL-3.0-or-later
//! Regression tests for the code-review fixes (one section per finding). Every test takes an
//! explicit `now`; nothing here depends on the wall clock.
use crate::common;
use aitrack::app::App;
use aitrack::settings::Settings;
use aitrack::store::{JsonlStore, Store};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < StdDuration::from_secs(30), "timed out");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}

// ---- #16: a busy Grok history lock keeps the previous Grok panel --------------------------

#[test]
fn a_busy_grok_history_lock_keeps_the_previous_grok_state() {
    let t = TempDir::new();
    let g = JsonlStore::new(t.join("g.jsonl"));
    g.append(
        &json!({"ok": true, "vendor": "grok", "source": "grok-billing",
        "ts": (Utc::now() - Duration::minutes(1)).to_rfc3339(),
        "weekly": {"pct": 8.0, "resets_at": (Utc::now() + Duration::days(3)).to_rfc3339()}}),
    )
    .unwrap();
    let app = Arc::new(
        App::with_poll_fn(
            Box::new(JsonlStore::new(t.join("u.jsonl"))),
            Arc::new(Settings::load(t.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        )
        .with_grok(
            Box::new(g),
            Box::new(|_| Ok(Value::Null)),
            None,
            Arc::new(|| true),
        ),
    );
    app.start();
    wait_for(|| !app.state().loading);
    wait_for(|| app.state().grok.is_some_and(|g| g.plan.is_some()));
    // force a rebuild (the Claude history changes), with the Grok history locked meanwhile
    app.history
        .append(&json!({"ok": false, "ts": Utc::now().to_rfc3339(), "error": "x"}))
        .unwrap();
    let during = app
        .grok
        .as_ref()
        .unwrap()
        .history
        .with_readings(|_| app.state());
    assert!(
        during.grok.as_ref().unwrap().plan.is_some(),
        "the Grok panel must not turn into a placeholder while the lock is busy"
    );
    // and once the lock is free the real state is rebuilt
    assert!(app.state().grok.unwrap().plan.is_some());
    app.stop();
    app.collector.join();
}

// ---- #17: cross-file de-duplication does not depend on input order ---------------------------

#[test]
fn a_duplicate_with_an_equal_timestamp_keeps_the_fuller_copy_in_either_order() {
    use aitrack::pricing::{Paths, PriceTable, TokenUsage};
    use aitrack::transcripts::{aggregate, Origin, ProjRange, Request};
    let t = TempDir::new();
    std::fs::write(
        t.join("pricing-overrides.json"),
        r#"{"default": {"input": 1, "output": 2, "cache_read": 0.5, "cache_write": 1.5, "cache_write_1h": 3}}"#,
    )
    .unwrap();
    let prices = PriceTable::load(&Paths::in_dir(t.path()), now(), false);
    let req = |output: u64| Request {
        at: (now() - Duration::hours(1)).timestamp(),
        origin: Origin::Claude,
        project: "p".into(),
        session: "s".into(),
        model: "claude-x".into(),
        usage: TokenUsage {
            input: 100,
            output,
            ..Default::default()
        },
        cost_ticks: None,
        sidechain: false,
        key: 77,
    };
    let (partial, fin) = (req(10), req(500));
    let tokens = |order: [&Request; 2]| {
        let r = aggregate(order.into_iter(), &prices, now(), 0);
        let range = r.range(ProjRange::All);
        range.projects.iter().map(|p| p.tokens.total()).sum::<u64>()
    };
    assert_eq!(tokens([&partial, &fin]), 600);
    assert_eq!(tokens([&fin, &partial]), 600);
}

// ---- #18: an older sample imported after the newest one is backfill, never a live poll -------

mod grok_fixture {
    use super::*;
    pub fn line(fetched: DateTime<Utc>, pct: f64) -> String {
        json!({"ts": fetched.to_rfc3339(), "level": "info", "msg": "billing: fetched credits config",
            "ctx": {"config": {"creditUsagePercent": pct, "currentPeriod": {"end": (now() + Duration::days(3)).to_rfc3339()}}}})
        .to_string()
    }
    pub fn env(t: &TempDir) -> aitrack::sources::Env {
        let mut env = aitrack::sources::Env::new(aitrack::sources::Os::Linux);
        env.home = Some(t.join("home"));
        env.grok_home = Some(t.join("gh"));
        std::fs::create_dir_all(t.join("gh/logs")).unwrap();
        env
    }
    pub fn write(env: &aitrack::sources::Env, lines: &[String]) {
        std::fs::write(aitrack::grok::log_path(env), lines.join("\n") + "\n").unwrap();
    }
}

#[test]
fn older_history_imported_after_the_newest_is_flagged_backfill() {
    use aitrack::live::Poller;
    let t = TempDir::new();
    let env = grok_fixture::env(&t);
    let store = JsonlStore::new(t.join("g.jsonl"));
    let poller = || {
        Poller::with_chain_for(
            vec![Box::new(aitrack::grok::GrokLog { env: env.clone() })],
            "grok",
        )
    };
    let at = |m: i64| now() - Duration::minutes(m);
    // first only the newest sample exists (it is stored as the live reading) ...
    grok_fixture::write(&env, &[grok_fixture::line(at(1), 9.0)]);
    poller().poll_at(&store, now()).unwrap();
    // ... later two older ones show up in the log (an upgrade, a partial import)
    grok_fixture::write(
        &env,
        &[
            grok_fixture::line(at(30), 7.0),
            grok_fixture::line(at(20), 8.0),
            grok_fixture::line(at(1), 9.0),
        ],
    );
    poller().poll_at(&store, now()).unwrap();
    let rows = store.read();
    assert_eq!(rows.len(), 3);
    let live: Vec<&Value> = rows.iter().filter(|r| r["backfill"] != true).collect();
    assert_eq!(live.len(), 1, "only the newest is a regular row: {rows:?}");
    assert_eq!(live[0]["weekly"]["pct"], 9.0);
}

// ---- #19: an expired credential file does not hide a usable one ------------------------------

struct FakeLogin(Option<(&'static str, Option<f64>)>);
impl aitrack::live::CredentialSource for FakeLogin {
    fn label(&self) -> String {
        "fake".into()
    }
    fn load(&self) -> Option<aitrack::live::Credential> {
        self.0.map(|(t, e)| aitrack::live::Credential {
            token: t.into(),
            expires_at_ms: e,
        })
    }
}

#[test]
fn an_expired_first_login_falls_through_to_a_current_second_one() {
    use aitrack::live::{read_token, CredentialSource};
    let now_s = now().timestamp() as f64;
    let (old, fresh) = (
        Some(("old", Some((now_s - 60.0) * 1000.0))),
        Some(("new", Some((now_s + 3600.0) * 1000.0))),
    );
    let src = |a, b| -> Vec<Box<dyn CredentialSource>> {
        vec![Box::new(FakeLogin(a)), Box::new(FakeLogin(b))]
    };
    assert_eq!(read_token(&src(old, fresh), now_s).unwrap(), "new");
    // a missing file in between is still skipped
    assert_eq!(read_token(&src(None, fresh), now_s).unwrap(), "new");
    // every file with a token expired: the "expired" message, not "no login found"
    let e = read_token(&src(old, old), now_s).unwrap_err();
    assert!(e.message.contains("expired"), "{e}");
    let e = read_token(&src(old, None), now_s).unwrap_err();
    assert!(e.message.contains("expired"), "{e}");
    // no token anywhere: unchanged
    assert!(read_token(&src(None, None), now_s)
        .unwrap_err()
        .message
        .contains("no Claude login"));
}

// ---- #20: the Grok login pick prefers one that is not expired ---------------------------------

#[test]
fn an_undated_grok_login_beats_an_expired_dated_one() {
    use aitrack::grok::select_session;
    let exp = |d: DateTime<Utc>| d.to_rfc3339();
    let pick = |auth: Value| {
        select_session(&auth.to_string(), now())
            .ok()
            .unwrap()
            .0
            .expires_at()
    };
    // expired + no expiry recorded: the undated one
    assert_eq!(
        pick(json!({
            "https://auth.x.ai::a": {"key": "k1", "expires_at": exp(now() - Duration::days(1))},
            "https://auth.x.ai::b": {"key": "k2"},
        })),
        None
    );
    // order of the entries does not matter
    assert_eq!(
        pick(json!({
            "https://auth.x.ai::b": {"key": "k2"},
            "https://auth.x.ai::a": {"key": "k1", "expires_at": exp(now() - Duration::days(1))},
        })),
        None
    );
    // a valid dated login beats both an expired one and an undated one
    let valid = now() + Duration::hours(3);
    assert_eq!(
        pick(json!({
            "https://auth.x.ai::a": {"key": "k1", "expires_at": exp(now() - Duration::days(1))},
            "https://auth.x.ai::b": {"key": "k2"},
            "https://auth.x.ai::c": {"key": "k3", "expires_at": exp(valid)},
        })),
        Some(valid)
    );
    // only expired ones: the latest of them (the caller then says "expired")
    let latest = now() - Duration::hours(1);
    assert_eq!(
        pick(json!({
            "https://auth.x.ai::a": {"key": "k1", "expires_at": exp(now() - Duration::days(1))},
            "https://auth.x.ai::c": {"key": "k3", "expires_at": exp(latest)},
        })),
        Some(latest)
    );
}

// ---- #22: readings with and without resets_at do not split one window -------------------------

#[test]
fn a_history_sample_without_a_reset_joins_the_window_of_a_stamped_one() {
    use aitrack::analysis::analyze;
    use aitrack::reading::Reading;
    use aitrack::stats::{compute, StatsRange};
    let start = now() - Duration::hours(40);
    let reset = start + Duration::hours(5);
    let row = |mins: i64, pct: f64, stamped: bool| {
        Reading::from_value(&json!({
            "ok": true, "ts": (start + Duration::minutes(mins)).to_rfc3339(),
            "session": {"pct": pct, "resets_at": stamped.then(|| reset.to_rfc3339())},
            "weekly": null,
        }))
    };
    // history (no reset), cache (reset), history, history, cache: one 5-hour window
    let rs = vec![
        row(5, 10.0, false),
        row(20, 12.0, true),
        row(40, 15.0, false),
        row(90, 21.0, false),
        row(120, 25.0, true),
    ];
    let plan = analyze(&rs, now()).unwrap();
    assert_eq!(plan.session_stats.windows, 1, "{:?}", plan.session_stats);
    assert_eq!(plan.session_stats.max_peak, Some(25.0));
    let st = compute(&rs, StatsRange::All, now(), 0);
    assert_eq!(st.session.peaks.len(), 1, "{:?}", st.session.peaks);
    assert_eq!(st.session.peaks[0].peak, 25.0);
    // a fall of 10+ points inside the window still ends it (the window reset early)
    let mut rs = rs;
    rs.push(row(150, 3.0, false));
    rs.push(row(160, 6.0, false));
    let plan = analyze(&rs, now()).unwrap();
    assert_eq!(plan.session_stats.windows, 2, "{:?}", plan.session_stats);
    // and unstamped samples that lie after every known reset stay inferred, as before
    let late = vec![
        row(0, 30.0, true),
        row(400, 40.0, false),
        row(410, 5.0, false),
    ];
    assert_eq!(analyze(&late, now()).unwrap().session_stats.windows, 3);
}

// ---- #24: a Grok period that has not ended is shown even when the time left rounds to 0 ------

#[test]
fn grok_stays_visible_until_its_period_has_really_ended() {
    use aitrack::collector::Status;
    use aitrack::reading::Reading;
    use aitrack::settings::Values;
    use aitrack::state::{build_grok_state, build_state};
    let status = Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    };
    let state_at = |reset_in_secs: i64| {
        let rs = vec![Reading::from_value(&json!({
            "ok": true, "source": "grok-billing", "vendor": "grok",
            "ts": (now() - Duration::minutes(1)).to_rfc3339(),
            "weekly": {"pct": 8.0, "resets_at": (now() + Duration::seconds(reset_in_secs)).to_rfc3339()},
        }))];
        let mut st = build_state(&[], Values::default(), status.clone(), now());
        st.grok = Some(build_grok_state(&rs, status.clone(), true, now()));
        st
    };
    let shown = |reset_in_secs: i64| {
        let st = state_at(reset_in_secs);
        aitrack::vendor::from_state_at(&st, now())
            .iter()
            .find(|v| v.id == aitrack::vendor::VendorId::Grok)
            .unwrap()
            .current
    };
    assert!(shown(5), "5 s left: the period has not ended");
    assert!(shown(0), "exactly at the reset instant");
    assert!(!shown(-5), "5 s past the reset: the period has ended");
    assert!(!shown(-3600));
    assert!(shown(86_400));
}

// ---- #23: "is the tool installed?" is not asked on every frame ---------------------------------

#[test]
fn tool_presence_is_checked_once_per_few_seconds_not_per_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let t = TempDir::new();
    let (claude_calls, grok_calls) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (c2, g2) = (claude_calls.clone(), grok_calls.clone());
    let app = App::with_poll_fn(
        Box::new(JsonlStore::new(t.join("u.jsonl"))),
        Arc::new(Settings::load(t.join("s.json"))),
        Box::new(|_| Ok(Value::Null)),
    )
    .with_grok(
        Box::new(JsonlStore::new(t.join("g.jsonl"))),
        Box::new(|_| Ok(Value::Null)),
        None,
        Arc::new(move || {
            g2.fetch_add(1, Ordering::SeqCst);
            true
        }),
    );
    app.set_claude_detect(Arc::new(move || {
        c2.fetch_add(1, Ordering::SeqCst);
        true
    }));
    for _ in 0..200 {
        app.state(); // one call per frame
        app.claude_detected();
        app.grok_detected();
    }
    assert!(claude_calls.load(Ordering::SeqCst) <= 2, "{claude_calls:?}");
    assert!(grok_calls.load(Ordering::SeqCst) <= 2, "{grok_calls:?}");
    // a zero lifetime asks every time (what the install/uninstall tests rely on)
    app.set_presence_ttl(StdDuration::ZERO);
    let before = claude_calls.load(Ordering::SeqCst);
    app.claude_detected();
    app.claude_detected();
    assert_eq!(claude_calls.load(Ordering::SeqCst), before + 2);
}

// ---- #6 (part): settings.json is written atomically and an unreadable file is kept -----------

#[test]
fn settings_are_saved_atomically_and_an_unreadable_file_is_kept_as_a_backup() {
    let t = TempDir::new();
    let path = t.join("settings.json");
    // a file that does not parse (for example a crash while it was written in place)
    std::fs::write(&path, "{\"interval_min\": 5.0, \"direction\": \"remain").unwrap();
    let s = Settings::load(&path);
    assert_eq!(s.get().interval_min, 15.0, "defaults are used meanwhile");
    s.set_interval(7.0);
    let bak = t.join("settings.json.bak");
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        "{\"interval_min\": 5.0, \"direction\": \"remain",
        "the unreadable file must survive"
    );
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["interval_min"], 7.0);
    // no temp file is left behind, and later saves do not touch the backup again
    assert!(!t.join("settings.json.tmp").exists());
    std::fs::write(&bak, "keep me").unwrap();
    s.set_interval(9.0);
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), "keep me");
    // a valid file never produces a backup
    let t2 = TempDir::new();
    let s2 = Settings::load(t2.join("settings.json"));
    s2.set_interval(7.0);
    s2.set_interval(8.0);
    assert!(!t2.join("settings.json.bak").exists());
    assert_eq!(
        Settings::load(t2.join("settings.json")).get().interval_min,
        8.0
    );
}

// ---- #1 #3 #12: where the bearer may go, body caps, header values -----------------------------

mod http_fixture {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A one-shot-per-connection server answering every request with `response`; counts them.
    pub struct Server {
        pub url: String,
        pub hits: Arc<AtomicUsize>,
    }

    pub fn serve(response: Vec<u8>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                h.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                let _ = c.write_all(&response);
            }
        });
        Server { url, hits }
    }

    pub fn redirect_to(location: &str, status: u16) -> Server {
        serve(
            format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
        )
    }

    pub fn ok_json(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

#[test]
fn endpoint_urls_are_restricted_to_the_two_real_https_hosts() {
    use aitrack::endpoints::{check_strict, CLAUDE_HOST, GROK_HOST};
    for ok in [
        "https://api.anthropic.com/api/oauth/usage",
        "https://api.anthropic.com:443/api/oauth/usage",
        "https://API.anthropic.com/x?y=1",
        "https://api.anthropic.com",
    ] {
        assert!(check_strict(ok, CLAUDE_HOST).is_ok(), "{ok}");
    }
    for bad in [
        "http://api.anthropic.com/api/oauth/usage",
        "https://api.anthropic.com.evil.example/x",
        "https://evil.example/api.anthropic.com",
        "https://user:pw@api.anthropic.com/x",
        "https://api.anthropic.com@evil.example/x",
        "https://api.anthropic.com:8443/x",
        "https://api.anthropic.com/x\r\nHost: evil",
        "https://api.anthropic.com /x",
        "ftp://api.anthropic.com/x",
        "//api.anthropic.com/x",
        "",
    ] {
        assert!(check_strict(bad, CLAUDE_HOST).is_err(), "{bad:?}");
    }
    assert!(check_strict(
        "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
        GROK_HOST
    )
    .is_ok());
    assert!(check_strict("https://api.anthropic.com/x", GROK_HOST).is_err());
    // the test hooks are what lets the mock servers below work; a real build has them off
    assert!(std::hint::black_box(aitrack::endpoints::TEST_HOOKS));
    assert!(!aitrack::endpoints::header_value_ok("a\r\nX-Evil: 1"));
    assert!(!aitrack::endpoints::header_value_ok("tok\u{7f}"));
    assert!(aitrack::endpoints::header_value_ok(
        "sk-ant-oat01-abc_DEF.123"
    ));
}

#[test]
fn a_redirect_is_not_followed_and_the_bearer_does_not_leave() {
    use aitrack::live::fetch_usage;
    let target = http_fixture::serve(http_fixture::ok_json("{}"));
    let redirect = http_fixture::redirect_to(&target.url, 302);
    let e = fetch_usage(&redirect.url, "tok", StdDuration::from_secs(5)).unwrap_err();
    assert!(e.message.contains("redirect"), "{e}");
    assert_eq!(
        target.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect target must never be contacted"
    );
}

#[test]
fn a_huge_usage_body_is_refused_and_a_huge_login_file_is_ignored() {
    use aitrack::live::{fetch_usage, CredentialSource, CredentialsFile};
    let big = format!("{{\"pad\": \"{}\"}}", "x".repeat(2 * 1024 * 1024));
    let s = http_fixture::serve(http_fixture::ok_json(&big));
    let e = fetch_usage(&s.url, "tok", StdDuration::from_secs(10)).unwrap_err();
    assert!(e.message.contains("larger than 1 MiB"), "{e}");
    // a normal body still works
    let s = http_fixture::serve(http_fixture::ok_json("{\"five_hour\": null}"));
    assert!(fetch_usage(&s.url, "tok", StdDuration::from_secs(5)).is_ok());

    let t = TempDir::new();
    let path = t.join(".credentials.json");
    let creds = |pad: usize| {
        format!(
            "{{\"claudeAiOauth\": {{\"accessToken\": \"tok\"}}, \"pad\": \"{}\"}}",
            "x".repeat(pad)
        )
    };
    std::fs::write(&path, creds(10)).unwrap();
    assert!(CredentialsFile { path: path.clone() }.load().is_some());
    std::fs::write(&path, creds(2 * 1024 * 1024)).unwrap();
    assert!(CredentialsFile { path }.load().is_none());
}

#[test]
fn a_token_with_control_characters_is_never_sent() {
    use aitrack::live::fetch_usage;
    let s = http_fixture::serve(http_fixture::ok_json("{}"));
    let e = fetch_usage(&s.url, "tok\r\nX-Evil: 1", StdDuration::from_secs(5)).unwrap_err();
    assert!(e.message.contains("unusable token"), "{e}");
    assert_eq!(s.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn grok_billing_refuses_unusable_login_values_and_oversized_replies() {
    use aitrack::grok::GrokBilling;
    use aitrack::sources::{Env, Os, UsageSource};
    let t = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.grok_home = Some(t.join("gh"));
    std::fs::create_dir_all(t.join("gh")).unwrap();
    let auth = |user: &str| {
        json!({"https://auth.x.ai::c": {"key": "xai-tok", "user_id": user,
            "expires_at": (now() + Duration::hours(2)).to_rfc3339()}})
        .to_string()
    };
    let mut billing = GrokBilling::new(env.clone());
    // a user id with a line break: no request is made
    let s = http_fixture::serve(http_fixture::ok_json("{}"));
    billing.url = s.url.clone();
    std::fs::write(t.join("gh/auth.json"), auth("u1\r\nX-Evil: 1")).unwrap();
    let e = billing.read(now()).err().unwrap();
    assert!(e.message.contains("unusable characters"), "{e}");
    assert_eq!(s.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    // a huge reply is refused, a redirect is not followed
    std::fs::write(t.join("gh/auth.json"), auth("u1")).unwrap();
    let big = format!("{{\"pad\": \"{}\"}}", "x".repeat(2 * 1024 * 1024));
    let s = http_fixture::serve(http_fixture::ok_json(&big));
    billing.url = s.url.clone();
    let e = billing.read(now()).err().unwrap();
    assert!(e.message.contains("larger than 1 MiB"), "{e}");
    let target = http_fixture::serve(http_fixture::ok_json("{}"));
    let redirect = http_fixture::redirect_to(&target.url, 307);
    billing.url = redirect.url.clone();
    let e = billing.read(now()).err().unwrap();
    assert!(e.message.contains("redirect"), "{e}");
    assert_eq!(target.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ---- O6: Retry-After cannot crash the window ---------------------------------------------------

#[test]
fn retry_after_values_that_are_not_plain_seconds_are_ignored_or_capped() {
    use aitrack::live::fetch_usage;
    let retry = |value: &str| {
        let body = "";
        let s = http_fixture::serve(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {value}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        );
        fetch_usage(&s.url, "tok", StdDuration::from_secs(5))
            .unwrap_err()
            .retry_after
    };
    assert_eq!(retry("120"), Some(120.0));
    assert_eq!(retry(" 7 "), Some(7.0));
    assert_eq!(retry("0"), Some(0.0));
    // not plain whole seconds: ignored
    for bad in [
        "inf",
        "NaN",
        "1e20",
        "-5",
        "1.5",
        "soon",
        "Wed, 21 Oct 2026 07:28:00 GMT",
        "",
    ] {
        assert_eq!(retry(bad), None, "{bad:?}");
    }
    // a huge but valid number is capped at a day
    assert_eq!(retry("99999999999"), Some(86_400.0));
}

#[test]
fn a_poll_that_fails_with_an_absurd_retry_after_does_not_kill_the_collector() {
    use aitrack::collector::Collector;
    use aitrack::live::CollectError;
    let t = TempDir::new();
    let settings = Arc::new(Settings::load(t.join("s.json")));
    let history = Arc::new(aitrack::store::History::new(Box::new(JsonlStore::new(
        t.join("u.jsonl"),
    ))));
    for bad in [f64::INFINITY, 1e20, f64::NAN, -3.0] {
        let c = Collector::new(
            history.clone(),
            settings.clone(),
            Box::new(move |_| {
                Err(CollectError {
                    message: "HTTP 429".into(),
                    retry_after: Some(bad),
                })
            }),
        );
        c.start();
        wait_for(|| c.status().polls >= 1);
        // scheduling the next poll must not panic and must give a finite wait
        let next = c.status().next_in.expect("a next poll is scheduled");
        assert!(next.is_finite() && next <= 86_400.0 + 1.0, "{bad}: {next}");
        c.stop();
        c.join();
    }
}

// ---- O5: the Grok CLI probe never blocks the caller --------------------------------------------

#[test]
fn looking_for_the_grok_cli_happens_on_a_background_thread() {
    let t = TempDir::new();
    let app = Arc::new(
        App::with_poll_fn(
            Box::new(JsonlStore::new(t.join("u.jsonl"))),
            Arc::new(Settings::load(t.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        )
        .with_grok(
            Box::new(JsonlStore::new(t.join("g.jsonl"))),
            Box::new(|_| Ok(Value::Null)),
            None,
            Arc::new(|| true),
        ),
    );
    // a probe that stalls (a PATH entry on a dead network share)
    app.set_grok_cli_probe(Arc::new(|| {
        std::thread::sleep(StdDuration::from_millis(600));
        true
    }));
    let t0 = Instant::now();
    assert!(!app.grok_cli_present(), "not known yet");
    assert!(
        t0.elapsed() < StdDuration::from_millis(300),
        "the caller was blocked: {:?}",
        t0.elapsed()
    );
    // the answer arrives later
    wait_for(|| app.grok_cli_present());
}

#[test]
fn live_billing_polls_every_five_minutes_keep_the_data_age_fresh() {
    use aitrack::grok::{GrokBilling, GrokLog};
    use aitrack::live::Poller;
    use aitrack::sources::{Env, Os};
    use aitrack::store::{JsonlStore, Store};
    let t = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.grok_home = Some(t.join("gh"));
    std::fs::create_dir_all(t.join("gh/logs")).unwrap();
    // an auth.json with a valid login, and a CLI log whose newest line is 50 minutes old
    std::fs::write(
        t.join("gh/auth.json"),
        json!({"https://auth.x.ai::c": {"key": "xai-tok", "user_id": "u1",
            "expires_at": (now() + Duration::hours(9)).to_rfc3339()}})
        .to_string(),
    )
    .unwrap();
    let end = now() + Duration::days(3);
    std::fs::write(
        aitrack::grok::log_path(&env),
        json!({"ts": (now() - Duration::minutes(50)).to_rfc3339(), "level": "info", "msg": "billing: fetched credits config",
            "ctx": {"config": {"creditUsagePercent": 12.0, "currentPeriod": {"end": end.to_rfc3339()}}}})
        .to_string()
            + "\n",
    )
    .unwrap();
    let body = json!({"config": {"creditUsagePercent": 12.0, "currentPeriod": {"end": end.to_rfc3339()}}, "subscriptionTier": "SuperGrok"}).to_string();
    let server = http_fixture::serve(http_fixture::ok_json(&body));
    let mut billing = GrokBilling::new(env.clone());
    billing.url = server.url.clone();
    let poller = Poller::with_chain_for(
        vec![Box::new(billing), Box::new(GrokLog { env: env.clone() })],
        "grok",
    );
    let store = JsonlStore::new(t.join("g.jsonl"));
    // every five minutes a full poll (live billing, same value each time) and a local tick between
    for k in (0..=8).rev() {
        let at = now() - Duration::minutes(k * 5 + 1);
        poller.poll_at(&store, at).unwrap();
        let _ = poller.poll_local_at(&store, at + Duration::minutes(2));
    }
    let readings: Vec<aitrack::reading::Reading> = store
        .read()
        .iter()
        .map(aitrack::reading::Reading::from_value)
        .collect();
    let (chip, v) = crate::release_review::grok_chip(&readings, 5.0);
    let age = (now() - v.observed_at.unwrap()).num_minutes();
    assert!(age <= 5, "age {age} min; source {:?}", v.source);
    assert!(chip.is_none(), "{chip:?}");
}

// ---- O7: closing the window ends a running Grok refresh ---------------------------------------

/// Whether process `pid` exists (`kill -0`: works on Linux and macOS, unlike `/proc`).
#[cfg(unix)]
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
#[test]
fn stopping_the_app_kills_the_hidden_grok_child_of_a_running_refresh() {
    use aitrack::grok_cli::{self, RefreshCfg};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    let t = TempDir::new();
    let script = t.join("fake-grok");
    let pidfile = t.join("pid");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let app = Arc::new(
        App::with_poll_fn(
            Box::new(JsonlStore::new(t.join("u.jsonl"))),
            Arc::new(Settings::load(t.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        )
        .with_grok(
            Box::new(JsonlStore::new(t.join("g.jsonl"))),
            Box::new(|_| Ok(Value::Null)),
            None,
            Arc::new(|| true),
        ),
    );
    // a refresh run with a fake CLI that would live for a minute; the app tracks its pid
    let slot = app.grok_refresh_pid_slot();
    let log = t.join("unified.jsonl");
    let s2 = slot.clone();
    app.set_grok_refresh(
        Arc::new(move || {
            let cfg = RefreshCfg {
                program: script.clone(),
                args: vec![],
                log: log.clone(),
                timeout: StdDuration::from_secs(30),
                grace: StdDuration::ZERO,
                poll_every: StdDuration::from_millis(20),
            };
            grok_cli::refresh_tracked(&cfg, &grok_cli::SystemHost, &s2).map(|_| None)
        }),
        Arc::new(|| true),
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d2 = done.clone();
    app.refresh_grok(move || d2.store(true, Ordering::SeqCst));
    wait_for(|| slot.load(Ordering::SeqCst) != 0 && pidfile.exists());
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    assert!(alive(&pid));
    // closing the window: stop() must end the child at once, not leave it for a minute
    let t0 = Instant::now();
    app.stop();
    wait_for(|| !alive(&pid));
    wait_for(|| done.load(Ordering::SeqCst));
    assert!(t0.elapsed() < StdDuration::from_secs(5));
    assert_eq!(
        slot.load(Ordering::SeqCst),
        0,
        "the slot is cleared when the run ends"
    );
}

// ---- O8: only the running instance's own window is raised --------------------------------------

#[test]
fn a_window_called_aitrack_of_another_process_is_not_ours() {
    use aitrack::instance::is_our_window;
    // an Explorer window titled "aiTrack" (the repo folder) while our pid is known
    assert!(!is_our_window("aiTrack", 4242, Some(1234)));
    assert!(is_our_window("aiTrack", 1234, Some(1234)));
    // the title still has to match
    assert!(!is_our_window("aiTrack - notes", 1234, Some(1234)));
    assert!(!is_our_window("", 1234, Some(1234)));
    // without a pid file the title alone decides (as before)
    assert!(is_our_window("aiTrack", 99, None));
}
