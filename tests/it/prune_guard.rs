// SPDX-License-Identifier: GPL-3.0-or-later
//! The clock guard of the 90-day prune (see `store::PruneGuard`): a prune that would delete more
//! than 30 days of history at once is held, the user is warned, and it runs on the next launch.
use crate::common;
use aitrack::alerts::{Notification, Notifier};
use aitrack::app::{App, PRUNE_GUARD_FILE};
use aitrack::collector::Collector;
use aitrack::settings::Settings;
use aitrack::store::{
    History, JsonlStore, PruneGuard, Store, CLOCK_WARNING, GUARD_DAYS, KEEP_DAYS,
};
use chrono::{DateTime, Duration, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn now() -> DateTime<Utc> {
    "2026-10-20T12:00:00Z".parse().unwrap()
}

fn rec(days_ago: i64) -> Value {
    let at = now() - Duration::days(days_ago);
    json!({"v": 1, "ts": at.to_rfc3339(), "ok": true,
           "session": {"pct": 10.0, "resets_at": (at + Duration::hours(5)).to_rfc3339()}})
}

/// One record a day, `from` to `to` days before `now()`.
fn daily(path: &Path, days: std::ops::RangeInclusive<i64>) {
    let mut f = std::fs::File::create(path).unwrap();
    for d in days {
        writeln!(f, "{}", rec(d)).unwrap();
    }
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

fn sibling(path: &Path, ext: &str) -> std::path::PathBuf {
    JsonlStore::new(path).sibling(ext)
}

/// A collector over `path` with the guard saved at `guard_path` (a fresh load = a new launch).
fn launch(path: &Path, guard_path: &Path) -> (Collector, Arc<PruneGuard>) {
    let dir = path.parent().unwrap();
    let store: Arc<dyn Store> = Arc::new(History::new(Box::new(JsonlStore::new(path))));
    let c = Collector::new(
        store,
        Arc::new(Settings::load(dir.join("s.json"))),
        Box::new(|_| Ok(Value::Null)),
    );
    let guard = Arc::new(PruneGuard::load(guard_path));
    c.set_prune_guard(guard.clone());
    (c, guard)
}

fn report(c: &Collector, at: DateTime<Utc>) -> aitrack::store::PruneReport {
    c.prune_blocking(at)[0].1.clone().unwrap()
}

#[test]
fn a_normal_daily_prune_is_not_held() {
    let d = TempDir::new();
    let path = d.join("usage.jsonl");
    // yesterday's prune left 90 days; today one more day falls out
    daily(&path, 1..=91);
    let rep = JsonlStore::new(&path)
        .prune_limited(now(), Some(GUARD_DAYS))
        .unwrap();
    assert!(!rep.held());
    assert_eq!(rep.removed, 1);
    assert_eq!(lines(&path), 90);
}

#[test]
fn days_are_counted_as_distinct_days_so_a_stray_old_record_does_not_hold_it() {
    let d = TempDir::new();
    let path = d.join("usage.jsonl");
    daily(&path, 1..=91);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    // several readings on one day, and one from years ago: three distinct days in all
    for h in 0..20 {
        let mut r = rec(92);
        r["ts"] = json!((now() - Duration::days(92) + Duration::minutes(h)).to_rfc3339());
        writeln!(f, "{r}").unwrap();
    }
    writeln!(f, "{}", rec(2000)).unwrap();
    drop(f);
    let rep = JsonlStore::new(&path)
        .prune_limited(now(), Some(GUARD_DAYS))
        .unwrap();
    assert!(!rep.held());
    assert_eq!(rep.removed, 22);
}

#[test]
fn a_prune_of_40_days_is_held_and_touches_nothing() {
    let d = TempDir::new();
    let path = d.join("usage.jsonl");
    daily(&path, 1..=(KEEP_DAYS + 40));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"not json\n")
        .unwrap();
    let bak = sibling(&path, "bak");
    std::fs::write(&bak, "the last good backup\n").unwrap();
    let before = std::fs::read(&path).unwrap();
    let rep = JsonlStore::new(&path)
        .prune_limited(now(), Some(GUARD_DAYS))
        .unwrap();
    assert!(rep.held());
    assert!(!rep.changed());
    assert_eq!(rep.held_days, 40);
    assert_eq!(rep.held_records, 40);
    assert_eq!(rep.removed, 0);
    assert!(rep.describe().contains(CLOCK_WARNING), "{}", rep.describe());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        "the last good backup\n"
    );
    assert!(!sibling(&path, "tmp").exists());
    assert!(!sibling(&path, "corrupt").exists());
    // exactly 30 days is not more than 30
    daily(&path, 1..=(KEEP_DAYS + 30));
    let rep = JsonlStore::new(&path)
        .prune_limited(now(), Some(GUARD_DAYS))
        .unwrap();
    assert!(!rep.held());
    assert_eq!(rep.removed, 30);
}

#[test]
fn the_flag_is_saved_read_back_after_a_restart_and_cleared_once_the_prune_ran() {
    let d = TempDir::new();
    let path = d.join("usage.jsonl");
    let guard_path = d.join(PRUNE_GUARD_FILE);
    daily(&path, 1..=(KEEP_DAYS + 40));
    let before = std::fs::read(&path).unwrap();
    let name = JsonlStore::new(&path).describe();

    // first launch: held, the flag is on disk, the guard says so once
    let (c, guard) = launch(&path, &guard_path);
    assert!(report(&c, now()).held());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!sibling(&path, "bak").exists());
    assert!(guard.held_this_run());
    assert!(guard.take_first_hold());
    assert!(!guard.take_first_hold());
    assert_eq!(guard.pending(), vec![name.clone()]);
    let saved: Value = serde_json::from_slice(&std::fs::read(&guard_path).unwrap()).unwrap();
    assert_eq!(saved, json!({"prune_on_next_launch": [name.clone()]}));
    // a second prune in the same run (daily, or "Prune now") is held again, without a new warning
    assert!(report(&c, now()).held());
    assert!(!guard.take_first_hold());
    drop(c);

    // next launch, the clock still far ahead: the user was warned, the prune runs once
    let (c, guard) = launch(&path, &guard_path);
    assert!(!guard.held_this_run());
    assert_eq!(guard.limit_for(&name), None);
    let rep = report(&c, now());
    assert!(!rep.held());
    assert_eq!(rep.removed, 40);
    assert_eq!(lines(&path), KEEP_DAYS as usize);
    assert!(sibling(&path, "bak").exists(), "the .bak is kept as before");
    assert!(!guard_path.exists(), "the flag is cleared");
    assert!(guard.pending().is_empty());
    // and the guard holds again
    assert_eq!(guard.limit_for(&name), Some(GUARD_DAYS));
    let later = now() + Duration::days(45);
    let rep = report(&c, later);
    assert!(rep.held());
    assert!(guard_path.exists());
    drop(c);

    // a third launch with nothing pending is guarded from the start
    std::fs::remove_file(&guard_path).unwrap();
    let (c, guard) = launch(&path, &guard_path);
    assert!(report(&c, later).held());
    assert!(guard.held_this_run());
}

#[test]
fn a_clock_fixed_before_the_next_launch_prunes_only_what_is_really_old() {
    let d = TempDir::new();
    let path = d.join("usage.jsonl");
    let guard_path = d.join(PRUNE_GUARD_FILE);
    // 89 days of use, and one reading that is really older than 90 days
    daily(&path, 1..=89);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("{}\n", rec(95)).as_bytes())
        .unwrap();
    // the clock jumped 60 days ahead
    let (c, _) = launch(&path, &guard_path);
    let rep = report(&c, now() + Duration::days(60));
    assert!(rep.held());
    assert_eq!(rep.held_days, 60);
    assert_eq!(lines(&path), 90);
    drop(c);
    // fixed before the next launch: the flagged prune is worked out again with the real clock
    let (c, guard) = launch(&path, &guard_path);
    let rep = report(&c, now());
    assert!(!rep.held());
    assert_eq!(rep.removed, 1, "only the reading older than 90 days goes");
    assert_eq!(lines(&path), 89);
    assert!(!guard_path.exists());
    assert_eq!(
        guard.limit_for(&JsonlStore::new(&path).describe()),
        Some(GUARD_DAYS)
    );
}

#[test]
fn extra_history_files_are_guarded_each_on_its_own() {
    let d = TempDir::new();
    let main = d.join("usage-grok.jsonl");
    let bot = d.join("usage-grok-bot.jsonl");
    let guard_path = d.join(PRUNE_GUARD_FILE);
    daily(&main, 1..=91);
    daily(&bot, 1..=(KEEP_DAYS + 50));
    let bot_before = std::fs::read(&bot).unwrap();
    let (c, guard) = launch(&main, &guard_path);
    let bot_store = JsonlStore::new(&bot);
    c.add_pruner("bot", move |now, limit| bot_store.prune_limited(now, limit));
    let r = c.prune_blocking(now());
    assert_eq!(r[0].1.as_ref().unwrap().removed, 1, "the main file prunes");
    assert!(r[1].1.as_ref().unwrap().held(), "the bot file is held");
    assert_eq!(std::fs::read(&bot).unwrap(), bot_before);
    assert_eq!(guard.pending(), vec!["bot".to_string()]);
}

#[test]
fn an_unreadable_or_missing_flag_file_means_nothing_pending() {
    let d = TempDir::new();
    let p = d.join(PRUNE_GUARD_FILE);
    assert!(PruneGuard::load(&p).pending().is_empty());
    std::fs::write(&p, "{garbage").unwrap();
    let g = PruneGuard::load(&p);
    assert!(g.pending().is_empty());
    assert_eq!(g.limit_for("x"), Some(GUARD_DAYS));
}

// ---- the app: toast, Diagnostics line, every history file ---------------------------------------

#[derive(Default)]
struct Recorder(Mutex<Vec<Notification>>);

impl Notifier for Recorder {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

fn app_over(d: &TempDir) -> Arc<App> {
    Arc::new(
        App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("usage.jsonl"))),
            Arc::new(Settings::load(d.join("settings.json"))),
            Box::new(|_| Ok(Value::Null)),
        )
        .with_grok(
            Box::new(JsonlStore::new(d.join("usage-grok.jsonl"))),
            Box::new(|_| Ok(Value::Null)),
            None,
            Arc::new(|| false),
        )
        .with_codex(
            Box::new(JsonlStore::new(d.join("usage-codex.jsonl"))),
            Box::new(|_| Ok(Value::Null)),
            None,
            Arc::new(|| false),
        ),
    )
}

#[test]
fn the_app_warns_once_with_a_toast_and_the_diagnostics_line() {
    let d = TempDir::new();
    daily(&d.join("usage.jsonl"), 1..=(KEEP_DAYS + 40));
    let app = app_over(&d);
    let rec = Arc::new(Recorder::default());
    app.set_notifier(rec.clone());
    assert_eq!(app.prune_warning(), None);
    app.collector.prune_blocking(now());
    {
        let sent = rec.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body, CLOCK_WARNING);
    }
    assert_eq!(
        CLOCK_WARNING,
        "Check Clock about to delete 30+ days of history on next launch."
    );
    assert_eq!(app.prune_warning(), Some(CLOCK_WARNING));
    assert!(app.take_show_diagnostics());
    assert!(!app.take_show_diagnostics());
    assert!(app.take_raise(), "the window comes to the front");
    assert!(d.join(PRUNE_GUARD_FILE).exists(), "next to settings.json");
    // another history file held in the same run: no second toast
    daily(&d.join("usage-codex.jsonl"), 1..=(KEEP_DAYS + 35));
    app.codex.as_ref().unwrap().collector.prune_blocking(now());
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    assert!(!app.take_show_diagnostics());
    let pending = app.prune_guard().pending();
    assert_eq!(pending.len(), 2, "{pending:?}");
}

#[test]
fn every_vendor_collector_goes_through_the_guard() {
    let d = TempDir::new();
    daily(&d.join("usage-grok.jsonl"), 1..=(KEEP_DAYS + 40));
    let before = std::fs::read(d.join("usage-grok.jsonl")).unwrap();
    let app = app_over(&d);
    app.set_notifier(Arc::new(Recorder::default()));
    let r = app.grok.as_ref().unwrap().collector.prune_blocking(now());
    assert!(r[0].1.as_ref().unwrap().held());
    assert_eq!(std::fs::read(d.join("usage-grok.jsonl")).unwrap(), before);
    assert_eq!(app.prune_warning(), Some(CLOCK_WARNING));
}

#[cfg(feature = "gui")]
mod view {
    use super::*;
    use aitrack::ui::{AitrackUi, Screen};
    use eframe::egui::{self, Context, Pos2, RawInput, Rect, ViewportId, ViewportInfo};

    fn frame(ctx: &Context, ui: &mut AitrackUi, t: f64, h: f32) {
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, h))),
            time: Some(t),
            viewports: std::iter::once((ViewportId::ROOT, ViewportInfo::default())).collect(),
            ..Default::default()
        };
        let _ = ctx.run(raw, |c| ui.draw(c));
    }

    #[test]
    fn a_held_prune_opens_settings_at_the_diagnostics_warning() {
        let d = TempDir::new();
        daily(&d.join("usage.jsonl"), 1..=(KEEP_DAYS + 40));
        let app = app_over(&d);
        app.set_notifier(Arc::new(Recorder::default()));
        // every other section open too, so the Diagnostics are far down and must be scrolled to
        for n in aitrack::settings::Sections::NAMES {
            if n != "diagnostics" {
                app.settings.set_section(n, true);
            }
        }
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app.clone());
        ui.sandbox();
        let h = 400.0;
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.2;
            frame(&ctx, &mut ui, t, h);
        }
        assert_eq!(ui.screen, Screen::Home);
        assert_eq!(ui.prune_warning_rect, None);
        app.collector.prune_blocking(now());
        for _ in 0..6 {
            t += 0.2;
            frame(&ctx, &mut ui, t, h);
        }
        assert_eq!(ui.screen, Screen::Settings);
        assert!(app.settings.get().sections.diagnostics);
        let r = ui.prune_warning_rect.expect("the warning is drawn");
        assert!(
            r.top() >= 0.0 && r.bottom() <= h,
            "the warning is scrolled into view: {r:?}"
        );
    }
}
