// SPDX-License-Identifier: GPL-3.0-or-later
//! The horizontal layout: its saved setting, the window widths, and the row of vendor blocks drawn
//! through headless egui.
use crate::common::TempDir;
use aitrack::settings::Settings;
use aitrack::view;
use serde_json::{json, Value};

#[test]
fn the_layout_defaults_to_vertical_and_persists() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert!(!s.get().horizontal);
    assert_eq!(s.get().block_width, None);
    s.set_horizontal(true);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["layout"], "horizontal");
    assert!(Settings::load(&path).get().horizontal);
    // anything but the two names leaves it as it was
    s.update(&json!({"layout": "diagonal"}), true);
    s.update(&json!({"layout": true}), true);
    assert!(s.get().horizontal);
    s.set_horizontal(false);
    assert!(!Settings::load(&path).get().horizontal);
}

#[test]
fn the_block_width_is_kept_in_range() {
    let d = TempDir::new();
    let s = Settings::load(d.join("s.json"));
    s.set_block_width(350.0);
    assert_eq!(s.get().block_width, Some(350.0));
    s.set_block_width(10.0);
    assert_eq!(s.get().block_width, Some(view::BLOCK_W_MIN as f64));
    s.set_block_width(1e9);
    assert_eq!(s.get().block_width, Some(1000.0));
    // a wrong type or a non-number leaves it; null forgets it
    s.update(&json!({"block_width": "wide"}), true);
    assert_eq!(s.get().block_width, Some(1000.0));
    s.update(&json!({"block_width": null}), true);
    assert_eq!(s.get().block_width, None);
}

#[test]
fn the_window_is_as_wide_as_its_blocks() {
    // three tools at the default 310 pt each, plus the window's margins
    assert_eq!(view::horizontal_width(3, view::BLOCK_W), 950.0);
    // no tools (the empty state) counts as one block
    assert_eq!(view::horizontal_width(0, view::BLOCK_W), 330.0);
    // a block is never narrower than the minimum
    assert_eq!(
        view::horizontal_width(2, 100.0),
        view::horizontal_min_width(2)
    );
    assert_eq!(view::horizontal_min_width(3), 3.0 * 270.0 + 20.0);
    // a width dragged by hand, read back as the width of one block
    assert_eq!(view::block_width_for(1220.0, 3), 400.0);
    assert_eq!(view::block_width_for(300.0, 3), view::BLOCK_W_MIN);
    assert_eq!(
        view::block_width_for(view::horizontal_width(4, 333.0), 4),
        333.0
    );
}

#[test]
fn the_small_window_widens_only_when_horizontal() {
    assert_eq!(view::small_width(false, 3), view::SMALL_WINDOW_W);
    assert_eq!(view::small_width(true, 3), 3.0 * view::SMALL_WINDOW_W);
    assert_eq!(view::small_width(true, 0), view::SMALL_WINDOW_W);
}

#[cfg(feature = "gui")]
mod window {
    use crate::common::TempDir;
    use aitrack::app::App;
    use aitrack::reading::Kind;
    use aitrack::settings::Settings;
    use aitrack::store::JsonlStore;
    use aitrack::ui::{AitrackUi, Screen};
    use aitrack::vendor::{self, VendorId};
    use aitrack::view;
    use chrono::Utc;
    use eframe::egui::{
        self, Context, Event, PointerButton, Pos2, RawInput, Rect, ViewportCommand, ViewportId,
    };
    use serde_json::Value;
    use std::sync::Arc;

    struct Win {
        ctx: Context,
        ui: AitrackUi,
        app: Arc<App>,
        width: f32,
        t: f64,
        /// Inner sizes the window asked for (the last one of a frame is what the window gets).
        sizes: Vec<egui::Vec2>,
        _d: TempDir,
    }

    impl Win {
        fn new(horizontal: bool) -> Win {
            let d = TempDir::new();
            let settings = Arc::new(Settings::load(d.join("s.json")));
            settings.set_horizontal(horizontal);
            let app = Arc::new(App::with_poll_fn(
                Box::new(JsonlStore::new(d.join("u.jsonl"))),
                settings,
                Box::new(|_| Ok(Value::Null)),
            ));
            let ctx = Context::default();
            let mut ui = AitrackUi::new(&ctx, app.clone());
            ui.sandbox();
            ui.mock = Some(vendor::demo_vendors(Utc::now()));
            let mut w = Win {
                ctx,
                ui,
                app,
                // a horizontal start too opens at the vertical width (see `run`); the first frame
                // widens it
                width: 310.0,
                t: 0.0,
                sizes: vec![],
                _d: d,
            };
            for _ in 0..4 {
                w.frame(vec![]);
            }
            w
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.t += 0.2;
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(self.width, 700.0),
                )),
                time: Some(self.t),
                events,
                ..Default::default()
            };
            let ui = &mut self.ui;
            let out = self.ctx.run(raw, |c| ui.draw(c));
            if let Some(v) = out.viewport_output.get(&ViewportId::ROOT) {
                for c in &v.commands {
                    if let ViewportCommand::InnerSize(s) = c {
                        self.sizes.push(*s);
                        // like the OS: the last request of the frame wins
                        self.width = s.x;
                    }
                }
            }
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

        fn row(&self, id: VendorId, kind: Kind) -> Rect {
            self.ui
                .row_rects
                .iter()
                .find(|(v, k, _)| *v == id && *k == kind)
                .map(|(_, _, r)| *r)
                .unwrap()
        }
    }

    #[test]
    fn the_blocks_sit_side_by_side_in_one_row() {
        let w = Win::new(true);
        let badges = &w.ui.badge_rects;
        assert!(badges.len() >= 3, "{badges:?}");
        // every badge on the same line, each further right than the one before
        for pair in badges.windows(2) {
            assert!(
                (pair[0].1.top() - pair[1].1.top()).abs() < 0.5,
                "{badges:?}"
            );
            assert!(pair[1].1.left() > pair[0].1.right() + 100.0, "{badges:?}");
        }
        // the window, opened at the vertical width, ends as wide as its blocks (no later size
        // request of the frame, such as the height fit, takes the old width back)
        let n = badges.len();
        assert_eq!(
            w.width,
            view::horizontal_width(n, view::BLOCK_W),
            "{:?}",
            w.sizes
        );
    }

    #[test]
    fn the_vertical_layout_still_stacks_them() {
        let w = Win::new(false);
        let badges = &w.ui.badge_rects;
        for pair in badges.windows(2) {
            assert!(pair[1].1.top() > pair[0].1.bottom(), "{badges:?}");
        }
        // the first frame of a vertical window leaves its width alone
        assert_eq!(w.width, 310.0, "{:?}", w.sizes);
    }

    #[test]
    fn a_clicked_row_opens_its_detail_under_the_whole_row() {
        let mut w = Win::new(true);
        let session = w.row(VendorId::Claude, Kind::Session);
        let lowest_row =
            w.ui.row_rects
                .iter()
                .map(|(_, _, r)| r.bottom())
                .fold(0.0_f32, f32::max);
        let before = w.ui.desired_height();
        w.click(session.center());
        assert_eq!(w.ui.expanded, Some((VendorId::Claude, Kind::Session)));
        // the rows did not move (the pane is not inside the Claude block) and the window grew
        assert_eq!(w.row(VendorId::Claude, Kind::Session), session);
        let after_rows =
            w.ui.row_rects
                .iter()
                .map(|(_, _, r)| r.bottom())
                .fold(0.0_f32, f32::max);
        assert_eq!(after_rows, lowest_row);
        assert!(w.ui.desired_height() > before + 40.0);
    }

    #[test]
    fn the_layout_switch_in_settings_changes_the_window() {
        let mut w = Win::new(false);
        w.ui.screen = Screen::Settings;
        w.app.settings.set_section("appearance", true);
        w.frame(vec![]);
        w.frame(vec![]);
        let horizontal =
            w.ui.layout_rects
                .iter()
                .find(|(h, _)| *h)
                .map(|(_, r)| *r)
                .expect("the Layout row is in Appearance");
        w.click(horizontal.center());
        assert!(w.app.settings.get().horizontal);
        let n = vendor::demo_vendors(Utc::now()).len();
        assert_eq!(
            w.width,
            view::horizontal_width(n, view::BLOCK_W),
            "{:?}",
            w.sizes
        );
        w.frame(vec![]);
        let vertical =
            w.ui.layout_rects
                .iter()
                .find(|(h, _)| !*h)
                .map(|(_, r)| *r)
                .unwrap();
        w.click(vertical.center());
        assert!(!w.app.settings.get().horizontal);
        // the vertical width again, and the wide one was not saved as the vertical window's
        assert_eq!(w.width, view::default_inner_size_pt().0, "{:?}", w.sizes);
        let saved = w.app.settings.get().window.and_then(|p| p.width);
        assert!(saved.is_none_or(|s| s < 400.0), "{saved:?}");
    }

    #[test]
    fn the_small_window_follows_the_layout() {
        let mut w = Win::new(true);
        let n = vendor::demo_vendors(Utc::now()).len();
        w.app.settings.set_small(true);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.width, view::small_width(true, n), "{:?}", w.sizes);
        // and back to the full row
        w.app.settings.set_small(false);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            w.width,
            view::horizontal_width(n, view::BLOCK_W),
            "{:?}",
            w.sizes
        );
        // the small window's blocks are side by side too
        let badges = &w.ui.badge_rects;
        assert!(badges.len() >= 2);
        assert!(
            (badges[0].1.top() - badges[1].1.top()).abs() < 0.5,
            "{badges:?}"
        );
    }

    #[test]
    fn the_logo_opens_the_project_page_in_both_layouts() {
        for horizontal in [false, true] {
            let mut w = Win::new(horizontal);
            let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let c = calls.clone();
            w.ui.url_opener = Arc::new(move |u| {
                c.lock().unwrap().push(u.to_owned());
                Ok(())
            });
            let logo = w.ui.logo_rect.expect("the header logo is drawn");
            w.click(logo.center());
            assert_eq!(*calls.lock().unwrap(), ["https://github.com/genen/aiTrack"]);
            assert_eq!(w.ui.screen, Screen::Home);
        }
    }
}
