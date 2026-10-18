// SPDX-License-Identifier: GPL-3.0-or-later
//! The real launcher (default `ui.launcher`) on a machine where nothing can be started: the failure
//! notice appears in the status bar and the attempts are logged. Its own test binary because it points
//! `AITRACK_LAUNCH_LOG` at a temp file (an environment variable is process-wide). Not on Windows or
//! macOS, where this would really try to open Claude.
#![cfg(all(feature = "gui", not(any(windows, target_os = "macos"))))]
mod common;

use aitrack::app::App;
use aitrack::settings::Settings;
use aitrack::store::JsonlStore;
use aitrack::ui::AitrackUi;
use aitrack::vendor::{self, VendorId};
use chrono::Utc;
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::Value;
use std::sync::Arc;

fn frame(ctx: &Context, ui: &mut AitrackUi, t: &mut f64, events: Vec<Event>) {
    *t += 0.2;
    let raw = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 900.0))),
        time: Some(*t),
        events,
        ..Default::default()
    };
    let _ = ctx.run(raw, |ctx| ui.draw(ctx));
}

#[test]
fn the_real_launcher_reports_failure_and_logs() {
    let dir = TempDir::new();
    let log_file = dir.join("launch.log");
    std::env::set_var("AITRACK_LAUNCH_LOG", &log_file);
    let settings = Arc::new(Settings::load(dir.join("s.json")));
    let store = Box::new(JsonlStore::new(dir.join("u.jsonl")));
    let app = Arc::new(App::with_poll_fn(
        store,
        settings,
        Box::new(|_| Ok(Value::Null)),
    ));
    let ctx = Context::default();
    let mut ui = AitrackUi::new(&ctx, app);
    ui.mock = Some(vendor::demo_vendors(Utc::now()));
    ui.launch_enabled = true;
    let mut t = 0.0;
    frame(&ctx, &mut ui, &mut t, vec![]);
    frame(&ctx, &mut ui, &mut t, vec![]);
    let p = ui
        .badge_rects
        .iter()
        .find(|(id, _)| *id == VendorId::Claude)
        .expect("no Claude badge")
        .1
        .center();
    frame(&ctx, &mut ui, &mut t, vec![Event::PointerMoved(p)]);
    let btn = |pressed| Event::PointerButton {
        pos: p,
        button: PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    frame(&ctx, &mut ui, &mut t, vec![btn(true)]);
    frame(&ctx, &mut ui, &mut t, vec![btn(false)]);
    let start = std::time::Instant::now();
    while ui.launch_notice().is_none() && start.elapsed().as_secs() < 5 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    frame(&ctx, &mut ui, &mut t, vec![]);
    assert_eq!(
        ui.launch_notice().as_deref(),
        Some("Could not launch Claude desktop")
    );
    std::env::remove_var("AITRACK_LAUNCH_LOG");
    assert!(std::fs::read_to_string(&log_file)
        .unwrap()
        .contains("claude://"));
}
