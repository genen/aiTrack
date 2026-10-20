// SPDX-License-Identifier: GPL-3.0-or-later
//! Usage history retention: 90-day pruning of the JSONL files (see `store::Store::prune`).
use crate::common;
use aitrack::collector::{prune_due, Collector};
use aitrack::settings::Settings;
use aitrack::store::{expired, prune_cutoff, History, JsonlStore, PruneReport, PruneStep, Store};
use chrono::{DateTime, Duration, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Arc;

fn now() -> DateTime<Utc> {
    "2026-10-20T12:00:00Z".parse().unwrap()
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

/// A Claude-shaped record observed `days_ago` days before `now()`, with a 5-hour session window.
fn rec(days_ago: i64) -> Value {
    let at = now() - Duration::days(days_ago);
    json!({"v": 1, "ts": iso(at), "ok": true,
           "session": {"pct": 10.0, "resets_at": iso(at + Duration::hours(5))},
           "weekly": null, "raw": {"five_hour": {"utilization": 10.0}}})
}

fn write_lines(path: &std::path::Path, lines: &[String]) {
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
}

fn line(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn expiry_follows_age_and_billing_window() {
    let cutoff = prune_cutoff(now());
    assert_eq!(cutoff, now() - Duration::days(90));
    assert!(expired(&rec(91), cutoff));
    assert!(!expired(&rec(89), cutoff));
    // observed_at wins over ts
    let mut r = rec(200);
    r["observed_at"] = json!(iso(now() - Duration::days(10)));
    assert!(!expired(&r, cutoff));
    // an old reading whose window ends after the cutoff is kept (the window is not cut in half)
    let mut r = rec(95);
    r["weekly"] = json!({"pct": 50.0, "resets_at": iso(now() - Duration::days(88))});
    assert!(!expired(&r, cutoff));
    let mut r = rec(120);
    r["bot_weekly"] = json!({"pct": 50.0, "resets_at": iso(now() + Duration::days(2))});
    assert!(!expired(&r, cutoff));
    // never delete what cannot be dated
    assert!(!expired(&json!({"ok": true}), cutoff));
    assert!(!expired(&json!({"ts": "garbage"}), cutoff));
}

#[test]
fn prune_removes_old_records_and_keeps_recent_lines_byte_for_byte() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    // key order and spacing of kept lines must survive untouched
    let odd = r#"{"ts":  "2026-10-19T00:00:00+00:00", "ok": true, "zzz": 1, "aaa": 2}"#.to_string();
    write_lines(
        &path,
        &[line(&rec(400)), line(&rec(100)), odd.clone(), line(&rec(1))],
    );
    let before = std::fs::read(&path).unwrap();
    let store = JsonlStore::new(&path);
    let rep = store.prune(now()).unwrap();
    assert_eq!(rep.removed, 2);
    assert_eq!(rep.kept, 2);
    assert_eq!(rep.corrupt, 0);
    assert_eq!(rep.bytes_before, before.len() as u64);
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after, format!("{odd}\n{}\n", line(&rec(1))));
    assert_eq!(rep.bytes_after, after.len() as u64);
    // the previous file is kept as .bak, no temp files are left
    assert_eq!(std::fs::read(store.sibling("bak")).unwrap(), before);
    assert!(!store.sibling("tmp").exists());
    assert!(!store.sibling("bak.tmp").exists());
    assert!(rep
        .describe()
        .contains("removed 2 records older than 90 days"));
}

#[test]
fn second_run_changes_nothing() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    write_lines(&path, &[line(&rec(300)), line(&rec(5)), "{torn".into()]);
    let store = JsonlStore::new(&path);
    assert!(store.prune(now()).unwrap().changed());
    let file = std::fs::read(&path).unwrap();
    let bak = std::fs::read(store.sibling("bak")).unwrap();
    let corrupt = std::fs::read(store.sibling("corrupt")).unwrap();
    let again = store.prune(now()).unwrap();
    assert!(!again.changed());
    assert_eq!(again.kept, 1);
    assert_eq!(again.bytes_before, again.bytes_after);
    assert_eq!(std::fs::read(&path).unwrap(), file);
    assert_eq!(std::fs::read(store.sibling("bak")).unwrap(), bak);
    assert_eq!(std::fs::read(store.sibling("corrupt")).unwrap(), corrupt);
}

#[test]
fn torn_last_line_and_junk_go_to_corrupt_file() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        "{}\nnot json\n[1,2]\n\n{}\n{{\"ts\":\"2026-10-",
        line(&rec(2)),
        line(&rec(1))
    )
    .unwrap();
    drop(f);
    let store = JsonlStore::new(&path);
    let rep = store.prune(now()).unwrap();
    assert_eq!(rep.corrupt, 3); // "not json", the array, the torn line; the blank line is dropped
    assert_eq!(rep.removed, 0);
    assert_eq!(rep.kept, 2);
    assert_eq!(
        std::fs::read_to_string(store.sibling("corrupt")).unwrap(),
        "not json\n[1,2]\n{\"ts\":\"2026-10-\n"
    );
    assert_eq!(store.read(), vec![rec(2), rec(1)]);
    // the rewritten file ends with a newline, so appends start on a fresh line
    store.append(&rec(0)).unwrap();
    assert_eq!(store.read().len(), 3);
}

#[test]
fn missing_file_is_nothing_to_do() {
    let dir = TempDir::new();
    let store = JsonlStore::new(dir.join("none.jsonl"));
    assert_eq!(store.prune(now()).unwrap(), PruneReport::default());
    assert!(!dir.join("none.jsonl").exists());
}

#[test]
fn crash_at_any_step_leaves_a_valid_file_and_the_next_run_finishes() {
    let steps = [
        PruneStep::CorruptSaved,
        PruneStep::TempWritten,
        PruneStep::BackupWritten,
        PruneStep::Replaced,
    ];
    let lines = vec![
        line(&rec(200)),
        line(&rec(30)),
        "junk".into(),
        line(&rec(1)),
    ];
    let all = vec![rec(200), rec(30), rec(1)];
    let pruned = vec![rec(30), rec(1)];
    for step in steps {
        let dir = TempDir::new();
        let path = dir.join("usage.jsonl");
        write_lines(&path, &lines);
        let store = JsonlStore::new(&path);
        // "crash": stop right after `step`
        store.prune_with(now(), &mut |s| s != step).unwrap();
        let seen = store.read();
        assert!(
            seen == all || seen == pruned,
            "after a crash at {step:?} the file holds the old or the new records, got {seen:?}"
        );
        // the next run completes the job, and the result is the same as an uninterrupted run
        store.prune(now()).unwrap();
        assert_eq!(store.read(), pruned, "{step:?}");
        assert!(!store.sibling("tmp").exists(), "{step:?}");
        let corrupt = std::fs::read_to_string(store.sibling("corrupt")).unwrap();
        assert!(corrupt.lines().all(|l| l == "junk"), "{step:?}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("junk"));
    }
}

#[test]
fn stale_temp_file_from_a_crash_is_ignored() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    write_lines(&path, &[line(&rec(200)), line(&rec(3))]);
    let store = JsonlStore::new(&path);
    std::fs::write(store.sibling("tmp"), b"{\"half\":").unwrap();
    store.prune(now()).unwrap();
    assert_eq!(store.read(), vec![rec(3)]);
    assert!(!store.sibling("tmp").exists());
}

#[test]
fn record_appended_during_the_rewrite_is_kept() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    write_lines(&path, &[line(&rec(200)), line(&rec(3))]);
    let store = JsonlStore::new(&path);
    let other = JsonlStore::new(&path); // e.g. a second process
    store
        .prune_with(now(), &mut |s| {
            if s == PruneStep::TempWritten || s == PruneStep::BackupWritten {
                other.append(&rec(0)).unwrap();
            }
            true
        })
        .unwrap();
    assert_eq!(store.read(), vec![rec(3), rec(0), rec(0)]);
}

#[test]
fn appends_racing_prunes_through_history_are_never_lost() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    write_lines(
        &path,
        &(0..50).map(|i| line(&rec(100 + i))).collect::<Vec<_>>(),
    );
    let h = Arc::new(History::new(Box::new(JsonlStore::new(&path))));
    h.load();
    let writer = {
        let h = h.clone();
        std::thread::spawn(move || {
            for i in 0..200 {
                let mut r = rec(0);
                r["n"] = json!(i);
                h.append(&r).unwrap();
            }
        })
    };
    for _ in 0..20 {
        h.prune(now()).unwrap();
    }
    writer.join().unwrap();
    h.prune(now()).unwrap();
    let on_disk = h.read();
    let ns: Vec<i64> = on_disk
        .iter()
        .filter_map(|v| v.get("n").and_then(Value::as_i64))
        .collect();
    assert_eq!(ns, (0..200).collect::<Vec<_>>());
    assert_eq!(on_disk.len(), 200, "all old records are gone");
    assert_eq!(h.try_with_readings(|r| r.len()), Some(200));
}

#[test]
fn history_prune_reloads_readings_and_bumps_version() {
    let dir = TempDir::new();
    let path = dir.join("usage.jsonl");
    write_lines(&path, &[line(&rec(150)), line(&rec(2)), line(&rec(1))]);
    let h = History::new(Box::new(JsonlStore::new(&path)));
    h.load();
    let v0 = h.version();
    assert_eq!(h.try_with_readings(|r| r.len()), Some(3));
    let rep = h.prune(now()).unwrap();
    assert_eq!(rep.removed, 1);
    assert_eq!(h.try_with_readings(|r| r.len()), Some(2));
    assert!(h.version() > v0);
    // nothing to do: no reload, same version
    let v1 = h.version();
    assert!(!h.prune(now()).unwrap().changed());
    assert_eq!(h.version(), v1);
    assert_eq!(h.size(), Some(std::fs::metadata(&path).unwrap().len()));
}

#[test]
fn prune_is_due_daily_by_the_wall_clock() {
    let t = now();
    assert!(prune_due(None, t));
    assert!(!prune_due(Some(t - Duration::hours(23)), t));
    assert!(prune_due(Some(t - Duration::hours(24)), t));
    // an app left open for months still prunes
    assert!(prune_due(Some(t - Duration::days(95)), t));
    // the clock went backwards: run rather than wait
    assert!(prune_due(Some(t + Duration::hours(1)), t));
}

#[test]
fn collector_prunes_its_store_and_extra_files_together() {
    let dir = TempDir::new();
    let main = dir.join("usage-grok.jsonl");
    let bot = dir.join("usage-grok-bot.jsonl");
    write_lines(&main, &[line(&rec(120)), line(&rec(1))]);
    write_lines(&bot, &[line(&rec(100)), line(&rec(99)), line(&rec(1))]);
    let store: Arc<dyn Store> = Arc::new(History::new(Box::new(JsonlStore::new(&main))));
    let settings = Arc::new(Settings::load(dir.join("s.json")));
    let c = Collector::new(store, settings, Box::new(|_| Ok(Value::Null)));
    let bot_store = JsonlStore::new(&bot);
    c.add_pruner("bot", move |now| bot_store.prune(now));
    let reports = c.prune_blocking(now());
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].1.as_ref().map(|r| r.removed), Ok(1));
    assert_eq!(reports[1].0, "bot");
    assert_eq!(reports[1].1.as_ref().map(|r| r.removed), Ok(2));
}
