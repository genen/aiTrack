// SPDX-License-Identifier: GPL-3.0-or-later
//! The empty state's "Open help" button, the Help link in Settings, and the Auto / On / Off
//! controls, through headless egui.
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::settings::{Settings, Track};
use aitrack::store::JsonlStore;
use aitrack::ui::{AitrackUi, Screen};
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Win {
    ctx: Context,
    ui: AitrackUi,
    app: Arc<App>,
    t: f64,
    _d: TempDir,
}

impl Win {
    fn new(installed: bool) -> Win {
        let d = TempDir::new();
        let settings = Arc::new(Settings::load(d.join("s.json")));
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            settings,
            Box::new(|_| Ok(Value::Null)),
        ));
        app.set_claude_detect(Arc::new(move || installed));
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app.clone());
        ui.sandbox();
        let mut w = Win {
            ctx,
            ui,
            app,
            t: 0.0,
            _d: d,
        };
        w.frame(vec![]);
        w.frame(vec![]);
        w
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.t += 0.2;
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 900.0))),
            time: Some(self.t),
            events,
            ..Default::default()
        };
        let ui = &mut self.ui;
        let _ = self.ctx.run(raw, |c| ui.draw(c));
    }

    fn click(&mut self, p: Pos2) {
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        self.frame(vec![Event::PointerMoved(p)]);
        self.frame(vec![btn(true)]);
        self.frame(vec![btn(false)]);
        self.frame(vec![]);
    }
}

#[test]
fn open_help_calls_the_opener_once_and_a_failure_shows_a_short_message() {
    let mut w = Win::new(false);
    let opened = Arc::new(AtomicUsize::new(0));
    let o = opened.clone();
    w.ui.help_opener = Arc::new(move || {
        o.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    w.frame(vec![]);
    let p =
        w.ui.help_rect
            .expect("the empty state offers Open help")
            .center();
    w.click(p);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert_eq!(w.ui.launch_notice(), None);
    // a failing opener
    w.ui.help_opener = Arc::new(|| Err("no browser".into()));
    w.click(p);
    assert_eq!(
        w.ui.launch_notice().as_deref(),
        Some("Could not open the help page")
    );
}

#[test]
fn no_help_button_when_a_tool_is_present() {
    let w = Win::new(true);
    assert!(w.ui.help_rect.is_none());
}

#[test]
fn the_settings_footer_has_a_help_link_and_the_vendor_rows_change_the_tracking() {
    let mut w = Win::new(false);
    w.ui.screen = Screen::Settings;
    w.app.settings.set_section("display", true);
    let opened = Arc::new(AtomicUsize::new(0));
    let o = opened.clone();
    w.ui.help_opener = Arc::new(move || {
        o.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    w.frame(vec![]);
    w.frame(vec![]);
    let p = w.ui.help_rect.expect("Help link in the footer").center();
    w.click(p);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    // Auto / On / Off for Claude
    let keys: Vec<String> = w.ui.track_rects.iter().map(|(k, _)| k.clone()).collect();
    for k in ["Show Claude:Auto", "Show Claude:On", "Show Claude:Off"] {
        assert!(keys.iter().any(|x| x == k), "{k} in {keys:?}");
    }
    let center = |w: &Win, k: &str| {
        w.ui.track_rects
            .iter()
            .find(|(x, _)| x == k)
            .unwrap()
            .1
            .center()
    };
    assert!(!w.app.claude_on(), "not installed, Auto");
    let p = center(&w, "Show Claude:On");
    w.click(p);
    assert_eq!(w.app.settings.get().claude_enabled, Some(true));
    assert!(w.app.claude_on());
    let p = center(&w, "Show Claude:Off");
    w.click(p);
    assert_eq!(w.app.settings.get().claude_enabled, Some(false));
    let p = center(&w, "Show Claude:Auto");
    w.click(p);
    assert_eq!(w.app.settings.get().claude_enabled, None);
    assert_eq!(
        Track::from_opt(w.app.settings.get().claude_enabled),
        Track::Auto
    );
}
