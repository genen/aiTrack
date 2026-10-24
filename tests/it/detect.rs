// SPDX-License-Identifier: GPL-3.0-or-later
//! Which tools are installed (temp HOME / APPDATA trees), the Auto / On / Off setting and its
//! migration, the empty state, and a tool "installed" while the app runs (real collector thread).
use crate::common;
use aitrack::alerts::{AlertKind, Notification, Notifier};
use aitrack::app::App;
use aitrack::collector::PollFn;
use aitrack::detect::*;
use aitrack::live::CollectError;
use aitrack::settings::{Settings, Track};
use aitrack::sources::{Env, Os};
use aitrack::state::State;
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::{self, VendorId};
use aitrack::view;
use common::TempDir;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn env(os: Os, root: &Path) -> Env {
    let mut e = Env::new(os);
    e.home = Some(root.to_path_buf());
    e.appdata = Some(root.join("AppData/Roaming"));
    e.localappdata = Some(root.join("AppData/Local"));
    e.xdg_config_home = Some(root.join(".config"));
    e
}

fn mkdir(p: impl AsRef<Path>) {
    std::fs::create_dir_all(p).unwrap();
}

// ---- detection -------------------------------------------------------------------------------

#[test]
fn nothing_installed_means_nothing_present() {
    let d = TempDir::new();
    for os in [Os::Windows, Os::Linux, Os::Mac] {
        assert_eq!(
            detect(&env(os, d.path())),
            Presence {
                claude: false,
                grok: false,
                codex: false
            },
            "{os:?}"
        );
    }
}

#[test]
fn the_claude_desktop_app_is_found_in_its_mac_folder() {
    let d = TempDir::new();
    mkdir(d.join("Library/Application Support/Claude"));
    assert!(claude_present(&env(Os::Mac, d.path())));
    assert!(!claude_present(&env(Os::Linux, d.path())));
}

#[test]
fn the_claude_desktop_app_is_found_in_its_windows_folders() {
    // the classic installer
    let d = TempDir::new();
    mkdir(d.join("AppData/Roaming/Claude"));
    assert!(claude_present(&env(Os::Windows, d.path())));
    // the Microsoft Store package (the id suffix is not known in advance)
    let d = TempDir::new();
    mkdir(d.join("AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude"));
    assert!(claude_present(&env(Os::Windows, d.path())));
    // an unrelated package does not count, nor does a Claude_ package without the data folder
    let d = TempDir::new();
    mkdir(d.join("AppData/Local/Packages/Other_abc/LocalCache/Roaming/Claude"));
    mkdir(d.join("AppData/Local/Packages/Claude_abc/LocalCache"));
    assert!(!claude_present(&env(Os::Windows, d.path())));
}

#[test]
fn claude_code_is_found_through_its_config_folder() {
    let d = TempDir::new();
    mkdir(d.join(".claude"));
    assert!(claude_present(&env(Os::Linux, d.path())));
    assert_eq!(
        claude_code_dir(&env(Os::Linux, d.path())),
        d.join(".claude")
    );
    // CLAUDE_CONFIG_DIR replaces ~/.claude
    let d = TempDir::new();
    let custom = d.join("elsewhere/cc");
    let mut e = env(Os::Linux, d.path());
    e.claude_config_dir = Some(custom.clone());
    assert!(!claude_present(&e), "the custom folder does not exist yet");
    mkdir(&custom);
    assert!(claude_present(&e));
    // ~/.claude is not consulted when the variable is set
    let d = TempDir::new();
    mkdir(d.join(".claude"));
    let mut e = env(Os::Linux, d.path());
    e.claude_config_dir = Some(d.join("nope"));
    assert!(!claude_present(&e));
}

#[test]
fn grok_is_found_through_its_home_folder() {
    let d = TempDir::new();
    mkdir(d.join(".grok"));
    assert!(grok_present(&env(Os::Windows, d.path())));
    let d = TempDir::new();
    let mut e = env(Os::Windows, d.path());
    e.grok_home = Some(d.join("custom-grok"));
    assert!(!grok_present(&e));
    mkdir(d.join("custom-grok"));
    assert!(grok_present(&e));
}

#[test]
fn all_four_combinations() {
    for (claude, grok) in [(false, false), (true, false), (false, true), (true, true)] {
        let d = TempDir::new();
        if claude {
            mkdir(d.join(".claude"));
        }
        if grok {
            mkdir(d.join(".grok"));
        }
        assert_eq!(
            detect(&env(Os::Linux, d.path())),
            Presence {
                claude,
                grok,
                codex: false
            }
        );
    }
}

// ---- the tri-state setting and its migration ------------------------------------------------

#[test]
fn auto_on_off_decide_with_and_without_detection() {
    let d = TempDir::new();
    let s = Settings::load(d.join("s.json"));
    assert_eq!((s.get().claude_enabled, s.get().grok_enabled), (None, None));
    // Auto follows detection
    assert!(s.get().claude_on(true) && !s.get().claude_on(false));
    assert!(s.get().grok_on(true) && !s.get().grok_on(false));
    // On shows it even when not found, Off hides it even when found
    s.set_claude_track(Track::On);
    s.set_grok_track(Track::Off);
    assert!(s.get().claude_on(false) && !s.get().grok_on(true));
    // persisted
    let again = Settings::load(d.join("s.json"));
    assert_eq!(
        (again.get().claude_enabled, again.get().grok_enabled),
        (Some(true), Some(false))
    );
    again.set_claude_track(Track::Auto);
    assert_eq!(Settings::load(d.join("s.json")).get().claude_enabled, None);
    for t in Track::ALL {
        assert_eq!(Track::from_opt(t.to_opt()), t);
    }
    assert_eq!(Track::Auto.label(), "Auto");
}

#[test]
fn the_older_track_grok_values_keep_their_meaning() {
    let d = TempDir::new();
    let p = d.join("s.json");
    // written by an older version: Grok explicitly on / off / automatic, no Claude key at all
    for (text, grok, claude) in [
        (r#"{"grok_enabled": true}"#, Track::On, Track::Auto),
        (r#"{"grok_enabled": false}"#, Track::Off, Track::Auto),
        (r#"{"grok_enabled": null}"#, Track::Auto, Track::Auto),
        (r#"{"interval_min": 5}"#, Track::Auto, Track::Auto),
        (
            r#"{"grok_enabled": "yes", "claude_enabled": 3}"#,
            Track::Auto,
            Track::Auto,
        ),
    ] {
        std::fs::write(&p, text).unwrap();
        let v = Settings::load(&p).get();
        assert_eq!(
            (
                Track::from_opt(v.grok_enabled),
                Track::from_opt(v.claude_enabled)
            ),
            (grok, claude),
            "{text}"
        );
    }
}

// ---- the empty state -------------------------------------------------------------------------

fn state_with(claude_on: bool) -> State {
    let mut st = State::loading(
        Default::default(),
        aitrack::collector::Status {
            error: None,
            polling: false,
            next_in: None,
            polls: 1,
            live: false,
            sample: None,
        },
    );
    st.loading = false;
    st.claude_on = claude_on;
    st
}

#[test]
fn nothing_to_track_shows_the_friendly_panel_and_the_status_text() {
    let st = state_with(false);
    assert!(st.nothing_to_track());
    let home = view::build_home(&st, chrono::Utc::now());
    let e = home.empty.expect("empty state");
    assert_eq!(e.title, "Nothing to track yet");
    assert!(
        e.body.contains("Claude desktop app or Claude Code")
            && e.body.contains("Grok CLI")
            && e.body.contains("press refresh"),
        "{}",
        e.body
    );
    assert!(e.help);
    assert_eq!(home.status.text, "No supported tools found");
    assert!(home.vendors.is_empty());
    // a present tool without a reading keeps the old messages and no help button
    let home = view::build_home(&state_with(true), chrono::Utc::now());
    let e = home.empty.expect("waiting");
    assert_eq!(e.title, "Taking first reading...");
    assert!(!e.help);
}

#[test]
fn a_present_tool_that_fails_shows_its_error_not_the_empty_state() {
    let mut st = state_with(true);
    st.collector.error = Some("no Claude login found".into());
    let home = view::build_home(&st, chrono::Utc::now());
    let e = home.empty.unwrap();
    assert_eq!(
        (e.title.as_str(), e.body.as_str()),
        ("No reading yet", "no Claude login found")
    );
    assert_ne!(home.status.text, "No supported tools found");
}

// ---- appearing and disappearing while the app runs -------------------------------------------

fn counting_poll(polls: Arc<AtomicUsize>) -> PollFn {
    Box::new(
        move |store: &dyn Store| -> Result<serde_json::Value, CollectError> {
            polls.fetch_add(1, Ordering::SeqCst);
            let rec = json!({"v": 1, "ts": aitrack::util::now_iso(), "ok": true, "source": "oauth",
            "session": {"pct": 30.0, "resets_at": null}, "weekly": null, "spend": null, "raw": {}});
            store.append(&rec).unwrap();
            Ok(rec)
        },
    )
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[derive(Default)]
struct Rec(Mutex<Vec<Notification>>);
impl Notifier for Rec {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

#[test]
fn a_tool_installed_while_running_makes_its_panel_appear_and_removal_hides_it() {
    let d = TempDir::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let settings = Arc::new(Settings::load(d.join("s.json")));
    let app = App::with_poll_fns(
        Box::new(JsonlStore::new(d.join("u.jsonl"))),
        settings,
        counting_poll(polls.clone()),
        Some(counting_poll(polls.clone())),
    );
    let installed = Arc::new(AtomicBool::new(false));
    let flag = installed.clone();
    app.set_claude_detect(Arc::new(move || flag.load(Ordering::SeqCst)));
    // presence answers are reused for a few seconds; this test flips the flag and looks at once
    app.set_presence_ttl(Duration::ZERO);
    let rec = Arc::new(Rec::default());
    app.set_notifier(rec.clone());
    app.start();
    wait_for("the first (gated) tick", || {
        app.collector.status().polls >= 1
    });
    // not installed: nothing was polled, nothing is shown, nothing was recorded
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "no polling noise for a missing tool"
    );
    let st = app.state();
    assert!(!st.claude_on && st.nothing_to_track());
    assert!(vendor::from_state(&st).is_empty());
    assert_eq!(
        view::build_home(&st, chrono::Utc::now())
            .empty
            .unwrap()
            .title,
        "Nothing to track yet"
    );
    assert!(app
        .history
        .try_with_readings(|r| r.is_empty())
        .unwrap_or(true));
    assert!(!app.claude_detected() && !app.claude_on());

    // "install" it: the panel exists at once, data follows on the next poll (Refresh now)
    installed.store(true, Ordering::SeqCst);
    assert!(
        app.state().claude_on,
        "the panel does not wait for a restart"
    );
    app.refresh();
    wait_for("the first real poll", || polls.load(Ordering::SeqCst) >= 1);
    wait_for("the reading", || {
        vendor::from_state(&app.state())
            .iter()
            .any(|v| v.id == VendorId::Claude)
    });
    let st = app.state();
    assert!(!st.nothing_to_track());
    assert_eq!(view::build_home(&st, chrono::Utc::now()).vendors.len(), 1);

    // forced Off: gone again and polling stops
    app.set_claude_track(Track::Off);
    let n = polls.load(Ordering::SeqCst);
    app.refresh();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(polls.load(Ordering::SeqCst), n);
    assert!(vendor::from_state(&app.state()).is_empty());
    // forced On while not installed: shown (with its failure state), polled
    installed.store(false, Ordering::SeqCst);
    app.set_claude_track(Track::On);
    app.refresh();
    wait_for("forced-on poll", || polls.load(Ordering::SeqCst) > n);
    assert!(app.state().claude_on);
    // and a missing tool sends no alerts
    app.set_claude_track(Track::Auto);
    assert!(rec
        .0
        .lock()
        .unwrap()
        .iter()
        .all(|n: &Notification| n.kind != AlertKind::Limit));
    app.stop();
    app.join();
}
