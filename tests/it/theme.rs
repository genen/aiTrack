// SPDX-License-Identifier: GPL-3.0-or-later
//! The `theme` setting (system | dark | light): persistence, clamping, and its effect on the window
//! through headless egui (System follows a fake OS preference, an explicit choice overrides it).
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::settings::{Settings, ThemeChoice};
use aitrack::store::JsonlStore;
use aitrack::ui::{AitrackUi, Screen};
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect, Theme};
use serde_json::{json, Value};
use std::sync::Arc;

#[test]
fn the_setting_defaults_to_system_persists_and_ignores_bad_values() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert_eq!(s.get().theme, ThemeChoice::System);
    s.set_theme(ThemeChoice::Light);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["theme"], "light");
    assert_eq!(Settings::load(&path).get().theme, ThemeChoice::Light);
    for bad in [json!("sepia"), json!(1), Value::Null, json!("")] {
        s.update(&json!({"theme": bad}), true);
        assert_eq!(s.get().theme, ThemeChoice::Light, "{bad}");
    }
    std::fs::write(&path, r#"{"interval_min": 5}"#).unwrap();
    assert_eq!(
        Settings::load(&path).get().theme,
        ThemeChoice::System,
        "older files follow the system"
    );
    for t in ThemeChoice::ALL {
        assert_eq!(ThemeChoice::parse(t.as_str()), Some(t));
    }
}

struct Win {
    ctx: Context,
    ui: AitrackUi,
    app: Arc<App>,
    os: Option<Theme>,
    t: f64,
    _d: TempDir,
}

impl Win {
    fn new(theme: Option<&str>, os: Theme) -> Win {
        let d = TempDir::new();
        if let Some(t) = theme {
            std::fs::write(d.join("s.json"), format!("{{\"theme\": \"{t}\"}}")).unwrap();
        }
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            Arc::new(Settings::load(d.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        ));
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app.clone());
        ui.sandbox();
        let mut w = Win {
            ctx,
            ui,
            app,
            os: Some(os),
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
            system_theme: self.os,
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
fn system_follows_the_os_preference_and_an_explicit_choice_overrides_it() {
    let mut w = Win::new(None, Theme::Light);
    assert_eq!(w.ctx.theme(), Theme::Light, "System follows the (fake) OS");
    w.os = Some(Theme::Dark);
    w.frame(vec![]);
    assert_eq!(w.ctx.theme(), Theme::Dark, "and keeps following it");
    // explicit choices win over the OS, from the very first frame
    assert_eq!(
        Win::new(Some("dark"), Theme::Light).ctx.theme(),
        Theme::Dark
    );
    assert_eq!(
        Win::new(Some("light"), Theme::Dark).ctx.theme(),
        Theme::Light
    );
}

#[test]
fn the_settings_control_changes_the_theme_live() {
    let mut w = Win::new(None, Theme::Dark);
    w.ui.screen = Screen::Settings;
    w.app.settings.set_section("appearance", true);
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(w.ui.theme_rects.len(), 3);
    let center = |w: &Win, t: ThemeChoice| {
        w.ui.theme_rects
            .iter()
            .find(|(x, _)| *x == t)
            .unwrap()
            .1
            .center()
    };
    let p = center(&w, ThemeChoice::Light);
    w.click(p);
    assert_eq!(w.app.settings.get().theme, ThemeChoice::Light);
    w.frame(vec![]);
    assert_eq!(
        w.ctx.theme(),
        Theme::Light,
        "applied live, although the OS says dark"
    );
    let p = center(&w, ThemeChoice::Dark);
    w.click(p);
    w.frame(vec![]);
    assert_eq!(w.ctx.theme(), Theme::Dark);
    let p = center(&w, ThemeChoice::System);
    w.os = Some(Theme::Light);
    w.click(p);
    w.frame(vec![]);
    assert_eq!(w.app.settings.get().theme, ThemeChoice::System);
    assert_eq!(w.ctx.theme(), Theme::Light, "back to following the OS");
}

#[test]
fn light_and_dark_share_the_same_spacing_so_buttons_are_the_same_size() {
    let ctx = Context::default();
    aitrack::theme::install(&ctx);
    let spacing_of = |t: Theme| ctx.style_of(t).spacing.clone();
    let dark = spacing_of(Theme::Dark);
    let light = spacing_of(Theme::Light);
    assert_eq!(dark.button_padding, light.button_padding);
    assert_eq!(dark.item_spacing, light.item_spacing);
    assert_eq!(light.button_padding, egui::vec2(10.0, 4.0));
}
