// SPDX-License-Identifier: GPL-3.0-or-later
//! Render the window offscreen (software rasteriser over egui's tessellated output) to PNG files,
//! so the layout can be eyeballed without a display:  cargo run --example render -- OUT_DIR
//!
//! This is an approximation of what the GPU backend draws (no text hinting differences are
//! modelled, fonts are egui's bundled ones unless Windows fonts exist), not a pixel-exact capture.

use aitrack::app::App;
use aitrack::collector::PollFn;
use aitrack::reading::Kind;
use aitrack::settings::{BadgeVariant, Settings};
use aitrack::store::{JsonlStore, Store};
use aitrack::ui::{AitrackUi, Screen};
use aitrack::util::now_iso;
use aitrack::vendor::{self, VendorId};
use aitrack::view::Range;
use chrono::{Duration, Utc};
use eframe::egui::{self, epaint, Color32, Context, Pos2, RawInput, Rect, TextureId};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

const PPP: f32 = 2.0;
const W: f32 = 310.0;
/// Width of the scene being drawn (a few scenes show a wider window).
static WIDTH_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn width() -> f32 {
    match WIDTH_BITS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => W,
        b => f32::from_bits(b),
    }
}

struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

fn synth(
    now: chrono::DateTime<Utc>,
    session_peak: f64,
    weekly: f64,
    with_models: bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seed = 12345u64;
    let mut rnd = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as f64 / (1u64 << 31) as f64
    };
    let start = now - Duration::days(3);
    let anchor = now + Duration::minutes(110); // current 5h window resets in ~1h50
    let mut t = start;
    let mut pct = 0.0;
    let mut cur_reset = anchor;
    while t < now {
        let win = {
            let k = ((anchor - t).num_minutes() as f64 / 300.0).floor() as i64;
            anchor - Duration::minutes(300 * k)
        };
        if win != cur_reset {
            cur_reset = win;
            pct = 0.0;
        }
        let busy = rnd() < 0.55;
        if busy {
            pct = (pct + rnd() * 9.0).min(100.0);
        }
        let in_current = cur_reset == anchor;
        let shown = if in_current {
            pct.min(session_peak)
        } else {
            pct
        };
        let frac = (t - start).num_minutes() as f64 / (3.0 * 24.0 * 60.0);
        let mut rec = json!({"v":1,"ts":t.to_rfc3339_opts(chrono::SecondsFormat::Secs,false),"ok":true,
            "session":{"pct":shown,"resets_at":cur_reset.to_rfc3339()},
            "weekly":{"pct":weekly*(0.35+0.65*frac),"resets_at":(now+Duration::days(3)).to_rfc3339()},
            "spend":{"enabled":true,"used":13.59,"limit":50.0},"raw":{}});
        if with_models {
            rec["limits"] = json!([
                {"kind":"weekly_scoped","percent":43,"resets_at":(now+Duration::days(3)).to_rfc3339(),"scope":{"model":{"display_name":"Fable"}}},
                {"kind":"weekly_scoped","percent":12,"resets_at":(now+Duration::days(3)).to_rfc3339(),"scope":{"model":{"display_name":"Opus"}}}]);
        }
        out.push(rec);
        t += Duration::minutes(15);
    }
    out
}

/// Turn synthetic records into what the desktop-history source stores: no reset times, no spend,
/// no per-model limits, tagged with the source.
fn as_desktop(mut records: Vec<Value>) -> Vec<Value> {
    for r in &mut records {
        r["session"]["resets_at"] = Value::Null;
        r["weekly"]["resets_at"] = Value::Null;
        r["spend"] = Value::Null;
        r["limits"] = Value::Null;
        r["source"] = json!("desktop-history");
        r["observed_at"] = r["ts"].clone();
    }
    records
}

fn make_app(
    dir: &std::path::Path,
    name: &str,
    records: Vec<Value>,
    fail: Option<&str>,
) -> Arc<App> {
    Arc::new(build_app(dir, name, records, fail))
}

/// Claude plus Grok (separate store): `grok_records` are what `usage-grok.jsonl` would hold.
fn make_app_grok(
    dir: &std::path::Path,
    name: &str,
    claude: Vec<Value>,
    grok_records: Vec<Value>,
) -> Arc<App> {
    let app = build_app(dir, name, claude, None);
    let path = dir.join(format!("{name}-grok.jsonl"));
    let _ = std::fs::remove_file(&path);
    let store = JsonlStore::new(&path);
    for r in &grok_records {
        store.append(r).unwrap();
    }
    let poll: PollFn = Box::new(|s: &dyn Store| {
        // "checked": nothing new is appended (the log has not changed)
        let mut rec = s.read().last().cloned().unwrap_or(Value::Null);
        rec["stored"] = json!(false);
        Ok(rec)
    });
    let app = app.with_grok(
        Box::new(JsonlStore::new(&path)),
        poll,
        None,
        Arc::new(|| true),
    );
    let g = app.grok.as_ref().unwrap();
    g.history.load();
    g.collector.poll_blocking();
    Arc::new(app)
}

/// Swallows every notification: rendering a scene is not a reason to show a toast.
struct Silent;

impl aitrack::alerts::Notifier for Silent {
    fn notify(&self, _: &aitrack::alerts::Notification) -> Result<(), String> {
        Ok(())
    }
}

fn build_app(dir: &std::path::Path, name: &str, records: Vec<Value>, fail: Option<&str>) -> App {
    let path = dir.join(format!("{name}.jsonl"));
    let _ = std::fs::remove_file(&path);
    let store = JsonlStore::new(&path);
    for r in &records {
        store.append(r).unwrap();
    }
    let settings_path = dir.join(format!("{name}-settings.json"));
    let _ = std::fs::remove_file(&settings_path); // start every scene from the defaults
    let settings = Arc::new(Settings::load(settings_path));
    let fail = fail.map(str::to_owned);
    let poll: PollFn = Box::new(move |s: &dyn Store| match &fail {
        Some(msg) => Err(aitrack::live::CollectError::new(msg.clone())),
        None => {
            let mut rec = s.read().last().cloned().unwrap_or(Value::Null);
            if rec["source"] == "desktop-history" || rec["source"] == "desktop-cache" {
                // "checked, nothing new": the app is not running, no row is added
                rec["stored"] = json!(false);
                return Ok(rec);
            }
            rec["ts"] = json!(aitrack::util::now_iso());
            s.append(&rec).unwrap();
            Ok(rec)
        }
    });
    let app = App::with_poll_fn(Box::new(JsonlStore::new(&path)), settings, poll);
    // Scenes at 75 % and above (and the Settings test scenes) must never raise a real toast on the
    // machine that renders them.
    app.set_notifier(Arc::new(Silent));
    app.history.load();
    app.collector.poll_blocking();
    app
}

fn run_frames(
    ctx: &Context,
    ui: &mut AitrackUi,
    texs: &mut HashMap<TextureId, Tex>,
    h: f32,
    hover: Option<(VendorId, Kind)>,
) -> Vec<egui::ClippedPrimitive> {
    let mut last = None;
    for i in 0..6 {
        let mut events = vec![];
        if let Some((v, k)) = hover.filter(|_| i >= 1) {
            if let Some((_, _, r)) = ui.row_rects.iter().find(|(a, b, _)| *a == v && *b == k) {
                events.push(egui::Event::PointerMoved(
                    r.center() + egui::vec2(20.0, 0.0),
                ));
            }
        }
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(width(), h))),
            time: Some(i as f64 * 0.3),
            events,
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| ui.draw(ctx));
        for (id, delta) in &out.textures_delta.set {
            let (w, h) = (delta.image.width(), delta.image.height());
            let pixels: Vec<Color32> = match &delta.image {
                epaint::ImageData::Color(c) => c.pixels.clone(),
                epaint::ImageData::Font(f) => f.srgba_pixels(None).collect(),
            };
            let tex = texs.entry(*id).or_insert_with(|| Tex {
                w: 0,
                h: 0,
                px: vec![],
            });
            match delta.pos {
                None => *tex = Tex { w, h, px: pixels },
                Some([x0, y0]) => {
                    for y in 0..h {
                        for x in 0..w {
                            tex.px[(y0 + y) * tex.w + x0 + x] = pixels[y * w + x];
                        }
                    }
                }
            }
        }
        last = Some(ctx.tessellate(out.shapes, PPP));
    }
    last.unwrap()
}

fn rasterize(
    prims: &[egui::ClippedPrimitive],
    texs: &HashMap<TextureId, Tex>,
    bg: Color32,
    h: f32,
) -> Vec<u8> {
    let (pw, ph) = ((width() * PPP) as usize, (h * PPP) as usize);
    let mut buf = vec![
        [
            bg.r() as f32 / 255.0,
            bg.g() as f32 / 255.0,
            bg.b() as f32 / 255.0
        ];
        pw * ph
    ];
    for prim in prims {
        let epaint::Primitive::Mesh(mesh) = &prim.primitive else {
            continue;
        };
        let Some(tex) = texs.get(&mesh.texture_id) else {
            continue;
        };
        let clip = prim.clip_rect;
        let (cx0, cy0) = (
            (clip.min.x * PPP).max(0.0) as usize,
            (clip.min.y * PPP).max(0.0) as usize,
        );
        let (cx1, cy1) = (
            ((clip.max.x * PPP).ceil() as usize).min(pw),
            ((clip.max.y * PPP).ceil() as usize).min(ph),
        );
        for tri in mesh.indices.chunks(3) {
            let v: Vec<_> = tri.iter().map(|&i| mesh.vertices[i as usize]).collect();
            let p: Vec<(f32, f32)> = v.iter().map(|v| (v.pos.x * PPP, v.pos.y * PPP)).collect();
            let area =
                (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
            if area.abs() < 1e-6 {
                continue;
            }
            let minx = p
                .iter()
                .map(|q| q.0)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(cx0 as f32) as usize;
            let maxx = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as usize).min(cx1);
            let miny = p
                .iter()
                .map(|q| q.1)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(cy0 as f32) as usize;
            let maxy = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as usize).min(cy1);
            for y in miny..maxy {
                for x in minx..maxx {
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = ((p[1].0 - px) * (p[2].1 - py) - (p[2].0 - px) * (p[1].1 - py)) / area;
                    let w1 = ((p[2].0 - px) * (p[0].1 - py) - (p[0].0 - px) * (p[2].1 - py)) / area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                    let vv = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                    let (tx, ty) = (
                        ((u * tex.w as f32) as usize).min(tex.w - 1),
                        ((vv * tex.h as f32) as usize).min(tex.h - 1),
                    );
                    let t = tex.px[ty * tex.w + tx].to_array().map(|c| c as f32 / 255.0);
                    let col: Vec<f32> = (0..4)
                        .map(|k| {
                            let c = |i: usize| v[i].color.to_array()[k] as f32 / 255.0;
                            (w0 * c(0) + w1 * c(1) + w2 * c(2)) * t[k]
                        })
                        .collect();
                    let d = &mut buf[y * pw + x];
                    for k in 0..3 {
                        d[k] = col[k] + d[k] * (1.0 - col[3]);
                    }
                }
            }
        }
    }
    buf.iter()
        .flat_map(|c| {
            [
                (c[0] * 255.0 + 0.5) as u8,
                (c[1] * 255.0 + 0.5) as u8,
                (c[2] * 255.0 + 0.5) as u8,
                255,
            ]
        })
        .collect()
}

fn save(path: &std::path::Path, rgba: &[u8], h: f32) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(file, (width() * PPP) as u32, (h * PPP) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}

type Setup = Box<dyn Fn(&Context, &mut AitrackUi, &Arc<App>)>;

struct Scene {
    name: &'static str,
    app: Arc<App>,
    setup: Setup,
    light: bool,
    hover: Option<(VendorId, Kind)>,
    /// Window width in points (`0` = the standard 310).
    width: f32,
}

fn scene(name: &'static str, app: Arc<App>, setup: Setup) -> Scene {
    Scene {
        name,
        app,
        setup,
        light: false,
        hover: None,
        width: 0.0,
    }
}

/// A snapshot for the Stats view: fabricated requests (not read from disk) priced with the
/// built-in table, the real plan statistics of `app`'s readings, and a few source folders.
fn stats_snapshot(app: &Arc<App>, populated: bool) -> aitrack::statsview::Snapshot {
    use aitrack::pricing::{PriceTable, TokenUsage};
    use aitrack::stats::{compute, StatsRange};
    use aitrack::statsview::{PlanData, Snapshot};
    use aitrack::transcripts::{aggregate, Origin, Request, SourceInfo};
    let now = Utc::now();
    let mut reqs: Vec<Request> = Vec::new();
    if populated {
        let mut add = |project: &str,
                       origin: Origin,
                       hours_ago: i64,
                       n: u64,
                       model: &str,
                       ticks: Option<u64>| {
            for i in 0..n {
                reqs.push(Request {
                    at: (now - Duration::hours(hours_ago) - Duration::minutes(i as i64))
                        .timestamp(),
                    origin,
                    project: project.into(),
                    session: format!("{project}-{}", i / 20),
                    model: model.into(),
                    usage: TokenUsage {
                        input: 3_000,
                        output: 1_200,
                        cache_read: 150_000,
                        cache_write_5m: 6_000,
                        cache_write_1h: 0,
                    },
                    cost_ticks: ticks,
                    sidechain: i % 5 == 0,
                    key: 0,
                });
            }
        };
        add("aiTrack", Origin::Claude, 1, 90, "claude-opus-4-8", None);
        add("aiTrack", Origin::Claude, 30, 120, "claude-opus-4-8", None);
        add(
            "website-redesign-with-a-long-name",
            Origin::Claude,
            50,
            70,
            "claude-sonnet-4-5",
            None,
        );
        add(
            "scratch",
            Origin::Claude,
            24 * 5,
            40,
            "claude-sonnet-4-5",
            None,
        );
        add(
            "dotfiles",
            Origin::Claude,
            24 * 12,
            6,
            "claude-haiku-4-5",
            None,
        );
        add(
            "grok-experiments",
            Origin::GrokUpdate,
            3,
            25,
            "grok-4",
            Some(180_000_000),
        );
        add("grok-experiments", Origin::GrokLog, 40, 10, "grok-4", None);
    }
    let prices = PriceTable::fallback();
    let report = aggregate(reqs.iter(), &prices, now, 0);
    let plan = app.history.with_readings(|r| PlanData {
        claude: StatsRange::ALL
            .into_iter()
            .map(|k| (k, compute(r, k, now, 0)))
            .collect(),
        grok: None,
    });
    let src = |label: &str, path: &str, exists: bool, files: usize| SourceInfo {
        label: label.into(),
        path: path.into(),
        exists,
        files,
        note: String::new(),
    };
    Snapshot {
        projects: Some(Arc::new(report)),
        sources: vec![
            src(
                "Claude Code transcripts",
                "C:\\Users\\you\\.claude\\projects",
                populated,
                if populated { 412 } else { 0 },
            ),
            src(
                "Grok sessions",
                "C:\\Users\\you\\.grok\\sessions",
                populated,
                3,
            ),
            src(
                "Grok CLI log",
                "C:\\Users\\you\\.grok\\logs\\unified.jsonl",
                false,
                0,
            ),
        ],
        scan: None,
        plan: Some(Arc::new(plan)),
        busy: false,
    }
}

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .map(Into::into)
        .unwrap_or_else(|| "render-out".into());
    std::fs::create_dir_all(&out).unwrap();
    let now = Utc::now();
    let claude = |name: &str, s: f64, w: f64| make_app(&out, name, synth(now, s, w, true), None);
    let mock = |ui: &mut AitrackUi| ui.mock = Some(vendor::demo_vendors(Utc::now()));
    let all4 = || make_app(&out, "demo", synth(now, 42.0, 58.0, true), None);

    let mut scenes = vec![
        scene(
            "compact_1vendor",
            claude("c1", 42.0, 58.0),
            Box::new(|_, _, _| {}),
        ),
        scene(
            "compact_expanded_weekly",
            claude("c2", 42.0, 58.0),
            Box::new(|_, ui, _| {
                ui.expanded = Some((VendorId::Claude, Kind::Weekly));
                ui.set_range(Range::Week);
            }),
        ),
        scene(
            "compact_expanded_session",
            claude("c3", 42.0, 58.0),
            Box::new(|_, ui, _| ui.expanded = Some((VendorId::Claude, Kind::Session))),
        ),
        scene(
            "compact_remaining",
            claude("c4", 42.0, 58.0),
            Box::new(|_, _, app| app.set_direction(aitrack::settings::Direction::Remaining)),
        ),
        scene(
            "compact_hot",
            claude("c5", 93.0, 91.0),
            Box::new(|_, _, _| {}),
        ),
        scene(
            "compact_4vendors",
            all4(),
            Box::new(move |_, ui, _| mock(ui)),
        ),
        scene(
            "compact_4vendors_expanded",
            all4(),
            Box::new(move |_, ui, _| {
                mock(ui);
                ui.expanded = Some((VendorId::Codex, Kind::Session));
            }),
        ),
        scene(
            "compact_4vendors_remaining",
            all4(),
            Box::new(move |_, ui, app| {
                mock(ui);
                app.set_direction(aitrack::settings::Direction::Remaining);
            }),
        ),
        scene(
            "settings",
            claude("set", 42.0, 58.0),
            Box::new(|_, ui, _| ui.screen = Screen::Settings),
        ),
        scene(
            "empty_error",
            make_app(
                &out,
                "err",
                vec![],
                Some("no Claude login found (looked in: ~/.claude/.credentials.json)"),
            ),
            Box::new(|_, _, _| {}),
        ),
    ];
    let mut light = scene(
        "compact_4vendors_light",
        all4(),
        Box::new(move |c, ui, _| {
            mock(ui);
            c.set_theme(egui::ThemePreference::Light);
        }),
    );
    light.light = true;
    scenes.push(light);
    // the badge variants (filled / dark / light) in both UI themes
    for (name, variant, is_light) in [
        ("badges_filled_dark", BadgeVariant::Filled, false),
        ("badges_dark_dark", BadgeVariant::Dark, false),
        ("badges_light_dark", BadgeVariant::Light, false),
        ("badges_filled_light", BadgeVariant::Filled, true),
        ("badges_dark_light", BadgeVariant::Dark, true),
        ("badges_light_light", BadgeVariant::Light, true),
    ] {
        let mut sc = scene(
            name,
            make_app(&out, name, synth(now, 42.0, 58.0, true), None),
            Box::new(move |c, ui, app| {
                mock(ui);
                app.set_badge_style(variant);
                if is_light {
                    c.set_theme(egui::ThemePreference::Light);
                }
            }),
        );
        sc.light = is_light;
        scenes.push(sc);
    }
    // Settings: collapsed (the default) and everything expanded, dark and light
    for (name, open_all, is_light) in [
        ("settings_collapsed", false, false),
        ("settings_collapsed_light", false, true),
        ("settings_expanded", true, false),
        ("settings_expanded_light", true, true),
    ] {
        let app = make_app(&out, name, synth(now, 42.0, 58.0, true), None);
        let mut sc = scene(
            name,
            app,
            Box::new(move |c, ui, app| {
                for sec in aitrack::settings::Sections::NAMES {
                    app.settings.set_section(sec, open_all || sec == "general");
                }
                ui.screen = Screen::Settings;
                if is_light {
                    c.set_theme(egui::ThemePreference::Light);
                }
            }),
        );
        sc.light = is_light;
        scenes.push(sc);
    }
    // only the Notifications section open (the long "Persistent warnings" description)
    for (name, is_light) in [
        ("settings_notifications", false),
        ("settings_notifications_light", true),
    ] {
        let app = make_app(&out, name, synth(now, 42.0, 58.0, true), None);
        let mut sc = scene(
            name,
            app,
            Box::new(move |c, ui, app| {
                for sec in aitrack::settings::Sections::NAMES {
                    app.settings.set_section(sec, sec == "notifications");
                }
                ui.screen = Screen::Settings;
                if is_light {
                    c.set_theme(egui::ThemePreference::Light);
                }
            }),
        );
        sc.light = is_light;
        scenes.push(sc);
    }
    // nothing installed: the empty state (and the Settings view with the Auto / On / Off rows)
    for (name, screen) in [
        ("nothing_to_track", Screen::Home),
        ("settings_vendor_rows", Screen::Settings),
    ] {
        let app = make_app(&out, name, synth(now, 42.0, 58.0, true), None);
        scenes.push(scene(
            name,
            app,
            Box::new(move |_, ui, app| {
                app.set_claude_detect(Arc::new(|| false));
                ui.screen = screen;
            }),
        ));
    }
    // the Stats view: populated, empty (no transcripts found), and updating
    for (name, populated, busy, light) in [
        ("stats_populated", true, false, false),
        ("stats_empty", false, false, false),
        ("stats_updating", true, true, false),
        ("stats_populated_light", true, false, true),
    ] {
        let app = make_app(&out, name, synth(now, 42.0, 58.0, true), None);
        let mut sc = scene(
            name,
            app,
            Box::new(move |c, ui, app| {
                let mut snap = stats_snapshot(app, populated);
                snap.busy = busy;
                app.set_stats(aitrack::statsview::StatsService::fixed(snap));
                ui.screen = Screen::Stats;
                if light {
                    c.set_theme(egui::ThemePreference::Light);
                }
            }),
        );
        sc.light = light;
        scenes.push(sc);
    }
    // the data-age chip and the stale-data tooltip line (mock vendors with old samples)
    let aged =
        |claude: Option<(&'static str, i64)>, grok: Option<(&'static str, i64)>, calm: bool| {
            move |ui: &mut AitrackUi| {
                let now = Utc::now();
                let mut vs = vendor::demo_vendors(now);
                vs.retain(|v| {
                    (claude.is_some() && v.id == VendorId::Claude)
                        || (grok.is_some() && v.id == VendorId::Grok)
                });
                for v in &mut vs {
                    let (src, mins) = if v.id == VendorId::Claude {
                        claude.unwrap()
                    } else {
                        grok.unwrap()
                    };
                    v.current = true;
                    v.source = Some(src.into());
                    v.observed_at = Some(now - Duration::minutes(mins));
                    if calm {
                        for g in [v.session.as_mut(), v.weekly.as_mut()]
                            .into_iter()
                            .flatten()
                        {
                            g.rate_per_hour = Some(0.2);
                            g.projected_pct = Some(g.pct + 1.0);
                            g.hours_to_limit = None;
                        }
                    }
                }
                ui.mock = Some(vs);
            }
        };
    for (name, claude, grok, calm, hover) in [
        ("chip_alone", Some(("desktop-cache", 38)), None, true, None),
        (
            "chip_and_empty_warning",
            Some(("desktop-cache", 45)),
            None,
            false,
            None,
        ),
        (
            "chip_grok_log_tooltip",
            None,
            Some(("grok-log", 120)),
            true,
            Some((VendorId::Grok, Kind::Weekly)),
        ),
        (
            "chip_both_vendors",
            Some(("desktop-history", 20)),
            Some(("grok-log", 200)),
            false,
            None,
        ),
        (
            "chip_fresh_none",
            Some(("desktop-cache", 5)),
            Some(("grok-billing", 3)),
            true,
            None,
        ),
    ] {
        let f = aged(claude, grok, calm);
        let mut sc = scene(name, all4(), Box::new(move |_, ui, _| f(ui)));
        sc.hover = hover;
        scenes.push(sc);
    }
    // Grok: a nine-hour-old last value (dimmed, chip) with the Refresh control in each state
    for (name, mode) in [
        ("grok_stale_refresh", aitrack::app::RefreshMode::Offer),
        ("grok_refreshing", aitrack::app::RefreshMode::Running),
        (
            "grok_refresh_failed",
            aitrack::app::RefreshMode::Failed(
                "Grok did not fetch billing within 10 s (signed out?)".into(),
            ),
        ),
    ] {
        let f = aged(None, Some(("grok-log", 540)), true);
        let mut sc = scene(
            name,
            all4(),
            Box::new(move |_, ui, _| {
                f(ui);
                ui.mock_refresh = Some(mode.clone());
            }),
        );
        sc.hover = None;
        scenes.push(sc);
    }
    // the light theme, screen by screen (contrast review)
    for (name, setup_kind) in [
        ("light_expanded", 0),
        ("light_settings", 1),
        ("light_tooltip", 2),
        ("light_chip", 3),
        ("light_hot", 4),
    ] {
        let app = if setup_kind == 4 {
            make_app(&out, name, synth(now, 93.0, 91.0, true), None)
        } else {
            make_app(&out, name, synth(now, 42.0, 58.0, true), None)
        };
        let f = aged(Some(("desktop-cache", 38)), Some(("grok-log", 120)), false);
        let mut sc = scene(
            name,
            app,
            Box::new(move |c, ui, _| {
                c.set_theme(egui::ThemePreference::Light);
                match setup_kind {
                    0 => {
                        mock(ui);
                        ui.expanded = Some((VendorId::Claude, Kind::Session));
                    }
                    1 => ui.screen = Screen::Settings,
                    2 => mock(ui),
                    3 => f(ui),
                    _ => {}
                }
            }),
        );
        sc.light = true;
        if setup_kind == 2 {
            sc.hover = Some((VendorId::Claude, Kind::Weekly));
        }
        scenes.push(sc);
    }
    let desk = |name: &str, minutes_old: i64| {
        make_app(
            &out,
            name,
            as_desktop(synth(
                now - Duration::minutes(minutes_old),
                42.0,
                58.0,
                false,
            )),
            None,
        )
    };
    scenes.push(scene(
        "desktop_fresh",
        desk("d1", 3),
        Box::new(|_, _, _| {}),
    ));
    scenes.push(scene(
        "desktop_app_closed",
        desk("d2", 47),
        Box::new(|_, ui, _| ui.expanded = Some((VendorId::Claude, Kind::Session))),
    ));
    // one reading from the desktop cache: real reset times, pace from the elapsed time of the window
    let cache_rec = |session: f64, weekly: f64, left_min: i64| {
        let ts = (now - Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        vec![
            json!({"v":1,"ts":ts,"ok":true,"source":"desktop-cache","observed_at":ts,
            "session":{"pct":session,"resets_at":(now+Duration::minutes(left_min)).to_rfc3339()},
            "weekly":{"pct":weekly,"resets_at":(now+Duration::hours(149)).to_rfc3339()},
            "spend":null,"limits":null,"raw":{}}),
        ]
    };
    scenes.push(scene(
        "inline_warning",
        make_app(&out, "w1", cache_rec(55.0, 8.0, 200), None),
        Box::new(|_, _, _| {}),
    ));
    scenes.push(scene(
        "inline_warning_both_remaining",
        make_app(&out, "w2", cache_rec(55.0, 93.0, 200), None),
        Box::new(|_, _, app| app.set_direction(aitrack::settings::Direction::Remaining)),
    ));
    // weekly would run out first: named in the header ("Weekly: empty in ~...")
    scenes.push(scene(
        "header_warning_weekly",
        make_app(&out, "w4", cache_rec(20.0, 70.0, 200), None),
        Box::new(|_, _, _| {}),
    ));
    // a long vendor name leaves little room: the compact text is used
    scenes.push(scene(
        "header_warning_long_name",
        all4(),
        Box::new(|_, ui, _| {
            let mut v = vendor::demo_vendors(Utc::now());
            let ag = &mut v[2];
            let g = ag.session.as_mut().unwrap();
            g.pct = 88.0;
            g.rate_per_hour = Some(25.0);
            g.hours_left = Some(2.0);
            g.hours_to_limit = Some(0.5);
            ui.mock = Some(v);
        }),
    ));
    // yesterday's history: 0% samples that must not look like the present
    let stale_zero = |hours_old: i64| {
        let ts =
            (now - Duration::hours(hours_old)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        vec![
            json!({"v":1,"ts":ts,"ok":true,"source":"desktop-history","observed_at":ts,
            "session":{"pct":0.0,"resets_at":null},"weekly":{"pct":0.0,"resets_at":null},
            "spend":null,"limits":null,"raw":{}}),
        ]
    };
    scenes.push(scene(
        "stale_no_current_data",
        make_app(&out, "st1", stale_zero(13), None),
        Box::new(|_, _, _| {}),
    ));
    let mut t = scene(
        "stale_tooltip",
        make_app(&out, "st2", stale_zero(13), None),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Session));
    scenes.push(t);
    let long_err = "Claude desktop usage is stale (last sample 13 h ago): open the Claude app; ~/.claude.json has no usable usage cache (file not found); usage API: no Claude login found (looked in: C:\\Users\\someone\\.claude\\.credentials.json). Sign in with Claude Code (run `claude`, then /login); a login that only exists in the Claude desktop app is not supported yet";
    let err_rec = json!({"v":1,"ts":now_iso(),"ok":false,"error":long_err});
    scenes.push(scene(
        "long_error_home",
        make_app(&out, "le1", vec![err_rec.clone()], Some(long_err)),
        Box::new(|_, _, _| {}),
    ));
    scenes.push(scene(
        "long_error_settings",
        make_app(&out, "le2", vec![err_rec], Some(long_err)),
        Box::new(|_, ui, _| ui.screen = Screen::Settings),
    ));
    let mut t = scene(
        "pace_tooltip",
        make_app(&out, "pt", cache_rec(55.0, 8.0, 200), None),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Session));
    scenes.push(t);
    // the reported case: weekly 1% with 6d 4h left (window opened ~20 h ago) must not warn
    let weekly_rec = |session: f64, weekly: f64| {
        let ts = (now - Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        vec![
            json!({"v":1,"ts":ts,"ok":true,"source":"desktop-cache","observed_at":ts,
            "session":{"pct":session,"resets_at":(now+Duration::minutes(200)).to_rfc3339()},
            "weekly":{"pct":weekly,"resets_at":(now+Duration::hours(148)).to_rfc3339()},
            "spend":null,"limits":null,"raw":{}}),
        ]
    };
    scenes.push(scene(
        "weekly_1pct_no_warning",
        make_app(&out, "wk1", weekly_rec(55.0, 1.0), None),
        Box::new(|_, _, _| {}),
    ));
    let mut t = scene(
        "weekly_1pct_tooltip",
        make_app(&out, "wk2", weekly_rec(30.0, 1.0), None),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Weekly));
    scenes.push(t);
    // Grok: weekly pool only. Real-data-shaped: 7% from the CLI log, period ending in ~1d 22h.
    let grok_rec = |minutes_ago: i64, pct: f64, source: &str| {
        let ts = (now - Duration::minutes(minutes_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        json!({"v":1,"ts":ts,"ok":true,"vendor":"grok","source":source,"observed_at":ts,
            "session":null,"weekly":{"pct":pct,"resets_at":(now+Duration::hours(46)+Duration::minutes(11)).to_rfc3339()},
            "spend":null,"limits":null,"raw":{"tier":"SuperGrok Heavy"}})
    };
    let grok_history = || {
        vec![
            grok_rec(60 * 60, 1.0, "grok-log"),
            grok_rec(60 * 40, 2.0, "grok-log"),
            grok_rec(60 * 20, 4.0, "grok-log"),
            grok_rec(60 * 6, 5.0, "grok-log"),
            grok_rec(14, 7.0, "grok-log"),
        ]
    };
    scenes.push(scene(
        "grok_only",
        make_app_grok(&out, "gk1", vec![], grok_history()),
        Box::new(|_, _, _| {}),
    ));
    scenes.push(scene(
        "claude_and_grok",
        make_app_grok(&out, "gk2", cache_rec(42.0, 8.0, 200), grok_history()),
        Box::new(|_, _, _| {}),
    ));
    // the README's light-theme picture: Claude and Grok, real vendors only
    let mut sc = scene(
        "readme_home_light",
        make_app_grok(&out, "rd1", cache_rec(42.0, 8.0, 200), grok_history()),
        Box::new(|c, _, _| c.set_theme(egui::ThemePreference::Light)),
    );
    sc.light = true;
    scenes.push(sc);
    let mut t = scene(
        "grok_tooltip",
        make_app_grok(&out, "gk3", cache_rec(42.0, 8.0, 200), grok_history()),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Grok, Kind::Weekly));
    scenes.push(t);
    scenes.push(scene(
        "grok_expanded_chart",
        make_app_grok(&out, "gk4", cache_rec(42.0, 8.0, 200), grok_history()),
        Box::new(|_, ui, _| {
            ui.expanded = Some((VendorId::Grok, Kind::Weekly));
            ui.set_range(Range::Week);
        }),
    ));
    // History chart: 24 h of uneven 5-15 minute samples, the cache ones carry the reset time and the
    // history ones do not (the user's flat 8-9 % screenshot), and a rising line
    let chart_records = |rising: bool| {
        let mut v = Vec::new();
        let (mut m, mut i) = (24 * 60 - 3, 0usize);
        while m > 0 {
            let pct = if rising {
                (1.0 + (24.0 * 60.0 - m as f64) / 60.0 * 1.6)
                    .min(60.0)
                    .round()
            } else {
                8.0 + (i % 3 / 2) as f64
            };
            let ts =
                (now - Duration::minutes(m)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
            let resets = (i % 2 == 0).then(|| (now + Duration::hours(46)).to_rfc3339());
            v.push(json!({"v":1,"ts":ts,"ok":true,"vendor":"grok",
                "source": if i % 2 == 0 {"grok-billing"} else {"grok-log"}, "observed_at": ts,
                "session":null,"weekly":{"pct":pct,"resets_at":resets},
                "spend":null,"limits":null,"raw":{"tier":"SuperGrok Heavy"}}));
            m -= [5, 15, 10, 7, 12][i % 5];
            i += 1;
        }
        v
    };
    for (name, rising, wide) in [
        ("chart_flat_narrow", false, false),
        ("chart_flat_wide", false, true),
        ("chart_rising_narrow", true, false),
        ("chart_rising_wide", true, true),
    ] {
        let mut sc = scene(
            name,
            make_app_grok(&out, name, cache_rec(42.0, 8.0, 200), chart_records(rising)),
            Box::new(|_, ui, _| {
                ui.expanded = Some((VendorId::Grok, Kind::Weekly));
                ui.set_range(Range::Day);
            }),
        );
        if wide {
            sc.width = 560.0;
        }
        scenes.push(sc);
    }
    scenes.push(scene(
        "grok_stale",
        make_app_grok(
            &out,
            "gk5",
            cache_rec(42.0, 8.0, 200),
            vec![grok_rec(60 * 13, 7.0, "grok-log")],
        ),
        Box::new(|_, _, _| {}),
    ));
    scenes.push(scene(
        "claude_launch_failed",
        make_app(&out, "lf1", cache_rec(42.0, 8.0, 200), None),
        Box::new(|_, ui, _| ui.set_launch_notice("Could not launch Claude desktop")),
    ));
    scenes.push(scene(
        "inline_no_warning",
        make_app(&out, "w3", cache_rec(20.0, 8.0, 200), None),
        Box::new(|_, _, _| {}),
    ));
    let mut t = scene(
        "desktop_tooltip_unknown_reset",
        desk("d3", 3),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Session));
    scenes.push(t);
    let mut t = scene(
        "compact_tooltip_weekly",
        claude("c6", 42.0, 58.0),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Weekly));
    scenes.push(t);
    let mut t = scene(
        "compact_tooltip_session",
        claude("c7", 42.0, 58.0),
        Box::new(|_, _, _| {}),
    );
    t.hover = Some((VendorId::Claude, Kind::Session));
    scenes.push(t);
    let mut t = scene(
        "compact_4vendors_tooltip",
        all4(),
        Box::new(move |_, ui, _| mock(ui)),
    );
    t.hover = Some((VendorId::Grok, Kind::Session));
    scenes.push(t);

    for sc in scenes {
        let ctx = Context::default();
        ctx.set_pixels_per_point(PPP);
        let mut ui = AitrackUi::new(&ctx, sc.app.clone());
        ctx.style_mut(|s| s.interaction.tooltip_delay = 0.0);
        (sc.setup)(&ctx, &mut ui, &sc.app);
        WIDTH_BITS.store(sc.width.to_bits(), std::sync::atomic::Ordering::Relaxed);
        let mut texs = HashMap::new();
        // pass 1 on a tall screen measures the content height the window would auto-size to
        run_frames(&ctx, &mut ui, &mut texs, 900.0, None);
        let h = ui.desired_height().ceil();
        let prims = run_frames(&ctx, &mut ui, &mut texs, h, sc.hover);
        if sc.name.starts_with("badges_") {
            for (v, r) in &ui.badge_rects {
                println!("badgerect {} {} {} {}", sc.name, v.name(), r.min.x, r.min.y);
            }
        }
        let bg = if sc.light {
            aitrack::theme::LIGHT.background
        } else {
            aitrack::theme::DARK.background
        };
        save(
            &out.join(format!("{}.png", sc.name)),
            &rasterize(&prims, &texs, bg, h),
            h,
        );
        println!("wrote {}.png ({}x{} px)", sc.name, width() as u32, h as u32);
    }
}
