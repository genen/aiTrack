// SPDX-License-Identifier: GPL-3.0-or-later
//! Badge style variants: the setting (persistence, bad values), the style table, the Settings control.
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::settings::{BadgeVariant, Settings};
use aitrack::store::JsonlStore;
use aitrack::theme::{self, Fill, DARK, LIGHT};
use aitrack::ui::{AitrackUi, Screen};
use aitrack::vendor::VendorId;
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::{json, Value};
use std::sync::Arc;

const VENDORS: [VendorId; 4] = [
    VendorId::Claude,
    VendorId::Codex,
    VendorId::Antigravity,
    VendorId::Grok,
];

#[test]
fn the_setting_defaults_to_auto_persists_and_ignores_bad_values() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    // a new settings file defaults to matching the theme
    assert_eq!(s.get().badge_style, BadgeVariant::Auto);
    s.set_badge_style(BadgeVariant::Dark);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["badge_style"], "dark");
    assert_eq!(Settings::load(&path).get().badge_style, BadgeVariant::Dark);
    // an unknown value, a wrong type and null leave it as it was
    for bad in [json!("neon"), json!(3), Value::Null, json!("")] {
        s.update(&json!({"badge_style": bad}), true);
        assert_eq!(s.get().badge_style, BadgeVariant::Dark, "{bad}");
    }
    s.update(&json!({"badge_style": "light"}), true);
    assert_eq!(Settings::load(&path).get().badge_style, BadgeVariant::Light);
    // a settings file written by an older version has no key: the default applies
    std::fs::write(&path, r#"{"interval_min": 5}"#).unwrap();
    assert_eq!(
        Settings::load(&path).get().badge_style,
        BadgeVariant::Filled
    );
    for v in BadgeVariant::ALL {
        assert_eq!(BadgeVariant::parse(v.as_str()), Some(v));
    }
}

#[test]
fn every_vendor_has_all_three_variants_with_a_readable_mark() {
    let lum =
        |c: egui::Color32| 0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32;
    for pal in [&DARK, &LIGHT] {
        for v in VENDORS {
            for variant in BadgeVariant::ALL {
                let st = theme::badge_style(v, variant, pal);
                assert!((0.2..=0.25).contains(&st.radius), "{v:?} {variant:?}");
                let fill = match st.fill {
                    Fill::Solid(c) => c,
                    Fill::Vertical(a, b) => egui::Color32::from_rgb(
                        ((a.r() as u16 + b.r() as u16) / 2) as u8,
                        ((a.g() as u16 + b.g() as u16) / 2) as u8,
                        ((a.b() as u16 + b.b() as u16) / 2) as u8,
                    ),
                };
                assert!(
                    (lum(fill) - lum(st.mark)).abs() > 40.0,
                    "{v:?} {variant:?}: mark not readable on the fill"
                );
                if variant == BadgeVariant::Light {
                    assert!(st.border.is_some(), "{v:?}: light badge needs an outline");
                }
            }
        }
    }
}

#[test]
fn the_final_colours_of_the_two_logo_vendors() {
    let hex = |c: egui::Color32| format!("{:02X}{:02X}{:02X}", c.r(), c.g(), c.b());
    let st = |v, b| theme::badge_style(v, b, &DARK);
    let top = |s: theme::BadgeStyle| match s.fill {
        Fill::Vertical(a, _) => hex(a),
        Fill::Solid(c) => hex(c),
    };
    let claude = st(VendorId::Claude, BadgeVariant::Filled);
    assert_eq!(
        (top(claude).as_str(), hex(claude.mark).as_str()),
        ("EE7654", "FFF1E8")
    );
    let claude_dark = st(VendorId::Claude, BadgeVariant::Dark);
    assert_eq!(
        (top(claude_dark).as_str(), hex(claude_dark.mark).as_str()),
        ("313131", "DE7857")
    );
    let grok = st(VendorId::Grok, BadgeVariant::Filled);
    assert_eq!(
        (top(grok).as_str(), hex(grok.mark).as_str()),
        ("0A0A0A", "E6E6EA")
    );
    let grok_dark = st(VendorId::Grok, BadgeVariant::Dark);
    assert_eq!(
        (top(grok_dark).as_str(), hex(grok_dark.mark).as_str()),
        ("08090B", "FEFEFE")
    );
    let light = st(VendorId::Claude, BadgeVariant::Light);
    assert_eq!(
        (top(light).as_str(), hex(light.mark).as_str()),
        ("F5F5F5", "E8694A")
    );
    assert_eq!(light.border.map(hex).as_deref(), Some("D8D8DC"));
}

#[test]
fn the_accent_colours_do_not_change_with_the_variant() {
    // LED strips and percentages use `Palette::vendor`, which the variants never touch
    assert_eq!(
        DARK.vendor(VendorId::Claude),
        egui::Color32::from_rgb(0xFF, 0x9E, 0x2E)
    );
    assert_eq!(
        DARK.vendor(VendorId::Grok),
        egui::Color32::from_rgb(0xD1, 0xD1, 0xD1)
    );
}

#[test]
fn white_reference_badges_get_an_outline_only_on_the_light_theme() {
    let w = theme::BADGE_CHATGPT;
    assert!(w.border.is_none() && w.light_border.is_some());
    assert!(theme::BADGE_PERPLEXITY.light_border.is_none());
}

#[test]
fn the_settings_control_changes_the_variant_live() {
    let d = TempDir::new();
    let settings = Arc::new(Settings::load(d.join("s.json")));
    let app = Arc::new(App::with_poll_fn(
        Box::new(JsonlStore::new(d.join("u.jsonl"))),
        settings.clone(),
        Box::new(|_| Ok(Value::Null)),
    ));
    let ctx = Context::default();
    let mut ui = AitrackUi::new(&ctx, app.clone());
    ui.sandbox();
    ui.screen = Screen::Settings;
    let mut t = 0.0;
    let mut frame = |ui: &mut AitrackUi, events: Vec<Event>| {
        t += 0.2;
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 900.0))),
            time: Some(t),
            events,
            ..Default::default()
        };
        let _ = ctx.run(raw, |c| ui.draw(c));
    };
    app.settings.set_section("appearance", true);
    frame(&mut ui, vec![]);
    frame(&mut ui, vec![]);
    let labels: Vec<_> = ui.variant_rects.iter().map(|(b, _)| *b).collect();
    assert_eq!(labels.len(), 4, "Auto, Filled, Dark, Light");
    for target in [
        BadgeVariant::Dark,
        BadgeVariant::Light,
        BadgeVariant::Filled,
    ] {
        let p = ui
            .variant_rects
            .iter()
            .find(|(b, _)| *b == target)
            .unwrap()
            .1
            .center();
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(&mut ui, vec![Event::PointerMoved(p)]);
        frame(&mut ui, vec![btn(true)]);
        frame(&mut ui, vec![btn(false)]);
        frame(&mut ui, vec![]);
        assert_eq!(settings.get().badge_style, target);
    }
}

#[test]
fn auto_resolves_to_the_dark_look_on_a_dark_theme_and_the_light_look_on_a_light_one() {
    assert_eq!(BadgeVariant::Auto.resolve(true), BadgeVariant::Dark);
    assert_eq!(BadgeVariant::Auto.resolve(false), BadgeVariant::Light);
    for v in [
        BadgeVariant::Filled,
        BadgeVariant::Dark,
        BadgeVariant::Light,
    ] {
        assert_eq!(v.resolve(true), v);
        assert_eq!(v.resolve(false), v);
    }
    // the style table agrees: Auto on a dark palette is the Dark look, on a light one the Light look
    for v in VENDORS {
        assert_eq!(
            theme::badge_style(v, BadgeVariant::Auto, &DARK),
            theme::badge_style(v, BadgeVariant::Dark, &DARK),
            "{v:?}"
        );
        assert_eq!(
            theme::badge_style(v, BadgeVariant::Auto, &LIGHT),
            theme::badge_style(v, BadgeVariant::Light, &LIGHT),
            "{v:?}"
        );
    }
}
