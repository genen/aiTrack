// SPDX-License-Identifier: GPL-3.0-or-later
//! The Stats screen through headless egui: it draws the populated and empty states, the range
//! choices change what is shown, and the header icon opens and closes it.
#![cfg(feature = "gui")]
use crate::common;
use aitrack::app::App;
use aitrack::pricing::PriceTable;
use aitrack::settings::Settings;
use aitrack::stats::StatsRange;
use aitrack::statsview::{Snapshot, StatsService};
use aitrack::store::JsonlStore;
use aitrack::transcripts::*;
use aitrack::ui::{AitrackUi, Screen};
use chrono::Utc;
use common::TempDir;
use eframe::egui::{self, Context, Event, PointerButton, Pos2, RawInput, Rect};
use serde_json::Value;
use std::sync::Arc;

fn request(project: &str, hours_ago: i64) -> Request {
    Request {
        at: (Utc::now() - chrono::Duration::hours(hours_ago)).timestamp(),
        origin: Origin::Claude,
        project: project.into(),
        session: "s".into(),
        model: "claude-sonnet-4-5".into(),
        usage: aitrack::pricing::TokenUsage {
            input: 1_000_000,
            ..Default::default()
        },
        cost_ticks: None,
        sidechain: false,
        key: 0,
    }
}

fn request_of(project: &str, origin: Origin, hours_ago: i64) -> Request {
    Request {
        origin,
        ..request(project, hours_ago)
    }
}

struct Win {
    ctx: Context,
    ui: AitrackUi,
    t: f64,
    _d: TempDir,
}

impl Win {
    fn new(reqs: &[Request]) -> Win {
        let d = TempDir::new();
        let settings = Arc::new(Settings::load(d.join("s.json")));
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            settings,
            Box::new(|_| Ok(Value::Null)),
        ));
        let report = aggregate(reqs.iter(), &PriceTable::fallback(), Utc::now(), 0);
        app.set_stats(StatsService::fixed(Snapshot {
            projects: Some(Arc::new(report)),
            sources: vec![],
            scan: None,
            plan: None,
            busy: false,
        }));
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app);
        ui.sandbox();
        ui.screen = Screen::Stats;
        let mut w = Win {
            ctx,
            ui,
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

    fn center(&self, key: &str) -> Pos2 {
        self.ui
            .stats_rects
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no {key}"))
            .1
            .center()
    }
}

#[test]
fn the_range_choices_change_the_selected_ranges() {
    let mut w = Win::new(&[request("a", 1), request("b", 24 * 10)]);
    assert_eq!(
        (w.ui.proj_range, w.ui.plan_range),
        (ProjRange::Days7, StatsRange::Last30d)
    );
    let labels: Vec<_> = w.ui.stats_rects.iter().map(|(k, _)| k.clone()).collect();
    for want in [
        "proj:7 days",
        "proj:30 days",
        "proj:All",
        "plan:7d",
        "plan:30d",
        "plan:90d",
        "plan:All",
    ] {
        assert!(labels.iter().any(|l| l == want), "{want} in {labels:?}");
    }
    let p = w.center("proj:All");
    w.click(p);
    assert_eq!(w.ui.proj_range, ProjRange::All);
    let p = w.center("plan:90d");
    w.click(p);
    assert_eq!(w.ui.plan_range, StatsRange::Last90d);
    let p = w.center("proj:30 days");
    w.click(p);
    assert_eq!(w.ui.proj_range, ProjRange::Days30);
}

#[test]
fn the_empty_state_and_an_app_without_a_stats_service_still_draw() {
    let mut w = Win::new(&[]);
    w.frame(vec![]);
    assert_eq!(w.ui.screen, Screen::Stats);
    // no service at all (an App that never set one up)
    let d = TempDir::new();
    let app = Arc::new(App::with_poll_fn(
        Box::new(JsonlStore::new(d.join("u.jsonl"))),
        Arc::new(Settings::load(d.join("s.json"))),
        Box::new(|_| Ok(Value::Null)),
    ));
    let ctx = Context::default();
    let mut ui = AitrackUi::new(&ctx, app);
    ui.sandbox();
    ui.screen = Screen::Stats;
    for i in 0..3 {
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 700.0))),
            time: Some(i as f64 * 0.2),
            ..Default::default()
        };
        let _ = ctx.run(raw, |c| ui.draw(c));
    }
    assert!(ui.desired_height() > 120.0);
}

fn rect_of(w: &Win, key: &str) -> Rect {
    w.ui.stats_rects
        .iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| {
            panic!(
                "no {key} in {:?}",
                w.ui.stats_rects.iter().map(|(k, _)| k).collect::<Vec<_>>()
            )
        })
        .1
}

#[test]
fn each_project_row_starts_with_its_vendor_logo_and_the_name_gets_what_is_left() {
    let long = "a-very-long-project-name-that-goes-on-and-on-and-on-forever-and-ever";
    let w = Win::new(&[
        request_of(long, Origin::Claude, 1),
        request_of("aiTrack", Origin::GrokLog, 2),
        request_of("aiTrack", Origin::Claude, 3),
    ]);
    let keys: Vec<_> = w.ui.stats_rects.iter().map(|(k, _)| k.as_str()).collect();
    // one logo per row, for both vendors; same-named rows are not merged
    let logos: Vec<_> = keys.iter().filter(|k| k.starts_with("logo:")).collect();
    assert_eq!(logos.len(), 3, "{keys:?}");
    assert!(keys.contains(&"logo:1:Claude"), "{keys:?}");
    assert!(
        keys.iter()
            .any(|k| k.starts_with("logo:") && k.ends_with(":Grok")),
        "{keys:?}"
    );
    for rank in 1..=3 {
        let name = rect_of(&w, &format!("name:{rank}"));
        let logo =
            w.ui.stats_rects
                .iter()
                .find(|(k, _)| k.starts_with(&format!("logo:{rank}:")))
                .unwrap()
                .1;
        // the logo is small, sits before the name, and the name never runs under it
        assert!((12.0..=14.0).contains(&logo.width()), "{logo:?}");
        assert!(logo.right() < name.left(), "{logo:?} {name:?}");
        assert!(
            logo.left() >= 0.0 && name.right() <= 310.0 - 56.0 - 50.0,
            "{name:?}"
        );
    }
    // the long name is cut to fit, still with room for most of it, never into the numbers
    let long_row = rect_of(&w, "name:1");
    assert!(long_row.width() > 60.0, "{long_row:?}");
}
