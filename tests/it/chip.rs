// SPDX-License-Identifier: GPL-3.0-or-later
//! The data-age chip and the stale-data tooltip line: thresholds per source, hidden when fresh,
//! the tooltip wording, and the header layout at 310 px (through headless egui).
use aitrack::reading::Kind;
use aitrack::vendor::{Vendor, VendorId};
use aitrack::view::*;
use chrono::{DateTime, Duration, TimeZone, Utc};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn status() -> aitrack::collector::Status {
    aitrack::collector::Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    }
}

fn vendor(id: VendorId, source: &str, age_min: f64) -> Vendor {
    vendor_at(now(), id, source, age_min)
}

fn vendor_at(at: DateTime<Utc>, id: VendorId, source: &str, age_min: f64) -> Vendor {
    let mut vs = aitrack::vendor::demo_vendors(at);
    let mut v = vs.remove(vs.iter().position(|v| v.id == id).unwrap());
    v.current = true;
    v.placeholder = false;
    v.source = Some(source.into());
    v.observed_at = Some(at - Duration::milliseconds((age_min * 60_000.0) as i64));
    v
}

#[test]
fn local_sources_get_the_chip_after_15_minutes_and_not_before() {
    for src in [
        "desktop-cache",
        "desktop-history",
        "claude-json",
        "grok-log",
    ] {
        let id = if src == "grok-log" {
            VendorId::Grok
        } else {
            VendorId::Claude
        };
        assert_eq!(chip_threshold_min(src, 15.0), Some(15.0), "{src}");
        assert!(
            age_chip(&vendor(id, src, 14.0), 15.0, now()).is_none(),
            "{src} fresh"
        );
        assert!(
            age_chip(&vendor(id, src, 15.0), 15.0, now()).is_none(),
            "{src} exactly at the threshold"
        );
        let c = age_chip(&vendor(id, src, 16.0), 15.0, now()).expect(src);
        assert_eq!(c.long, "about 16 minutes ago");
        assert_eq!(c.compact, "16 min");
    }
}

#[test]
fn network_sources_wait_for_two_poll_intervals_but_at_least_ten_minutes() {
    // interval 15 min: 30 min; interval 1 min: the 10 min floor; interval 60: 120 min
    assert_eq!(chip_threshold_min("oauth", 15.0), Some(30.0));
    assert_eq!(chip_threshold_min("grok-billing", 1.0), Some(10.0));
    assert_eq!(chip_threshold_min("oauth", 60.0), Some(120.0));
    assert!(age_chip(&vendor(VendorId::Claude, "oauth", 29.0), 15.0, now()).is_none());
    assert!(age_chip(&vendor(VendorId::Claude, "oauth", 31.0), 15.0, now()).is_some());
    assert!(age_chip(&vendor(VendorId::Grok, "grok-billing", 9.0), 1.0, now()).is_none());
    assert!(age_chip(&vendor(VendorId::Grok, "grok-billing", 11.0), 1.0, now()).is_some());
    assert_eq!(chip_threshold_min("something-else", 15.0), None);
    assert!(age_chip(
        &vendor(VendorId::Claude, "something-else", 500.0),
        15.0,
        now()
    )
    .is_none());
}

#[test]
fn no_chip_without_current_data_or_for_placeholders_or_without_a_sample_time() {
    let mut v = vendor(VendorId::Claude, "desktop-cache", 90.0);
    v.current = false;
    assert!(
        age_chip(&v, 15.0, now()).is_none(),
        "'No current data' has its own status"
    );
    let mut v = vendor(VendorId::Grok, "grok-log", 90.0);
    v.placeholder = true;
    assert!(age_chip(&v, 15.0, now()).is_none());
    let mut v = vendor(VendorId::Claude, "desktop-cache", 90.0);
    v.observed_at = None;
    assert!(age_chip(&v, 15.0, now()).is_none());
}

#[test]
fn the_chip_tooltips_say_what_the_data_is_and_what_to_do() {
    let c = age_chip(
        &vendor(VendorId::Claude, "desktop-cache", 38.0),
        15.0,
        now(),
    )
    .unwrap();
    assert_eq!(
        c.tooltip,
        "Data comes from the Claude desktop app's local cache, last updated about 38 minutes ago. Open the Claude app or press refresh for it to update."
    );
    let g = age_chip(&vendor(VendorId::Grok, "grok-log", 120.0), 15.0, now()).unwrap();
    assert_eq!(
        g.tooltip,
        "Live billing failed, so this comes from the Grok CLI's local log, last updated about 2 hours ago. Run a grok command or press refresh to update."
    );
    assert_eq!(g.compact, "2 h");
    let o = age_chip(&vendor(VendorId::Claude, "oauth", 45.0), 15.0, now()).unwrap();
    assert!(o.tooltip.contains("Check your connection"), "{}", o.tooltip);
    for c in [&c, &g, &o] {
        assert!(
            c.tooltip.matches(". ").count() <= 1 && c.tooltip.ends_with('.'),
            "two sentences: {}",
            c.tooltip
        );
    }
}

#[test]
fn age_wording() {
    assert_eq!(age_long(1.0), "about 1 minute ago");
    assert_eq!(age_long(59.0), "about 59 minutes ago");
    assert_eq!(age_long(60.0), "about 1 hour ago");
    assert_eq!(age_long(125.0), "about 2 hours ago");
    assert_eq!(age_long(60.0 * 50.0), "about 2 days ago");
    assert_eq!(age_compact(38.0), "38 min");
    assert_eq!(age_compact(130.0), "2 h");
    assert_eq!(age_compact(60.0 * 50.0), "2 d");
}

fn tip_lines(v: &Vendor, kind: Kind) -> Vec<(String, Tone)> {
    let st = aitrack::state::State::loading(Default::default(), status());
    let home = build_home_for(&st, std::slice::from_ref(v), now());
    let row = home.vendors[0]
        .rows
        .iter()
        .find(|r| r.kind == kind)
        .unwrap();
    row.tip.iter().map(|t| (t.text.clone(), t.tone)).collect()
}

#[test]
fn the_tooltip_warning_appears_after_an_hour_and_is_the_last_line() {
    for (age, shown) in [
        (59.0, false),
        (60.0, false),
        (60.5, true),
        (61.0, true),
        (200.0, true),
    ] {
        let v = vendor(VendorId::Claude, "desktop-cache", age);
        let tip = tip_lines(&v, Kind::Weekly);
        let last = tip.last().unwrap();
        assert_eq!(last.1 == Tone::Caution, shown, "{age} min: {last:?}");
        // the existing source line stays
        assert!(
            tip.iter()
                .any(|t| t.0.starts_with("From Claude desktop app")),
            "{tip:?}"
        );
    }
}

#[test]
fn the_warning_text_matches_the_source_and_uses_the_age_of_the_sample() {
    let claude = tip_lines(
        &vendor(VendorId::Claude, "desktop-cache", 125.0),
        Kind::Session,
    );
    assert_eq!(
        claude.last().unwrap().0,
        "Data is about 2 h old: the Claude app has not refreshed its usage cache."
    );
    let grok = tip_lines(&vendor(VendorId::Grok, "grok-log", 125.0), Kind::Weekly);
    assert_eq!(
        grok.last().unwrap().0,
        "Data is about 2 h old: Grok figures come from the CLI's local log, which only updates when you use the CLI."
    );
    let hist = tip_lines(
        &vendor(VendorId::Claude, "desktop-history", 70.0),
        Kind::Session,
    );
    assert!(
        hist.last().unwrap().0.starts_with("Data is about 1 h old"),
        "{hist:?}"
    );
    let net = tip_lines(&vendor(VendorId::Claude, "oauth", 90.0), Kind::Session);
    assert!(net.last().unwrap().0.contains("usage service"), "{net:?}");
    // not for a reading without current data (those rows are dashes with their own text)
    let mut v = vendor(VendorId::Claude, "desktop-cache", 300.0);
    v.current = false;
    assert!(tip_lines(&v, Kind::Session)
        .iter()
        .all(|t| t.1 != Tone::Caution));
    // a fresh sample never warns, whatever the poll did
    assert!(tip_lines(
        &vendor(VendorId::Claude, "desktop-cache", 3.0),
        Kind::Session
    )
    .iter()
    .all(|t| t.1 != Tone::Caution));
}

#[test]
fn the_vendor_view_carries_the_chip_and_none_when_fresh() {
    let st = aitrack::state::State::loading(Default::default(), status());
    let old = vendor(VendorId::Claude, "desktop-cache", 38.0);
    let fresh = vendor(VendorId::Grok, "grok-billing", 2.0);
    let home = build_home_for(&st, &[old, fresh], now());
    assert!(home.vendors[0]
        .age_chip
        .as_ref()
        .is_some_and(|c| c.compact == "38 min"));
    assert!(home.vendors[1].age_chip.is_none());
}

// ---- layout at 310 px (headless egui) --------------------------------------------------------

#[cfg(feature = "gui")]
mod layout {
    use super::*;
    use crate::common::TempDir;
    use aitrack::app::App;
    use aitrack::settings::Settings;
    use aitrack::store::JsonlStore;
    use aitrack::ui::AitrackUi;
    use eframe::egui::{self, epaint::Shape, Context, Pos2, RawInput, Rect};
    use serde_json::Value;
    use std::sync::Arc;

    fn draw(vendors: Vec<Vendor>) -> (Vec<(String, Rect)>, f32) {
        let d = TempDir::new();
        let app = Arc::new(App::with_poll_fn(
            Box::new(JsonlStore::new(d.join("u.jsonl"))),
            Arc::new(Settings::load(d.join("s.json"))),
            Box::new(|_| Ok(Value::Null)),
        ));
        let ctx = Context::default();
        let mut ui = AitrackUi::new(&ctx, app);
        ui.sandbox();
        ui.mock = Some(vendors);
        let mut texts = Vec::new();
        for i in 0..4 {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 700.0))),
                time: Some(i as f64 * 0.2),
                ..Default::default()
            };
            let out = ctx.run(raw, |c| ui.draw(c));
            texts.clear();
            for cs in &out.shapes {
                collect(&cs.shape, &mut texts);
            }
        }
        (texts, ui.desired_height())
    }

    fn collect(s: &Shape, out: &mut Vec<(String, Rect)>) {
        match s {
            Shape::Text(t) => out.push((t.galley.text().to_string(), t.visual_bounding_rect())),
            Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
            _ => {}
        }
    }

    fn has(texts: &[(String, Rect)], needle: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(needle))
    }

    #[test]
    fn the_long_form_fits_alone_and_the_header_height_does_not_change() {
        let calm = |mut v: Vendor| {
            for g in [v.session.as_mut(), v.weekly.as_mut()]
                .into_iter()
                .flatten()
            {
                g.rate_per_hour = Some(0.2);
                g.projected_pct = Some(g.pct + 1.0);
                g.hours_to_limit = None;
            }
            v
        };
        let (with, h_with) = draw(vec![calm(vendor_at(
            Utc::now(),
            VendorId::Claude,
            "desktop-cache",
            38.0,
        ))]);
        let (without, h_without) = draw(vec![calm(vendor_at(
            Utc::now(),
            VendorId::Claude,
            "desktop-cache",
            2.0,
        ))]);
        assert!(has(&with, "about 38 minutes ago"), "{with:?}");
        assert!(!has(&without, "minutes ago"));
        assert_eq!(
            h_with, h_without,
            "the chip never changes the header height"
        );
    }

    #[test]
    fn next_to_an_empty_warning_the_chip_shortens_and_nothing_overflows_or_wraps() {
        // Grok at 94 % weekly: "Empty in ~10h" warning, plus a 50 minute old log sample
        let (texts, _) = draw(vec![vendor_at(
            Utc::now(),
            VendorId::Grok,
            "grok-log",
            50.0,
        )]);
        assert!(has(&texts, "Empty in"), "{texts:?}");
        assert!(
            has(&texts, "50 min"),
            "compact chip text expected: {texts:?}"
        );
        assert!(
            !has(&texts, "about 50 minutes ago"),
            "the long form does not fit next to the warning"
        );
        for (t, r) in texts.iter().filter(|(t, _)| !t.is_empty()) {
            assert!(r.right() <= 310.5 + 1.0, "{t:?} overflows: {r:?}");
        }
    }

    /// All tooltip areas currently on screen.
    fn tooltip_rects(ctx: &Context) -> Vec<Rect> {
        ctx.memory(|m| {
            m.areas()
                .visible_layer_ids()
                .iter()
                .filter(|l| l.order == egui::Order::Tooltip)
                .filter_map(|l| m.area_rect(l.id))
                .collect()
        })
    }

    #[test]
    fn the_age_chip_tooltip_wraps_and_stays_inside_the_window_in_both_themes() {
        for light in [false, true] {
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
            let mut ui = AitrackUi::new(&ctx, app);
            ui.sandbox();
            ui.mock = Some(vec![vendor_at(
                Utc::now(),
                VendorId::Claude,
                "desktop-cache",
                46.0,
            )]);
            let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(310.0, 700.0));
            let mut t = 0.0;
            let mut frame = |ui: &mut AitrackUi, events: Vec<egui::Event>| {
                t += 0.2;
                let raw = RawInput {
                    screen_rect: Some(screen),
                    time: Some(t),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(raw, |c| ui.draw(c));
            };
            for _ in 0..4 {
                frame(&mut ui, vec![]);
            }
            // sweep the pointer along the header line until the chip's tooltip shows
            let y = ui.badge_rects[0].1.center().y;
            let mut found = None;
            for x in (120..305).step_by(6) {
                frame(&mut ui, vec![egui::Event::PointerGone]);
                frame(
                    &mut ui,
                    vec![egui::Event::PointerMoved(Pos2::new(x as f32, y))],
                );
                for _ in 0..5 {
                    frame(&mut ui, vec![]);
                }
                if let Some(r) = tooltip_rects(&ctx).first() {
                    found = Some((x, *r));
                    break;
                }
            }
            let (x, r) =
                found.unwrap_or_else(|| panic!("no tooltip on the header (light {light})"));
            assert!(
                screen.contains_rect(r),
                "tooltip {r:?} (pointer at x={x}, light {light}) leaves the window {screen:?}"
            );
            // the whole tooltip (text + frame) stays within the window minus 12 pt on each side
            assert!(
                r.width() <= 310.0 - 24.0 + 0.5,
                "{r:?} wider than the window minus its margins"
            );
        }
    }

    #[test]
    fn the_tooltip_cap_is_the_window_minus_a_margin_and_never_huge() {
        // text width = window - 24 - the frame around the text, within 120..=280
        assert_eq!(aitrack::ui::tooltip_width(310.0, 12.0), 274.0);
        assert_eq!(aitrack::ui::tooltip_width(310.0, 0.0), 280.0);
        assert_eq!(aitrack::ui::tooltip_width(250.0, 12.0), 214.0);
        assert_eq!(aitrack::ui::tooltip_width(100.0, 12.0), 120.0);
        assert_eq!(aitrack::ui::tooltip_width(2000.0, 12.0), 280.0);
    }
}
