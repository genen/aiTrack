// SPDX-License-Identifier: GPL-3.0-or-later
//! Synthetic Chromium-cache fixtures (blockfile and simple cache) written to a temp dir.
//! They follow the documented layouts; they prove the reader's logic, not that a real Windows
//! cache looks exactly like this.
use crate::common;
use aitrack::analysis::Gauge;
use aitrack::cache;
use aitrack::live::Poller;
use aitrack::sources::{self, DesktopCache, DesktopHistory, Env, Os, UsageSource};
use aitrack::store::{JsonlStore, Store};
use aitrack::view;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

const ORG_A: &str = "org-aaaa-secret-1111";
const ORG_B: &str = "org-bbbb-secret-2222";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn usage_url(org: &str) -> String {
    format!("https://claude.ai/api/organizations/{org}/usage")
}

fn usage_body(five: f64, seven: f64) -> Value {
    json!({
        "five_hour": {"utilization": five, "resets_at": (now() + Duration::minutes(200)).to_rfc3339()},
        "seven_day": {"utilization": seven, "resets_at": (now() + Duration::hours(149)).to_rfc3339()},
        "extra_usage": {"is_enabled": false},
    })
}

fn http_date(t: DateTime<Utc>) -> String {
    t.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// A response-info pickle: payload size, flags(3), two times, then the length-prefixed headers.
fn response_info(headers: &[(&str, String)], status: &str, flags: u32) -> Vec<u8> {
    let mut h = format!("HTTP/1.1 {status}\0").into_bytes();
    for (k, v) in headers {
        h.extend(format!("{k}: {v}\0").bytes());
    }
    h.push(0);
    let mut payload = Vec::new();
    payload.extend(flags.to_le_bytes());
    payload.extend(0i64.to_le_bytes()); // request time
    payload.extend(0i64.to_le_bytes()); // response time
    payload.extend((h.len() as u32).to_le_bytes());
    payload.extend(&h);
    while payload.len() % 4 != 0 {
        payload.push(0);
    }
    let mut out = (payload.len() as u32).to_le_bytes().to_vec();
    out.extend(payload);
    out
}

fn json_headers(date: DateTime<Utc>, encoding: Option<&str>) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("Content-Type", "application/json".to_string()),
        ("Date", http_date(date)),
    ];
    if let Some(e) = encoding {
        v.push(("Content-Encoding", e.to_string()));
    }
    v
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

// ------------------------------------------------------------ blockfile builder

struct Block {
    dir: PathBuf,
    d1: Vec<u8>, // data_1: 256-byte blocks (entries and small streams)
    d2: Vec<u8>, // data_2: 1 KiB blocks (bodies)
    b1: usize,
    b2: usize,
    ext: usize,
    entries: Vec<usize>,
    /// Creation time (unix seconds) written into the next entry, like Chromium does.
    created: Option<i64>,
}

fn block_header(entry_size: i32, file: u16, used: &[usize]) -> Vec<u8> {
    let mut h = vec![0u8; 8192];
    h[0..4].copy_from_slice(&0xC104CAC3u32.to_le_bytes());
    h[4..8].copy_from_slice(&0x20000u32.to_le_bytes());
    h[8..10].copy_from_slice(&file.to_le_bytes());
    h[12..16].copy_from_slice(&entry_size.to_le_bytes());
    for &b in used {
        let w = 80 + (b / 32) * 4;
        let cur = u32::from_le_bytes(h[w..w + 4].try_into().unwrap());
        h[w..w + 4].copy_from_slice(&(cur | 1 << (b % 32)).to_le_bytes());
    }
    h
}

impl Block {
    fn new(dir: &Path) -> Block {
        std::fs::create_dir_all(dir).unwrap();
        Block {
            dir: dir.to_path_buf(),
            d1: vec![],
            d2: vec![],
            b1: 0,
            b2: 0,
            ext: 0,
            entries: vec![],
            created: None,
        }
    }

    fn put1(&mut self, bytes: &[u8]) -> u32 {
        let blocks = bytes.len().div_ceil(256).max(1);
        let addr = 0x8000_0000 | 2 << 28 | ((blocks as u32 - 1) << 24) | 1 << 16 | self.b1 as u32;
        let mut padded = bytes.to_vec();
        padded.resize(blocks * 256, 0);
        self.d1.extend(padded);
        self.b1 += blocks;
        addr
    }

    fn put2(&mut self, bytes: &[u8]) -> u32 {
        let blocks = bytes.len().div_ceil(1024).max(1);
        let addr = 0x8000_0000 | 3 << 28 | ((blocks as u32 - 1) << 24) | 2 << 16 | self.b2 as u32;
        let mut padded = bytes.to_vec();
        padded.resize(blocks * 1024, 0);
        self.d2.extend(padded);
        self.b2 += blocks;
        addr
    }

    fn put_external(&mut self, bytes: &[u8]) -> u32 {
        self.ext += 1;
        std::fs::write(self.dir.join(format!("f_{:06x}", self.ext)), bytes).unwrap();
        0x8000_0000 | self.ext as u32
    }

    /// Add one cache entry. `body_addr` overrides where the body goes (default: data_2).
    fn add_raw(&mut self, key: &str, meta: &[u8], body: &[u8], external: bool) {
        // reserve the entry block first so its index is the current one
        let entry_block = self.b1;
        self.d1.extend(vec![0u8; 256]);
        self.b1 += 1;
        let meta_addr = self.put1(meta);
        let body_addr = if external {
            self.put_external(body)
        } else {
            self.put2(body)
        };
        let mut e = vec![0u8; 256];
        if let Some(c) = self.created.take() {
            // base::Time internal value: microseconds since 1601-01-01
            e[24..32].copy_from_slice(&((c + 11_644_473_600) * 1_000_000).to_le_bytes());
        }
        e[32..36].copy_from_slice(&(key.len() as i32).to_le_bytes());
        e[40..44].copy_from_slice(&(meta.len() as i32).to_le_bytes());
        e[44..48].copy_from_slice(&(body.len() as i32).to_le_bytes());
        e[56..60].copy_from_slice(&meta_addr.to_le_bytes());
        e[60..64].copy_from_slice(&body_addr.to_le_bytes());
        e[96..96 + key.len()].copy_from_slice(key.as_bytes());
        self.d1[entry_block * 256..entry_block * 256 + 256].copy_from_slice(&e);
        self.entries.push(entry_block);
    }

    /// A well-formed JSON usage response cached for `org`.
    fn add_usage(&mut self, org: &str, date: DateTime<Utc>, encoding: Option<&str>, body: &Value) {
        let raw = body.to_string().into_bytes();
        let stored = match encoding {
            Some("gzip") => gzip(&raw),
            Some("zstd") => ruzstd::encoding::compress_to_vec(
                &raw[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
            _ => raw,
        };
        let meta = response_info(&json_headers(date, encoding), "200 OK", 3);
        self.add_raw(&usage_url(org), &meta, &stored, false);
    }

    fn finish(self) -> PathBuf {
        let used1: Vec<usize> = (0..self.b1).collect();
        let used2: Vec<usize> = (0..self.b2).collect();
        let mut f1 = block_header(256, 1, &used1);
        f1.extend(&self.d1);
        std::fs::write(self.dir.join("data_1"), f1).unwrap();
        if self.b2 > 0 {
            let mut f2 = block_header(1024, 2, &used2);
            f2.extend(&self.d2);
            std::fs::write(self.dir.join("data_2"), f2).unwrap();
        }
        std::fs::write(self.dir.join("index"), b"not read by aiTrack").unwrap();
        let _ = self.entries;
        self.dir
    }
}

// ------------------------------------------------------------ simple cache builder

fn simple_entry(dir: &Path, key: &str, meta: &[u8], body: &[u8], sha: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let eof = |size: usize, flags: u32| {
        let mut e = 0xf4fa6f45970d41d8u64.to_le_bytes().to_vec();
        e.extend(flags.to_le_bytes());
        e.extend(0u32.to_le_bytes());
        e.extend((size as u32).to_le_bytes());
        e.extend(0u32.to_le_bytes());
        e
    };
    let mut f = 0xfcfb6d1ba7725c30u64.to_le_bytes().to_vec();
    f.extend(5u32.to_le_bytes());
    f.extend((key.len() as u32).to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(key.as_bytes());
    f.extend(body);
    f.extend(eof(body.len(), 1));
    f.extend(meta);
    if sha {
        f.extend([7u8; 32]);
    }
    f.extend(eof(meta.len(), if sha { 3 } else { 1 }));
    let name = format!(
        "{:016x}_0",
        key.len() as u64 * 0x1234_5678 + dir.read_dir().unwrap().count() as u64
    );
    std::fs::write(dir.join(name), f).unwrap();
}

fn cache_dir(t: &TempDir) -> PathBuf {
    t.join("Cache_Data")
}

fn body_of(e: &cache::UsageEntry) -> &Value {
    e.body.as_ref().expect("body should parse")
}

// ------------------------------------------------------------ reading entries

#[test]
fn blockfile_entry_plain_gzip_zstd_and_external_bodies() {
    let t = TempDir::new();
    let mut b = Block::new(&cache_dir(&t));
    let d = now() - Duration::minutes(7);
    b.add_usage(ORG_A, d, None, &usage_body(55.0, 8.0));
    b.add_usage(
        ORG_A,
        d - Duration::minutes(30),
        Some("gzip"),
        &usage_body(10.0, 1.0),
    );
    b.add_usage(
        ORG_A,
        d - Duration::minutes(60),
        Some("zstd"),
        &usage_body(20.0, 2.0),
    );
    // a big body kept in an external f_ file
    let meta = response_info(&json_headers(d - Duration::minutes(90), None), "200 OK", 3);
    b.add_raw(
        &usage_url(ORG_A),
        &meta,
        usage_body(33.0, 3.0).to_string().as_bytes(),
        true,
    );
    b.finish();

    let scan = cache::scan_usage_entries_at(&cache_dir(&t), now());
    assert!(scan.dir_exists && scan.notes.is_empty(), "{:?}", scan.notes);
    assert_eq!(scan.entries.len(), 4);
    let mut e: Vec<_> = scan.entries.iter().collect();
    e.sort_by_key(|e| std::cmp::Reverse(e.observed));
    let fives: Vec<f64> = e
        .iter()
        .map(|e| body_of(e)["five_hour"]["utilization"].as_f64().unwrap())
        .collect();
    assert_eq!(fives, [55.0, 10.0, 20.0, 33.0]);
    assert_eq!(e[0].observed, Some(d)); // from the Date header
    assert_eq!(e[1].encoding.as_deref(), Some("gzip"));
    assert_eq!(e[2].encoding.as_deref(), Some("zstd"));
    assert!(e.iter().all(|x| x.org == ORG_A));
}

#[test]
fn simple_cache_entries_with_and_without_key_hash() {
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let d = now() - Duration::minutes(3);
    for (org, sha, enc) in [(ORG_A, false, None), (ORG_B, true, Some("gzip"))] {
        let body = usage_body(41.0, 9.0).to_string().into_bytes();
        let body = if enc.is_some() { gzip(&body) } else { body };
        let meta = response_info(&json_headers(d, enc), "200 OK", 3);
        simple_entry(
            &dir,
            &format!(
                "1/0/_dk_https://claude.ai https://claude.ai {}",
                usage_url(org)
            ),
            &meta,
            &body,
            sha,
        );
    }
    // an unrelated entry in the same folder
    simple_entry(
        &dir,
        "https://claude.ai/api/bootstrap",
        &response_info(&json_headers(d, None), "200 OK", 3),
        b"{}",
        false,
    );
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert_eq!(scan.files, 3);
    assert_eq!(scan.entries.len(), 2);
    for e in &scan.entries {
        assert_eq!(body_of(e)["seven_day"]["utilization"], 9.0);
        assert_eq!(e.observed, Some(d));
    }
}

#[test]
fn keys_headers_and_non_usage_entries_are_filtered() {
    let k = |s: &str| cache::usage_org_from_key(s.as_bytes());
    assert_eq!(k(&usage_url("abc")).as_deref(), Some("abc"));
    assert_eq!(
        k(&format!(
            "1/0/_dk_https://claude.ai https://claude.ai {}?x=1",
            usage_url("abc")
        ))
        .as_deref(),
        Some("abc")
    );
    assert_eq!(
        k("https://claude.ai/api/organizations/abc/usage_extra"),
        None
    );
    assert_eq!(
        k("https://claude.ai/api/organizations/abc/usage/more"),
        None
    );
    assert_eq!(k("https://claude.ai/api/organizations//usage"), None);
    assert_eq!(k("https://example.com/api/organizations/abc/usage"), None);

    let t = TempDir::new();
    let mut b = Block::new(&cache_dir(&t));
    let d = now() - Duration::minutes(1);
    let body = usage_body(1.0, 1.0).to_string();
    // wrong status, wrong content type, truncated flag, other URL: all ignored
    b.add_raw(
        &usage_url(ORG_A),
        &response_info(&json_headers(d, None), "429 Too Many Requests", 3),
        body.as_bytes(),
        false,
    );
    b.add_raw(
        &usage_url(ORG_A),
        &response_info(
            &[("Content-Type", "text/html".into()), ("Date", http_date(d))],
            "200 OK",
            3,
        ),
        body.as_bytes(),
        false,
    );
    b.add_raw(
        &usage_url(ORG_A),
        &response_info(&json_headers(d, None), "200 OK", 3 | 1 << 12),
        body.as_bytes(),
        false,
    );
    b.add_raw(
        "https://claude.ai/api/bootstrap",
        &response_info(&json_headers(d, None), "200 OK", 3),
        body.as_bytes(),
        false,
    );
    b.add_usage(ORG_A, d, None, &usage_body(2.0, 2.0));
    b.finish();
    let scan = cache::scan_usage_entries_at(&cache_dir(&t), now());
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(body_of(&scan.entries[0])["five_hour"]["utilization"], 2.0);
}

#[test]
fn damaged_and_unsupported_entries_give_errors_not_panics() {
    let t = TempDir::new();
    let mut b = Block::new(&cache_dir(&t));
    let d = now() - Duration::minutes(1);
    let meta = response_info(&json_headers(d, Some("br")), "200 OK", 3);
    b.add_raw(&usage_url(ORG_A), &meta, b"\x0b\x01brotli-ish", false);
    let meta = response_info(&json_headers(d, None), "200 OK", 3);
    b.add_raw(&usage_url(ORG_B), &meta, b"{ not json", false);
    b.add_raw(&usage_url("org-c"), &meta, b"[1,2]", false);
    b.add_raw(&usage_url("org-d"), &meta, &[0x1f, 0x8b, 1, 2, 3], false); // broken gzip
    b.add_raw(
        &usage_url("org-e"),
        b"garbage without any http headers",
        b"{}",
        false,
    );
    b.finish();
    let scan = cache::scan_usage_entries_at(&cache_dir(&t), now());
    assert_eq!(scan.entries.len(), 5);
    let errs: Vec<String> = scan
        .entries
        .iter()
        .map(|e| e.body.clone().unwrap_err())
        .collect();
    assert!(errs[0].contains("brotli"), "{errs:?}");
    assert!(errs[1].contains("not valid JSON"), "{errs:?}");
    assert!(errs[2].contains("not a JSON object"), "{errs:?}");
    assert!(errs[3].contains("gzip"), "{errs:?}");
    assert!(errs[4].contains("no HTTP headers"), "{errs:?}");
}

#[test]
fn truncated_and_garbage_files_are_survived() {
    let t = TempDir::new();
    let mut b = Block::new(&cache_dir(&t));
    b.add_usage(ORG_A, now(), None, &usage_body(5.0, 5.0));
    let dir = b.finish();
    // cut data_1 in the middle of the entry's streams, drop data_2 altogether
    let full = std::fs::read(dir.join("data_1")).unwrap();
    std::fs::write(dir.join("data_1"), &full[..8192 + 300]).unwrap();
    std::fs::remove_file(dir.join("data_2")).unwrap();
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert!(scan.entries.iter().all(|e| e.body.is_err()) && scan.entries.len() <= 1);
    // header-only, empty and random files
    std::fs::write(dir.join("data_1"), &full[..100]).unwrap();
    std::fs::write(dir.join("data_3"), []).unwrap();
    std::fs::write(dir.join("data_4"), vec![0xAB; 20000]).unwrap();
    std::fs::write(dir.join("0123456789abcdef_0"), b"short").unwrap();
    std::fs::write(dir.join("fedcba9876543210_0"), vec![0u8; 500]).unwrap();
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert!(scan.entries.is_empty());
    assert!(scan.files >= 5);
    // a directory that does not exist
    let none = cache::scan_usage_entries_at(&t.join("nope"), now());
    assert!(!none.dir_exists && none.entries.is_empty());
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_reported_in_notes() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempDir::new();
    let mut b = Block::new(&cache_dir(&t));
    b.add_usage(ORG_A, now(), None, &usage_body(5.0, 5.0));
    let dir = b.finish();
    let f = dir.join("data_1");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&f).is_ok() {
        return; // running as root: permissions are not enforced, nothing to simulate
    }
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert!(scan.entries.is_empty());
    assert!(
        scan.notes
            .iter()
            .any(|n| n.starts_with("data_1:") && n.contains("locked or not readable")),
        "{:?}",
        scan.notes
    );
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
}

// ------------------------------------------------------------ the source and the poller

fn env_with_data_dir(t: &TempDir) -> (Env, PathBuf) {
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.xdg_config_home = Some(t.join("xdg"));
    (env, t.join("xdg/Claude"))
}

fn write_history(data_dir: &Path, samples: Vec<Value>) {
    std::fs::create_dir_all(data_dir).unwrap();
    std::fs::write(
        data_dir.join("plan-usage-history.json"),
        json!({"version": 2, "samples": samples}).to_string(),
    )
    .unwrap();
}

fn hist_sample(minutes_ago: i64, org: &str, fh: f64, sd: f64) -> Value {
    json!({"t": (now() - Duration::minutes(minutes_ago)).timestamp_millis(), "org": org, "u": {"fh": fh, "sd": sd}})
}

fn chain(env: &Env) -> Poller {
    Poller::with_chain(vec![
        Box::new(DesktopCache { env: env.clone() }),
        Box::new(DesktopHistory { env: env.clone() }),
    ])
}

#[test]
fn cache_is_the_current_reading_with_reset_times_and_history_is_still_backfilled() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(
        &data,
        vec![
            hist_sample(780, ORG_A, 10.0, 8.0),
            hist_sample(770, ORG_A, 11.0, 8.0),
        ],
    ); // 13 h old
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(2),
        Some("gzip"),
        &usage_body(55.0, 8.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));

    let rec = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "desktop-cache");
    assert_eq!(rec["session"]["pct"], 55.0);
    assert_eq!(
        rec["session"]["resets_at"],
        (now() + Duration::minutes(200)).to_rfc3339()
    );
    assert_eq!(rec["weekly"]["pct"], 8.0);
    assert_eq!(
        rec["observed_at"],
        (now() - Duration::minutes(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    );
    assert_eq!(rec["raw"]["extra_usage"]["is_enabled"], false); // same normalisation as the endpoint
    assert!(rec.get("backfill").is_none());
    let all = store.read();
    assert_eq!(all.len(), 3); // 2 imported history samples + the cache reading
    assert_eq!(
        all.iter()
            .filter(|r| r["source"] == "desktop-history" && r["backfill"] == true)
            .count(),
        2
    );

    // nothing changed: no duplicates, and a fresh poller (new process) agrees
    assert_eq!(chain(&env).poll_at(&store, now()).unwrap()["stored"], false);
    assert_eq!(
        chain(&env)
            .poll_at(&store, now() + Duration::minutes(15))
            .unwrap()["stored"],
        false
    );
    assert_eq!(store.read().len(), 3);

    // the app refreshes its cache: one new row
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() + Duration::minutes(10),
        None,
        &usage_body(60.0, 9.0),
    );
    b.finish();
    let rec = chain(&env)
        .poll_at(&store, now() + Duration::minutes(11))
        .unwrap();
    assert_eq!(rec["session"]["pct"], 60.0);
    assert_eq!(store.read().len(), 4);
}

/// Regression: the DEFAULT chain (what the window and `--collect` use) must try the cache first.
/// Earlier tests built the chain by hand, so a missing default entry went unnoticed.
#[test]
fn the_default_chain_reads_the_cache_first() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(&data, vec![hist_sample(780, ORG_A, 0.0, 0.0)]); // stale, like the user's PC
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(1),
        Some("gzip"),
        &usage_body(64.0, 9.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let rec = Poller::with_env(&env, "http://127.0.0.1:9/unused", None)
        .poll_at(&store, now())
        .unwrap();
    assert_eq!(rec["source"], "desktop-cache");
    assert_eq!(rec["session"]["pct"], 64.0);
    assert_eq!(rec["weekly"]["pct"], 9.0);
}

fn cache_dir_in(data: &Path) -> PathBuf {
    data.join("Cache").join("Cache_Data")
}

#[test]
fn the_chain_note_says_about_the_same_age_below_one_minute() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(&data, vec![hist_sample(1, ORG_A, 70.0, 20.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    // 40 s older than the history sample (above the 30 s tie rule, below a minute)
    b.add_usage(
        ORG_A,
        now() - Duration::seconds(100),
        None,
        &usage_body(50.0, 15.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let rec = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "desktop-history");
    let note = rec["note"].as_str().expect("a chain note");
    assert!(
        note.contains("desktop-cache entry was about the same age as the desktop-history sample"),
        "{note}"
    );
    assert!(!note.contains("0 min"), "{note}");
}

#[test]
fn a_newer_history_sample_beats_an_older_cache_entry() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(&data, vec![hist_sample(1, ORG_A, 70.0, 20.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(40),
        None,
        &usage_body(50.0, 15.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let rec = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(
        (rec["source"].as_str(), rec["session"]["pct"].as_f64()),
        (Some("desktop-history"), Some(70.0))
    );
    // the history sample reports percentages only: it borrows the cache entry's reset time (the
    // window it defines still contains the sample and the percentage has not dropped), marked as
    // carried, and the record says why the cache entry did not supply the reading
    assert_eq!(rec["session"]["resets_carried"], true);
    assert_eq!(
        rec["session"]["resets_at"]
            .as_str()
            .map(|t| t[..16].to_owned()),
        Some((now() + Duration::minutes(200)).to_rfc3339()[..16].to_owned())
    );
    assert_eq!(rec["weekly"]["resets_carried"], true);
    let note = rec["note"].as_str().expect("a chain note");
    assert!(
        note.contains("desktop-cache entry was")
            && note.contains("older than the desktop-history sample"),
        "{note}"
    );
    assert_eq!(store.read().len(), 2);
    assert_eq!(
        store
            .read()
            .iter()
            .filter(|r| r["backfill"] == true)
            .count(),
        1
    ); // the older cache row
}

#[test]
fn orgs_are_never_mixed() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    // the newest history sample belongs to org A; org B has a *newer* cache entry that must not be used
    write_history(&data, vec![hist_sample(600, ORG_A, 1.0, 1.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_B,
        now() - Duration::minutes(1),
        None,
        &usage_body(99.0, 99.0),
    );
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(30),
        None,
        &usage_body(25.0, 5.0),
    );
    b.finish();
    let obs = DesktopCache { env: env.clone() }.read(now()).unwrap();
    assert_eq!(obs.latest.session.unwrap().pct, 25.0);

    // org A has no entry at all: nothing is used (and no org id leaks into the message)
    let t2 = TempDir::new();
    let (env2, data2) = env_with_data_dir(&t2);
    write_history(&data2, vec![hist_sample(600, ORG_A, 1.0, 1.0)]);
    let mut b = Block::new(&cache_dir_in(&data2));
    b.add_usage(
        ORG_B,
        now() - Duration::minutes(1),
        None,
        &usage_body(99.0, 99.0),
    );
    b.finish();
    let e = DesktopCache { env: env2 }.read(now()).unwrap_err();
    assert!(e.message.contains("no usage entry"), "{e}");
    assert!(!e.message.contains("org-"));

    // without a history file the newest entry of any org is taken
    let t3 = TempDir::new();
    let (env3, data3) = env_with_data_dir(&t3);
    let mut b = Block::new(&cache_dir_in(&data3));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(30),
        None,
        &usage_body(25.0, 5.0),
    );
    b.add_usage(
        ORG_B,
        now() - Duration::minutes(3),
        None,
        &usage_body(44.0, 6.0),
    );
    b.finish();
    assert_eq!(
        DesktopCache { env: env3 }
            .read(now())
            .unwrap()
            .latest
            .session
            .unwrap()
            .pct,
        44.0
    );
}

#[test]
fn stale_cache_falls_through_with_a_clear_message() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(180),
        None,
        &usage_body(25.0, 5.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let e = chain(&env).poll_at(&store, now()).unwrap_err();
    assert!(
        e.message
            .contains("Claude desktop cache is stale (last response 3 h ago): open the Claude app"),
        "{e}"
    );
    assert!(
        e.message.contains("Claude desktop history not found"),
        "{e}"
    );
    let all = store.read();
    assert_eq!(all.len(), 2); // the stale cache reading kept as backfill + one error
    assert_eq!(all[0]["backfill"], true);
    assert_eq!(all[1]["ok"], false);
}

#[test]
fn missing_cache_and_unsupported_encoding_fall_through_softly() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let e = DesktopCache { env: env.clone() }.read(now()).unwrap_err();
    assert!(e.message.contains("cache not found"), "{e}");
    let mut b = Block::new(&cache_dir_in(&data));
    let meta = response_info(&json_headers(now(), Some("br")), "200 OK", 3);
    b.add_raw(&usage_url(ORG_A), &meta, b"compressed", false);
    b.finish();
    let e = DesktopCache { env }.read(now()).unwrap_err();
    assert!(
        e.message.contains("unusable") && e.message.contains("brotli"),
        "{e}"
    );
}

#[test]
fn cache_without_utilization_fields_is_rejected() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now(),
        None,
        &json!({"five_hour": {"utilization": 300}, "seven_day": null, "x": 1}),
    );
    b.finish();
    let e = DesktopCache { env }.read(now()).unwrap_err();
    assert!(e.message.contains("no five_hour/seven_day"), "{e}");
}

#[test]
fn windows_store_package_layout_is_found_through_the_glob() {
    let t = TempDir::new();
    let local = t.join("Local");
    let data = local.join("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(4),
        None,
        &usage_body(55.0, 8.0),
    );
    b.finish();
    let mut env = Env::new(Os::Windows);
    env.appdata = Some(t.join("Roaming"));
    env.localappdata = Some(local);
    let obs = DesktopCache { env }.read(now()).unwrap();
    assert_eq!(obs.latest.weekly.unwrap().pct, 8.0);
}

// ------------------------------------------------------------ --sources section

#[test]
fn diagnose_has_a_cache_section_with_numbers_only() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(&data, vec![hist_sample(780, ORG_A, 10.0, 8.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(2),
        Some("gzip"),
        &usage_body(55.0, 8.0),
    );
    b.finish();
    let out = sources::diagnose(&env, now());
    for want in [
        "1. desktop-cache",
        "cache files scanned: 3",
        "usage entries found: 1 (1 for the org of the newest history sample)",
        "matching usage entry: yes",
        "entry age: 2 min",
        "content-encoding: gzip",
        "five_hour: 55% used, resets in 3 h",
        "seven_day: 8% used, resets in 6 d",
        "2. desktop-history",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    for secret in [ORG_A, "org-", "is_enabled", "extra_usage"] {
        assert!(!out.contains(secret), "leaked {secret:?} in:\n{out}");
    }
}

// ------------------------------------------------------------ pace and the inline warning

fn gauge(pct: f64, hours_left: Option<f64>, rate: Option<f64>) -> Gauge {
    Gauge {
        pct,
        resets_at: None,
        hours_left,
        rate_per_hour: rate,
        projected_pct: None,
        hours_to_limit: None,
        window_hours: 5.0,
        window_id: String::new(),
        resets_carried: false,
        reset_passed: false,
        rate_method: None,
    }
}

#[test]
fn inline_warning_only_when_the_limit_comes_before_the_reset() {
    // 55% used, 3h20m left -> 33 pts/h -> empty in ~1h22m, before the reset
    let w = view::empty_warning(&gauge(55.0, Some(3.0 + 20.0 / 60.0), Some(33.0))).unwrap();
    assert_eq!(w, "Empty in ~1h 22m \u{b7} resets in 3h 20m");
    // slow enough to last until the reset: nothing
    assert_eq!(
        view::empty_warning(&gauge(55.0, Some(3.0), Some(10.0))),
        None
    );
    // no pace, no reset time, or an expired window: nothing
    assert_eq!(view::empty_warning(&gauge(55.0, Some(3.0), None)), None);
    assert_eq!(view::empty_warning(&gauge(55.0, None, Some(33.0))), None);
    assert_eq!(
        view::empty_warning(&gauge(55.0, Some(0.0), Some(33.0))),
        None
    );
    assert_eq!(
        view::empty_warning(&gauge(55.0, Some(3.0), Some(0.0))),
        None
    );
    // already at the limit
    assert_eq!(
        view::empty_warning(&gauge(100.0, Some(2.0), Some(5.0))).unwrap(),
        "At the limit \u{b7} resets in 2h"
    );
}

#[test]
fn rows_carry_the_warning_and_the_tooltip_is_unchanged() {
    use aitrack::collector::Status;
    use aitrack::settings::Values;
    use aitrack::state::build_state;
    // one reading from the cache: the pace comes from the elapsed time of the window
    let rd = aitrack::reading::Reading::from_value(&json!({
        "ok": true, "source": "desktop-cache", "ts": (now() - Duration::minutes(1)).to_rfc3339(),
        "session": {"pct": 55.0, "resets_at": (now() + Duration::minutes(200)).to_rfc3339()},
        "weekly": {"pct": 8.0, "resets_at": (now() + Duration::hours(149)).to_rfc3339()},
    }));
    let st = build_state(
        &[rd],
        Values::default(),
        Status {
            error: None,
            polling: false,
            next_in: None,
            polls: 1,
            sample: None,
            live: false,
        },
        now(),
    );
    let home = view::build_home(&st, now());
    let rows = &home.vendors[0].rows;
    // the warning lives in the vendor header now (the session would run out first)
    let w = home.vendors[0].warn.as_ref().unwrap();
    assert_eq!(w.kind, aitrack::reading::Kind::Session);
    assert!(
        w.full.starts_with("Empty in ~1h 2") && w.full.ends_with("resets in 3h 20m"),
        "{w:?}"
    );
    assert!(
        w.compact.starts_with("Empty in ~1h 2") && !w.compact.contains("resets"),
        "{w:?}"
    );
    // ... and the weekly window (8% in 19 h) is nowhere near the limit
    assert_eq!(rows.len(), 2);
    // tooltip lines are the same as before (reset line, then the pace note)
    assert!(
        rows[0].tip[0]
            .text
            .starts_with("Session resets in 3h 20m ("),
        "{:?}",
        rows[0].tip
    );
    assert!(
        rows[0].tip[1]
            .text
            .starts_with("Limit in ~1.4h at this pace"),
        "{:?}",
        rows[0].tip
    );
}

fn vendor_with(session: Option<Gauge>, weekly: Option<Gauge>) -> aitrack::vendor::Vendor {
    let mut v = aitrack::vendor::Vendor::new(aitrack::vendor::VendorId::Claude);
    v.session = session;
    v.weekly = weekly;
    v
}

#[test]
fn header_warning_picks_the_most_urgent_window_and_names_the_weekly_one() {
    use aitrack::reading::Kind;
    // only the session would run out
    let w = view::header_warning(&vendor_with(
        Some(gauge(55.0, Some(3.0 + 20.0 / 60.0), Some(33.0))),
        Some(gauge(8.0, Some(100.0), Some(0.4))),
    ))
    .unwrap();
    assert_eq!(w.kind, Kind::Session);
    assert_eq!(w.full, "Empty in ~1h 22m \u{b7} resets in 3h 20m");
    assert_eq!(w.compact, "Empty in ~1h 22m");
    // only the weekly would run out: it is named
    let wk = Gauge {
        window_hours: 168.0,
        ..gauge(80.0, Some(5.0 * 24.0), Some(0.4))
    }; // 50 h to empty
    let w = view::header_warning(&vendor_with(
        Some(gauge(10.0, Some(3.0), Some(1.0))),
        Some(wk.clone()),
    ))
    .unwrap();
    assert_eq!(w.kind, Kind::Weekly);
    assert_eq!(w.full, "Weekly: empty in ~2d 2h \u{b7} resets in 5d");
    assert_eq!(w.compact, "Weekly: empty in ~2d 2h");
    // both would run out: the sooner one wins (session: 15 min vs weekly: 2d 2h)
    let w = view::header_warning(&vendor_with(
        Some(gauge(70.0, Some(3.0), Some(120.0))),
        Some(wk),
    ))
    .unwrap();
    assert_eq!(
        (w.kind, w.compact.as_str()),
        (Kind::Session, "Empty in ~15m")
    );
    // the weekly wins when it is the more urgent one (1 h to empty vs 5 h for the session)
    let wk2 = Gauge {
        window_hours: 168.0,
        ..gauge(95.0, Some(50.0), Some(5.0))
    };
    let w = view::header_warning(&vendor_with(
        Some(gauge(50.0, Some(6.0), Some(10.0))),
        Some(wk2),
    ))
    .unwrap();
    assert_eq!(
        (w.kind, w.compact.as_str()),
        (Kind::Weekly, "Weekly: empty in ~1h")
    );
}

#[test]
fn no_header_warning_when_nothing_runs_out() {
    assert_eq!(
        view::header_warning(&vendor_with(Some(gauge(20.0, Some(3.0), Some(1.0))), None)),
        None
    );
    assert_eq!(view::header_warning(&vendor_with(None, None)), None);
    assert_eq!(
        view::header_warning(&vendor_with(Some(gauge(20.0, None, Some(50.0))), None)),
        None
    );
    let at_limit =
        view::header_warning(&vendor_with(Some(gauge(100.0, Some(2.0), None)), None)).unwrap();
    assert_eq!(
        (at_limit.compact.as_str(), at_limit.full.as_str()),
        ("At the limit", "At the limit \u{b7} resets in 2h")
    );
}

// ------------------------------------------------------------ live local polling, real path

#[test]
fn live_local_polling_through_the_default_chain_and_the_real_collector() {
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use std::sync::Arc;
    use std::time::{Duration as StdDuration, Instant};

    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !f() {
            assert!(
                t.elapsed() < StdDuration::from_secs(10),
                "timed out waiting for {what}"
            );
            std::thread::sleep(StdDuration::from_millis(20));
        }
    };
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let real_now = Utc::now();
    // history 13 h old (the real-PC situation), cache entry from 2 minutes ago
    let old = |m: i64, fh: f64| json!({"t": (real_now - Duration::minutes(m)).timestamp_millis(), "org": ORG_A, "u": {"fh": fh, "sd": 8.0}});
    write_history(&data, vec![old(790, 0.0), old(780, 0.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        real_now - Duration::minutes(2),
        None,
        &usage_body(55.0, 8.0),
    );
    b.finish();

    let usage = t.join("usage.jsonl");
    let poller = Poller::with_env(&env, "http://127.0.0.1:1/usage", None); // the real default chain
    let app = App::new(
        Box::new(JsonlStore::new(&usage)),
        Arc::new(Settings::load(t.join("s.json"))),
        poller,
    );
    app.collector
        .set_local_interval_override(Some(StdDuration::from_millis(100)));
    app.start();
    let rows = || JsonlStore::new(&usage).read();
    wait("first poll to store the cache reading", &|| {
        rows().iter().any(|r| r["source"] == "desktop-cache")
    });
    // (the rows of one poll are appended one after another: the cache reading can be on disk while
    // a backfilled history row is not yet, so count only after the poll has finished)
    wait("the first poll to finish", &|| {
        app.collector.status().polls >= 1
    });
    let n = rows().len();
    assert_eq!(n, 3, "2 backfilled history rows + the cache reading");

    // many local ticks with an unchanged cache: no duplicate rows
    wait("several local polls", &|| app.collector.status().polls >= 6);
    assert_eq!(rows().len(), n);
    let st = app.state();
    assert_eq!(
        st.plan.as_ref().unwrap().session.as_ref().unwrap().pct,
        55.0
    );
    assert!(app.collector.status().live);
    assert_eq!(
        app.collector.status().sample.unwrap().source,
        "desktop-cache"
    );

    // the app refreshes its cache: the next local tick picks it up (one new row, new number)
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        Utc::now() - Duration::minutes(1),
        None,
        &usage_body(60.0, 9.0),
    );
    b.finish();
    wait("the refreshed cache entry", &|| rows().len() == n + 1);
    wait("state to show it", &|| {
        app.state()
            .plan
            .is_some_and(|p| p.session.is_some_and(|g| g.pct == 60.0))
    });
    std::thread::sleep(StdDuration::from_millis(400));
    assert_eq!(rows().len(), n + 1);
    app.stop();
    app.collector.join();
}

#[test]
fn scanning_a_400_file_cache_stays_cheap_on_every_poll() {
    use std::time::Instant;
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    for i in 0..6 {
        b.add_usage(
            ORG_A,
            now() - Duration::minutes(i),
            None,
            &usage_body(10.0 + i as f64, 1.0),
        );
    }
    b.finish();
    // the rest of a realistic cache: lots of external f_ files and a couple of big block files
    for i in 100..490 {
        std::fs::write(dir.join(format!("f_{i:06x}")), vec![0xA5u8; 20_000]).unwrap();
    }
    std::fs::write(dir.join("data_3"), vec![0u8; 4 * 1024 * 1024]).unwrap();
    let files = std::fs::read_dir(&dir).unwrap().count();
    assert!(files >= 390, "{files}");

    let decodes_at_start = cache::decodes_on_this_thread();
    let t0 = Instant::now();
    let cold = cache::scan_usage_entries_at(&dir, now());
    let cold_t = t0.elapsed();
    let _ = cache::scan_usage_entries_cached_at(&dir, now()); // warms the decode memo
    assert!(
        cache::decodes_on_this_thread() > decodes_at_start,
        "the counter sees the work (scans decode on the calling thread)"
    );
    let t1 = Instant::now();
    let decoded_before = cache::decodes_on_this_thread();
    let mut warm = cache::scan_usage_entries_cached_at(&dir, now());
    for _ in 0..19 {
        warm = cache::scan_usage_entries_cached_at(&dir, now());
    }
    let warm_t = t1.elapsed() / 20;
    // the work done, not the wall time (a loaded CI machine is slow at reading 400 files): the
    // unchanged bodies are decoded zero times again after the warm-up scan
    assert_eq!(
        cache::decodes_on_this_thread(),
        decoded_before,
        "repeated scans must not decode unchanged bodies again"
    );
    eprintln!(
        "cache scan, {files} files: first {cold_t:?}, repeated (files re-read, body decode memoised by content hash) {warm_t:?} per scan"
    );
    assert_eq!(cold.entries.len(), 6);
    assert_eq!(warm.entries.len(), 6);
    assert!(warm_t < StdDurationLimit::MAX, "{warm_t:?}"); // only a pathology guard

    // a change anywhere is seen (nothing is skipped on size or mtime)
    let mut b2 = Block::new(&dir);
    b2.add_usage(ORG_A, now(), None, &usage_body(77.0, 1.0));
    b2.finish();
    let after = cache::scan_usage_entries_cached_at(&dir, now());
    assert_eq!(after.entries.len(), 1);
    assert_eq!(body_of(&after.entries[0])["five_hour"]["utilization"], 77.0);
}

struct StdDurationLimit;
impl StdDurationLimit {
    const MAX: std::time::Duration = std::time::Duration::from_secs(2);
}

#[test]
fn the_reported_weekly_one_percent_case_through_the_real_chain_gives_no_warning() {
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use std::sync::Arc;
    use std::time::{Duration as StdDuration, Instant};
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let real_now = Utc::now();
    // what the app's cache holds on the user's PC: whole-number percentages, weekly 1% with 6d 4h
    // left, session 55% with 3h 20m left
    let body = json!({
        "five_hour": {"utilization": 55, "resets_at": (real_now + Duration::minutes(200)).to_rfc3339()},
        "seven_day": {"utilization": 1, "resets_at": (real_now + Duration::hours(148)).to_rfc3339()},
    });
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(ORG_A, real_now - Duration::minutes(1), None, &body);
    b.finish();
    let app = App::new(
        Box::new(JsonlStore::new(t.join("usage.jsonl"))),
        Arc::new(Settings::load(t.join("s.json"))),
        Poller::with_env(&env, "http://127.0.0.1:1/usage", None),
    );
    app.start();
    let t0 = Instant::now();
    let home = loop {
        if let Some(plan) = app.state().plan {
            if plan.session.is_some() {
                break view::build_home(&app.state(), Utc::now());
            }
        }
        assert!(t0.elapsed() < StdDuration::from_secs(10), "no reading");
        std::thread::sleep(StdDuration::from_millis(20));
    };
    let claude = &home.vendors[0];
    // exactly one warning: the session (55% with no history: ~1h 22m), never the weekly window
    let w = claude.warn.as_ref().expect("session warning");
    assert_eq!(w.kind, aitrack::reading::Kind::Session);
    assert!(
        w.full.starts_with("Empty in ~1h 2") && w.full.ends_with("resets in 3h 19m")
            || w.full.ends_with("resets in 3h 20m"),
        "{w:?}"
    );
    assert_eq!(claude.rows[1].value, "1%");
    let weekly_tip: Vec<&str> = claude.rows[1].tip.iter().map(|l| l.text.as_str()).collect();
    assert!(
        weekly_tip
            .iter()
            .any(|l| l.starts_with("Pace: +0.") && l.ends_with("(avg since window start)")),
        "{weekly_tip:?}"
    );
    app.stop();
    app.collector.join();
}

// ------------------------------------------------------------ in-place updates (lazy mtime)

fn set_mtime(path: &Path, t: std::time::SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}

/// Replace `from` by `to` (same length) inside a file, in place, then put the old mtime back:
/// what a memory-mapped write by Chromium can look like on Windows.
fn rewrite_in_place(path: &Path, from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let meta = std::fs::metadata(path).unwrap();
    let (mtime, size) = (meta.modified().unwrap(), meta.len());
    let mut data = std::fs::read(path).unwrap();
    let at = data
        .windows(from.len())
        .position(|w| w == from)
        .expect("old bytes present");
    data[at..at + from.len()].copy_from_slice(to);
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.write_all(&data).unwrap();
    }
    set_mtime(path, mtime);
    let after = std::fs::metadata(path).unwrap();
    assert_eq!(
        (after.len(), after.modified().unwrap()),
        (size, mtime),
        "fixture must look untouched"
    );
}

fn five_of(scan: &cache::CacheScan) -> f64 {
    assert_eq!(scan.entries.len(), 1);
    body_of(&scan.entries[0])["five_hour"]["utilization"]
        .as_f64()
        .unwrap()
}

#[test]
fn a_blockfile_body_rewritten_in_place_with_the_same_size_and_mtime_is_seen() {
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(8),
        None,
        &usage_body(23.0, 4.0),
    );
    b.finish();
    // a long-lived caller scanning repeatedly (what the 60 s local tick does)
    assert_eq!(
        five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
        23.0
    );
    assert_eq!(
        five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
        23.0
    );
    rewrite_in_place(
        &dir.join("data_2"),
        b"\"utilization\":23.0",
        b"\"utilization\":25.0",
    );
    assert_eq!(
        five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
        25.0
    );
    // the headers (Date) can change in place too
    rewrite_in_place(
        &dir.join("data_1"),
        http_date(now() - Duration::minutes(8)).as_bytes(),
        http_date(now() - Duration::minutes(1)).as_bytes(),
    );
    let scan = cache::scan_usage_entries_cached_at(&dir, now());
    assert_eq!(scan.entries[0].observed, Some(now() - Duration::minutes(1)));
}

#[test]
fn a_simple_cache_file_rewritten_in_place_with_the_same_size_and_mtime_is_seen() {
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let body = usage_body(23.0, 4.0).to_string().into_bytes();
    let meta = response_info(
        &json_headers(now() - Duration::minutes(8), None),
        "200 OK",
        3,
    );
    simple_entry(&dir, &usage_url(ORG_A), &meta, &body, true);
    assert_eq!(
        five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
        23.0
    );
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    rewrite_in_place(&file, b"\"utilization\":23.0", b"\"utilization\":25.0");
    assert_eq!(
        five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
        25.0
    );
}

#[test]
fn the_running_chain_sees_an_in_place_update_through_the_same_source() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let real_now = Utc::now();
    let dir = cache_dir_in(&data);
    let body = |five: f64| {
        json!({
            "five_hour": {"utilization": five, "resets_at": (real_now + Duration::minutes(200)).to_rfc3339()},
            "seven_day": {"utilization": 4.0, "resets_at": (real_now + Duration::hours(100)).to_rfc3339()},
        })
    };
    let mut b = Block::new(&dir);
    b.add_usage(ORG_A, real_now - Duration::minutes(8), None, &body(23.0));
    b.finish();
    let src = DesktopCache { env };
    let first = src.read(real_now).unwrap();
    assert_eq!(first.latest.session.unwrap().pct, 23.0);
    rewrite_in_place(
        &dir.join("data_2"),
        b"\"utilization\":23.0",
        b"\"utilization\":25.0",
    );
    let second = src.read(real_now).unwrap();
    assert_eq!(
        second.latest.session.unwrap().pct,
        25.0,
        "the long-lived source must not serve the old parse"
    );
}

// ------------------------------------------------------------ choosing between entries

fn two_entries(
    first: (i64, Option<i64>, f64),
    second: (i64, Option<i64>, f64),
) -> cache::CacheScan {
    // (Date header minutes ago, creation minutes ago, five_hour %)
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    for (date, created, five) in [first, second] {
        b.created = created.map(|m| (now() - Duration::minutes(m)).timestamp());
        b.add_usage(
            ORG_A,
            now() - Duration::minutes(date),
            None,
            &usage_body(five, 4.0),
        );
    }
    b.finish();
    let scan = cache::scan_usage_entries_at(&dir, now());
    std::mem::forget(t); // keep the files for the scan above only
    scan
}

fn picked_five(scan: &cache::CacheScan) -> (f64, String) {
    let p = cache::pick_best(&scan.entries).unwrap();
    (
        body_of(p.entry)["five_hour"]["utilization"]
            .as_f64()
            .unwrap(),
        p.reason,
    )
}

#[test]
fn an_older_date_header_with_a_newer_creation_time_carries_the_newer_body() {
    // entry 1: Date 10 min ago but stored 1 min ago (revalidated / Date earlier than the write), 25 %
    // entry 2: Date 5 min ago, 23 %
    let scan = two_entries((10, Some(1), 25.0), (5, None, 23.0));
    let (five, reason) = picked_five(&scan);
    assert_eq!(five, 25.0, "{reason}");
    assert_eq!(scan.entries[0].observed, Some(now() - Duration::minutes(1)));
    // and the other way round, insertion order swapped
    let scan = two_entries((5, None, 23.0), (10, Some(1), 25.0));
    assert_eq!(picked_five(&scan).0, 25.0);
}

#[test]
fn the_newer_date_header_wins_when_the_creation_time_is_older() {
    let scan = two_entries((1, None, 25.0), (9, Some(9), 23.0));
    assert_eq!(picked_five(&scan).0, 25.0);
    let scan = two_entries((9, Some(9), 23.0), (1, None, 25.0));
    assert_eq!(picked_five(&scan).0, 25.0);
}

#[test]
fn simultaneous_entries_prefer_the_higher_utilization_in_the_same_window() {
    for order in [
        ((2, None, 23.0), (2, None, 25.0)),
        ((2, None, 25.0), (2, None, 23.0)),
    ] {
        let scan = two_entries(order.0, order.1);
        let (five, reason) = picked_five(&scan);
        assert_eq!(five, 25.0);
        assert!(reason.contains("higher utilization"), "{reason}");
    }
    // within 30 seconds counts as simultaneous, a minute apart does not
    let scan = two_entries((2, None, 23.0), (1, None, 22.0));
    assert_eq!(
        picked_five(&scan).0,
        22.0,
        "a minute newer wins even with a lower figure"
    );
}

#[test]
fn a_garbage_creation_time_is_ignored() {
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    b.created = Some(-5_000_000_000); // not a believable date
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(3),
        None,
        &usage_body(10.0, 1.0),
    );
    b.finish();
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert_eq!(scan.entries[0].created, None);
    assert_eq!(scan.entries[0].observed, Some(now() - Duration::minutes(3)));
}

/// Review F9: "believable" creation times are judged against the time the caller passes, not the
/// real clock, so the result does not depend on the date the test runs.
#[test]
fn the_creation_time_bound_uses_the_given_now() {
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    b.created = Some((now() - Duration::minutes(1)).timestamp());
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(3),
        None,
        &usage_body(10.0, 1.0),
    );
    b.finish();
    let at = |n: DateTime<Utc>| cache::scan_usage_entries_at(&dir, n).entries[0].created;
    assert_eq!(at(now()), Some(now() - Duration::minutes(1)));
    // seen from a time long before the entry was written, it is not believable
    assert_eq!(at(now() - Duration::days(30)), None);
}

#[test]
fn sources_lists_every_matching_entry_with_masked_keys_and_the_selection() {
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let real_now = Utc::now();
    let dir = cache_dir_in(&data);
    let body = |five: f64| {
        json!({
            "five_hour": {"utilization": five, "resets_at": (real_now + Duration::minutes(200)).to_rfc3339()},
            "seven_day": {"utilization": 4.0, "resets_at": (real_now + Duration::hours(100)).to_rfc3339()},
        })
    };
    let mut b = Block::new(&dir);
    b.add_usage(
        ORG_A,
        real_now - Duration::minutes(9),
        Some("gzip"),
        &body(23.0),
    );
    b.created = Some((real_now - Duration::minutes(1)).timestamp());
    b.add_usage(ORG_A, real_now - Duration::minutes(12), None, &body(25.0));
    b.finish();
    write_history(&data, vec![hist_sample(3, ORG_A, 24.0, 4.0)]);
    let text = sources::diagnose(&env, real_now);
    assert!(text.contains("entry 1: blockfile in data_1"), "{text}");
    assert!(text.contains("entry 2: blockfile in data_1"), "{text}");
    assert!(text.contains("<org>/usage"), "{text}");
    assert!(
        !text.contains(ORG_A),
        "the org id must never be printed: {text}"
    );
    assert!(
        text.contains("Date header:")
            && text.contains("creation time:")
            && text.contains("file mtime:"),
        "{text}"
    );
    assert!(
        text.contains("selected: entry 2 (newest timestamp)"),
        "{text}"
    );
    assert!(text.contains("chain verdict:"), "{text}");
    assert!(text.contains("five_hour 25% used"), "{text}");
}

#[test]
fn scanning_four_8mb_block_files_on_every_poll_stays_cheap() {
    use std::time::Instant;
    let t = TempDir::new();
    let dir = cache_dir(&t);
    let mut b = Block::new(&dir);
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(2),
        Some("zstd"),
        &usage_body(25.0, 4.0),
    );
    b.finish();
    for i in 3..7 {
        // a real block file header (so every entry slot is examined) followed by empty blocks
        let mut f = block_header(256, i as u16, &[]);
        f.resize(8 * 1024 * 1024, 0);
        std::fs::write(dir.join(format!("data_{i}")), f).unwrap();
    }
    for i in 100..490 {
        std::fs::write(dir.join(format!("f_{i:06x}")), vec![0xA5u8; 20_000]).unwrap();
    }
    let t0 = Instant::now();
    let n = 10;
    for _ in 0..n {
        assert_eq!(
            five_of(&cache::scan_usage_entries_cached_at(&dir, now())),
            25.0
        );
    }
    let per = t0.elapsed() / n;
    eprintln!("full re-read of 4 x 8 MB block files + 400 other files: {per:?} per scan");
    assert!(per < std::time::Duration::from_millis(400), "{per:?}");
}

#[test]
fn a_history_sample_whose_percentage_dropped_does_not_borrow_a_reset_time() {
    // the window the cache entry describes has ended (usage fell from 50 to 20): nothing is invented
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    write_history(&data, vec![hist_sample(1, ORG_A, 20.0, 5.0)]);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(40),
        None,
        &usage_body(50.0, 15.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let rec = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(rec["source"], "desktop-history");
    assert!(rec["session"]["resets_at"].is_null(), "{rec}");
    assert!(rec["session"].get("resets_carried").is_none());
}

#[test]
fn a_reset_time_from_the_stored_history_is_carried_across_polls_and_restarts() {
    // poll 1: the cache entry reports resets; poll 2 (a new Poller, as after a restart): only the
    // newer history sample is available, the reset time comes from usage.jsonl
    let t = TempDir::new();
    let (env, data) = env_with_data_dir(&t);
    let mut b = Block::new(&cache_dir_in(&data));
    b.add_usage(
        ORG_A,
        now() - Duration::minutes(8),
        None,
        &usage_body(50.0, 15.0),
    );
    b.finish();
    let store = JsonlStore::new(t.join("usage.jsonl"));
    let first = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(first["source"], "desktop-cache");
    assert!(first["session"]["resets_at"].is_string());
    // the cache entry goes away (the app is closed and its cache cleared); a newer history sample exists
    std::fs::remove_dir_all(cache_dir_in(&data)).unwrap();
    write_history(&data, vec![hist_sample(1, ORG_A, 55.0, 16.0)]);
    let second = chain(&env).poll_at(&store, now()).unwrap();
    assert_eq!(second["source"], "desktop-history");
    assert_eq!(second["session"]["resets_carried"], true, "{second}");
    assert_eq!(
        second["session"]["resets_at"],
        first["session"]["resets_at"]
    );
    let note = second["note"].as_str().unwrap();
    assert!(note.contains("desktop-cache skipped"), "{note}");
}
