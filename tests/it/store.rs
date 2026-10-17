// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::store::{History, JsonlStore, Store};
use common::TempDir;
use serde_json::json;
use std::io::Write;

#[test]
fn store_skips_torn_lines() {
    let dir = TempDir::new();
    let store = JsonlStore::new(dir.join("u.jsonl"));
    store.append(&json!({"a": 1})).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("u.jsonl"))
        .unwrap()
        .write_all(b"{\"b\":")
        .unwrap();
    assert_eq!(store.read(), vec![json!({"a": 1})]);
    // appending after a torn line must not corrupt the new record
    store.append(&json!({"c": 3})).unwrap();
    assert_eq!(store.read(), vec![json!({"a": 1}), json!({"c": 3})]);
}

#[test]
fn store_creates_parent_dirs_and_reads_missing_as_empty() {
    let dir = TempDir::new();
    let store = JsonlStore::new(dir.join("deep/er/u.jsonl"));
    assert!(store.read().is_empty());
    store.append(&json!({"x": true})).unwrap();
    assert_eq!(store.read().len(), 1);
}

#[test]
fn store_format_is_compact_lines() {
    let dir = TempDir::new();
    let store = JsonlStore::new(dir.join("u.jsonl"));
    store
        .append(&json!({"v": 1, "ts": "t", "ok": false, "error": "x"}))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("u.jsonl")).unwrap(),
        "{\"v\":1,\"ts\":\"t\",\"ok\":false,\"error\":\"x\"}\n"
    );
}

#[test]
fn history_loads_existing_and_tracks_appends() {
    let dir = TempDir::new();
    let path = dir.join("u.jsonl");
    JsonlStore::new(&path)
        .append(&json!({"v":1,"ts":"2026-10-14T10:00:00+00:00","ok":true,"session":{"pct":5.0,"resets_at":"2026-10-14T15:00:00+00:00"},"weekly":null,"raw":{"big":"payload"}}))
        .unwrap();
    let h = History::new(Box::new(JsonlStore::new(&path)));
    assert!(h.try_with_readings(|r| r.len()).is_none()); // not loaded yet
    h.load();
    assert_eq!(h.try_with_readings(|r| r.len()), Some(1));
    let v0 = h.version();
    h.append(&json!({"v":1,"ts":"2026-10-14T10:15:00+00:00","ok":false,"error":"HTTP 500"}))
        .unwrap();
    assert!(h.version() > v0);
    let (n, failed) = h.with_readings(|r| (r.len(), r[1].failed));
    assert_eq!((n, failed), (2, true));
    assert_eq!(JsonlStore::new(&path).read().len(), 2);
}
