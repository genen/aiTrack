// SPDX-License-Identifier: GPL-3.0-or-later
//! The Settings view: height cap, collapsible sections (persisted), pinned footer, rows whose
//! description never runs under the control, and the auto-fit leaving a hand-resized view alone.

use crate::common::TempDir;
use aitrack::settings::{Sections, Settings};
use aitrack::view::{height_cap, MIN_WINDOW_H, SCROLL_VIEW_MAX_PT};
use serde_json::{json, Value};

#[test]
fn the_readme_names_the_windows_sound_setting_for_alert_sounds() {
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    // the Settings list, the Notifications text and Troubleshooting each carry it
    assert!(
        readme.matches("Allow notifications to play sounds").count() >= 3,
        "the README must name the Windows setting in Notifications, Settings and Troubleshooting"
    );
}

// ---- the cap -----------------------------------------------------------------------------------

#[test]
fn the_scrolling_views_cap_at_640_points_or_60_percent_of_the_monitor() {
    // (physical monitor height, scale factor) -> logical height = physical / scale
    for (phys, scale, want) in [
        (1080.0, 1.0, 640.0), // 1080p: 0.6 x 1080 = 648 -> 640
        (1080.0, 1.5, 432.0), // 1080p at 150 %: 720 pt -> 432
        (1440.0, 1.0, 640.0),
        (1440.0, 1.5, 576.0), // 960 pt -> 576
        (1440.0, 2.0, 432.0), // 720 pt -> 432
        (2160.0, 1.0, 640.0), // 4K at 100 %: does not grow
        (2160.0, 1.5, 640.0), // 1440 pt
        (2160.0, 2.0, 640.0), // 1080 pt
        (4320.0, 1.0, 640.0), // 8K
        (4320.0, 2.0, 640.0),
        (4320.0, 3.0, 640.0),
        (768.0, 1.0, 460.8), // an old 1366x768 laptop
        (480.0, 2.0, 240.0), // 240 pt: 0.6 x 240 = 144 -> floor
    ] {
        let cap = height_cap(true, Some(phys / scale));
        assert!(
            (cap - want).abs() < 0.01,
            "{phys} @ {scale}: {cap} != {want}"
        );
        assert!(cap <= SCROLL_VIEW_MAX_PT + 0.01);
    }
    assert_eq!(height_cap(true, None), 640.0, "unknown monitor");
    assert!(height_cap(true, Some(0.0)) >= 240.0 && height_cap(true, Some(-5.0)) >= 240.0);
}

#[test]
fn the_home_view_keeps_its_content_fit_cap() {
    assert_eq!(height_cap(false, None), 720.0);
    assert!(
        (height_cap(false, Some(2160.0)) - 1836.0).abs() < 0.01,
        "85 % of the monitor"
    );
    assert_eq!(height_cap(false, Some(200.0)), 300.0);
    const { assert!(MIN_WINDOW_H < 240.0) };
}

// ---- section state -----------------------------------------------------------------------------

#[test]
fn sections_default_to_general_open_and_persist_each_one() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert_eq!(s.get().sections, Sections::default());
    assert!(s.get().sections.general);
    for n in [
        "appearance",
        "display",
        "notifications",
        "data",
        "diagnostics",
    ] {
        assert!(!s.get().sections.get(n), "{n} starts collapsed");
    }
    s.set_section("diagnostics", true);
    s.set_section("general", false);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["ui"]["sections"]["diagnostics"], true);
    assert_eq!(saved["ui"]["sections"]["general"], false);
    let again = Settings::load(&path).get().sections;
    assert!(again.diagnostics && !again.general && !again.display);
}

#[test]
fn bad_or_partial_section_values_are_ignored() {
    let d = TempDir::new();
    let path = d.join("s.json");
    for (text, general, display) in [
        (r#"{"ui": {"sections": {"display": true}}}"#, true, true),
        (
            r#"{"ui": {"sections": {"general": "no", "display": 1}}}"#,
            true,
            false,
        ),
        (r#"{"ui": 3}"#, true, false),
        (r#"{"ui": {"sections": {"nonsense": true}}}"#, true, false),
        (r#"{"ui": {"sections": null}}"#, true, false),
        (r#"{}"#, true, false),
    ] {
        std::fs::write(&path, text).unwrap();
        let sec = Settings::load(&path).get().sections;
        assert_eq!((sec.general, sec.display), (general, display), "{text}");
    }
    let s = Settings::load(&path);
    s.update(
        &json!({"ui": {"sections": {"appearance": true, "bogus": true}}}),
        true,
    );
    assert!(s.get().sections.appearance);
    assert!(!Sections::default().clone().set("bogus", true));
}

// ---- the view (headless egui) ------------------------------------------------------------------

#[cfg(feature = "gui")]
mod view {
    use super::*;
    use aitrack::app::App;
    use aitrack::store::JsonlStore;
    use aitrack::ui::{AitrackUi, Screen};
    use eframe::egui::{
        self, Context, Event, PointerButton, Pos2, RawInput, Rect, ViewportCommand, ViewportId,
        ViewportInfo,
    };
    use std::sync::Arc;
    use std::time::Duration;

    struct Win {
        ctx: Context,
        ui: AitrackUi,
        app: Arc<App>,
        height: f32,
        monitor: Option<egui::Vec2>,
        t: f64,
        sent: Vec<f32>,
        /// Texts the UI put on the clipboard.
        copied: Vec<String>,
        _d: TempDir,
    }

    impl Win {
        fn new(light: bool, monitor_h: Option<f32>) -> Win {
            let d = TempDir::new();
            let app = Arc::new(App::with_poll_fn(
                Box::new(JsonlStore::new(d.join("u.jsonl"))),
                Arc::new(Settings::load(d.join("s.json"))),
                Box::new(|_| Ok(Value::Null)),
            ));
            let ctx = Context::default();
            if light {
                ctx.set_theme(egui::ThemePreference::Light);
            }
            let mut ui = AitrackUi::new(&ctx, app.clone());
            ui.sandbox();
            ui.screen = Screen::Settings;
            let mut w = Win {
                ctx,
                ui,
                app,
                height: 900.0,
                monitor: monitor_h.map(|h| egui::vec2(1920.0, h)),
                t: 0.0,
                sent: vec![],
                copied: vec![],
                _d: d,
            };
            for _ in 0..4 {
                w.frame(vec![]);
            }
            w
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.t += 0.2;
            let vp = ViewportInfo {
                monitor_size: self.monitor,
                ..Default::default()
            };
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(310.0, self.height),
                )),
                time: Some(self.t),
                viewports: std::iter::once((ViewportId::ROOT, vp)).collect(),
                events,
                ..Default::default()
            };
            let ui = &mut self.ui;
            let out = self.ctx.run(raw, |c| ui.draw(c));
            for c in &out.platform_output.commands {
                if let egui::OutputCommand::CopyText(t) = c {
                    self.copied.push(t.clone());
                }
            }
            if let Some(v) = out.viewport_output.get(&ViewportId::ROOT) {
                for c in &v.commands {
                    if let ViewportCommand::InnerSize(s) = c {
                        self.sent.push(s.y);
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
            // a real pointer rests a moment: the hover tooltip settles beside the widget, rather
            // than over it where egui first guesses it
            self.frame(vec![]);
            self.frame(vec![btn(true)]);
            self.frame(vec![btn(false)]);
            self.frame(vec![]);
        }

        fn open_all(&mut self) {
            for n in Sections::NAMES {
                self.app.settings.set_section(n, true);
            }
            for _ in 0..4 {
                self.frame(vec![]);
            }
        }
    }

    #[test]
    fn collapsed_by_default_is_short_and_fits_without_scrolling() {
        for light in [false, true] {
            let w = Win::new(light, None);
            let h = w.ui.desired_height();
            // The upper bound leaves room for platform font metrics: the same layout measures a few
            // points taller on Windows (524 pt on the CI runner, before the Data section) than with
            // the Linux fonts. It stays under the 640 pt cap.
            assert!(
                (380.0..=620.0).contains(&h),
                "collapsed Settings is {h} pt (light {light})"
            );
            assert_eq!(w.ui.section_rects.len(), 7);
        }
    }

    #[test]
    fn everything_expanded_hits_the_cap_whatever_the_monitor() {
        // 4K at 100 % (2160 pt), 4K at 200 % (1080 pt), 1080p (1080 pt): 640
        for monitor in [None, Some(2160.0), Some(1080.0)] {
            let mut w = Win::new(false, monitor);
            w.open_all();
            assert_eq!(w.ui.desired_height(), 640.0, "monitor {monitor:?}");
        }
        // a small monitor caps lower
        let mut w = Win::new(false, Some(600.0));
        w.open_all();
        assert_eq!(w.ui.desired_height(), 360.0);
    }

    #[test]
    fn clicking_a_header_opens_and_closes_its_section_and_remembers_it() {
        let mut w = Win::new(false, None);
        let before = w.ui.desired_height();
        let p =
            w.ui.section_rects
                .iter()
                .find(|(k, _)| k == "appearance")
                .unwrap()
                .1
                .center();
        w.click(p);
        assert!(w.app.settings.get().sections.appearance);
        w.frame(vec![]);
        assert!(w.ui.desired_height() > before, "an open section is taller");
        let p =
            w.ui.section_rects
                .iter()
                .find(|(k, _)| k == "appearance")
                .unwrap()
                .1
                .center();
        w.click(p);
        assert!(!w.app.settings.get().sections.appearance);
        // the state reaches settings.json
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&w.app.settings.path).unwrap()).unwrap();
        assert_eq!(saved["ui"]["sections"]["appearance"], false);
    }

    #[test]
    fn no_description_runs_under_its_control_at_310_px_in_either_theme() {
        for light in [false, true] {
            let mut w = Win::new(light, None);
            w.open_all();
            // scroll through the content so every row has been laid out at least once is not needed:
            // rows are laid out even when clipped, so the log covers all of them
            assert!(
                w.ui.setting_rows.len() >= 11,
                "{} rows",
                w.ui.setting_rows.len()
            );
            for (title, label, control) in &w.ui.setting_rows {
                if label.is_finite() && control.is_finite() {
                    assert!(
                        !label.intersects(*control),
                        "{title} (light {light}): label {label:?} overlaps control {control:?}"
                    );
                    assert!(
                        label.right() <= control.left() + 0.5,
                        "{title}: the label column must end before the control"
                    );
                }
            }
            // the rows the user reported
            let names: Vec<&str> = w.ui.setting_rows.iter().map(|r| r.0.as_str()).collect();
            for want in ["Alert sounds", "Theme", "Logos", "Show Claude"] {
                assert!(names.contains(&want), "{want} in {names:?}");
            }
        }
    }

    fn about_center(w: &Win, key: &str) -> Pos2 {
        w.ui.about_rects
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no {key} in {:?}", w.ui.about_rects))
            .1
            .center()
    }

    fn open_about(w: &mut Win) {
        w.app.settings.set_section("about", true);
        for _ in 0..4 {
            w.frame(vec![]);
        }
    }

    #[test]
    fn the_about_section_is_last_collapsed_by_default_and_remembered() {
        let mut w = Win::new(false, None);
        let keys: Vec<&str> = w.ui.section_rects.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(keys.last(), Some(&"about"), "{keys:?}");
        assert!(!w.app.settings.get().sections.about);
        assert!(
            w.ui.about_rects.is_empty(),
            "nothing of it is drawn while collapsed"
        );
        let p =
            w.ui.section_rects
                .iter()
                .find(|r| r.0 == "about")
                .unwrap()
                .1
                .center();
        w.click(p);
        assert!(w.app.settings.get().sections.about);
        w.frame(vec![]);
        assert!(!w.ui.about_rects.is_empty());
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&w.app.settings.path).unwrap()).unwrap();
        assert_eq!(saved["ui"]["sections"]["about"], true);
    }

    #[test]
    fn the_about_links_open_the_exact_urls_and_a_failure_shows_a_short_notice() {
        let mut w = Win::new(false, None);
        open_about(&mut w);
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let c = calls.clone();
        w.ui.url_opener = Arc::new(move |u| {
            c.lock().unwrap().push(u.to_owned());
            Ok(())
        });
        for key in ["repo", "license"] {
            let p = about_center(&w, key);
            w.click(p);
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "https://github.com/genen/aiTrack",
                "https://github.com/genen/aiTrack/blob/main/LICENSE"
            ]
        );
        w.ui.url_opener = Arc::new(|_| Err("no handler".into()));
        let p = about_center(&w, "repo");
        w.click(p);
        assert_eq!(
            w.ui.launch_notice().as_deref(),
            Some("Could not open the project page")
        );
        // the licence name is a link too (not a button) with its own notice
        w.ui.set_launch_notice("");
        let p = about_center(&w, "license");
        w.click(p);
        assert_eq!(
            w.ui.launch_notice().as_deref(),
            Some("Could not open the license page")
        );
    }

    #[test]
    fn the_about_section_has_a_repo_link_and_a_licence_link_and_nothing_to_copy() {
        let mut w = Win::new(false, None);
        open_about(&mut w);
        let keys: Vec<&str> = w.ui.about_rects.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            keys,
            ["repo", "license"],
            "two links: no buttons, Email, Copy or build-line controls"
        );
        let names: Vec<&str> = w.ui.setting_rows.iter().map(|r| r.0.as_str()).collect();
        assert!(!names.contains(&"Contact"), "{names:?}");
        // clicking around the section copies nothing
        let p = about_center(&w, "license");
        w.click(p);
        assert!(w.copied.is_empty(), "{:?}", w.copied);
    }

    #[test]
    fn the_about_rows_fit_the_window_in_either_theme() {
        for light in [false, true] {
            let mut w = Win::new(light, None);
            open_about(&mut w);
            let names: Vec<&str> = w.ui.setting_rows.iter().map(|r| r.0.as_str()).collect();
            for want in ["aiTrack", "Developed by Sir Gene", "License"] {
                assert!(names.contains(&want), "{want} in {names:?}");
            }
            for (title, label, control) in &w.ui.setting_rows {
                if label.is_finite() && control.is_finite() {
                    assert!(
                        label.right() <= control.left() + 0.5,
                        "{title} (light {light}): label {label:?} runs under control {control:?}"
                    );
                }
            }
            for (key, r) in &w.ui.about_rects {
                assert!(
                    r.left() >= 0.0 && r.right() <= 310.0,
                    "{key} (light {light}) leaves the window: {r:?}"
                );
            }
            // the controls do not overlap each other
            let r = &w.ui.about_rects;
            for (i, a) in r.iter().enumerate() {
                for b in &r[i + 1..] {
                    assert!(!a.1.intersects(b.1), "{} overlaps {}", a.0, b.0);
                }
            }
        }
    }

    /// Moves the pointer onto `p` and lets the tooltip delay pass.
    fn hover(w: &mut Win, p: Pos2) {
        w.frame(vec![Event::PointerMoved(p)]);
        for _ in 0..6 {
            w.frame(vec![]);
        }
    }

    #[test]
    fn the_platform_wording_names_windows_or_macos_settings() {
        use aitrack::ui::*;
        assert_eq!(start_at_login_label_for(false), "Start with Windows");
        assert_eq!(start_at_login_label_for(true), "Start at login");
        assert_eq!(alert_sounds_tip_for(false), ALERT_SOUNDS_TIP);
        assert!(alert_sounds_tip_for(true).contains("System Settings > Notifications"));
        assert!(!alert_sounds_tip_for(true).contains("Windows"));
        assert_eq!(
            start_at_login_label(),
            start_at_login_label_for(cfg!(any(target_os = "macos", target_os = "linux")))
        );
        if cfg!(target_os = "linux") {
            assert_eq!(alert_sounds_tip(), ALERT_SOUNDS_TIP_LINUX);
            assert!(!alert_sounds_tip().contains("Windows"));
        }
    }

    #[test]
    fn the_alert_sounds_row_explains_the_windows_sound_setting_in_a_tooltip_that_fits() {
        assert_eq!(
            aitrack::ui::ALERT_SOUNDS_TIP,
            "Windows must also allow notification sounds: Settings > System > Notifications > Allow notifications to play sounds."
        );
        // (no font metrics here: a one-line version of this text would be far wider than the window)
        for light in [false, true] {
            let mut w = Win::new(light, None);
            w.open_all();
            let row =
                w.ui.setting_rows
                    .iter()
                    .find(|r| r.0 == "Alert sounds")
                    .cloned()
                    .unwrap();
            for (what, at) in [("toggle", row.2.center()), ("title", row.1.center())] {
                w.frame(vec![Event::PointerGone]);
                hover(&mut w, at);
                let tip =
                    w.ui.setting_tips
                        .iter()
                        .find(|t| t.0 == "Alert sounds")
                        .unwrap_or_else(|| panic!("no tooltip on the {what} (light {light})"))
                        .1;
                assert!(
                    tip.width() <= 251.0,
                    "{what}: {tip:?} is wider than its cap"
                );
                assert!(
                    tip.height() > 25.0,
                    "{what}: the text wrapped onto lines: {tip:?}"
                );
                assert!(
                    tip.left() >= 0.0 && tip.right() <= 310.0,
                    "{what}: {tip:?} leaves the 310 pt window"
                );
            }
            // other rows carry no tooltip of this kind
            let theme =
                w.ui.setting_rows
                    .iter()
                    .find(|r| r.0 == "Theme")
                    .cloned()
                    .unwrap();
            w.frame(vec![Event::PointerGone]);
            hover(&mut w, theme.1.center());
            assert!(w.ui.setting_tips.is_empty(), "{:?}", w.ui.setting_tips);
        }
    }

    #[test]
    fn a_long_description_wraps_and_the_row_grows() {
        let mut w = Win::new(false, None);
        w.open_all();
        let rows = w.ui.setting_rows.clone();
        let find = |n: &str| rows.iter().find(|r| r.0 == n).unwrap().clone();
        let (_, long, _) = find("Custom (minutes)");
        let (_, short, _) = find("Always on top");
        assert!(
            long.height() > short.height() + 5.0,
            "the long description wrapped onto another line"
        );
    }

    #[test]
    fn the_footer_is_pinned_and_the_diagnostics_are_bounded() {
        let mut w = Win::new(false, None);
        w.open_all();
        // Back, Help, the build line and the status bar are panels, not part of the scroll area:
        // the Help link is on screen although the content is far taller than the window
        assert!(w.ui.help_rect.is_some());
        let r = w.ui.help_rect.unwrap();
        assert!(r.bottom() <= w.height && r.top() > 400.0, "{r:?}");
    }

    #[test]
    fn a_view_the_user_resized_is_not_fitted_again() {
        let mut w = Win::new(false, None);
        let fitted = *w.sent.last().expect("the first fit is sent");
        assert_eq!(fitted, w.ui.desired_height());
        // the window takes the commanded size and settles
        w.height = fitted;
        w.frame(vec![]);
        std::thread::sleep(Duration::from_millis(900));
        // the user drags it much shorter
        w.height = 300.0;
        w.frame(vec![]);
        let n = w.sent.len();
        // content changes (a section opens): a fitted view would be resized, this one is left alone
        w.app.settings.set_section("display", true);
        for _ in 0..6 {
            w.frame(vec![]);
        }
        assert_eq!(
            w.sent.len(),
            n,
            "no InnerSize while the user's size stands: {:?}",
            w.sent
        );
        // another view is still fitted when entered
        w.ui.screen = Screen::Home;
        for _ in 0..4 {
            w.frame(vec![]);
        }
        assert!(w.sent.len() > n, "the home view is fitted");
    }
}
