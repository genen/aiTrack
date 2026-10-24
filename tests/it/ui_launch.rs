// SPDX-License-Identifier: GPL-3.0-or-later
//! The Claude badge in the window, through headless egui: clickable (pointer cursor, tooltip) only
//! for Claude, one launch per click, the failure notice in the status bar, and a disabled launcher
//! (Linux default) that does nothing and does not change the cursor.
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::settings::Settings;
use aitrack::store::JsonlStore;
use aitrack::ui::{AitrackUi, Launcher};
use aitrack::vendor::{self, VendorId};
use chrono::Utc;
use common::TempDir;
use eframe::egui::{self, Context, CursorIcon, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Win {
    ctx: Context,
    ui: AitrackUi,
    t: f64,
    _dir: TempDir,
}

impl Win {
    fn new(enabled: bool, launcher: Option<Launcher>) -> Win {
        Self::build(enabled, launcher, false)
    }

    /// With a Grok part (its own store and collector), so the refresh control can work.
    fn build(enabled: bool, launcher: Option<Launcher>, grok: bool) -> Win {
        let dir = TempDir::new();
        let settings = Arc::new(Settings::load(dir.join("s.json")));
        let store = Box::new(JsonlStore::new(dir.join("u.jsonl")));
        let mut app = App::with_poll_fn(store, settings, Box::new(|_| Ok(Value::Null)));
        if grok {
            app = app.with_grok(
                Box::new(JsonlStore::new(dir.join("g.jsonl"))),
                Box::new(|_| Ok(Value::Null)),
                None,
                Arc::new(|| true),
            );
        }
        let app = Arc::new(app);
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app);
        // (the real launchers are kept only where they cannot start anything: Linux)
        let real = (ui.launcher.clone(), ui.grok_launcher.clone());
        ui.sandbox();
        if cfg!(not(any(windows, target_os = "macos"))) {
            (ui.launcher, ui.grok_launcher) = real;
        }
        ui.mock = Some(vendor::demo_vendors(Utc::now()));
        ui.launch_enabled = enabled;
        if let Some(l) = launcher {
            ui.launcher = l;
        }
        let mut w = Win {
            ctx,
            ui,
            t: 0.0,
            _dir: dir,
        };
        w.frame(vec![]);
        w.frame(vec![]);
        w
    }

    fn frame(&mut self, events: Vec<Event>) -> CursorIcon {
        self.t += 0.2;
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 900.0))),
            time: Some(self.t),
            events,
            ..Default::default()
        };
        let ui = &mut self.ui;
        self.ctx
            .run(raw, |ctx| ui.draw(ctx))
            .platform_output
            .cursor_icon
    }

    fn badge(&self, v: VendorId) -> Pos2 {
        self.ui
            .badge_rects
            .iter()
            .find(|(id, _)| *id == v)
            .unwrap_or_else(|| panic!("no badge for {v:?}"))
            .1
            .center()
    }

    fn hover(&mut self, p: Pos2) -> CursorIcon {
        self.frame(vec![Event::PointerMoved(p)]);
        self.frame(vec![])
    }

    fn click(&mut self, p: Pos2) {
        self.frame(vec![Event::PointerMoved(p)]);
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        self.frame(vec![btn(true)]);
        self.frame(vec![btn(false)]);
        self.frame(vec![]);
    }
}

fn counting() -> (Launcher, Arc<AtomicUsize>) {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    (
        Arc::new(move |_, _| {
            n2.fetch_add(1, Ordering::SeqCst);
        }),
        n,
    )
}

#[test]
fn clicking_the_claude_badge_launches_once_with_a_pointer_cursor() {
    let (l, n) = counting();
    let mut w = Win::new(true, Some(l));
    let claude = w.badge(VendorId::Claude);
    assert_eq!(w.hover(claude), CursorIcon::PointingHand);
    assert_eq!(n.load(Ordering::SeqCst), 0, "hovering must not launch");
    w.click(claude);
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn antigravity_badge_is_not_clickable() {
    let (l, n) = counting();
    let mut w = Win::new(true, Some(l));
    let p = w.badge(VendorId::Antigravity);
    assert_ne!(w.hover(p), CursorIcon::PointingHand, "Antigravity badge");
    w.click(p);
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

#[test]
fn disabled_launcher_is_a_no_op_without_a_cursor_change() {
    let (l, n) = counting();
    let mut w = Win::new(false, Some(l));
    let claude = w.badge(VendorId::Claude);
    assert_ne!(w.hover(claude), CursorIcon::PointingHand);
    w.click(claude);
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failed_launch_shows_a_short_notice() {
    let l: Launcher = Arc::new(|ctx, _| ctx.request_repaint());
    let mut w = Win::new(true, Some(l));
    assert_eq!(w.ui.launch_notice(), None);
    w.ui.set_launch_notice("Could not launch Claude desktop");
    w.frame(vec![]);
    assert_eq!(
        w.ui.launch_notice().as_deref(),
        Some("Could not launch Claude desktop")
    );
}

#[test]
fn clicking_the_grok_badge_opens_grok_not_claude() {
    let (claude_l, claude_n) = counting();
    let (grok_l, grok_n) = counting();
    let mut w = Win::new(true, Some(claude_l));
    w.ui.grok_launcher = grok_l;
    let grok = w.badge(VendorId::Grok);
    assert_eq!(w.hover(grok), CursorIcon::PointingHand);
    assert_eq!(grok_n.load(Ordering::SeqCst), 0, "hovering must not launch");
    w.click(grok);
    assert_eq!(grok_n.load(Ordering::SeqCst), 1);
    assert_eq!(claude_n.load(Ordering::SeqCst), 0);
    // disabled launcher: no pointer, no launch
    let (l, _) = counting();
    let (gl, gn) = counting();
    let mut w = Win::new(false, Some(l));
    w.ui.grok_launcher = gl;
    let grok = w.badge(VendorId::Grok);
    assert_ne!(w.hover(grok), CursorIcon::PointingHand);
    w.click(grok);
    assert_eq!(gn.load(Ordering::SeqCst), 0);
}

#[test]
fn clicking_the_codex_badge_opens_codex_not_claude() {
    let (claude_l, claude_n) = counting();
    let (codex_l, codex_n) = counting();
    let mut w = Win::new(true, Some(claude_l));
    w.ui.codex_launcher = codex_l;
    let codex = w.badge(VendorId::Codex);
    assert_eq!(w.hover(codex), CursorIcon::PointingHand);
    assert_eq!(
        codex_n.load(Ordering::SeqCst),
        0,
        "hovering must not launch"
    );
    w.click(codex);
    assert_eq!(codex_n.load(Ordering::SeqCst), 1);
    assert_eq!(claude_n.load(Ordering::SeqCst), 0);
}

#[test]
fn the_grok_refresh_line_offers_runs_once_and_shows_progress() {
    use aitrack::app::RefreshMode;
    let mut w = Win::build(true, None, true);
    let runs = Arc::new(AtomicUsize::new(0));
    let r2 = runs.clone();
    w.ui.app().set_grok_refresh(
        Arc::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }),
        Arc::new(|| true),
    );
    // hidden by default (mock vendors, no real Grok data and mode not forced -> real mode decides)
    w.ui.mock_refresh = Some(RefreshMode::Hidden);
    w.frame(vec![]);
    assert!(w.ui.refresh_rect.is_none());
    // offered: a click starts exactly one refresh
    w.ui.mock_refresh = Some(RefreshMode::Offer);
    w.frame(vec![]);
    let r = w.ui.refresh_rect.expect("refresh control");
    assert_eq!(w.hover(r.center()), CursorIcon::PointingHand);
    w.click(r.center());
    let t0 = std::time::Instant::now();
    while runs.load(Ordering::SeqCst) == 0 && t0.elapsed().as_secs() < 5 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // progress and failure texts take the same place
    for m in [
        RefreshMode::Running,
        RefreshMode::Failed("Grok did not fetch billing within 10 s (signed out?)".into()),
    ] {
        w.ui.mock_refresh = Some(m);
        w.frame(vec![]);
        assert!(w.ui.refresh_rect.is_some());
    }
}

#[test]
fn settings_has_a_refresh_grok_now_button_only_when_the_cli_is_found() {
    use aitrack::ui::Screen;
    let mut w = Win::build(true, None, true);
    let runs = Arc::new(AtomicUsize::new(0));
    let r2 = runs.clone();
    w.ui.app().set_grok_refresh(
        Arc::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }),
        Arc::new(|| true),
    );
    w.ui.screen = Screen::Settings;
    w.ui.app().settings.set_section("display", true);
    w.frame(vec![]);
    w.frame(vec![]);
    let r = w.ui.settings_refresh_rect.expect("Refresh Grok now button");
    w.click(r.center());
    let t0 = std::time::Instant::now();
    while runs.load(Ordering::SeqCst) == 0 && t0.elapsed().as_secs() < 5 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // no CLI: no button
    let mut w = Win::build(true, None, true);
    w.ui.app()
        .set_grok_refresh(Arc::new(|| Ok(None)), Arc::new(|| false));
    w.ui.screen = Screen::Settings;
    w.ui.app().settings.set_section("display", true);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.ui.settings_refresh_rect.is_none());
}

/// Every test that builds a UI must call `sandbox()`: a click on a link or a logo would otherwise
/// start a real browser or app on the machine running the tests (it started Safari on macOS CI).
#[test]
fn every_test_that_builds_a_ui_sandboxes_its_openers() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it");
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let src = std::fs::read_to_string(&p).unwrap();
        // (the needles are split so that this file does not match itself)
        let built = src.matches(concat!("AitrackUi", "::new(")).count();
        let boxed = src.matches(concat!("ui.", "sandbox();")).count();
        assert!(
            boxed >= built,
            "{} builds a UI without sandbox()",
            p.display()
        );
    }
}
