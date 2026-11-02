// SPDX-License-Identifier: GPL-3.0-or-later
//! The header's heart (Donate) button through headless egui: it opens the donation page from
//! wherever the user is, Back and the heart return there, and the page's QR code and Donate link
//! open the PayPal donation page.
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::settings::Settings;
use aitrack::store::JsonlStore;
use aitrack::ui::{AitrackUi, Screen};
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::Value;
use std::sync::{Arc, Mutex};

const DONATE: &str = "https://www.paypal.com/donate/?hosted_button_id=YR6YMJ953Y9CE";

struct Win {
    ctx: Context,
    ui: AitrackUi,
    t: f64,
    width: f32,
    _d: TempDir,
}

impl Win {
    fn new(width: f32) -> Win {
        let d = TempDir::new();
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            Arc::new(Settings::load(d.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        ));
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app);
        ui.sandbox();
        let mut w = Win {
            ctx,
            ui,
            t: 0.0,
            width,
            _d: d,
        };
        w.frame(vec![]);
        w.frame(vec![]);
        w
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.t += 0.2;
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                egui::vec2(self.width, 900.0),
            )),
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

    fn heart(&mut self) {
        let p = self.ui.heart_rect.expect("heart drawn").center();
        self.click(p);
    }

    fn rect(&self, key: &str) -> Rect {
        self.ui
            .donate_rects
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no {key}"))
            .1
    }
}

#[test]
fn the_heart_opens_the_donation_page_and_the_heart_again_closes_it() {
    let mut w = Win::new(310.0);
    assert_eq!(w.ui.screen, Screen::Home);
    w.heart();
    assert_eq!(w.ui.screen, Screen::Donate);
    assert_eq!(
        w.ui.donate_rects
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["donate_qr", "donate"]
    );
    w.heart();
    assert_eq!(w.ui.screen, Screen::Home);
}

#[test]
fn back_returns_to_the_screen_the_donation_page_was_opened_from() {
    let mut w = Win::new(310.0);
    w.ui.screen = Screen::Settings;
    w.frame(vec![]);
    w.heart();
    assert_eq!(w.ui.screen, Screen::Donate);
    w.heart();
    assert_eq!(w.ui.screen, Screen::Settings, "the heart again goes back");
    w.heart();
    w.ui.screen = Screen::Donate;
    w.ui.donate_from = Screen::Stats;
    w.frame(vec![]);
    let back = w.ui.back_rect.expect("Back drawn").center();
    w.click(back);
    assert_eq!(w.ui.screen, Screen::Stats);
}

#[test]
fn the_qr_code_and_the_donate_link_open_the_donation_page() {
    let mut w = Win::new(310.0);
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let c = calls.clone();
    w.ui.url_opener = Arc::new(move |u| {
        c.lock().unwrap().push(u.to_owned());
        Ok(())
    });
    w.heart();
    let (code, link) = (w.rect("donate_qr"), w.rect("donate"));
    assert!(
        link.top() > code.bottom(),
        "the word Donate is under the code"
    );
    w.click(code.center());
    w.click(link.center());
    assert_eq!(*calls.lock().unwrap(), [DONATE, DONATE]);
    w.ui.url_opener = Arc::new(|_| Err("no handler".into()));
    w.click(link.center());
    assert_eq!(
        w.ui.launch_notice().as_deref(),
        Some("Could not open the donation page")
    );
}

#[test]
fn the_page_and_the_header_fit_the_narrowest_window() {
    let mut w = Win::new(270.0);
    w.heart();
    assert_eq!(w.ui.screen, Screen::Donate);
    let code = w.rect("donate_qr");
    // 33 modules plus a 4-module quiet zone on each side, 5 pt per module
    assert!((code.width() - 205.0).abs() < 0.5, "{}", code.width());
    assert!((code.width() - code.height()).abs() < 0.5);
    assert!(code.left() >= 0.0 && code.right() <= 270.0);
    let heart = w.ui.heart_rect.unwrap();
    let shrink = w.ui.shrink_rect.unwrap();
    assert!(
        heart.left() > shrink.right(),
        "the heart is right of Small window"
    );
    assert!(heart.right() <= 270.0 && shrink.left() >= 0.0);
}
