// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::live::{self, CollectError, CredentialsFile, Poller};
use aitrack::store::{JsonlStore, Store};
use common::*;
use serde_json::json;

fn poller(url: &str, dir: &TempDir) -> Poller {
    Poller::new(
        url,
        vec![Box::new(CredentialsFile {
            path: dir.join(".credentials.json"),
        })],
    )
}

fn setup() -> (TempDir, Mock, JsonlStore) {
    let dir = TempDir::new();
    write_creds(dir.path(), "tok-123", now_ms() + 3_600_000.0);
    let mock = Mock::start();
    let store = JsonlStore::new(dir.join("usage.jsonl"));
    (dir, mock, store)
}

#[test]
fn poll_stores_full_record_and_sends_token() {
    let (dir, mock, store) = setup();
    let rec = poller(&mock.url, &dir).poll_once(&store).unwrap();
    {
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen[0][0].as_deref(), Some("Bearer tok-123"));
        assert_eq!(seen[0][1].as_deref(), Some("oauth-2025-04-20"));
        assert_eq!(seen[0][2].as_deref(), Some("application/json"));
        assert_eq!(seen[0][3].as_deref(), Some("aitrack"));
    }
    let saved = store.read();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["v"], 1);
    assert_eq!(saved[0]["ok"], true);
    assert_eq!(saved[0]["session"]["pct"], 42.0);
    assert_eq!(saved[0]["weekly"]["pct"], 21.5);
    assert_eq!(
        saved[0]["spend"],
        json!({"enabled": true, "used": 13.59, "limit": 50.0})
    );
    assert!(saved[0]["raw"].get("future_field").is_none()); // unknown fields are not stored
    assert_eq!(
        saved[0]["limits"][0]["scope"]["model"]["display_name"],
        "Fable"
    );
    assert!(!std::fs::read_to_string(dir.join("usage.jsonl"))
        .unwrap()
        .contains("tok-123"));
    assert_eq!(live::describe(&rec), "session 42%, weekly 22%");
}

#[test]
fn failures_are_recorded_and_429_carries_retry_after() {
    let (dir, mock, store) = setup();
    mock.set_status(429);
    let err = poller(&mock.url, &dir).poll_once(&store).unwrap_err();
    assert_eq!(err.retry_after, Some(120.0));
    assert_eq!(err.message, "HTTP 429");
    let saved = store.read();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["ok"], false);
    assert_eq!(saved[0]["error"], "HTTP 429");
}

#[test]
fn rejected_login_and_bad_bodies() {
    let (dir, mock, store) = setup();
    let p = poller(&mock.url, &dir);
    mock.set_status(401);
    let e = p.poll_once(&store).unwrap_err();
    assert!(e.message.contains("login rejected (HTTP 401)"), "{e}");
    mock.set_status(403);
    assert!(p
        .poll_once(&store)
        .unwrap_err()
        .message
        .contains("HTTP 403"));
    mock.set_status(200);
    mock.non_json
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        p.poll_once(&store).unwrap_err().message,
        "unexpected (non-JSON) response"
    );
}

#[test]
fn network_error_is_reported() {
    let (dir, _mock, store) = setup();
    // nothing listens on port 1
    let e = poller("http://127.0.0.1:1/usage", &dir)
        .poll_once(&store)
        .unwrap_err();
    assert!(e.message.starts_with("network error:"), "{e}");
    assert_eq!(store.read()[0]["ok"], false);
}

#[test]
fn expired_and_missing_token() {
    let dir = TempDir::new();
    write_creds(dir.path(), "x", 1000.0);
    let sources: Vec<Box<dyn live::CredentialSource>> = vec![Box::new(CredentialsFile {
        path: dir.join(".credentials.json"),
    })];
    let e = live::read_token(&sources, live::unix_now()).unwrap_err();
    assert!(e.message.contains("expired"), "{e}");

    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    let e = live::read_token(&sources, live::unix_now()).unwrap_err();
    assert!(e.message.contains("no Claude login"), "{e}");
    assert!(e.message.contains(".credentials.json"));
}

#[test]
fn malformed_credentials_are_skipped_in_favour_of_next_source() {
    let a = TempDir::new();
    let b = TempDir::new();
    std::fs::write(a.join(".credentials.json"), "{not json").unwrap();
    write_creds(b.path(), "good", now_ms() + 10_000.0);
    let sources: Vec<Box<dyn live::CredentialSource>> = vec![
        Box::new(CredentialsFile {
            path: a.join(".credentials.json"),
        }),
        Box::new(CredentialsFile {
            path: b.join(".credentials.json"),
        }),
    ];
    assert_eq!(
        live::read_token(&sources, live::unix_now()).unwrap(),
        "good"
    );
}

#[test]
fn credential_source_order() {
    let s = live::default_sources_from(Some("/cfg"), Some("/home/me".into()));
    let labels: Vec<String> = s.iter().map(|s| s.label()).collect();
    // (macOS adds the Keychain item as a last source)
    let keychain = usize::from(cfg!(target_os = "macos"));
    assert_eq!(labels.len(), 2 + keychain);
    assert!(labels[0]
        .replace('\\', "/")
        .ends_with("/cfg/.credentials.json"));
    assert!(labels[1]
        .replace('\\', "/")
        .ends_with("/home/me/.claude/.credentials.json"));
    assert_eq!(
        live::default_sources_from(None, Some("/h".into())).len(),
        1 + keychain
    );
}

#[test]
fn run_once_exit_codes() {
    let (dir, mock, store) = setup();
    let p = poller(&mock.url, &dir);
    let mut logs: Vec<String> = Vec::new();
    assert_eq!(
        live::run(&p, &store, 15.0, true, &mut |l| logs.push(l.to_string())),
        0
    );
    assert!(logs[0].contains("every 15 min"));
    assert!(logs[1].contains("session 42%, weekly 22%"));
    mock.set_status(500);
    assert_eq!(
        live::run(&p, &store, 15.0, true, &mut |l| logs.push(l.to_string())),
        1
    );
    assert!(logs.last().unwrap().contains("failed: HTTP 500"));
}

#[test]
fn normalize_handles_odd_shapes() {
    let rec = live::normalize(
        &json!({"five_hour": {"utilization": "x"}, "spend": {}}),
        "2026-01-01T00:00:00+00:00",
    );
    assert!(rec["session"].is_null() && rec["weekly"].is_null() && rec["spend"].is_null());
    let rec = live::normalize(
        &json!({"spend": {"enabled": false, "used": {"amount_minor": 5}}}),
        "t",
    );
    assert_eq!(
        rec["spend"],
        json!({"enabled": false, "used": 5.0, "limit": null})
    );
}

#[test]
fn keychain_json_parses_like_the_credentials_file() {
    // fixture shaped like Claude Code's stored login; the token is fake
    let json = br#"{"claudeAiOauth":{"accessToken":"test-token","expiresAt":4102444800000.0}}"#;
    let c = live::parse_credentials(json).unwrap();
    assert_eq!(c.token, "test-token");
    assert_eq!(c.expires_at_ms, Some(4102444800000.0));
    assert!(live::parse_credentials(b"not json").is_none());
    assert!(live::parse_credentials(br#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
    assert!(live::parse_credentials(br#"{"other":1}"#).is_none());
}

#[test]
fn the_keychain_source_is_only_listed_on_a_mac_and_is_empty_elsewhere() {
    let dir = TempDir::new();
    let v = live::default_sources_from(None, Some(dir.path().to_path_buf()));
    let has_kc = v.iter().any(|s| s.label().contains(live::KEYCHAIN_SERVICE));
    assert_eq!(has_kc, cfg!(target_os = "macos"));
    #[cfg(not(target_os = "macos"))]
    assert!(live::CredentialSource::load(&live::KeychainItem).is_none());
    // no login anywhere: the usual 'not signed in' error
    #[cfg(not(target_os = "macos"))]
    {
        let err = live::read_token(&v, 0.0).unwrap_err();
        assert!(err.message.contains("no Claude login found"));
    }
}

#[test]
fn collect_error_type() {
    let e = CollectError::new("boom");
    assert_eq!(e.to_string(), "boom");
    assert_eq!(e.retry_after, None);
}

#[cfg(unix)]
#[test]
fn a_command_that_never_answers_is_killed_after_the_limit() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut c = std::process::Command::new("sleep");
    c.arg("30");
    let err = aitrack::live::run_bounded(&mut c, Duration::from_millis(300)).unwrap_err();
    assert!(err.contains("timed out"), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    let mut c = std::process::Command::new("echo");
    c.arg("hi");
    let out = aitrack::live::run_bounded(&mut c, Duration::from_secs(5)).unwrap();
    assert!(out.status.success() && out.stdout.starts_with(b"hi"));
}

#[test]
fn normalize_stores_only_known_fields_and_no_ids() {
    let body = json!({
        "five_hour": {"utilization": 40.0, "resets_at": "2026-10-14T15:00:00+00:00", "organization_id": "org-SECRET"},
        "seven_day": {"utilization": 10.0, "resets_at": null},
        "organization_uuid": "org-SECRET",
        "account": {"id": "acct-SECRET", "email": "x@example.com"},
        "extra_usage": {"is_enabled": false, "user_id": "u-SECRET"},
        "limits": [{"percent": 5.0, "account_id": "acct-SECRET",
                    "scope": {"model": {"display_name": "Fable", "id": "fable-1"}}}],
        "future_field": {"keep": "me"},
    });
    let rec = aitrack::live::normalize(&body, "2026-10-14T10:00:00+00:00");
    assert_eq!(rec["session"]["pct"], 40.0);
    assert_eq!(rec["limits"][0]["scope"]["model"]["display_name"], "Fable");
    assert_eq!(rec["raw"]["extra_usage"]["is_enabled"], false);
    let text = rec.to_string();
    assert!(!text.contains("SECRET"), "an id reached the stored record");
    assert!(!text.contains("example.com"));
    assert!(rec["raw"].get("future_field").is_none());
}

#[test]
fn unix_secs_follows_the_given_time() {
    let t = chrono::DateTime::parse_from_rfc3339("2026-10-14T10:00:00.500+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(aitrack::live::unix_secs(t), t.timestamp() as f64 + 0.5);
}
