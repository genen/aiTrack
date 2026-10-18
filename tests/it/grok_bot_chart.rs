// SPDX-License-Identifier: GPL-3.0-or-later
//! The Grok Bot weekly row opens a detail pane with a history chart of its own readings
//! (`bot_weekly` records), like the Claude and Grok rows. Pure checks take an explicit `now`; the
//! window click test draws with the real clock (the window does), so its reading is relative to it.
use aitrack::grok_bot::{self, BotReading};
use aitrack::reading::{Kind, Reading};
use aitrack::view::{self, chart_data, Range};
use chrono::{DateTime, Duration, TimeZone, Utc};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
}

fn bot(pct: f64, read_min_ago: i64, resets_in_h: i64) -> BotReading {
    BotReading {
        pct,
        resets_at: now() + Duration::hours(resets_in_h),
        read_at: now() - Duration::minutes(read_min_ago),
        expires_at: None,
    }
}

/// A stored history record (the real writer's format) taken `mins_ago` before `now()`.
fn stored(mins_ago: i64, pct: f64, resets_in_h: i64) -> Reading {
    let r = bot(pct, mins_ago, resets_in_h);
    Reading::from_value(&grok_bot::record(&r, r.read_at))
}

#[test]
fn stored_bot_records_chart_as_weekly_with_gaps_as_gaps() {
    // 10 readings 10 min apart, a 4 h hole, then 3 more; one weekly window throughout
    let mut rs: Vec<Reading> = (0..10)
        .map(|i| stored(600 - i * 10, 10.0 + i as f64, 40))
        .collect();
    rs.extend((0..3).map(|i| stored(200 - i * 10, 30.0 + i as f64, 40)));
    let d = chart_data(&rs, Kind::Weekly, Range::Day, now());
    assert_eq!(d.segments.len(), 2, "{:?}", d.segments);
    assert_eq!(d.segments[0].len(), 10);
    assert_eq!(d.segments[1].len(), 3);
    assert_eq!(d.y_max, 100.0, "peak 32 % is above the zoom limit");
    // the session kind has nothing: the bot only has a weekly meter
    assert!(chart_data(&rs, Kind::Session, Range::Day, now())
        .segments
        .is_empty());
}

#[test]
fn a_new_weekly_period_cuts_the_line_and_old_readings_stay() {
    let rs = vec![
        stored(300, 80.0, -100),
        stored(290, 82.0, -100),
        stored(60, 3.0, 160),
        stored(50, 4.0, 160),
    ];
    let d = chart_data(&rs, Kind::Weekly, Range::Day, now());
    assert_eq!(d.segments.len(), 2, "reset between the windows");
    // a reading far older than the range is simply outside it, never invented
    let old = vec![stored(60 * 24 * 3, 50.0, 10)];
    assert!(chart_data(&old, Kind::Weekly, Range::Day, now())
        .segments
        .is_empty());
    assert_eq!(
        chart_data(&old, Kind::Weekly, Range::All, now())
            .segments
            .len(),
        1,
        "All keeps stale readings"
    );
}

#[test]
fn tracker_history_feeds_the_chart_without_an_account_id() {
    use aitrack::sources::{Env, Os};
    use aitrack::store::JsonlStore;
    let dir = crate::common::TempDir::new();
    let store = JsonlStore::new(dir.join("b.jsonl"));
    use aitrack::store::Store;
    for m in [30, 20, 10] {
        let r = bot(40.0 + m as f64 / 10.0, m, 40);
        store.append(&grok_bot::record(&r, r.read_at)).unwrap();
    }
    let t = grok_bot::BotTracker::new(
        Env::new(Os::Linux),
        Box::new(JsonlStore::new(dir.join("b.jsonl"))),
    );
    let rs = t.readings();
    assert_eq!(rs.len(), 3);
    let d = chart_data(&rs, Kind::Weekly, Range::Day, now());
    assert_eq!(d.segments.len(), 1);
    assert_eq!(d.segments[0].len(), 3);
}

#[test]
fn bot_detail_follows_the_staleness_rules() {
    // fresh: percent, reset, no note, chart on, flagged as the bot pane
    let d = view::build_bot_detail(&bot(59.17, 2, 25), None, false, now());
    assert_eq!(d.title, "Grok Bot weekly");
    assert_eq!((d.value.as_str(), d.unit), ("59", "%"));
    assert_eq!(d.resets.as_deref(), Some("Resets in 25h"));
    assert!(d.note.is_none() && d.chart == Some(Kind::Weekly) && d.bot);
    // remaining mode flips the number
    let d = view::build_bot_detail(&bot(59.17, 2, 25), None, true, now());
    assert_eq!((d.value.as_str(), d.unit), ("41", "% left"));
    // old: kept, with a note, and never a pace or projection row
    let d = view::build_bot_detail(&bot(59.17, 180, 25), None, false, now());
    assert_eq!(d.value, "59");
    assert!(d
        .note
        .as_ref()
        .unwrap()
        .text
        .contains("open the Grok Bot app"));
    assert!(d.rows.iter().all(|(k, _)| k != "Pace" && k != "At reset"));
    // period ended: dashes, say so, still charts
    let d = view::build_bot_detail(&bot(59.17, 180, -1), None, false, now());
    assert_eq!((d.value.as_str(), d.unit), ("--", ""));
    assert_eq!(
        d.resets.as_deref(),
        Some("Reset passed, waiting for a new reading")
    );
    assert!(d.chart.is_some());
}

/// Three weekly periods of the Grok Bot (peaks 40, 80, 100 %), each with two readings, all ended
/// before `now()`.
fn finished_history() -> Vec<Reading> {
    let mut rs = Vec::new();
    for (i, peak) in [40.0, 80.0, 100.0].into_iter().enumerate() {
        let weeks_ago = 3 - i as i64; // 3, 2, 1 weeks ago
        let at = weeks_ago * 7 * 24 * 60;
        let resets_in_h = -(weeks_ago - 1) * 7 * 24 + 12 - 12; // the period ended `weeks_ago-1` weeks ago
        rs.push(stored(at, peak / 2.0, resets_in_h - 24));
        rs.push(stored(at - 60, peak, resets_in_h - 24));
    }
    rs
}

fn row<'a>(d: &'a view::Detail, label: &str) -> Option<&'a str> {
    d.rows
        .iter()
        .find(|(k, _)| k == label)
        .map(|(_, v)| v.as_str())
}

#[test]
fn bot_detail_shows_the_same_four_stats_as_the_other_panes() {
    let rs = finished_history();
    let st = view::bot_window_stats(&rs, now()).unwrap();
    let d = view::build_bot_detail(&bot(59.17, 2, 25), Some(&st), false, now());
    assert_eq!(row(&d, "Windows"), Some("3"));
    assert_eq!(row(&d, "Avg peak"), Some("73%"));
    assert_eq!(row(&d, "Max peak"), Some("100%"));
    assert_eq!(row(&d, "Hit rate"), Some("33%"));
    // same labels and order as the Claude / Grok panes: the four stats come last
    let labels: Vec<&str> = d.rows.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        &labels[labels.len() - 4..],
        ["Windows", "Avg peak", "Max peak", "Hit rate"]
    );
    // also when the period has ended
    let d = view::build_bot_detail(&bot(59.17, 180, -1), Some(&st), false, now());
    assert_eq!(row(&d, "Windows"), Some("3"));
    assert_eq!(row(&d, "Hit rate"), Some("33%"));
}

#[test]
fn bot_stats_agree_with_the_stats_page_for_the_same_readings() {
    use aitrack::stats::{compute, StatsRange};
    let rs = finished_history();
    let st = view::bot_window_stats(&rs, now()).unwrap();
    let page = compute(&rs, StatsRange::All, now(), 0);
    assert_eq!(st.windows, page.weekly.windows);
    assert_eq!(
        st.avg_peak.map(|x| x.round()),
        page.weekly.avg_peak.map(f64::round)
    );
    assert_eq!(st.max_peak, page.weekly.max_peak);
    assert_eq!(st.hit_rate, page.weekly.hit_rate.map(|h| h.round() as i64));
}

#[test]
fn bot_detail_without_history_has_no_stats_block() {
    assert!(view::bot_window_stats(&[], now()).is_none());
    let d = view::build_bot_detail(&bot(59.17, 2, 25), None, false, now());
    assert!(row(&d, "Windows").is_none() && row(&d, "Hit rate").is_none());
}

#[test]
fn bot_stats_with_one_reading_count_one_window() {
    let rs = vec![stored(5, 30.0, 100)];
    let st = view::bot_window_stats(&rs, now()).unwrap();
    assert_eq!(
        (st.windows, st.max_peak, st.hit_rate),
        (1, Some(30.0), Some(0))
    );
    let d = view::build_bot_detail(&bot(30.0, 5, 100), Some(&st), false, now());
    assert_eq!(row(&d, "Windows"), Some("1"));
    assert_eq!(row(&d, "Avg peak"), Some("30%"));
    assert_eq!(row(&d, "Hit rate"), Some("0%"));
}

#[test]
fn bot_stats_ignore_readings_from_the_future() {
    let mut rs = vec![stored(5, 30.0, 100)];
    rs.push(stored(-600, 99.0, 100)); // stamped 10 h ahead of now()
    let st = view::bot_window_stats(&rs, now()).unwrap();
    assert_eq!(st.max_peak, Some(30.0));
}

#[cfg(feature = "gui")]
mod window {
    use super::*;
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use aitrack::store::JsonlStore;
    use aitrack::ui::AitrackUi;
    use aitrack::vendor::{self, VendorId};
    use eframe::egui::{self, Context, CursorIcon, Event, PointerButton, Pos2, RawInput, Rect};
    use serde_json::Value;
    use std::sync::Arc;

    struct Win {
        ctx: Context,
        ui: AitrackUi,
        t: f64,
        _dir: crate::common::TempDir,
    }

    impl Win {
        fn new() -> Win {
            let dir = crate::common::TempDir::new();
            let app = Arc::new(App::with_poll_fn(
                Box::new(JsonlStore::new(dir.join("u.jsonl"))),
                Arc::new(Settings::load(dir.join("s.json"))),
                Box::new(|_| Ok(Value::Null)),
            ));
            let ctx = Context::default();
            let mut ui = AitrackUi::new(&ctx, app);
            ui.sandbox();
            let real = Utc::now();
            let mut vs = vendor::demo_vendors(real);
            let g = vs.iter_mut().find(|v| v.id == VendorId::Grok).unwrap();
            g.current = true;
            g.source = Some("grok-billing".into());
            g.observed_at = Some(real);
            g.bot = Some(BotReading {
                pct: 59.0,
                resets_at: real + Duration::hours(30),
                read_at: real - Duration::minutes(2),
                expires_at: None,
            });
            ui.mock = Some(vs);
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
                .run(raw, |c| ui.draw(c))
                .platform_output
                .cursor_icon
        }

        fn click(&mut self, p: Pos2) {
            self.frame(vec![Event::PointerMoved(p)]);
            let b = |pressed| Event::PointerButton {
                pos: p,
                button: PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            self.frame(vec![b(true)]);
            self.frame(vec![b(false)]);
            self.frame(vec![]);
            self.frame(vec![]);
        }

        fn bot_row(&self) -> Pos2 {
            self.ui.bot_row_rect.expect("bot row drawn").center()
        }

        fn weekly_row(&self, v: VendorId) -> Pos2 {
            self.ui
                .row_rects
                .iter()
                .find(|(id, k, _)| *id == v && *k == Kind::Weekly)
                .unwrap()
                .2
                .center()
        }
    }

    #[test]
    fn the_bot_row_is_clickable_and_toggles_one_pane_at_a_time() {
        let mut w = Win::new();
        let p = w.bot_row();
        w.frame(vec![Event::PointerMoved(p)]);
        assert_eq!(w.frame(vec![]), CursorIcon::PointingHand);
        assert!(!w.ui.bot_expanded);
        w.click(p);
        assert!(w.ui.bot_expanded && w.ui.expanded.is_none());
        // the open pane does not shift the row itself; a second click closes it
        let p = w.bot_row();
        w.click(p);
        assert!(!w.ui.bot_expanded);
        // open it, then a Grok Weekly click replaces it with that pane
        let p = w.bot_row();
        w.click(p);
        assert!(w.ui.bot_expanded);
        let g = w.weekly_row(VendorId::Grok);
        w.click(g);
        assert!(!w.ui.bot_expanded);
        assert_eq!(w.ui.expanded, Some((VendorId::Grok, Kind::Weekly)));
        // and the bot row replaces that one
        let p = w.bot_row();
        w.click(p);
        assert!(w.ui.bot_expanded && w.ui.expanded.is_none());
    }

    #[test]
    fn the_other_rows_still_toggle_as_before() {
        let mut w = Win::new();
        let c = w.weekly_row(VendorId::Claude);
        w.click(c);
        assert_eq!(w.ui.expanded, Some((VendorId::Claude, Kind::Weekly)));
        let c = w.weekly_row(VendorId::Claude);
        w.click(c);
        assert!(w.ui.expanded.is_none() && !w.ui.bot_expanded);
    }
}
