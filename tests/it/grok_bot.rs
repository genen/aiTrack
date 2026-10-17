// SPDX-License-Identifier: GPL-3.0-or-later
//! The Grok Bot app's second weekly meter: file-name decoding, parsing, the folder scan (and that
//! nothing but the matching file is opened), staleness, the history file, diagnostics and the view
//! row. Fixtures are synthetic (a made-up account); every clock-dependent check takes an explicit
//! `now`. The one App test builds its fixture relative to the real clock, like the other App tests.
use crate::common;
use aitrack::collector::Status;
use aitrack::grok_bot::{self, BotReading, BotTracker};
use aitrack::settings::Values;
use aitrack::sources::{Env, Os};
use aitrack::state::State;
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::{Vendor, VendorId};
use aitrack::view::{self, Cell, Tone};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::json;
use std::path::Path;

const ACCOUNT: &str = "acct-test";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
}

/// RFC 4648 base32 with padding (test-side encoder).
fn b32(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in data {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(A[((acc << (5 - bits)) & 31) as usize] as char);
    }
    while !out.len().is_multiple_of(8) {
        out.push('=');
    }
    out
}

fn key_name(account: &str) -> String {
    format!("sand.client.slice.account.grok%{account}.weekly-usage.cache")
}

fn blob_name(key: &str) -> String {
    format!("{}.blob", b32(key.as_bytes()))
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn cache_json(
    pct: f64,
    resets: DateTime<Utc>,
    read: DateTime<Utc>,
    expires: DateTime<Utc>,
) -> String {
    json!({
        "schemaVersion": 1,
        "value": {
            "kind": "weekly-usage",
            "selectedTeamId": null,
            "reading": {
                "usage": {
                    "percentUsed": pct,
                    "nextResetMs": ms(resets),
                    "isSandTrial": false,
                    "hasNonZeroIncludedLimit": true,
                    "isTeamSeat": false,
                    "onDemand": null,
                    "grokPlanLabel": "SuperGrok"
                },
                "readAtMs": ms(read)
            },
            "expiresAtMs": ms(expires)
        }
    })
    .to_string()
}

fn resets() -> DateTime<Utc> {
    now() + Duration::days(1) + Duration::hours(1)
}

struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Fx {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.join("Grok Bot/sand-client-persistence")).unwrap();
        Fx { dir }
    }
    fn persistence(&self) -> std::path::PathBuf {
        self.dir.join("Grok Bot/sand-client-persistence")
    }
    fn env(&self) -> Env {
        let mut e = Env::new(Os::Linux);
        e.grok_bot_dir = Some(self.persistence());
        e
    }
    fn put(&self, account: &str, body: &str) {
        std::fs::write(self.persistence().join(blob_name(&key_name(account))), body).unwrap();
    }
    fn fresh(&self, pct: f64) {
        let read = now() - Duration::minutes(2);
        self.put(
            ACCOUNT,
            &cache_json(pct, resets(), read, read + Duration::hours(2)),
        );
    }
}

fn reading(pct: f64, read_min_ago: i64) -> BotReading {
    BotReading {
        pct,
        resets_at: resets(),
        read_at: now() - Duration::minutes(read_min_ago),
        expires_at: None,
    }
}

// ---------------------------------------------------------------- names

#[test]
fn base32_decodes_the_rfc_vectors_in_either_case_with_or_without_padding() {
    use grok_bot::base32_decode as d;
    assert_eq!(d("MZXW6YTBOI======").unwrap(), b"foobar");
    assert_eq!(d("MZXW6YTBOI").unwrap(), b"foobar");
    assert_eq!(d("mzxw6ytboi").unwrap(), b"foobar");
    assert_eq!(d("MZXW6===").unwrap(), b"foo");
    assert_eq!(d("").unwrap(), b"");
    assert!(d("MZXW6YT!OI").is_none());
    assert!(d("MZXW1YTBOI").is_none(), "1 is not in the alphabet");
}

#[test]
fn only_the_weekly_usage_cache_name_matches() {
    let good = blob_name(&key_name(ACCOUNT));
    assert!(grok_bot::is_weekly_usage_file(&good));
    // padding stripped, lower case
    let stripped = good.replace('=', "");
    assert!(grok_bot::is_weekly_usage_file(&stripped));
    assert!(grok_bot::is_weekly_usage_file(&stripped.to_lowercase()));
    // other keys, other extensions, junk
    assert!(!grok_bot::is_weekly_usage_file(&blob_name(
        "sand.client.slice.account.grok%x.session.cache"
    )));
    assert!(!grok_bot::is_weekly_usage_file(
        &good.replace(".blob", ".json")
    ));
    assert!(!grok_bot::is_weekly_usage_file("auth.json"));
    assert!(!grok_bot::is_weekly_usage_file("sand-secrets.json"));
    assert!(!grok_bot::is_weekly_usage_file(".blob"));
    assert!(!grok_bot::is_weekly_usage_file("!!!.blob"));
}

// ---------------------------------------------------------------- parsing

#[test]
fn parses_the_reported_layout() {
    let read = now() - Duration::minutes(2);
    let exp = read + Duration::hours(2);
    let r = grok_bot::parse(cache_json(59.174814, resets(), read, exp).as_bytes()).unwrap();
    assert!((r.pct - 59.174814).abs() < 1e-9);
    assert_eq!(r.resets_at, resets());
    assert_eq!(r.read_at, read);
    assert_eq!(r.expires_at, Some(exp));
    // the expiry is optional
    let mut v: serde_json::Value =
        serde_json::from_str(&cache_json(1.0, resets(), read, exp)).unwrap();
    v["value"].as_object_mut().unwrap().remove("expiresAtMs");
    assert_eq!(
        grok_bot::parse(v.to_string().as_bytes())
            .unwrap()
            .expires_at,
        None
    );
}

#[test]
fn surprises_in_the_layout_are_skipped_not_fatal() {
    let read = now();
    let good = cache_json(10.0, resets(), read, read);
    let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v: serde_json::Value = serde_json::from_str(&good).unwrap();
        f(&mut v);
        grok_bot::parse(v.to_string().as_bytes())
    };
    assert!(mutate(&|_| {}).is_some());
    assert!(mutate(&|v| v["value"]["reading"]["usage"]["percentUsed"] = json!("59")).is_none());
    assert!(mutate(&|v| v["value"]["reading"]["usage"]["percentUsed"] = json!(-1.0)).is_none());
    assert!(mutate(&|v| v["value"]["reading"]["usage"]["percentUsed"] = json!(250.0)).is_none());
    assert!(mutate(&|v| v["value"]["reading"]["usage"]["nextResetMs"] = json!(null)).is_none());
    assert!(mutate(&|v| v["value"]["reading"]["usage"]["nextResetMs"] = json!(0)).is_none());
    assert!(mutate(&|v| v["value"]["reading"]["readAtMs"] = json!(1e300)).is_none());
    assert!(mutate(&|v| {
        v["value"].as_object_mut().unwrap().remove("reading");
    })
    .is_none());
    assert!(mutate(&|v| *v = json!([1, 2, 3])).is_none());
    for junk in [&b""[..], b"{", b"null", b"\xff\xfe\x00", b"{\"value\":5}"] {
        assert!(grok_bot::parse(junk).is_none());
    }
    // 100.2 is clamped (rounding noise), not rejected
    let r = mutate(&|v| v["value"]["reading"]["usage"]["percentUsed"] = json!(100.2)).unwrap();
    assert_eq!(r.pct, 100.0);
}

// ---------------------------------------------------------------- the folder scan

#[test]
fn scan_finds_the_reading() {
    let f = Fx::new();
    f.fresh(59.17);
    let s = grok_bot::scan_dir(&f.persistence());
    assert!(s.dir_exists);
    assert_eq!((s.matched, s.unusable), (1, 0));
    assert!((s.reading.unwrap().pct - 59.17).abs() < 1e-9);
    assert_eq!(grok_bot::read(&f.env()).unwrap().pct, 59.17);
}

#[test]
fn with_two_accounts_the_newest_read_wins() {
    let f = Fx::new();
    let old = now() - Duration::hours(5);
    let new = now() - Duration::minutes(3);
    f.put("acct-old", &cache_json(11.0, resets(), old, old));
    f.put("acct-new", &cache_json(22.0, resets(), new, new));
    f.put(
        "acct-mid",
        &cache_json(33.0, resets(), now() - Duration::hours(1), now()),
    );
    let s = grok_bot::scan_dir(&f.persistence());
    assert_eq!(s.matched, 3);
    assert_eq!(s.reading.unwrap().pct, 22.0);
}

#[test]
fn unusable_matching_files_are_counted_and_skipped() {
    let f = Fx::new();
    f.put("acct-bad", "{ not json");
    f.put("acct-huge", &" ".repeat(200 * 1024));
    let s = grok_bot::scan_dir(&f.persistence());
    assert_eq!((s.matched, s.unusable), (2, 2));
    assert!(s.reading.is_none());
    f.fresh(5.0);
    assert_eq!(
        grok_bot::scan_dir(&f.persistence()).reading.unwrap().pct,
        5.0
    );
}

#[test]
fn a_missing_folder_is_not_an_error() {
    let f = Fx::new();
    let s = grok_bot::scan_dir(&f.dir.join("nope"));
    assert!(!s.dir_exists && s.reading.is_none());
}

fn read_atime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().accessed().unwrap()
}

fn age_file(p: &Path) {
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800); // 2000-01-01
    let newer = old + std::time::Duration::from_secs(86_400);
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_times(
        std::fs::FileTimes::new()
            .set_accessed(old)
            .set_modified(newer),
    )
    .unwrap();
}

/// The secrets files next to the cache must never be opened. Opening updates the access time, so
/// each decoy gets an ancient one; a control file proves this file system does record reads
/// (otherwise the access-time check is skipped and says so).
#[test]
fn nothing_but_the_matching_file_is_opened() {
    let f = Fx::new();
    f.fresh(40.0);
    let parent = f.dir.join("Grok Bot");
    let decoys = [
        parent.join("auth.json"),
        parent.join("sand-secrets.json"),
        f.persistence().join("auth.json"),
        f.persistence().join("sand-secrets.json"),
        f.persistence()
            .join(blob_name("sand.client.slice.account.grok%x.session.cache")),
        f.persistence().join("NOTBASE32!.blob"),
    ];
    for d in &decoys {
        std::fs::write(d, "SECRET-SHOULD-NEVER-BE-READ").unwrap();
        age_file(d);
    }
    let control = f.dir.join("control.txt");
    std::fs::write(&control, "x").unwrap();
    age_file(&control);
    let _ = std::fs::read(&control);
    let records_reads = read_atime(&control)
        > std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let before: Vec<_> = decoys.iter().map(|d| read_atime(d)).collect();

    let s = grok_bot::scan_dir(&f.persistence());
    assert_eq!((s.matched, s.unusable), (1, 0));
    assert_eq!(s.reading.unwrap().pct, 40.0);

    if records_reads {
        let after: Vec<_> = decoys.iter().map(|d| read_atime(d)).collect();
        assert_eq!(before, after, "a file that does not match was opened");
    } else {
        eprintln!("note: this file system does not record read times; access-time check skipped");
    }
}

#[cfg(unix)]
#[test]
fn a_matching_name_that_is_a_link_is_not_followed() {
    let f = Fx::new();
    let secret = f.dir.join("Grok Bot/sand-secrets.json");
    std::fs::write(&secret, cache_json(99.0, resets(), now(), now())).unwrap();
    std::os::unix::fs::symlink(&secret, f.persistence().join(blob_name(&key_name(ACCOUNT))))
        .unwrap();
    let s = grok_bot::scan_dir(&f.persistence());
    assert_eq!(s.matched, 0, "links are ignored");
    assert!(s.reading.is_none());
}

#[test]
fn the_default_folder_is_macos_or_windows_and_the_override_works_everywhere() {
    let mut mac = Env::new(Os::Mac);
    mac.home = Some("/Users/test".into());
    assert_eq!(
        grok_bot::persistence_dir(&mac).unwrap(),
        Path::new("/Users/test/Library/Application Support/Grok Bot/sand-client-persistence")
    );
    // Windows: %APPDATA% when set, else <home>\AppData\Roaming
    let mut win = Env::new(Os::Windows);
    win.appdata = Some("/roaming".into());
    win.home = Some("/home/test".into());
    assert_eq!(
        grok_bot::persistence_dir(&win).unwrap(),
        Path::new("/roaming/Grok Bot/sand-client-persistence")
    );
    win.appdata = None;
    assert_eq!(
        grok_bot::persistence_dir(&win).unwrap(),
        Path::new("/home/test/AppData/Roaming/Grok Bot/sand-client-persistence")
    );
    win.home = None;
    assert!(grok_bot::persistence_dir(&win).is_none());
    let mut e = Env::new(Os::Linux);
    e.home = Some("/home/test".into());
    assert!(grok_bot::persistence_dir(&e).is_none());
    assert!(grok_bot::read(&e).is_none());
    let mut e = Env::new(Os::Windows);
    e.grok_bot_dir = Some("/x".into());
    assert_eq!(grok_bot::persistence_dir(&e).unwrap(), Path::new("/x"));
}

// ---------------------------------------------------------------- staleness

#[test]
fn stale_after_60_minutes_or_past_the_apps_own_expiry() {
    let mut r = reading(10.0, 60);
    assert!(!r.stale(now()), "exactly 60 minutes is not yet stale");
    r.read_at = now() - Duration::minutes(61);
    assert!(r.stale(now()));
    let mut r = reading(10.0, 5);
    assert!(!r.stale(now()));
    r.expires_at = Some(now() - Duration::seconds(1));
    assert!(r.stale(now()));
    r.expires_at = Some(now() + Duration::minutes(1));
    assert!(!r.stale(now()));
    let mut r = reading(10.0, 5);
    r.resets_at = now() - Duration::seconds(1);
    assert!(r.period_ended(now()));
}

// ---------------------------------------------------------------- history file

#[test]
fn the_history_gets_changes_and_a_heartbeat_but_no_identifiers() {
    let f = Fx::new();
    let hist = f.dir.join("usage-grok-bot.jsonl");
    let tracker = BotTracker::new(f.env(), Box::new(JsonlStore::new(&hist)));
    assert_eq!(tracker.latest(), None);

    tracker.refresh(now());
    assert_eq!(JsonlStore::new(&hist).read().len(), 0, "no file, no record");

    f.fresh(50.0);
    tracker.refresh(now());
    let v0 = tracker.version();
    assert!(v0 > 0);
    assert_eq!(tracker.latest().unwrap().pct, 50.0);
    // unchanged, a minute later: nothing new, version unchanged
    tracker.refresh(now() + Duration::minutes(1));
    assert_eq!(tracker.version(), v0);
    assert_eq!(JsonlStore::new(&hist).read().len(), 1);
    // unchanged for 15 minutes: one heartbeat record
    tracker.refresh(now() + Duration::minutes(15));
    assert_eq!(JsonlStore::new(&hist).read().len(), 2);
    // a changed percentage is recorded at once
    f.fresh(51.0);
    tracker.refresh(now() + Duration::minutes(16));
    assert_eq!(JsonlStore::new(&hist).read().len(), 3);
    assert!(tracker.version() > v0);

    // a restart does not repeat the last record
    let again = BotTracker::new(f.env(), Box::new(JsonlStore::new(&hist)));
    again.refresh(now() + Duration::minutes(17));
    assert_eq!(JsonlStore::new(&hist).read().len(), 3);

    let text = std::fs::read_to_string(&hist).unwrap();
    assert!(!text.contains(ACCOUNT) && !text.contains("sand.client") && !text.contains(".blob"));
    let last = JsonlStore::new(&hist).read().pop().unwrap();
    assert_eq!(last["vendor"], "grok");
    assert_eq!(last["source"], "grok-bot-cache");
    assert_eq!(last["bot_weekly"]["pct"], 51.0);
    assert!(last["bot_weekly"]["resets_at"].is_string());
}

#[test]
fn old_grok_history_stays_readable_and_the_bot_file_is_separate() {
    // a pre-existing usage-grok.jsonl line parses as before; the bot record is not a Reading of it
    let old = json!({"v":1,"ts":"2026-10-14T10:00:00+00:00","ok":true,"vendor":"grok","source":"grok-log",
        "weekly":{"pct":7.0,"resets_at":"2026-10-20T10:00:00+00:00"}});
    let r = aitrack::reading::Reading::from_value(&old);
    assert_eq!(r.weekly.unwrap().pct, 7.0);
    let rec = grok_bot::record(&reading(59.0, 2), now());
    assert!(
        rec.get("weekly").is_none(),
        "no clash with the CLI's weekly field"
    );
}

// ---------------------------------------------------------------- diagnostics

#[test]
fn diagnostics_say_found_stale_or_missing_and_nothing_identifying() {
    let f = Fx::new();
    let d = grok_bot::diagnose(&f.env(), now());
    assert!(d.contains("not found (no weekly usage cache"), "{d}");
    f.fresh(59.0);
    let d = grok_bot::diagnose(&f.env(), now());
    assert!(d.contains("grok-bot-cache: found - 59% used"), "{d}");
    let old = now() - Duration::hours(3);
    f.put(ACCOUNT, &cache_json(59.0, resets(), old, old));
    let d = grok_bot::diagnose(&f.env(), now());
    assert!(d.contains("found, stale"), "{d}");
    let mut e = f.env();
    e.grok_bot_dir = Some(f.dir.join("missing"));
    assert!(grok_bot::diagnose(&e, now()).contains("data folder is missing"));
    let lin = Env::new(Os::Linux);
    assert!(grok_bot::diagnose(&lin, now()).contains("not available on this platform"));
    f.put("acct-bad", "junk");
    for text in [
        grok_bot::diagnose(&f.env(), now()),
        aitrack::sources::diagnose(&f.env(), now()),
    ] {
        assert!(
            !text.contains(ACCOUNT) && !text.contains("acct-bad"),
            "{text}"
        );
        assert!(
            !text.contains(".blob") && !text.contains("sand.client"),
            "{text}"
        );
        assert!(!text.contains("sand-client-persistence"), "{text}");
    }
}

// ---------------------------------------------------------------- the view

fn status() -> Status {
    Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    }
}

fn grok_vendor(bot: Option<BotReading>) -> Vendor {
    let mut vs = aitrack::vendor::demo_vendors(now());
    let mut v = vs.remove(vs.iter().position(|v| v.id == VendorId::Grok).unwrap());
    v.current = true;
    v.source = Some("grok-billing".into());
    v.observed_at = Some(now());
    v.bot = bot;
    v
}

fn home(v: Vendor, remaining: bool) -> view::VendorView {
    let values = Values {
        direction: if remaining {
            aitrack::settings::Direction::Remaining
        } else {
            aitrack::settings::Direction::Used
        },
        ..Default::default()
    };
    let st = State::loading(values, status());
    view::build_home_for(&st, &[v], now()).vendors.remove(0)
}

fn lines(r: &view::Row) -> Vec<&str> {
    r.tip.iter().map(|l| l.text.as_str()).collect()
}

#[test]
fn without_the_source_the_grok_block_is_unchanged() {
    let vv = home(grok_vendor(None), false);
    assert_eq!(vv.rows.len(), 1);
    assert_eq!(vv.rows[0].label, "Weekly");
    assert!(!vv.rows[0].secondary);
}

#[test]
fn the_bot_bar_sits_under_weekly_with_a_tooltip() {
    let vv = home(grok_vendor(Some(reading(59.17, 2))), false);
    assert_eq!(vv.rows.len(), 2);
    assert_eq!(vv.rows[0].label, "Weekly");
    let b = &vv.rows[1];
    assert_eq!((b.label, b.value.as_str()), ("Grok Bot", "59%"));
    assert!(b.secondary && !b.stale && !b.dim);
    assert_eq!(b.cells.iter().filter(|c| **c == Cell::Lit).count(), 12);
    let t = lines(b);
    assert!(t[0].starts_with("Grok Bot resets in 1d 1h"), "{t:?}");
    assert!(t.contains(&"Reading 2 min ago"), "{t:?}");
    assert!(
        t.iter().any(|l| l.contains("Grok Bot app's cache")
            && l.contains("only every half hour or so")),
        "{t:?}"
    );
    assert!(b.tip.iter().all(|l| l.tone != Tone::Caution));
    assert!(vv.age_chip.is_none(), "fresh: no chip");
    // the first row still has no bot content
    assert!(!lines(&vv.rows[0]).iter().any(|l| l.contains("Grok Bot")));
}

#[test]
fn remaining_mode_flips_the_bot_value_too() {
    let vv = home(grok_vendor(Some(reading(59.17, 2))), true);
    assert_eq!(vv.rows[1].value, "41% left");
}

#[test]
fn an_old_reading_is_kept_dimmed_with_chip_and_no_projection() {
    let vv = home(grok_vendor(Some(reading(59.17, 130))), false);
    let b = &vv.rows[1];
    assert_eq!(b.value, "59%", "the last value stays");
    assert!(b.dim && !b.stale);
    let caution: Vec<_> = b.tip.iter().filter(|l| l.tone == Tone::Caution).collect();
    assert_eq!(caution.len(), 1);
    assert!(caution[0].text.contains("2 h old"), "{}", caution[0].text);
    assert!(!lines(b)
        .iter()
        .any(|l| l.contains("pace") || l.contains("Empty")));
    let chip = vv.age_chip.expect("chip for the old Grok Bot reading");
    assert_eq!(chip.compact, "2 h");
    assert!(chip.tooltip.contains("Grok Bot"), "{}", chip.tooltip);
    // at exactly 60 minutes it is neither dimmed nor chipped; the app rewrites its cache only every
    // 30 to 40 minutes, so a 40 minute old reading is normal
    let vv = home(grok_vendor(Some(reading(59.17, 60))), false);
    assert!(!vv.rows[1].dim);
    assert!(vv.age_chip.is_none());
    let vv = home(grok_vendor(Some(reading(59.17, 40))), false);
    assert!(vv.age_chip.is_none());
    let vv = home(grok_vendor(Some(reading(59.17, 61))), false);
    assert!(vv.rows[1].dim && vv.age_chip.is_some());
}

#[test]
fn past_the_apps_own_expiry_counts_as_stale() {
    let mut r = reading(30.0, 10);
    r.expires_at = Some(now() - Duration::minutes(1));
    let b = home(grok_vendor(Some(r)), false).rows.remove(1);
    assert!(b.dim);
}

#[test]
fn an_ended_period_shows_dashes_not_last_weeks_number() {
    let mut r = reading(59.17, 2);
    r.resets_at = now() - Duration::minutes(5);
    let b = home(grok_vendor(Some(r)), false).rows.remove(1);
    assert_eq!(b.value, "--");
    assert!(b.stale && b.secondary);
    assert!(b.cells.iter().all(|c| *c == Cell::Off));
    assert!(lines(&b)[0].contains("period has ended"), "{:?}", lines(&b));
}

#[test]
fn a_vendors_own_chip_wins_over_the_bot_chip() {
    let mut v = grok_vendor(Some(reading(10.0, 200)));
    v.source = Some("grok-log".into());
    v.observed_at = Some(now() - Duration::minutes(40));
    let vv = home(v, false);
    assert!(vv
        .age_chip
        .unwrap()
        .tooltip
        .contains("Grok CLI's local log"));
}

// ---------------------------------------------------------------- through the real App

#[test]
fn the_app_reads_the_cache_after_a_grok_poll_and_the_view_gets_the_row() {
    use aitrack::app::App;
    use aitrack::grok::{self, GrokLog};
    use aitrack::live::Poller;
    use aitrack::settings::Settings;
    use serde_json::Value;
    use std::sync::Arc;

    let dir = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.xdg_config_home = Some(dir.join("xdg"));
    env.claude_config_dir = Some(dir.join("cfg"));
    env.grok_home = Some(dir.join("grokhome"));
    env.grok_bot_dir = Some(dir.join("persist"));
    std::fs::create_dir_all(dir.join("grokhome/logs")).unwrap();
    std::fs::create_dir_all(dir.join("persist")).unwrap();
    // the CLI log: a fresh snapshot (relative to the real clock, like the other App tests)
    let t0 = Utc::now();
    let end = t0 + Duration::days(4);
    let fmt = |d: DateTime<Utc>| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let line = json!({"ts": fmt(t0 - Duration::minutes(3)), "level": "info",
        "msg": "billing: fetched credits config",
        "ctx": {"config": {"creditUsagePercent": 7.0, "currentPeriod": {"end": fmt(end)}}}});
    std::fs::write(dir.join("grokhome/logs/unified.jsonl"), format!("{line}\n")).unwrap();
    let read = t0 - Duration::minutes(4);
    std::fs::write(
        dir.join("persist").join(blob_name(&key_name(ACCOUNT))),
        cache_json(
            59.0,
            t0 + Duration::days(2),
            read,
            read + Duration::hours(1),
        ),
    )
    .unwrap();

    let e2 = env.clone();
    let poller = Poller::with_chain_for(vec![Box::new(GrokLog { env: env.clone() })], "grok");
    let app = App::new(
        Box::new(JsonlStore::new(dir.join("usage.jsonl"))),
        Arc::new(Settings::load(dir.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None),
    )
    .with_grok_poller(
        Box::new(JsonlStore::new(dir.join("usage-grok.jsonl"))),
        poller,
        Arc::new(move || grok::detected(&e2)),
    )
    .with_grok_bot(
        Box::new(JsonlStore::new(dir.join("usage-grok-bot.jsonl"))),
        env.clone(),
    );
    app.start();
    let t = std::time::Instant::now();
    loop {
        let st = app.state();
        if let Some(b) = st.grok.as_ref().and_then(|g| g.bot) {
            assert_eq!(b.pct, 59.0);
            let home = view::build_home(&st, Utc::now());
            let rows = &home.vendors[0].rows;
            assert_eq!(rows.len(), 2);
            assert_eq!((rows[0].label, rows[1].label), ("Weekly", "Grok Bot"));
            break;
        }
        assert!(t.elapsed().as_secs() < 10, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    app.stop();
    app.join();
    let stored: Vec<Value> = JsonlStore::new(dir.join("usage-grok-bot.jsonl")).read();
    assert!(!stored.is_empty());
    // the CLI history file holds no bot record
    assert!(JsonlStore::new(dir.join("usage-grok.jsonl"))
        .read()
        .iter()
        .all(|r| r.get("bot_weekly").is_none()));
}
