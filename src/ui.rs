// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: thin egui code that draws the view-model from [`crate::view`] / [`crate::state`].
//! No network or disk I/O happens here; the collector thread does all of that.

use crate::app::App;
use crate::reading::Kind;
use crate::settings::{BadgeVariant, Direction, ThemeChoice, Track};
use crate::state::State;
use crate::stats::StatsRange;
use crate::statsview;
use crate::theme::{self, BadgeStyle, Fill, Palette};
use crate::transcripts::ProjRange;
use crate::vendor::{self, Vendor, VendorId};
use crate::view::{self, Cell, ChartData, Detail, Range};
use chrono::{Local, Utc};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, Context, CornerRadius, FontId, Frame, Layout, Margin,
    Painter, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Screen {
    Home,
    Settings,
    Stats,
}

/// Opens a URL (https or mailto) with its registered handler.
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// A short message for the status bar ("Could not launch Claude desktop") with when it was set.
type LaunchMsg = Arc<Mutex<Option<(String, Instant)>>>;

/// Called on a click on the Claude badge.
pub type Launcher = Arc<dyn Fn(&Context, &LaunchMsg) + Send + Sync>;

fn real_launcher() -> Launcher {
    Arc::new(|ctx, msg| {
        let (ctx, msg) = (ctx.clone(), msg.clone());
        crate::launch::launch_claude_async(crate::launch::log_path(), move |ok| {
            if !ok {
                *msg.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(("Could not launch Claude desktop".into(), Instant::now()));
            }
            ctx.request_repaint();
        });
    })
}

fn real_grok_launcher() -> Launcher {
    Arc::new(|ctx, msg| {
        let (ctx, msg) = (ctx.clone(), msg.clone());
        crate::launch::launch_grok_async(crate::launch::log_path(), move |ok| {
            if !ok {
                *msg.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(("Could not launch Grok".into(), Instant::now()));
            }
            ctx.request_repaint();
        });
    })
}

struct ChartCache {
    version: u64,
    vendor: VendorId,
    kind: Kind,
    /// Drawn from the Grok Bot's own history.
    bot: bool,
    range: Range,
    at: Instant,
    data: ChartData,
    /// Nothing at all is stored for this chart ("No history yet").
    none_stored: bool,
}

pub struct AitrackUi {
    app: Arc<App>,
    pub screen: Screen,
    /// The row whose detail pane is open (one at a time).
    pub expanded: Option<(VendorId, Kind)>,
    /// The Grok Bot weekly row's detail pane is open (it shares the Grok block with the Grok
    /// rows, so it is tracked apart from `expanded`; at most one of the two is set).
    pub bot_expanded: bool,
    /// Where the Grok Bot row was drawn last frame (tests and the render tool click it).
    pub bot_row_rect: Option<Rect>,
    /// Draw these vendors instead of the real data (screenshots of the multi-vendor look).
    pub mock: Option<Vec<Vendor>>,
    /// Screen rect of each vendor row from the last frame (used by `examples/render` to hover).
    pub row_rects: Vec<(VendorId, Kind, Rect)>,
    /// Screen rect of each vendor badge from the last frame (for tests / the render tool).
    pub badge_rects: Vec<(VendorId, Rect)>,
    /// The Claude badge launches the desktop app (Windows, macOS and Linux by default).
    pub launch_enabled: bool,
    /// What "Open help" does (the default opens [`crate::launch::HELP_URL`]; tests inject a fake).
    pub help_opener: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// What the About links do with a URL (https or mailto); tests inject a fake.
    pub url_opener: UrlOpener,
    /// Screen rect of each About control (`repo`, `license`).
    pub about_rects: Vec<(String, Rect)>,
    /// Header logo textures by (light theme, pixel size); `None` = the image did not decode.
    logos: std::collections::HashMap<(bool, u32), Option<egui::TextureHandle>>,
    /// Screen rect of the "Open help" button on the empty state (for tests / the render tool).
    pub help_rect: Option<Rect>,
    /// Screen rect of each Auto / On / Off choice in Settings, keyed `Show Claude:Auto` ...
    pub track_rects: Vec<(String, Rect)>,
    /// Screen rect of each "Badge style" choice in the Settings view (for tests / the render tool).
    pub variant_rects: Vec<(BadgeVariant, Rect)>,
    /// Screen rect of each Theme choice in the Settings view.
    pub theme_rects: Vec<(ThemeChoice, Rect)>,
    theme_sent: Option<ThemeChoice>,
    /// What a click on the Claude badge does (the real launcher by default; tests inject a fake).
    pub launcher: Launcher,
    /// What a click on the Grok badge does (the real launcher by default; tests inject a fake).
    pub grok_launcher: Launcher,
    /// Screen rect of the Grok block's Refresh control from the last frame, when shown.
    pub refresh_rect: Option<Rect>,
    /// Screen rect of the "Refresh Grok now" button in Settings, when shown.
    pub settings_refresh_rect: Option<Rect>,
    /// Draw this refresh state for Grok instead of the real one (screenshots, tests).
    pub mock_refresh: Option<crate::app::RefreshMode>,
    launch_msg: LaunchMsg,
    desired_height: f32,
    fits: std::collections::HashMap<Screen, view::FitState>,
    /// When the last size was commanded, and for which view.
    size_sent_at: Option<Instant>,
    size_for: Option<Screen>,
    /// Screen rect of each Settings section header (for tests / the render tool).
    pub section_rects: Vec<(String, Rect)>,
    /// For each Settings row: its title, the rect of the label column and of the control.
    pub setting_rows: Vec<(String, Rect, Rect)>,
    /// Hover notes shown in the last frame of the Settings view: (row title, rect of the text).
    pub setting_tips: Vec<(String, Rect)>,
    pub range: Range,
    /// The user picked a range: the automatic default never changes it again.
    range_user: bool,
    /// Which vendor / kind the automatic default was last worked out for, and when.
    default_range_at: Option<(VendorId, Kind, bool, Instant)>,
    custom: String,
    custom_local: String,
    test_result: Option<String>,
    copied_at: Option<Instant>,
    startup_error: Option<String>,
    on_top_sent: Option<bool>,
    chart: Option<ChartCache>,
    /// Decides when the window position is saved (see `placement`).
    tracker: crate::placement::Tracker,
    /// Stats view: the selected ranges, and the readings versions the plan numbers were asked for.
    pub proj_range: ProjRange,
    pub plan_range: StatsRange,
    plan_versions: Option<(u64, u64)>,
    /// Screen rect of each range choice of the Stats view (for tests / the render tool).
    pub stats_rects: Vec<(String, Rect)>,
    /// Screen rect of the header's shrink button from the last frame (tests / the render tool).
    pub shrink_rect: Option<Rect>,
    /// The small-window state the window was last set up for (decorations, level, size limits).
    small_sent: Option<bool>,
    /// The inner height last requested for the small window.
    small_h_sent: Option<f32>,
}

fn theme_pref(t: ThemeChoice) -> egui::ThemePreference {
    match t {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
        ThemeChoice::Light => egui::ThemePreference::Light,
    }
}

fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn prop(size: f32) -> FontId {
    FontId::proportional(size)
}

impl AitrackUi {
    pub fn new(ctx: &Context, app: Arc<App>) -> Self {
        theme::install(ctx);
        let first = app.settings.get();
        ctx.set_theme(theme_pref(first.theme));
        let app_window = first.window;
        let c = ctx.clone();
        app.set_waker(move || c.request_repaint());
        Self {
            app,
            screen: Screen::Home,
            expanded: None,
            bot_expanded: false,
            bot_row_rect: None,
            mock: None,
            row_rects: vec![],
            badge_rects: vec![],
            launch_enabled: cfg!(any(windows, target_os = "macos", target_os = "linux")),
            variant_rects: vec![],
            theme_rects: vec![],
            theme_sent: Some(first.theme),
            help_opener: Arc::new(|| {
                crate::launch::open_help(
                    &crate::launch::SystemSpawner,
                    Some(&crate::launch::log_path()),
                )
            }),
            url_opener: Arc::new(|url| {
                crate::launch::open_url(
                    &crate::launch::SystemSpawner,
                    "about",
                    url,
                    Some(&crate::launch::log_path()),
                )
            }),
            about_rects: vec![],
            help_rect: None,
            logos: Default::default(),
            track_rects: vec![],
            launcher: real_launcher(),
            grok_launcher: real_grok_launcher(),
            refresh_rect: None,
            settings_refresh_rect: None,
            mock_refresh: None,
            launch_msg: Arc::default(),
            desired_height: 0.0,
            fits: Default::default(),
            size_sent_at: None,
            size_for: None,
            section_rects: vec![],
            setting_rows: vec![],
            setting_tips: vec![],
            range: Range::Day,
            range_user: false,
            default_range_at: None,
            custom: String::new(),
            custom_local: String::new(),
            test_result: None,
            copied_at: None,
            startup_error: None,
            on_top_sent: None,
            chart: None,
            tracker: crate::placement::Tracker::with_saved(app_window.as_ref()),
            proj_range: ProjRange::Days7,
            plan_range: StatsRange::Last30d,
            plan_versions: None,
            stats_rects: vec![],
            shrink_rect: None,
            // the window was built for the saved mode (see `run`)
            small_sent: Some(first.small),
            small_h_sent: None,
        }
    }

    /// The small Help link ([`crate::launch::HELP_URL`]).
    fn help_link(&mut self, ui: &mut Ui, pal: &Palette) {
        let help = ui
            .add(
                egui::Label::new(
                    RichText::new("Help")
                        .font(prop(11.5))
                        .underline()
                        .color(pal.blue),
                )
                .sense(Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(crate::launch::HELP_URL);
        self.help_rect = Some(help.rect);
        if help.clicked() {
            if let Err(e) = (self.help_opener)() {
                crate::alerts::log_line(&format!("could not open the help page: {e}"));
                self.set_launch_notice("Could not open the help page");
            }
        }
    }

    /// "aiTrack v0.1.0 · build 214 (8acc7ac) · 2026-10-14 10:55" (click to copy).
    fn build_line(&mut self, ui: &mut Ui, pal: &Palette) {
        let line = crate::buildinfo::line();
        let copied = self
            .copied_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
        let text = if copied {
            "Copied".to_owned()
        } else {
            line.clone()
        };
        let resp = ui
            .add(
                egui::Label::new(RichText::new(text).font(mono(9.0)).color(pal.muted))
                    .wrap()
                    .sense(Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Click to copy");
        if resp.clicked() {
            ui.ctx().copy_text(line);
            self.copied_at = Some(Instant::now());
        }
    }

    /// Pick a history range (as a click on its button does); the automatic default then stays out
    /// of the way.
    pub fn set_range(&mut self, r: Range) {
        self.range = r;
        self.range_user = true;
    }

    /// The app this window shows (tests and the render tool reach its settings through it).
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// The message currently shown in the status bar about launching Claude (`None` after a few seconds).
    pub fn launch_notice(&self) -> Option<String> {
        self.launch_msg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(6))
            .map(|(m, _)| m)
    }

    /// Show `text` in the status bar for a few seconds (used by the launcher and the render tool).
    pub fn set_launch_notice(&self, text: &str) {
        *self.launch_msg.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((text.into(), Instant::now()));
    }

    /// Remember where the window is: saved only after a move or resize has settled, never while
    /// minimised (see `placement`).
    fn track_window(&mut self, ctx: &Context) {
        let info = ctx.input(|i| i.viewport().clone());
        let sample = crate::placement::sample_for(
            crate::placement::mode(),
            info.outer_rect.map(|o| (o.min.x as f64, o.min.y as f64)),
            info.inner_rect.map(|i| i.width() as f64),
            info.native_pixels_per_point.unwrap_or(1.0) as f64,
            info.minimized.unwrap_or(false),
        );
        if let Some(s) = self.tracker.observe(Instant::now(), sample) {
            self.app
                .settings
                .set_window(Some(self.keep_full_width(s.to_pos())));
        }
        if self.tracker.pending() {
            ctx.request_repaint_after(crate::placement::SETTLE + Duration::from_millis(150));
        }
    }

    /// Save the window place once more on exit (if it changed since the last save).
    pub fn flush_window(&mut self) {
        if let Some(s) = self.tracker.flush() {
            self.app
                .settings
                .set_window(Some(self.keep_full_width(s.to_pos())));
        }
    }

    /// The small window shares the full window's place but not its width: while small, the saved
    /// width stays the full window's.
    fn keep_full_width(&self, mut pos: crate::settings::WindowPos) -> crate::settings::WindowPos {
        let v = self.app.settings.get();
        if v.small {
            pos.width = v.window.and_then(|w| w.width);
        }
        pos
    }

    /// "Reset window position": forget the saved place and move the window to the default spot.
    fn reset_window(&mut self, ctx: &Context) {
        self.app.settings.set_window(None);
        let (x, y) = crate::placement::RESET_POS;
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
        // the move is not a new place to remember until the user moves the window themselves
        let width = ctx.screen_rect().width() as f64;
        self.tracker.mark_saved(Some(crate::placement::Sample {
            x: x as f64,
            y: y as f64,
            width,
            scale: ctx
                .input(|i| i.viewport().native_pixels_per_point)
                .unwrap_or(1.0) as f64,
            minimized: false,
        }));
    }

    /// Set the window up for small or full mode when the mode changes: no title bar and a
    /// smaller minimum while small; back to the saved width (the height re-fits) when full.
    fn apply_small(&mut self, ctx: &Context, small: bool) {
        if self.small_sent == Some(small) {
            return;
        }
        self.small_sent = Some(small);
        self.small_h_sent = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!small));
        if small {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(vec2(
                view::SMALL_WINDOW_W,
                40.0,
            )));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(vec2(
                MIN_WINDOW_W,
                view::MIN_WINDOW_H,
            )));
            let w = self
                .app
                .settings
                .get()
                .window
                .and_then(|w| w.width)
                .map_or(view::default_inner_size_pt().0, |w| w as f32);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(
                w,
                view::default_inner_size_pt().1,
            )));
            // the home view fits itself again
            self.fits.remove(&Screen::Home);
            self.size_for = None;
        }
        ctx.request_repaint();
    }

    /// The small window: only the vendor blocks, a hair of border, no title bar. Dragging any part
    /// moves the window, a single click does nothing, a double-click goes back to the full window.
    fn draw_small(
        &mut self,
        ctx: &Context,
        pal: &Palette,
        st: &State,
        home: &view::HomeView,
        vendors: &[Vendor],
    ) {
        let mut content_h = 0.0;
        egui::CentralPanel::default()
            .frame(
                // the window is transparent (see `run`): only the rounded blocks are drawn, so no
                // square corners show around them
                Frame::new()
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(Margin::same(SMALL_BORDER as i8)),
            )
            .show(ctx, |ui| {
                // under everything else, so the rows (hover only) keep their tooltips
                let bg = ui.interact(
                    ui.max_rect(),
                    ui.id().with("small-window"),
                    Sense::click_and_drag(),
                );
                if bg.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if bg.double_clicked() {
                    self.app.settings.set_small(false);
                }
                let top = ui.cursor().top();
                if home.empty.is_some() {
                    ui.label(
                        RichText::new("No AI tools found (double-click for details)")
                            .font(prop(11.5))
                            .color(pal.muted),
                    );
                } else {
                    self.vendor_stack(ui, pal, st, home, vendors, true);
                }
                content_h = ui.cursor().top() - top;
            });
        let h = (content_h + 2.0 * SMALL_BORDER).ceil();
        if self.small_h_sent.is_none_or(|s| (s - h).abs() > 1.0) {
            self.small_h_sent = Some(h);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(
                view::SMALL_WINDOW_W,
                h,
            )));
        }
        let soon = st.collector.next_in.is_some_and(|n| n < 90.0);
        ctx.request_repaint_after(Duration::from_secs(if soon { 1 } else { 5 }));
    }

    /// Draw one frame.
    pub fn draw(&mut self, ctx: &Context) {
        self.track_window(ctx);
        self.help_rect = None;
        self.settings_refresh_rect = None;
        let st = self.app.state();
        let pal = *Palette::of(ctx.theme());
        let now = Utc::now();
        // Every tooltip (egui's plain hover text included) wraps inside the window: the default
        // width is 600 pt, far wider than this 310 pt window, and the text ran off the edge.
        let tip_w = {
            let st = ctx.style();
            // (the width egui wraps at is the text's; the frame around it adds its margins and outline)
            let frame = st.spacing.menu_margin.sum().x + 2.0 * st.visuals.window_stroke.width;
            tooltip_width(ctx.screen_rect().width(), frame)
        };
        if (ctx.style().spacing.tooltip_width - tip_w).abs() > 0.5 {
            ctx.style_mut(|s| s.spacing.tooltip_width = tip_w);
        }
        // the colour theme applies live (System keeps following the operating system)
        if self.theme_sent != Some(st.settings.theme) {
            self.theme_sent = Some(st.settings.theme);
            ctx.set_theme(theme_pref(st.settings.theme));
        }
        let small = st.settings.small;
        self.apply_small(ctx, small);
        let on_top = st.settings.on_top || small;
        if self.on_top_sent != Some(on_top) {
            self.on_top_sent = Some(on_top);
            let level = if on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        }
        let vendors = match &self.mock {
            Some(m) => m.clone(),
            None => vendor::from_state(&st),
        };
        let mut home = view::build_home_for(&st, &vendors, now);
        // a failed launch of the Claude app is told in the status bar for a few seconds
        if let Some((m, at)) = self
            .launch_msg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            if at.elapsed() < Duration::from_secs(6) {
                home.status = view::StatusLine {
                    dot: view::Dot::Err,
                    text: m,
                    full: None,
                };
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }

        if small {
            self.draw_small(ctx, &pal, &st, &home, &vendors);
            return;
        }

        let status_h = egui::TopBottomPanel::bottom("status")
            .frame(
                Frame::new()
                    .fill(pal.deep)
                    .stroke(Stroke::new(1.0_f32, pal.line))
                    .inner_margin(Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| status_bar(ui, &pal, &home.status))
            .response
            .rect
            .height();

        // Settings and Stats: "Back" stays pinned above the footer (the content scrolls)
        let build_h = 0.0;
        let mut nav_h = 0.0;
        if self.screen != Screen::Home {
            nav_h = egui::TopBottomPanel::bottom("nav")
                .frame(
                    Frame::new()
                        .fill(pal.background)
                        .inner_margin(Margin::symmetric(12, 4)),
                )
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        if pill(ui, &pal, "Back", false).clicked() {
                            self.screen = Screen::Home;
                        }
                        if self.screen == Screen::Settings {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                self.help_link(ui, &pal);
                            });
                        }
                    });
                    // which build is this? (click to copy)
                    if self.screen == Screen::Settings {
                        self.build_line(ui, &pal);
                    }
                })
                .response
                .rect
                .height();
        }

        let panels_top = ctx.available_rect().bottom();
        let mut content_bottom = 0.0;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(pal.background).inner_margin(Margin {
                left: 10,
                right: 10,
                top: 8,
                bottom: 8,
            }))
            .show(ctx, |ui| {
                self.header(ui, &pal, &st);
                ui.add_space(4.0);
                content_bottom = match self.screen {
                    Screen::Home => self.home(ui, &pal, &st, &home, &vendors),
                    Screen::Settings => self.settings(ui, &pal, &st),
                    Screen::Stats => self.stats_view(ui, &pal, now),
                };
            });

        // Fit the window to its content, up to a cap (see `view::height_cap`); beyond it the content
        // scrolls. Only re-sent when the fit changes, and never for a view whose height the user
        // changed by hand (a size that differs from the one we commanded, once it has settled).
        let monitor_h = ctx.input(|i| i.viewport().monitor_size).map(|m| m.y);
        let cap = view::height_cap(self.screen != Screen::Home, monitor_h);
        // measured from the layout (empty strip above the pinned panels), not from summed heights:
        // equal to content + panels on Windows, and immune to anything extra a platform adds
        let _ = (status_h, build_h, nav_h);
        self.desired_height = view::fit_height(
            ctx.screen_rect().height(),
            panels_top,
            content_bottom,
            8.0,
            view::MIN_WINDOW_H,
            cap,
        );
        let actual_h = ctx.screen_rect().height();
        if self.size_for != Some(self.screen) {
            self.size_for = Some(self.screen);
            self.size_sent_at = None;
            if let Some(f) = self.fits.get_mut(&self.screen) {
                // back on a view: re-fit once (unless the user sized it)
                f.sent = None;
                f.age_ms = None;
            }
        }
        let mut fit = self.fits.remove(&self.screen).unwrap_or_default();
        fit.age_ms = self.size_sent_at.map(|t| t.elapsed().as_millis());
        let action = view::fit_decide(&fit, self.desired_height, actual_h);
        fit.apply(action, actual_h);
        match action {
            view::FitAction::Request(h) => {
                self.size_sent_at = Some(Instant::now());
                let w = ctx.screen_rect().width();
                // safe diagnostics: sizes only
                crate::alerts::log_line(&format!(
                    "window fit: requested inner height {h:.0} pt (window {actual_h:.0} x {:.0} pt, {:.2} px/pt, monitor {:?}, retry {})",
                    ctx.screen_rect().width(),
                    ctx.pixels_per_point(),
                    monitor_h.map(|m| m.round()),
                    fit.retries
                ));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(w, h)));
                ctx.request_repaint();
            }
            view::FitAction::UserSized => crate::alerts::log_line(&format!(
                "window fit: height {actual_h:.0} pt differs from the request; treated as a hand resize"
            )),
            view::FitAction::GiveUp => crate::alerts::log_line(&format!(
                "window fit: the OS ignored the size request (still {actual_h:.0} pt); giving up"
            )),
            view::FitAction::Keep => {}
        }
        self.fits.insert(self.screen, fit);

        if st.collector.polling {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else if self
            .copied_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(1600))
        {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else {
            let soon = st.collector.next_in.is_some_and(|n| n < 90.0);
            ctx.request_repaint_after(Duration::from_secs(if soon { 1 } else { 5 }));
        }
    }

    // ------------------------------------------------------------ header
    /// The logo at the top left: the gauge ("dial") from `assets/icon`, white for dark themes and
    /// navy for light ones, in the embedded size closest to the screen scale. If the image cannot
    /// be decoded the old drawn logo is used. Same footprint either way.
    fn header_logo(&mut self, ui: &mut Ui, pal: &Palette) {
        const LOGO: f32 = 24.0;
        let (r, _) = ui.allocate_exact_size(vec2(LOGO, LOGO), Sense::hover());
        let ppp = ui.ctx().pixels_per_point();
        let (size, bytes) = crate::appicon::dial_for(!pal.dark, LOGO * ppp);
        let key = (!pal.dark, size);
        let ctx = ui.ctx().clone();
        self.logos.entry(key).or_insert_with(|| {
            crate::appicon::decode(bytes).map(|img| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [img.width as usize, img.height as usize],
                    &img.pixels,
                );
                ctx.load_texture(
                    format!("logo-{size}-{}", key.0),
                    image,
                    egui::TextureOptions::LINEAR,
                )
            })
        });
        match self.logos.get(&key).and_then(|t| t.as_ref()) {
            Some(tex) => {
                ui.painter().image(
                    tex.id(),
                    r,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => paint_fallback_logo(ui.painter(), pal, r),
        }
    }

    fn header(&mut self, ui: &mut Ui, pal: &Palette, st: &State) {
        ui.horizontal(|ui| {
            self.header_logo(ui, pal);
            ui.label(
                RichText::new("aiTrack")
                    .font(prop(15.0))
                    .strong()
                    .color(pal.foreground),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let gear = icon_button(
                    ui,
                    pal,
                    Icon::Sliders,
                    self.screen == Screen::Settings,
                    false,
                );
                if gear.on_hover_text("Settings").clicked() {
                    self.screen = if self.screen == Screen::Home {
                        Screen::Settings
                    } else {
                        Screen::Home
                    };
                }
                let bars = icon_button(ui, pal, Icon::Bars, self.screen == Screen::Stats, false);
                if bars.on_hover_text("Stats").clicked() {
                    self.screen = if self.screen == Screen::Stats {
                        Screen::Home
                    } else {
                        Screen::Stats
                    };
                }
                let spin = st.collector.polling;
                if icon_button(ui, pal, Icon::Refresh, false, spin)
                    .on_hover_text("Refresh now")
                    .clicked()
                {
                    self.app.refresh();
                }
                let shrink = icon_button(ui, pal, Icon::Shrink, false, false);
                self.shrink_rect = Some(shrink.rect);
                if shrink
                    .on_hover_text("Small window (double-click it to come back)")
                    .clicked()
                {
                    self.screen = Screen::Home;
                    self.app.settings.set_small(true);
                }
            });
        });
    }

    // ------------------------------------------------------------ home

    /// Content height the window needs (used for auto-sizing and by `examples/render`).
    pub fn desired_height(&self) -> f32 {
        self.desired_height
    }

    /// Returns the bottom of the content (for auto-sizing).
    fn home(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        st: &State,
        home: &view::HomeView,
        vendors: &[Vendor],
    ) -> f32 {
        if let Some(e) = &home.empty {
            card(ui, pal, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(&e.title)
                            .font(prop(14.0))
                            .strong()
                            .color(pal.foreground),
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(RichText::new(&e.body).font(mono(11.5)).color(pal.muted))
                            .wrap(),
                    );
                    ui.add_space(10.0);
                    self.help_rect = None;
                    if e.help {
                        let resp = pill(ui, pal, "Open help", false);
                        self.help_rect = Some(resp.rect);
                        if resp.clicked() {
                            if let Err(e) = (self.help_opener)() {
                                crate::alerts::log_line(&format!(
                                    "could not open the help page: {e}"
                                ));
                                self.set_launch_notice("Could not open the help page");
                            }
                        }
                        ui.add_space(10.0);
                    }
                });
            });
            return ui.cursor().top();
        }
        if self
            .expanded
            .is_some_and(|(id, _)| !home.vendors.iter().any(|v| v.id == id))
        {
            self.expanded = None;
        }
        if self.bot_expanded
            && !home
                .vendors
                .iter()
                .any(|v| v.rows.iter().any(|r| r.secondary))
        {
            self.bot_expanded = false;
        }
        let top = ui.cursor().top();
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.vendor_stack(ui, pal, st, home, vendors, false)
            });
        top + out.content_size.y
    }

    /// Vendor blocks stacked flush: shared 1px dividers, rounded outer corners only.
    fn vendor_stack(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        st: &State,
        home: &view::HomeView,
        vendors: &[Vendor],
        small: bool,
    ) {
        let remaining = st.settings.remaining();
        let n = home.vendors.len();
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut stack: Option<Rect> = None;
        self.row_rects.clear();
        self.bot_row_rect = None;
        self.badge_rects.clear();
        self.refresh_rect = None;
        for (i, vv) in home.vendors.iter().enumerate() {
            let (first, last) = (i == 0, i + 1 == n);
            let r = STACK_RADIUS;
            let cr = CornerRadius {
                nw: if first { r } else { 0 },
                ne: if first { r } else { 0 },
                sw: if last { r } else { 0 },
                se: if last { r } else { 0 },
            };
            let accent = pal.vendor(vv.id);
            let value_w = if remaining { 60.0 } else { 38.0 };
            let block = Frame::new()
                .fill(pal.panel)
                .corner_radius(cr)
                .inner_margin(Margin {
                    left: 14,
                    right: 10,
                    top: 6,
                    bottom: 6,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // in the small window a click does nothing (the window is dragged by any part)
                    let launchable = !small
                        && self.launch_enabled
                        && matches!(vv.id, VendorId::Claude | VendorId::Grok);
                    let variant = st.settings.badge_style;
                    if vendor_header(ui, pal, vv, launchable, variant, &mut self.badge_rects) {
                        let launcher = if vv.id == VendorId::Grok {
                            self.grok_launcher.clone()
                        } else {
                            self.launcher.clone()
                        };
                        launcher(ui.ctx(), &self.launch_msg);
                    }
                    // a longer label ("Grok Bot") pushes every bar of the block right, so they line up
                    let strip_x = if vv.rows.iter().any(|r| r.label.len() > 7) {
                        60.0
                    } else {
                        50.0
                    };
                    for row in &vv.rows {
                        if small {
                            let resp = small_row_widget(ui, pal, row, vv.id, value_w, strip_x);
                            resp.on_hover_ui(|ui| tooltip(ui, pal, row));
                            continue;
                        }
                        let open = if row.secondary {
                            self.bot_expanded
                        } else {
                            self.expanded == Some((vv.id, row.kind))
                        };
                        let resp = row_widget(ui, pal, row, vv.id, open, value_w, strip_x);
                        if row.secondary {
                            self.bot_row_rect = Some(resp.rect);
                            if resp.clicked() {
                                self.bot_expanded = !open;
                                self.expanded = None;
                            }
                        } else {
                            self.row_rects.push((vv.id, row.kind, resp.rect));
                            if resp.clicked() {
                                self.expanded = if open { None } else { Some((vv.id, row.kind)) };
                                self.bot_expanded = false;
                            }
                        }
                        resp.on_hover_ui(|ui| tooltip(ui, pal, row));
                    }
                    if small {
                        return;
                    }
                    if vv.id == VendorId::Grok {
                        self.grok_refresh_line(ui, pal, st);
                    }
                    if let Some((b, v)) = vendors
                        .iter()
                        .find(|v| v.id == vv.id)
                        .and_then(|v| v.bot.as_ref().map(|b| (b, v)))
                        .filter(|_| self.bot_expanded)
                    {
                        let d = view::build_bot_detail_with(
                            b,
                            v.bot_stats.as_ref(),
                            v.bot_gauge.as_ref(),
                            remaining,
                            Utc::now(),
                        );
                        self.detail(ui, pal, &d);
                    } else if let Some((id, kind)) = self.expanded.filter(|(id, _)| *id == vv.id) {
                        if let Some(v) = vendors.iter().find(|v| v.id == id) {
                            let d = view::build_detail(v, kind, remaining);
                            self.detail(ui, pal, &d);
                        }
                    }
                });
            let rect = block.response.rect;
            // The stripe is the block's own rounded shape clipped to its left edge, so it follows
            // the outer corners.
            let stripe_clip = Rect::from_min_size(rect.min, vec2(STRIPE_W, rect.height()))
                .intersect(ui.clip_rect());
            ui.painter()
                .with_clip_rect(stripe_clip)
                .rect_filled(rect, cr, accent);
            let p = ui.painter();
            if !first {
                p.line_segment(
                    [rect.left_top(), rect.right_top()],
                    Stroke::new(1.0_f32, pal.line),
                );
            }
            stack = Some(stack.map_or(rect, |s| s.union(rect)));
        }
        if let Some(rect) = stack {
            ui.painter().rect_stroke(
                rect,
                STACK_RADIUS as f32,
                Stroke::new(1.0_f32, pal.line),
                StrokeKind::Inside,
            );
        }
    }

    /// The small line under Grok's rows: "Refresh" when its data is missing or old (starts the Grok
    /// CLI in the background so it fetches usage), "Refreshing..." while that runs, or why it failed.
    fn grok_refresh_line(&mut self, ui: &mut Ui, pal: &Palette, st: &State) {
        use crate::app::RefreshMode;
        let mode = self
            .mock_refresh
            .clone()
            .unwrap_or_else(|| self.app.grok_refresh_mode(st, Utc::now()));
        if mode == RefreshMode::Hidden {
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click());
        let cy = rect.center().y;
        let font = prop(10.5);
        let p = ui.painter();
        match &mode {
            RefreshMode::Offer => {
                let hovered = resp.hovered();
                let col = if hovered { pal.foreground } else { pal.muted };
                p.text(
                    Pos2::new(rect.right() - 2.0, cy),
                    Align2::RIGHT_CENTER,
                    "Refresh",
                    font.clone(),
                    col,
                );
                let w = p.layout_no_wrap("Refresh".into(), font, col).size().x;
                refresh_icon(p, Pos2::new(rect.right() - w - 12.0, cy), col);
                self.refresh_rect = Some(rect);
                let resp = resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(
                        "Starts the Grok CLI in the background for a moment so it fetches your usage, then closes it. Nothing is sent to Grok and no credits are used.",
                    );
                if resp.clicked() {
                    let ctx = ui.ctx().clone();
                    self.app.refresh_grok(move || ctx.request_repaint());
                }
            }
            RefreshMode::Running => {
                p.text(
                    Pos2::new(rect.right() - 2.0, cy),
                    Align2::RIGHT_CENTER,
                    "Refreshing\u{2026}",
                    font,
                    pal.muted,
                );
                self.refresh_rect = Some(rect);
                ui.ctx().request_repaint_after(Duration::from_millis(400));
            }
            RefreshMode::Failed(m) => {
                let mut job =
                    egui::text::LayoutJob::simple(m.clone(), font, pal.red, rect.width() - 4.0);
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                job.wrap.overflow_character = Some('\u{2026}');
                let g = p.ctx().fonts(|f| f.layout_job(job));
                p.galley(
                    Pos2::new(rect.right() - 2.0 - g.size().x, cy - g.size().y / 2.0),
                    g,
                    pal.red,
                );
                self.refresh_rect = Some(rect);
                resp.on_hover_text(m);
            }
            RefreshMode::Hidden => {}
        }
    }

    fn detail(&mut self, ui: &mut Ui, pal: &Palette, d: &Detail) {
        let accent = pal.level(d.vendor, d.level);
        ui.add_space(6.0);
        divider(ui, pal);
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&d.title)
                    .font(prop(12.5))
                    .strong()
                    .color(pal.foreground),
            );
            ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                if !d.unit.is_empty() {
                    ui.label(RichText::new(d.unit).font(mono(12.0)).color(accent));
                }
                ui.label(
                    RichText::new(&d.value)
                        .font(mono(24.0))
                        .strong()
                        .color(accent),
                );
            });
        });
        let mut caption = d.resets.clone().unwrap_or_default();
        if let Some(n) = &d.note {
            if !caption.is_empty() {
                caption.push_str(" \u{b7} ");
            }
            caption.push_str(&n.text);
        }
        if !caption.is_empty() {
            let col = if d.note.as_ref().is_some_and(|n| n.warn) {
                pal.red
            } else {
                pal.muted
            };
            ui.add(egui::Label::new(RichText::new(caption).font(mono(11.0)).color(col)).wrap());
        }
        if let Some(kind) = d.chart {
            ui.add_space(2.0);
            self.history(ui, pal, d.vendor, kind, d.bot, pal.vendor(d.vendor));
        }
        ui.add_space(2.0);
        for pair in d.rows.chunks(2) {
            ui.columns(2, |cols| {
                for (col, (k, v)) in cols.iter_mut().zip(pair) {
                    col.horizontal(|ui| {
                        ui.label(RichText::new(k).font(prop(11.5)).color(pal.muted));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(v).font(mono(11.5)).color(pal.foreground));
                        });
                    });
                }
            });
        }
    }

    fn history(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        vendor: VendorId,
        kind: Kind,
        bot: bool,
        color: Color32,
    ) {
        // until the user picks a range, open with the shortest one that shows all the data
        if !self.range_user
            && self.default_range_at.is_none_or(|(v, k, b, at)| {
                v != vendor || k != kind || b != bot || at.elapsed() > Duration::from_secs(30)
            })
        {
            let now = Utc::now();
            let r = self
                .app
                .chart_readings(vendor, bot, |rs| view::default_range(rs, kind, now));
            if let Some(r) = r {
                self.range = r;
                self.default_range_at = Some((vendor, kind, bot, Instant::now()));
            }
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("History").font(prop(12.0)).color(pal.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for r in Range::ALL.iter().rev() {
                    let sel = *r == self.range;
                    let text = RichText::new(r.label()).font(mono(11.0)).color(if sel {
                        pal.foreground
                    } else {
                        pal.muted
                    });
                    let b = egui::Button::new(text)
                        .min_size(vec2(0.0, 18.0))
                        .fill(if sel { pal.selected } else { pal.button })
                        .stroke(Stroke::new(
                            1.0_f32,
                            if sel { color } else { pal.button_border },
                        ));
                    if ui.add(b).clicked() {
                        self.set_range(*r);
                    }
                }
            });
        });
        let version = self.app.chart_version(vendor, bot);
        let stale = self.chart.as_ref().is_none_or(|c| {
            c.version != version
                || c.vendor != vendor
                || c.kind != kind
                || c.bot != bot
                || c.range != self.range
                || c.at.elapsed() > Duration::from_secs(30)
        });
        if stale {
            let (range, now) = (self.range, Utc::now());
            let made = self.app.chart_readings(vendor, bot, |r| {
                (view::chart_data(r, kind, range, now), r.is_empty())
            });
            if let Some((data, none_stored)) = made {
                self.chart = Some(ChartCache {
                    version,
                    vendor,
                    kind,
                    bot,
                    range,
                    at: Instant::now(),
                    data,
                    none_stored,
                });
            }
        }
        let data = self.chart.as_ref().map(|c| &c.data);
        let empty = if bot && self.chart.as_ref().is_some_and(|c| c.none_stored) {
            "No history yet"
        } else {
            "No readings in this range"
        };
        history_chart(ui, pal, data, color, self.range, empty);
    }

    // ------------------------------------------------------------ settings

    fn settings(&mut self, ui: &mut Ui, pal: &Palette, st: &State) -> f32 {
        let v = st.settings;
        let top = ui.cursor().top();
        let mut variant_rects: Vec<(BadgeVariant, Rect)> = Vec::new();
        let mut track_rects: Vec<(String, Rect)> = Vec::new();
        let mut theme_rects: Vec<(ThemeChoice, Rect)> = Vec::new();
        let mut sec_rects: Vec<(String, Rect)> = Vec::new();
        let mut about_rects: Vec<(String, Rect)> = Vec::new();
        ROW_LOG.with(|l| l.borrow_mut().clear());
        TIP_LOG.with(|l| l.borrow_mut().clear());
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let toggled_general = section_card(
                    ui,
                    pal,
                    "General",
                    "general",
                    v.sections.general,
                    &mut sec_rects,
                    |ui| {
                    ui.label(
                        RichText::new("Update frequency")
                            .font(prop(13.0))
                            .color(pal.foreground),
                    );
                    ui.horizontal(|ui| {
                        for p in view::presets(v.interval_min) {
                            if pill(ui, pal, p.label, p.selected).clicked() {
                                self.app.set_interval(p.minutes);
                            }
                        }
                    });
                    if view::fast_warning(v.interval_min) {
                        ui.label(
                            RichText::new(
                                "Polling every minute may hit rate limits. 15 is recommended.",
                            )
                            .font(prop(11.5))
                            .color(pal.orange),
                        );
                    }
                    divider(ui, pal);
                    setting_row(
                        ui,
                        pal,
                        "Custom (minutes)",
                        "Online check: Claude usage service and Grok billing",
                        |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut self.custom)
                                    .desired_width(56.0)
                                    .font(mono(12.0)),
                            );
                            if r.lost_focus() {
                                match self.custom.trim().parse::<f64>() {
                                    Ok(n) if n > 0.0 && n.is_finite() => self.app.set_interval(n),
                                    _ => {}
                                }
                            }
                            if !r.has_focus() {
                                self.custom = fmt_minutes(self.app.settings.get().interval_min);
                            }
                        },
                    );
                    divider(ui, pal);
                    setting_row(
                        ui,
                        pal,
                        "Local refresh (seconds)",
                        "Re-reads the Claude app's files",
                        |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut self.custom_local)
                                    .desired_width(56.0)
                                    .font(mono(12.0)),
                            );
                            if r.lost_focus() {
                                if let Ok(n) = self.custom_local.trim().parse::<f64>() {
                                    if n.is_finite() && n > 0.0 {
                                        self.app.set_local_interval(n);
                                    }
                                }
                            }
                            if !r.has_focus() {
                                self.custom_local =
                                    fmt_minutes(self.app.settings.get().local_interval_sec);
                            }
                        },
                    );
                    divider(ui, pal);
                    let note = self
                        .startup_error
                        .clone()
                        .unwrap_or_else(|| "Launch when you sign in".into());
                    setting_row(ui, pal, start_at_login_label(), &note, |ui| {
                        if toggle(ui, pal, v.startup).clicked() {
                            match self.app.set_startup(!v.startup) {
                                Ok(()) => self.startup_error = None,
                                Err(e) => self.startup_error = Some(e),
                            }
                        }
                    });
                    },
                );
                if let Some(o) = toggled_general {
                    self.app.settings.set_section("general", o);
                }
                let toggled_appearance = section_card(
                    ui,
                    pal,
                    "Appearance",
                    "appearance",
                    v.sections.appearance,
                    &mut sec_rects,
                    |ui| {
                    setting_row(ui, pal, "Theme", "Window colours", |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            for t in ThemeChoice::ALL.into_iter().rev() {
                                let r = pill(ui, pal, t.label(), v.theme == t);
                                theme_rects.push((t, r.rect));
                                if r.clicked() {
                                    self.app.settings.set_theme(t);
                                }
                            }
                        });
                    });
                    divider(ui, pal);
                    setting_row(ui, pal, "Logos", "", |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            for b in BadgeVariant::ALL.into_iter().rev() {
                                let mut r = pill(ui, pal, b.label(), v.badge_style == b);
                                if b == BadgeVariant::Auto {
                                    r = r.on_hover_text(
                                        "Dark logos on a dark window, light logos on a light one",
                                    );
                                }
                                variant_rects.push((b, r.rect));
                                if r.clicked() {
                                    self.app.set_badge_style(b);
                                }
                            }
                        });
                    });
                    },
                );
                if let Some(o) = toggled_appearance {
                    self.app.settings.set_section("appearance", o);
                }
                let toggled_display = section_card(
                    ui,
                    pal,
                    "Display",
                    "display",
                    v.sections.display,
                    &mut sec_rects,
                    |ui| {
                    setting_row(ui, pal, "Show remaining", "Count down what is left", |ui| {
                        if toggle(ui, pal, v.remaining()).clicked() {
                            self.app.set_direction(if v.remaining() {
                                Direction::Used
                            } else {
                                Direction::Remaining
                            });
                        }
                    });
                    divider(ui, pal);
                    setting_row(
                        ui,
                        pal,
                        "Window position",
                        "Reopens where you left it",
                        |ui| {
                            if pill(ui, pal, "Reset", false).clicked() {
                                self.reset_window(ui.ctx());
                            }
                        },
                    );
                    divider(ui, pal);
                    setting_row(
                        ui,
                        pal,
                        "Always on top",
                        if crate::placement::on_top_unavailable() {
                            "Not available on native Wayland (see README)"
                        } else {
                            "Keep the window above others"
                        },
                        |ui| {
                            if toggle(ui, pal, v.on_top).clicked() {
                                self.app.set_on_top(!v.on_top);
                            }
                        },
                    );
                    for (name, found, track, is_claude) in [
                        (
                            "Show Claude",
                            self.app.claude_detected(),
                            Track::from_opt(v.claude_enabled),
                            true,
                        ),
                        (
                            "Show Grok",
                            self.app.grok_detected(),
                            Track::from_opt(v.grok_enabled),
                            false,
                        ),
                    ] {
                        if !is_claude && self.app.grok.is_none() {
                            continue;
                        }
                        divider(ui, pal);
                        let subtitle = if found {
                            format!("Found on this {}", crate::util::this_machine())
                        } else {
                            "Not found".to_string()
                        };
                        setting_row(ui, pal, name, &subtitle, |ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                for t in Track::ALL.into_iter().rev() {
                                    let r = pill(ui, pal, t.label(), track == t);
                                    track_rects.push((format!("{}:{}", name, t.label()), r.rect));
                                    if r.clicked() {
                                        if is_claude {
                                            self.app.set_claude_track(t);
                                        } else {
                                            self.app.set_grok_track(t);
                                        }
                                    }
                                }
                            });
                        });
                    }
                    if self.app.grok.is_some() && self.app.grok_cli_present() {
                        divider(ui, pal);
                        let running = self.app.grok_refreshing();
                        // the outcome stays here until the next click (the window is covered)
                        let offset = chrono::Local::now().offset().local_minus_utc();
                        let subtitle = self
                            .app
                            .grok_refresh_status()
                            .text(offset)
                            .unwrap_or_else(|| "Starts the Grok CLI briefly to fetch usage".into());
                        let subtitle = subtitle.as_str();
                        setting_row(ui, pal, "Refresh Grok now", subtitle, |ui| {
                            let r = pill(ui, pal, if running { "Refreshing" } else { "Refresh" }, false);
                            self.settings_refresh_rect = Some(r.rect);
                            if r.clicked() && !running {
                                let ctx = ui.ctx().clone();
                                self.app.refresh_grok(move || ctx.request_repaint());
                            }
                        });
                    }
                    },
                );
                if let Some(o) = toggled_display {
                    self.app.settings.set_section("display", o);
                }
                let toggled_notifications = section_card(
                    ui,
                    pal,
                    "Notifications",
                    "notifications",
                    v.sections.notifications,
                    &mut sec_rects,
                    |ui| {
                    setting_row(
                        ui,
                        pal,
                        "Usage alerts",
                        "Toasts as limits get close",
                        |ui| {
                            if toggle(ui, pal, v.notifications).clicked() {
                                self.app.set_notifications(!v.notifications);
                            }
                        },
                    );
                    ui.add_space(4.0);
                    setting_row_tip(ui, pal, "Alert sounds", "A reminder sound for warnings and limits", alert_sounds_tip(), |ui| {
                        let resp = toggle(ui, pal, v.alert_sounds);
                        if resp.clicked() {
                            self.app.settings.set_alert_sounds(!v.alert_sounds);
                        }
                        hover_note(resp, pal, "Alert sounds", alert_sounds_tip());
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Send a test toast").font(prop(11.5)).color(pal.muted));
                    ui.horizontal(|ui| {
                        for (label, kind) in [
                            ("Info", crate::alerts::AlertKind::Info),
                            ("Warning", crate::alerts::AlertKind::Warning),
                            ("Limit", crate::alerts::AlertKind::Limit),
                        ] {
                            if pill(ui, pal, label, false).clicked() {
                                self.test_result = Some(match self.app.send_test_notification(kind) {
                                    Ok(()) => format!(
                                        "Sent the {} style. If nothing appeared, see notify.log in ~/.aitrack",
                                        label.to_lowercase()
                                    ),
                                    Err(e) => format!("Could not send: {e}"),
                                });
                            }
                        }
                    });
                    if let Some(r) = &self.test_result {
                        ui.add(
                            egui::Label::new(RichText::new(r).font(prop(11.5)).color(pal.muted))
                                .wrap(),
                        );
                    }
                    },
                );
                if let Some(o) = toggled_notifications {
                    self.app.settings.set_section("notifications", o);
                }
                let toggled_diagnostics = section_card(
                    ui,
                    pal,
                    "Diagnostics",
                    "diagnostics",
                    v.sections.diagnostics,
                    &mut sec_rects,
                    |ui| {
                    let copied = self
                        .copied_at
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
                    if pill(ui, pal, if copied { "Copied" } else { "Copy log" }, false).clicked() {
                        ui.ctx().copy_text(view::copy_log(&st.diagnostics));
                        self.copied_at = Some(Instant::now());
                    }
                    ui.add_space(4.0);
                    if st.diagnostics.is_empty() {
                        ui.label(
                            RichText::new("No polls recorded yet.")
                                .font(mono(11.5))
                                .color(pal.muted),
                        );
                    }
                    egui::ScrollArea::vertical()
                        .max_height(DIAG_ROWS_SHOWN as f32 * 17.0)
                        .id_salt("diag")
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for d in st.diagnostics.iter().take(DIAG_ROWS_SHOWN) {
                                diag_row(ui, pal, d);
                            }
                        });
                    },
                );
                if let Some(o) = toggled_diagnostics {
                    self.app.settings.set_section("diagnostics", o);
                }
                let toggled_about = section_card(
                    ui,
                    pal,
                    "About",
                    "about",
                    v.sections.about,
                    &mut sec_rects,
                    |ui| self.about_body(ui, pal, &mut about_rects),
                );
                if let Some(o) = toggled_about {
                    self.app.settings.set_section("about", o);
                }
            });
        self.section_rects = sec_rects;
        self.about_rects = about_rects;
        self.setting_rows = ROW_LOG.with(|l| l.borrow().clone());
        self.setting_tips = TIP_LOG.with(|l| l.borrow().clone());
        self.variant_rects = variant_rects;
        self.track_rects = track_rects;
        self.theme_rects = theme_rects;
        top + out.content_size.y
    }
}

impl AitrackUi {
    /// Replaces every opener and launcher with one that does nothing and succeeds, so a click in
    /// a test can never start a browser or an app. Every test that builds a UI calls this first.
    pub fn sandbox(&mut self) {
        self.help_opener = Arc::new(|| Ok(()));
        self.url_opener = Arc::new(|_| Ok(()));
        self.launcher = Arc::new(|_, _| {});
        self.grok_launcher = Arc::new(|_, _| {});
    }

    // ------------------------------------------------------------ about

    /// Open `url` through the injected opener; a failure shows `failure` in the status bar.
    fn open_about_link(&self, url: &str, failure: &str) {
        if let Err(e) = (self.url_opener)(url) {
            crate::alerts::log_line(&format!("could not open {url}: {e}"));
            self.set_launch_notice(failure);
        }
    }

    /// Settings > About: project link, developer and contact, licence.
    fn about_body(&mut self, ui: &mut Ui, pal: &Palette, rects: &mut Vec<(String, Rect)>) {
        // (the build line is at the bottom of Settings; here the name links to the project)
        let mut open_repo = false;
        setting_row_ui(
            ui,
            pal,
            "aiTrack",
            |ui| {
                let r = link_label(ui, pal, "github.com/genen/aiTrack", crate::launch::REPO_URL);
                rects.push(("repo".into(), r.rect));
                open_repo = r.clicked();
            },
            "",
            |_| {},
        );
        if open_repo {
            self.open_about_link(crate::launch::REPO_URL, "Could not open the project page");
        }
        ui.add_space(4.0);
        setting_row(
            ui,
            pal,
            "Developed by Sir Gene",
            crate::launch::CONTACT_EMAIL,
            |_| {},
        );
        // Linux: which display backend the window runs on (empty elsewhere)
        let backend = crate::placement::backend_report();
        if !backend.is_empty() {
            ui.add_space(4.0);
            setting_row(ui, pal, "Display", &backend, |_| {});
        }
        ui.add_space(4.0);
        let mut open_license = false;
        setting_row_ui(
            ui,
            pal,
            "License",
            |ui| {
                let r = link_label(ui, pal, "GPL-3.0-or-later", crate::launch::LICENSE_URL);
                rects.push(("license".into(), r.rect));
                open_license = r.clicked();
            },
            "",
            |_| {},
        );
        if open_license {
            self.open_about_link(
                crate::launch::LICENSE_URL,
                "Could not open the licence page",
            );
        }
    }

    // ------------------------------------------------------------ stats

    /// The Stats view: Today, Projects (token / cost usage from local transcripts) and Plan
    /// (the statistics of `stats.rs` over the readings). Everything is computed by the stats
    /// service's thread; this only draws the latest snapshot.
    fn stats_view(&mut self, ui: &mut Ui, pal: &Palette, now: chrono::DateTime<Utc>) -> f32 {
        let top = ui.cursor().top();
        let service = self.app.stats().cloned();
        let snap = service.as_ref().map(|s| s.snapshot()).unwrap_or_default();
        // ask for fresh plan numbers when the readings changed since the last request
        let versions = (
            self.app.history.version(),
            self.app.grok.as_ref().map_or(0, |g| g.history.version()),
        );
        if self.plan_versions != Some(versions) {
            self.plan_versions = Some(versions);
            if let Some(s) = &service {
                s.request_plan();
            }
        }
        let page = statsview::build_page(&snap, self.proj_range, self.plan_range, now);
        let variant = self.app.settings.get().badge_style;
        let mut rects: Vec<(String, Rect)> = Vec::new();
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                stats_today(ui, pal, &page);
                ui.add_space(8.0);
                card(ui, pal, |ui| {
                    ui.horizontal(|ui| {
                        section_title(ui, pal, "Projects");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            for r in ProjRange::SHOWN.into_iter().rev() {
                                let resp = pill(ui, pal, r.label(), self.proj_range == r);
                                rects.push((format!("proj:{}", r.label()), resp.rect));
                                if resp.clicked() {
                                    self.proj_range = r;
                                }
                            }
                        });
                    });
                    stats_projects(ui, pal, &page, variant, &mut rects);
                });
                ui.add_space(8.0);
                card(ui, pal, |ui| {
                    ui.horizontal(|ui| {
                        section_title(ui, pal, "Plan");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            for r in StatsRange::ALL.into_iter().rev() {
                                let resp = pill(ui, pal, r.label(), self.plan_range == r);
                                rects.push((format!("plan:{}", r.label()), resp.rect));
                                if resp.clicked() {
                                    self.plan_range = r;
                                }
                            }
                        });
                    });
                    stats_plan(ui, pal, &page);
                });
            });
        self.stats_rects = rects;
        if page.updating {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        top + out.content_size.y
    }
}

/// Cut `text` with an ellipsis so it fits `max_w` in `font`.
fn ellipsize(p: &Painter, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |t: &str| {
        p.layout_no_wrap(t.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut n = chars.len();
    while n > 1 {
        n -= 1;
        let t: String = chars[..n].iter().collect::<String>() + "\u{2026}";
        if width(&t) <= max_w {
            return t;
        }
    }
    "\u{2026}".into()
}

fn stats_today(ui: &mut Ui, pal: &Palette, page: &statsview::StatsPage) {
    card(ui, pal, |ui| {
        ui.horizontal(|ui| {
            section_title(ui, pal, "Today");
            if page.updating {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("updating\u{2026}")
                            .font(prop(11.0))
                            .color(pal.muted),
                    );
                });
            }
        });
        match &page.today {
            Some(t) => {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&t.cost)
                            .font(prop(22.0))
                            .strong()
                            .color(pal.foreground),
                    );
                    ui.label(
                        RichText::new(format!("{} tokens", t.tokens))
                            .font(prop(12.0))
                            .color(pal.muted),
                    );
                });
            }
            None => {
                ui.label(
                    RichText::new("Scanning local transcripts\u{2026}")
                        .font(prop(12.0))
                        .color(pal.muted),
                );
            }
        }
    });
}

/// Side of the vendor logo at the start of a project row.
const PROJECT_LOGO: f32 = 13.0;

fn stats_projects(
    ui: &mut Ui,
    pal: &Palette,
    page: &statsview::StatsPage,
    variant: BadgeVariant,
    rects: &mut Vec<(String, Rect)>,
) {
    let block = &page.projects;
    if let Some(e) = &block.empty {
        ui.label(
            RichText::new(&e.message)
                .font(prop(13.0))
                .color(pal.foreground),
        );
        ui.add_space(2.0);
        ui.label(RichText::new("Checked:").font(prop(11.0)).color(pal.muted));
        for f in &e.folders {
            // long paths have no break points: let them wrap anywhere
            let mut job = egui::text::LayoutJob::simple(
                f.clone(),
                prop(10.5),
                pal.muted,
                ui.available_width(),
            );
            job.wrap.break_anywhere = true;
            ui.label(job);
        }
        return;
    }
    if page.today.is_none() {
        ui.label(
            RichText::new("Scanning\u{2026}")
                .font(prop(12.0))
                .color(pal.muted),
        );
        return;
    }
    let w = ui.available_width();
    for r in &block.rows {
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::hover());
        let p = ui.painter();
        let (rank_w, cost_w, tok_w, gap) = (16.0, 56.0, 50.0, 5.0);
        let c = pal.vendor(r.vendor);
        let row_y = rect.top() + 9.0;
        p.text(
            Pos2::new(rect.left(), row_y),
            Align2::LEFT_CENTER,
            r.rank.to_string(),
            prop(10.5),
            pal.muted,
        );
        // the vendor's logo (same badge as the headers) leads the row; the name gets what is left
        let logo = Rect::from_center_size(
            Pos2::new(rect.left() + rank_w + PROJECT_LOGO / 2.0, row_y),
            vec2(PROJECT_LOGO, PROJECT_LOGO),
        );
        paint_vendor_badge(p, logo, r.vendor, variant, pal);
        rects.push((format!("logo:{}:{}", r.rank, r.vendor.name()), logo));
        let name_x = rect.left() + rank_w + PROJECT_LOGO + gap;
        let name_w = (rect.right() - tok_w - cost_w - 8.0 - name_x).max(0.0);
        let name = ellipsize(p, &r.name, &prop(12.5), name_w);
        let name_rect = p.text(
            Pos2::new(name_x, row_y),
            Align2::LEFT_CENTER,
            name,
            prop(12.5),
            pal.foreground,
        );
        rects.push((format!("name:{}", r.rank), name_rect));
        p.text(
            Pos2::new(rect.right() - cost_w, rect.top() + 9.0),
            Align2::RIGHT_CENTER,
            &r.tokens,
            mono(11.0),
            pal.muted,
        );
        p.text(
            Pos2::new(rect.right(), rect.top() + 9.0),
            Align2::RIGHT_CENTER,
            &r.cost,
            mono(12.0),
            pal.foreground,
        );
        // the thin bar
        let bar = Rect::from_min_size(
            Pos2::new(rect.left() + rank_w, rect.bottom() - 6.0),
            vec2(w - rank_w, 3.0),
        );
        p.rect_filled(bar, 1.5, pal.graph);
        let fill = Rect::from_min_size(bar.min, vec2((bar.width() * r.frac).max(2.0), 3.0));
        p.rect_filled(fill, 1.5, c);
        resp.on_hover_text(&r.detail);
    }
    if let Some(m) = &block.more {
        ui.label(RichText::new(m).font(prop(11.0)).color(pal.muted));
    }
    if let Some(t) = &block.total {
        ui.add_space(2.0);
        ui.label(RichText::new(t).font(prop(11.5)).color(pal.muted));
    }
}

fn stats_plan(ui: &mut Ui, pal: &Palette, page: &statsview::StatsPage) {
    if let Some(n) = &page.plan_note {
        ui.label(RichText::new(n).font(prop(12.0)).color(pal.muted));
    }
    for (i, b) in page.plan.iter().enumerate() {
        if i > 0 {
            ui.add_space(4.0);
            divider(ui, pal);
            ui.add_space(4.0);
        }
        ui.label(
            RichText::new(&b.title)
                .font(prop(12.5))
                .strong()
                .color(pal.vendor(b.vendor)),
        );
        for (k, v) in &b.rows {
            ui.horizontal(|ui| {
                ui.label(RichText::new(k).font(prop(11.5)).color(pal.muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(v).font(prop(11.5)).color(pal.foreground))
                            .wrap(),
                    );
                });
            });
        }
    }
}

/// Run the window until it is closed. Blocks the calling (GUI) thread.
pub fn run(app: Arc<App>) -> eframe::Result {
    let on_top = app.settings.get().on_top;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(crate::instance::WINDOW_TITLE)
        .with_inner_size(vec2(
            crate::view::default_inner_size_pt().0,
            crate::view::default_inner_size_pt().1,
        ))
        .with_min_inner_size([MIN_WINDOW_W, crate::view::MIN_WINDOW_H])
        .with_icon(app_icon());
    if cfg!(target_os = "linux") {
        // GNOME matches the window to aitrack.desktop (StartupWMClass=aitrack) by this id
        viewport = viewport.with_app_id("aitrack");
    }
    // reopen where the window was left, when that place is still on a screen
    let saved = app.settings.get().window;
    #[cfg(target_os = "linux")]
    let backend = prepare_display();
    #[cfg(not(target_os = "linux"))]
    {
        let monitors = crate::placement::monitors();
        let outcome = saved
            .as_ref()
            .map(|w| crate::placement::try_place(w, &monitors, cfg!(target_os = "macos")));
        if let Some(o) = &outcome {
            crate::alerts::log_line(&crate::placement::describe(saved.as_ref(), o));
        }
        if let Some(Ok(((x, y), width))) = outcome {
            viewport = viewport
                .with_position([x, y])
                .with_inner_size([width, crate::view::default_inner_size_pt().1]);
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(w) = saved.as_ref() {
        let r = crate::placement::linux_place(w, crate::placement::mode());
        crate::alerts::log_line(&crate::placement::describe_linux(w, &r));
        if let Ok(p) = r {
            if let Some((x, y)) = p.pos {
                viewport = viewport.with_position([x, y]);
            }
            viewport = viewport.with_inner_size([p.width, crate::view::default_inner_size_pt().1]);
        }
    }
    let small = app.settings.get().small;
    if small {
        // the small window: no title bar, floating, sized to its content on the first frame
        viewport = viewport
            .with_decorations(false)
            .with_min_inner_size([crate::view::SMALL_WINDOW_W, 40.0])
            .with_inner_size([crate::view::SMALL_WINDOW_W, crate::view::SMALL_WINDOW_H]);
    }
    if on_top || small {
        viewport = viewport.with_always_on_top();
    }
    // A window is transparent from its creation or never, and the full window can turn small at any
    // time: so the window is always transparent. The full window's
    // panels paint every pixel; the small window paints only its rounded blocks.
    viewport = viewport.with_transparent(true);
    let demo = std::env::var_os("AITRACK_DEMO").is_some_and(|v| v == "1");
    if demo {
        crate::alerts::log_line("window: AITRACK_DEMO=1, showing mock vendors");
    }
    #[allow(unused_mut)]
    let mut opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    if backend == crate::placement::BackendChoice::XWayland {
        use winit::platform::x11::EventLoopBuilderExtX11;
        opts.event_loop_builder = Some(Box::new(|b| {
            b.with_x11();
        }));
    }
    eframe::run_native(
        crate::instance::WINDOW_TITLE,
        opts,
        Box::new(move |cc| {
            #[cfg(target_os = "linux")]
            report_actual_backend(cc, backend);
            let mut ui = AitrackUi::new(&cc.egui_ctx, app);
            if demo {
                // MOCK vendors (AITRACK_DEMO=1), to preview the look: never real data
                ui.mock = Some(crate::vendor::demo_vendors(Utc::now()));
            }
            Ok(Box::new(Window { ui }))
        }),
    )
}

#[cfg(target_os = "linux")]
static LINUX_BACKEND: std::sync::OnceLock<crate::placement::BackendChoice> =
    std::sync::OnceLock::new();

/// Linux: decide X11 (XWayland) or native Wayland, once, and export a found `DISPLAY`. Call it
/// from `main` before any thread starts (it sets an environment variable); `run` calls it too, so
/// the first call wins. Falls back to native Wayland when no X display answers.
#[cfg(target_os = "linux")]
pub fn prepare_display() -> crate::placement::BackendChoice {
    use crate::placement::{backend_line, current_decision, set_backend_report, PosMode};
    *LINUX_BACKEND.get_or_init(|| {
        let dec = current_decision();
        if let Some(d) = &dec.display {
            // Invariant: `set_var` is only sound while no other thread can read the environment.
            // `main` calls this before the settings, the app or any worker exists, so the process
            // is still single-threaded here; keep it first, and do not start threads above it.
            debug_assert!(
                std::fs::read_dir("/proc/self/task").map_or(true, |d| d.count() == 1),
                "prepare_display must run before any thread starts"
            );
            std::env::set_var("DISPLAY", d);
        }
        let line = backend_line(dec.choice, None);
        crate::alerts::log_line(&format!("window: {line} [{}]", dec.why));
        set_backend_report(line);
        crate::placement::set_mode(PosMode::of(dec.choice));
        dec.choice
    })
}

/// Which backend the window really got, from its window handle.
#[cfg(target_os = "linux")]
fn actual_backend(
    handle: Result<winit::raw_window_handle::RawWindowHandle, ()>,
) -> crate::placement::ActualBackend {
    use crate::placement::ActualBackend;
    use winit::raw_window_handle::RawWindowHandle as H;
    match handle {
        Ok(H::Xlib(_)) | Ok(H::Xcb(_)) => ActualBackend::X11,
        Ok(H::Wayland(_)) => ActualBackend::Wayland,
        _ => ActualBackend::Unknown,
    }
}

/// After the window exists: log and show what it runs on, warn when that is not what was chosen.
#[cfg(target_os = "linux")]
fn report_actual_backend(
    cc: &eframe::CreationContext<'_>,
    chosen: crate::placement::BackendChoice,
) {
    use crate::placement::{backend_line, backend_mismatch, set_backend_report};
    use winit::raw_window_handle::HasWindowHandle;
    let actual = actual_backend(cc.window_handle().map(|h| h.as_raw()).map_err(|_| ()));
    let line = backend_line(chosen, Some(actual));
    crate::alerts::log_line(&format!("window: {line}"));
    if let Some(w) = backend_mismatch(chosen, actual) {
        crate::alerts::log_line(&w);
        eprintln!("aitrack: {w}");
    }
    set_backend_report(line);
}

struct Window {
    ui: AitrackUi,
}

impl eframe::App for Window {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.ui.draw(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // see the transparent viewport in `run`
        [0.0; 4]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.ui.flush_window();
        self.ui.app.stop();
    }
}

/// The original drawn logo (a green rounded square with a chart line): used when the icon image
/// cannot be decoded.
fn paint_fallback_logo(p: &Painter, pal: &Palette, r: Rect) {
    let r = Rect::from_center_size(r.center(), vec2(20.0, 20.0));
    p.rect_filled(r, 6.0, pal.green);
    let q = |x: f32, y: f32| r.min + vec2(x, y) * (20.0 / 24.0);
    p.add(Shape::line(
        vec![q(4.0, 17.0), q(9.0, 11.0), q(13.0, 15.0), q(20.0, 6.0)],
        Stroke::new(
            2.0_f32,
            if pal.dark {
                pal.background
            } else {
                Color32::WHITE
            },
        ),
    ));
}

/// The window / taskbar icon: the rounded-square app icon, or a small drawn one if it cannot be
/// decoded.
fn app_icon() -> egui::IconData {
    if let Some(img) = crate::appicon::decode(crate::appicon::WINDOW_256) {
        return egui::IconData {
            rgba: img.pixels,
            width: img.width,
            height: img.height,
        };
    }
    drawn_app_icon()
}

fn drawn_app_icon() -> egui::IconData {
    let n = 32usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            // rounded square
            let inside = dx.abs() < 14.0
                && dy.abs() < 14.0
                && (dx.abs() - 8.0).max(0.0).hypot((dy.abs() - 8.0).max(0.0)) < 6.0;
            // three LED bars
            let bar = (0..3).any(|i| {
                let top = 9.0 + i as f32 * 5.0;
                (y as f32) >= top
                    && (y as f32) < top + 3.0
                    && (x as f32) >= 8.0
                    && (x as f32) < 24.0 - i as f32 * 4.0
            });
            let c = if !inside {
                [0, 0, 0, 0]
            } else if bar {
                [0x6B, 0xEB, 0x5C, 255]
            } else {
                [0x25, 0x25, 0x27, 255]
            };
            rgba[(y * n + x) * 4..][..4].copy_from_slice(&c);
        }
    }
    egui::IconData {
        rgba,
        width: n as u32,
        height: n as u32,
    }
}

// ---------------------------------------------------------------- widgets

/// One diagnostics line: time, a drawn check/cross (no font dependency), text.
fn diag_row(ui: &mut Ui, pal: &Palette, d: &crate::state::Diag) {
    let col = if d.ok { pal.green } else { pal.red };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(view::local_time(&d.ts))
                .font(mono(11.0))
                .color(pal.muted),
        );
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 12.0), Sense::hover());
        let c = r.center();
        let st = Stroke::new(1.6_f32, col);
        let p = ui.painter();
        if d.ok {
            p.add(Shape::line(
                vec![
                    c + vec2(-4.0, 0.5),
                    c + vec2(-1.0, 3.5),
                    c + vec2(4.0, -3.5),
                ],
                st,
            ));
        } else {
            p.line_segment([c + vec2(-3.5, -3.5), c + vec2(3.5, 3.5)], st);
            p.line_segment([c + vec2(-3.5, 3.5), c + vec2(3.5, -3.5)], st);
        }
    });
    // the message gets its own full-width lines and wraps anywhere, so a long path or message can
    // never push the card wider than the window
    let mut job = egui::text::LayoutJob::simple(
        d.text.clone(),
        mono(11.0),
        if d.ok { pal.foreground } else { pal.red },
        ui.available_width(),
    );
    job.wrap.break_anywhere = true;
    ui.add(egui::Label::new(job));
}

fn fmt_minutes(m: f64) -> String {
    if m.fract() == 0.0 {
        format!("{}", m as i64)
    } else {
        format!("{m}")
    }
}

fn card<R>(ui: &mut Ui, pal: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(pal.panel)
        .stroke(Stroke::new(1.0_f32, pal.line))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section_title(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(prop(11.0))
            .strong()
            .color(pal.muted),
    );
    ui.add_space(2.0);
}

fn divider(ui: &mut Ui, pal: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, pal.line);
}

thread_local! {
    /// Title, label-column rect and control rect of the settings rows drawn this frame.
    static ROW_LOG: std::cell::RefCell<Vec<(String, Rect, Rect)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The tooltips shown this frame: (row title, the rect the text took).
    static TIP_LOG: std::cell::RefCell<Vec<(String, Rect)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One settings row: the control at the right edge, laid out first so it keeps its room, and the
/// title and description in the space to its left. A long description wraps onto further lines
/// (the row grows) instead of running under the control.
fn setting_row(ui: &mut Ui, pal: &Palette, title: &str, sub: &str, right: impl FnOnce(&mut Ui)) {
    setting_row_tip(ui, pal, title, sub, "", right);
}

/// Hover text of the Alert sounds row (Settings > Notifications) on Windows.
pub const ALERT_SOUNDS_TIP: &str = "Windows must also allow notification sounds: Settings > System > Notifications > Allow notifications to play sounds.";

/// The same on macOS (osascript banners are listed under "Script Editor").
pub const ALERT_SOUNDS_TIP_MAC: &str = "macOS must also allow notification sounds: System Settings > Notifications > Script Editor > Play sound for notifications.";

/// The Alert sounds hover text for `mac` or Windows.
pub fn alert_sounds_tip_for(mac: bool) -> &'static str {
    if mac {
        ALERT_SOUNDS_TIP_MAC
    } else {
        ALERT_SOUNDS_TIP
    }
}

/// The same on Linux (GNOME plays its own sound; there is no per-app sound).
pub const ALERT_SOUNDS_TIP_LINUX: &str = "GNOME plays its own notification sound; there is no per-app sound. Turn it off under Settings > Sound > System Sounds or Do Not Disturb.";

/// The Alert sounds hover text for this platform.
pub fn alert_sounds_tip() -> &'static str {
    if cfg!(target_os = "linux") {
        ALERT_SOUNDS_TIP_LINUX
    } else {
        alert_sounds_tip_for(cfg!(target_os = "macos"))
    }
}

/// Label of the launch-at-sign-in row for `mac` or Windows.
pub fn start_at_login_label_for(mac: bool) -> &'static str {
    if mac {
        "Start at login"
    } else {
        "Start with Windows"
    }
}

/// Label of the launch-at-sign-in row on this platform.
pub fn start_at_login_label() -> &'static str {
    if cfg!(target_os = "linux") {
        "Start at login"
    } else {
        start_at_login_label_for(cfg!(target_os = "macos"))
    }
}

/// Widest a settings tooltip gets: it wraps inside the 310 pt window instead of running off it.
const SETTING_TIP_WIDTH: f32 = 250.0;

/// A wrapped hover note on `resp`. The space it took is logged under `title` (for the layout tests).
fn hover_note(resp: egui::Response, pal: &Palette, title: &str, text: &str) -> egui::Response {
    resp.on_hover_ui(|ui| {
        ui.set_max_width(SETTING_TIP_WIDTH.min(ui.ctx().style().spacing.tooltip_width));
        ui.add(egui::Label::new(RichText::new(text).font(prop(12.0)).color(pal.foreground)).wrap());
        TIP_LOG.with(|l| l.borrow_mut().push((title.to_owned(), ui.min_rect())));
    })
}

/// [`setting_row`] with a hover note on the title and description (`tip` empty = none).
fn setting_row_tip(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    sub: &str,
    tip: &str,
    right: impl FnOnce(&mut Ui),
) {
    setting_row_ui(
        ui,
        pal,
        title,
        |ui| {
            if !sub.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(sub).font(prop(11.5)).color(pal.muted)).wrap(),
                );
            }
        },
        tip,
        right,
    );
}

/// A settings row whose line under the title is whatever `below` draws (a link, for example).
fn setting_row_ui(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    below: impl FnOnce(&mut Ui),
    tip: &str,
    right: impl FnOnce(&mut Ui),
) {
    let (mut label_rect, mut ctrl_rect) = (Rect::NOTHING, Rect::NOTHING);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ctrl_rect = ui.scope(right).response.rect;
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let labels = ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(title).font(prop(13.0)).color(pal.foreground),
                        )
                        .wrap(),
                    );
                    below(ui);
                });
                label_rect = labels.response.rect;
                if !tip.is_empty() {
                    // (a column of labels senses nothing by itself: ask for hover over its area)
                    let id = ui.id().with(("setting-tip", title));
                    let hover = ui.interact(label_rect, id, Sense::hover());
                    hover_note(hover, pal, title, tip);
                }
            });
        });
    });
    ROW_LOG.with(|l| {
        l.borrow_mut()
            .push((title.to_owned(), label_rect, ctrl_rect))
    });
}

/// A card whose header (title and a chevron) opens and closes it. Returns the new open state when
/// the header was clicked.
fn section_card(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    key: &str,
    open: bool,
    rects: &mut Vec<(String, Rect)>,
    body: impl FnOnce(&mut Ui),
) -> Option<bool> {
    let mut toggled = None;
    card(ui, pal, |ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        rects.push((key.to_owned(), rect));
        let col = if resp.hovered() {
            pal.foreground
        } else {
            pal.muted
        };
        let p = ui.painter();
        p.text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            title.to_uppercase(),
            prop(11.0),
            col,
        );
        // chevron: pointing down when open, right when closed
        let c = Pos2::new(rect.right() - 7.0, rect.center().y);
        let pts = if open {
            [
                c + vec2(-4.0, -2.0),
                c + vec2(0.0, 2.5),
                c + vec2(4.0, -2.0),
            ]
        } else {
            [
                c + vec2(-2.0, -4.0),
                c + vec2(2.5, 0.0),
                c + vec2(-2.0, 4.0),
            ]
        };
        p.add(Shape::line(pts.to_vec(), Stroke::new(1.6_f32, col)));
        if resp.clicked() {
            toggled = Some(!open);
        }
        if open {
            ui.add_space(2.0);
            body(ui);
        }
    });
    toggled
}

fn pill(ui: &mut Ui, pal: &Palette, text: &str, selected: bool) -> Response {
    let col = if selected { pal.foreground } else { pal.muted };
    let mut b = egui::Button::new(RichText::new(text).font(prop(12.0)).color(col));
    if selected {
        b = b.fill(pal.selected).stroke(Stroke::new(1.0_f32, pal.blue));
    }
    ui.add(b)
}

fn toggle(ui: &mut Ui, pal: &Palette, on: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let (fill, stroke, knob) = if on {
        (pal.selected, pal.blue, pal.blue)
    } else {
        (pal.button, pal.button_border, pal.muted)
    };
    ui.painter().rect(
        rect,
        11.0,
        fill,
        Stroke::new(1.0_f32, stroke),
        StrokeKind::Inside,
    );
    let x = if on {
        rect.right() - 11.0
    } else {
        rect.left() + 11.0
    };
    ui.painter()
        .circle_filled(Pos2::new(x, rect.center().y), 7.0, knob);
    resp
}

#[derive(Clone, Copy)]
enum Icon {
    Refresh,
    Sliders,
    Bars,
    Shrink,
}

fn icon_button(ui: &mut Ui, pal: &Palette, icon: Icon, active: bool, spin: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    let fill = if active {
        pal.selected
    } else if resp.hovered() {
        pal.hover
    } else {
        pal.button
    };
    p.rect(
        rect,
        6.0,
        fill,
        Stroke::new(1.0_f32, if active { pal.blue } else { pal.button_border }),
        StrokeKind::Inside,
    );
    let col = if resp.hovered() || active {
        pal.foreground
    } else {
        pal.muted
    };
    let st = Stroke::new(1.6_f32, col);
    let c = rect.center();
    match icon {
        Icon::Refresh => {
            let t = if spin {
                ui.input(|i| i.time) as f32 * 6.0
            } else {
                0.0
            };
            let r = 6.0;
            let a0 = 0.5 + t;
            let sweep = 4.6;
            let pt = |a: f32| c + vec2(a.cos(), a.sin()) * r;
            let pts: Vec<Pos2> = (0..=24).map(|i| pt(a0 + sweep * i as f32 / 24.0)).collect();
            p.add(Shape::line(pts, st));
            let end = a0 + sweep;
            let e = pt(end);
            let tan = vec2(-end.sin(), end.cos());
            let rad = vec2(end.cos(), end.sin());
            p.add(Shape::line(
                vec![e - tan * 4.5 + rad * 3.0, e, e - tan * 1.0 - rad * 4.5],
                st,
            ));
        }
        Icon::Bars => {
            // three bars of different height on a baseline
            let o = c - vec2(7.5, 7.5);
            let q = |x: f32, y: f32| o + vec2(x, y) * (15.0 / 24.0);
            for (x, top) in [(5.0, 13.0), (11.0, 5.0), (17.0, 9.0)] {
                p.line_segment([q(x, 20.0), q(x, top)], Stroke::new(2.4_f32, col));
            }
        }
        Icon::Sliders => {
            let o = c - vec2(7.5, 7.5);
            let q = |x: f32, y: f32| o + vec2(x, y) * (15.0 / 24.0);
            for (a, b) in [
                ((4.0, 7.0), (14.0, 7.0)),
                ((18.0, 7.0), (20.0, 7.0)),
                ((4.0, 17.0), (6.0, 17.0)),
                ((10.0, 17.0), (20.0, 17.0)),
            ] {
                p.line_segment([q(a.0, a.1), q(b.0, b.1)], st);
            }
            p.circle_stroke(q(16.0, 7.0), 1.6, st);
            p.circle_stroke(q(8.0, 17.0), 1.6, st);
        }
        Icon::Shrink => {
            // two arrows pointing in from opposite corners, each with an L-shaped head
            let o = c - vec2(7.5, 7.5);
            let q = |x: f32, y: f32| o + vec2(x, y) * (15.0 / 24.0);
            p.line_segment([q(20.0, 4.0), q(14.0, 10.0)], st);
            p.add(Shape::line(
                vec![q(14.0, 5.0), q(14.0, 10.0), q(19.0, 10.0)],
                st,
            ));
            p.line_segment([q(4.0, 20.0), q(10.0, 14.0)], st);
            p.add(Shape::line(
                vec![q(5.0, 14.0), q(10.0, 14.0), q(10.0, 19.0)],
                st,
            ));
        }
    }
    resp
}

/// Placeholder strip for "no current data": dim dashes, so it can never be read as a 0 % reading.
pub fn led_dashes(p: &Painter, rect: Rect, color: Color32) {
    let n = view::SEGMENTS as f32;
    let gap = 2.0;
    let w = (rect.width() - gap * (n - 1.0)) / n;
    for i in 0..view::SEGMENTS {
        let x = rect.left() + i as f32 * (w + gap);
        let dash = Rect::from_min_size(
            Pos2::new(x + w * 0.1, rect.center().y - 0.75),
            vec2(w * 0.8, 1.5),
        );
        p.rect_filled(dash, 0.75, color.gamma_multiply(0.45));
    }
}

/// Segmented LED strip: lit segments with a faint bloom, unlit and projected ones in the same hue.
pub fn led_strip(p: &Painter, rect: Rect, cells: &[Cell], color: Color32, dark: bool) {
    let n = cells.len().max(1) as f32;
    let gap = 2.0;
    let w = (rect.width() - gap * (n - 1.0)) / n;
    let seg = |i: usize| {
        Rect::from_min_size(
            Pos2::new(rect.left() + i as f32 * (w + gap), rect.top()),
            vec2(w, rect.height()),
        )
    };
    for (i, c) in cells.iter().enumerate() {
        // the bloom is a glow on a dark window; on a light one it only smudges
        if *c == Cell::Lit && dark {
            for (grow, alpha) in theme::BLOOM {
                p.rect_filled(seg(i).expand(grow), 3.0 + grow, color.gamma_multiply(alpha));
            }
        }
    }
    for (i, c) in cells.iter().enumerate() {
        let col = match c {
            Cell::Lit => color,
            Cell::Projected => color.gamma_multiply(if dark {
                theme::PROJECTED_ALPHA
            } else {
                theme::LIGHT_PROJECTED_ALPHA
            }),
            Cell::Off => color.gamma_multiply(if dark {
                theme::UNLIT_ALPHA
            } else {
                theme::LIGHT_UNLIT_ALPHA
            }),
        };
        p.rect_filled(seg(i), 2.0, col);
    }
}

/// Diagnostics rows visible at once (the section scrolls inside beyond this).
const DIAG_ROWS_SHOWN: usize = 8;

const STACK_RADIUS: u8 = 10;
const STRIPE_W: f32 = 4.0;
/// Smallest width of the full window (points).
const MIN_WINDOW_W: f32 = 270.0;
/// Border around the vendor blocks in the small window (points; transparent, it only keeps the
/// blocks' outline from being clipped).
const SMALL_BORDER: f32 = 1.0;

const BADGE: f32 = 18.0;

/// Paints a vendor's logo inside its badge square (`ink` contrasts with the vendor colour).
/// A vendor without an entry here gets its letter.
type BadgePainter = fn(&Painter, Rect, Color32);

fn badge_painter(v: VendorId) -> Option<BadgePainter> {
    match v {
        VendorId::Claude => Some(paint_claude_mark),
        VendorId::Grok => Some(paint_grok_mark),
        _ => None,
    }
}

/// Paint a badge square: the fill (gradient or solid), the outline if any. The logo comes after.
pub fn paint_badge(p: &Painter, rect: Rect, style: &BadgeStyle, dark: bool) {
    let radius = rect.width() * style.radius;
    match style.fill {
        Fill::Solid(c) => {
            p.rect_filled(rect, radius, c);
        }
        Fill::Vertical(top, bottom) => paint_gradient(p, rect, radius, top, bottom),
    }
    let border = style
        .border
        .or_else(|| style.light_border.filter(|_| !dark));
    if let Some(b) = border {
        p.rect_stroke(rect, radius, Stroke::new(1.0_f32, b), StrokeKind::Inside);
    }
}

/// The whole badge of a vendor in `variant`: fill, outline and logo (or its letter), scaled to
/// `rect`. Used by the vendor headers and by `examples/make_toast_icons`.
pub fn paint_vendor_badge(
    p: &Painter,
    rect: Rect,
    v: VendorId,
    variant: BadgeVariant,
    pal: &Palette,
) {
    let style = theme::badge_style(v, variant, pal);
    paint_badge(p, rect, &style, pal.dark);
    match badge_painter(v) {
        Some(paint) => paint(p, rect, style.mark),
        None => {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                v.badge().to_string(),
                prop(11.0 * rect.width() / BADGE),
                style.mark,
            );
        }
    }
}

/// A rounded square with a vertical gradient. The edge stays anti-aliased: the bottom colour is
/// drawn first as an ordinary rounded rectangle and the gradient mesh sits half a pixel inside.
fn paint_gradient(p: &Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    p.rect_filled(rect, radius, bottom);
    let r = rect.shrink(0.5);
    let radius = (radius - 0.5).max(0.0);
    let mut pts: Vec<Pos2> = Vec::new();
    for (corner, a0) in [
        (Pos2::new(r.right() - radius, r.top() + radius), -90.0_f32),
        (Pos2::new(r.right() - radius, r.bottom() - radius), 0.0),
        (Pos2::new(r.left() + radius, r.bottom() - radius), 90.0),
        (Pos2::new(r.left() + radius, r.top() + radius), 180.0),
    ] {
        for i in 0..=6 {
            let a = (a0 + 90.0 * i as f32 / 6.0).to_radians();
            pts.push(corner + vec2(a.cos(), a.sin()) * radius);
        }
    }
    let at = |y: f32| {
        let t = ((y - r.top()) / r.height()).clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color32::from_rgb(
            f(top.r(), bottom.r()),
            f(top.g(), bottom.g()),
            f(top.b(), bottom.b()),
        )
    };
    let vert = |pos: Pos2| egui::epaint::Vertex {
        pos,
        uv: egui::epaint::WHITE_UV,
        color: at(pos.y),
    };
    let mut mesh = egui::Mesh::default();
    mesh.vertices.push(vert(r.center()));
    mesh.vertices.extend(pts.iter().map(|q| vert(*q)));
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.indices.extend([0, 1 + i, 1 + (i + 1) % n]);
    }
    p.add(Shape::mesh(mesh));
}

/// A stroke of varying width along the polyline `pts` (position, half-width): one convex quad per
/// segment, each reaching a little into the next so no seams show, plus round end caps. Convex
/// polygons are anti-aliased by egui, so the edges stay crisp at 18 px.
fn strip(p: &Painter, pts: &[(Pos2, f32)], ink: Color32) {
    const OVERLAP: f32 = 0.25;
    let dir_at = |i: usize| {
        let (a, b) = (
            pts[i.saturating_sub(1)].0,
            pts[(i + 1).min(pts.len() - 1)].0,
        );
        (b - a).normalized()
    };
    for i in 0..pts.len().saturating_sub(1) {
        let ((a, wa), (b, wb)) = (pts[i], pts[i + 1]);
        let (da, db) = (dir_at(i), dir_at(i + 1));
        let (na, nb) = (vec2(-da.y, da.x), vec2(-db.y, db.x));
        let (a, b) = (a - da * OVERLAP, b + db * OVERLAP);
        p.add(Shape::convex_polygon(
            vec![a + na * wa, b + nb * wb, b - nb * wb, a - na * wa],
            ink,
            Stroke::NONE,
        ));
    }
    for (c, w) in [pts[0], pts[pts.len() - 1]] {
        if w > 0.3 {
            p.circle_filled(c, w, ink);
        }
    }
}

/// [`strip`] along a straight line `from -> to` with half-width `r(t)`, t in 0..=1.
fn taper(p: &Painter, from: Pos2, to: Pos2, r: impl Fn(f32) -> f32, ink: Color32) {
    let n = (((to - from).length() / 0.8).ceil() as usize).max(1);
    let pts: Vec<(Pos2, f32)> = (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            (from + (to - from) * t, r(t))
        })
        .collect();
    strip(p, &pts, ink);
}

/// The Claude mark: a solid soft centre and twelve thick, round-ended rays, evenly spaced and a
/// touch narrower towards the centre; two are a little shorter. Vector shapes only.
fn paint_claude_mark(p: &Painter, badge: Rect, ink: Color32) {
    const SHORT: [usize; 2] = [3, 8];
    let c = badge.center();
    let size = badge.width();
    let (reach, outer_r, inner_r, core) = (size * 0.42, size * 0.043, size * 0.031, size * 0.1);
    for i in 0..12 {
        let a = (i as f32 * 30.0 + 8.0).to_radians();
        let dir = vec2(a.sin(), -a.cos());
        let len = if SHORT.contains(&i) { 0.9 } else { 1.0 };
        taper(
            p,
            c + dir * core * 0.6,
            c + dir * (reach * len - outer_r),
            |t| inner_r + (outer_r - inner_r) * t,
            ink,
        );
    }
    p.circle_filled(c, core, ink);
}

/// The Grok mark: a thick ring split into two tapered arcs (open where the slash passes, at the
/// upper right and the lower left) and one thin tapered diagonal slash from the lower left to the
/// upper right, reaching outside the ring.
fn paint_grok_mark(p: &Painter, badge: Rect, ink: Color32) {
    let c = badge.center();
    let size = badge.width();
    let (ring, w) = (size * 0.245, size * 0.11);
    // screen angles (y down, clockwise): gaps centred on -45 (upper right) and 135 (lower left)
    let gap = 11.0_f32;
    for (from, to) in [(-45.0 + gap, 135.0 - gap), (135.0 + gap, 315.0 - gap)] {
        let steps = 28;
        let pts: Vec<(Pos2, f32)> = (0..=steps)
            .map(|i| {
                let t = i as f32 / steps as f32;
                let a = (from + (to - from) * t).to_radians();
                // full thickness over most of the arc, tapering to a point at both ends
                let k = (t.min(1.0 - t) / 0.2).min(1.0).powf(0.8);
                (c + vec2(a.cos(), a.sin()) * ring, (w / 2.0 * k).max(0.05))
            })
            .collect();
        strip(p, &pts, ink);
    }
    let dir = vec2(1.0, -1.0).normalized();
    let reach = size * 0.45;
    taper(
        p,
        c - dir * reach,
        c + dir * reach,
        |t| size * 0.026 * (1.0 - (2.0 * t - 1.0).abs().powf(1.3)).max(0.0).powf(0.6) + 0.05,
        ink,
    );
}

/// Vendor badge + name, and (when a window would run out before it resets) the red warning right
/// after the name. Fixed height: the warning picks the longest text that fits, never wraps.
/// Returns true when the (clickable) badge was clicked.
fn vendor_header(
    ui: &mut Ui,
    pal: &Palette,
    v: &view::VendorView,
    clickable: bool,
    variant: BadgeVariant,
    badges: &mut Vec<(VendorId, Rect)>,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    let p = ui.painter();
    let badge = Rect::from_center_size(
        Pos2::new(rect.left() + BADGE / 2.0, rect.center().y),
        vec2(BADGE, BADGE),
    );
    paint_vendor_badge(p, badge, v.id, variant, pal);
    badges.push((v.id, badge));
    let mut clicked = false;
    if clickable {
        // the Claude logo opens the Claude desktop app
        let r = ui
            .interact(badge, ui.id().with(("badge", v.id)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if v.id == VendorId::Grok {
                "Open Grok"
            } else {
                "Open Claude desktop"
            });
        clicked = r.clicked();
    }
    let p = ui.painter();
    let name = p.layout_no_wrap(v.name.clone(), prop(13.0), pal.foreground);
    let name_right = badge.right() + 7.0 + name.size().x;
    p.galley(
        Pos2::new(badge.right() + 7.0, rect.center().y - name.size().y / 2.0),
        name,
        pal.foreground,
    );
    let chip = v.age_chip.as_ref();
    if v.warn.is_none() && chip.is_none() {
        return clicked;
    }
    let p = ui.painter().clone();
    let font = prop(10.5);
    let tw = |t: &str| {
        p.layout_no_wrap(t.to_owned(), font.clone(), pal.orange)
            .size()
            .x
    };
    // room after the name for the warning (icon + text) and the chip (icon + age + info icon)
    let x0 = name_right + 10.0;
    let total = rect.right() - x0;
    const GAP: f32 = 8.0;
    let chip_w = |t: Option<&str>| 11.0 + t.map_or(0.0, |t| 4.0 + tw(t)) + 5.0 + 12.0;
    let warn_w = |t: &str| 14.0 + tw(t);
    // (warning text, chip text); the warning falls back to its compact form before the chip loses
    // its long wording, and never wraps or overflows
    #[derive(Clone, Copy)]
    enum W {
        Full,
        Compact,
        Cut,
    }
    #[derive(Clone, Copy)]
    enum C {
        Long,
        Compact,
        Min,
    }
    let wtext = |w: W| match (&v.warn, w) {
        (Some(x), W::Full) => Some(x.full.as_str()),
        (Some(x), _) => Some(x.compact.as_str()),
        (None, _) => None,
    };
    let ctext = |c: C| match (chip, c) {
        (Some(x), C::Long) => Some(x.long.as_str()),
        (Some(x), C::Compact) => Some(x.compact.as_str()),
        _ => None,
    };
    let candidates: &[(W, C)] = &[
        (W::Full, C::Long),
        (W::Full, C::Compact),
        (W::Compact, C::Long),
        (W::Compact, C::Compact),
        (W::Compact, C::Min),
        (W::Cut, C::Min),
    ];
    let mut choice = (W::Cut, C::Min);
    for &(wf, cf) in candidates {
        let used = wtext(wf).map_or(0.0, |t| warn_w(t) + if chip.is_some() { GAP } else { 0.0 })
            + if chip.is_some() {
                chip_w(ctext(cf))
            } else {
                0.0
            };
        if used <= total {
            choice = (wf, cf);
            break;
        }
    }
    let chip_width = if chip.is_some() {
        chip_w(ctext(choice.1))
    } else {
        0.0
    };
    let cy = rect.center().y;
    if let Some(w) = &v.warn {
        let text = wtext(choice.0).unwrap_or(&w.compact).to_owned();
        let room = (total - chip_width - if chip.is_some() { GAP } else { 0.0 } - 14.0).max(10.0);
        let mut job = egui::text::LayoutJob::simple(text, font.clone(), pal.red, room);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('\u{2026}');
        let g = p.ctx().fonts(|f| f.layout_job(job));
        warn_icon(&p, pal, Pos2::new(x0 + 5.0, cy));
        p.galley(Pos2::new(x0 + 14.0, cy - g.size().y / 2.0), g, pal.red);
        resp.on_hover_text(&w.full);
    }
    if let Some(c) = chip {
        let left = rect.right() - chip_width;
        let chip_rect =
            Rect::from_min_size(Pos2::new(left, rect.top()), vec2(chip_width, rect.height()));
        tri_icon(&p, pal, Pos2::new(left + 5.5, cy), pal.orange);
        let mut x = left + 11.0;
        if let Some(t) = ctext(choice.1) {
            let g = p.layout_no_wrap(t.to_owned(), font.clone(), pal.orange);
            x += 4.0;
            p.galley(Pos2::new(x, cy - g.size().y / 2.0), g.clone(), pal.orange);
            x += g.size().x;
        }
        info_icon(&p, pal.orange, Pos2::new(x + 5.0 + 6.0, cy));
        ui.interact(chip_rect, ui.id().with(("age-chip", v.id)), Sense::hover())
            .on_hover_text(&c.tooltip);
    }
    clicked
}

/// Two arrows chasing each other (a refresh symbol), drawn from shapes.
fn refresh_icon(p: &Painter, c: Pos2, col: Color32) {
    let r = 4.0_f32;
    let stroke = Stroke::new(1.2_f32, col);
    let pts: Vec<Pos2> = (0..=14)
        .map(|i| {
            let a = (i as f32 / 14.0) * std::f32::consts::TAU * 0.78 + 0.6;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    p.add(Shape::line(pts.clone(), stroke));
    // arrow head at the end of the arc
    let end = pts[pts.len() - 1];
    let prev = pts[pts.len() - 2];
    let dir = (end - prev).normalized();
    let nrm = vec2(-dir.y, dir.x);
    p.add(Shape::convex_polygon(
        vec![end + dir * 2.8, end + nrm * 2.2, end - nrm * 2.2],
        col,
        Stroke::NONE,
    ));
}

/// A circled "i" drawn from shapes (no font glyph needed).
fn info_icon(p: &Painter, color: Color32, c: Pos2) {
    p.circle_stroke(c, 5.2, Stroke::new(1.1_f32, color));
    p.circle_filled(c + vec2(0.0, -2.2), 0.8, color);
    p.line_segment(
        [c + vec2(0.0, -0.4), c + vec2(0.0, 2.6)],
        Stroke::new(1.3_f32, color),
    );
}

/// "Session" / "Weekly": muted label, LED strip, percentage. Click toggles the detail pane.
fn row_widget(
    ui: &mut Ui,
    pal: &Palette,
    row: &view::Row,
    vendor: VendorId,
    open: bool,
    value_w: f32,
    strip_x: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    if resp.hovered() || open {
        p.rect_filled(
            rect.expand2(vec2(4.0, 0.0)),
            4.0,
            pal.hover.gamma_multiply(if open { 0.7 } else { 0.45 }),
        );
    }
    let color = pal.level(vendor, row.level);
    let cy = rect.center().y;
    p.text(
        Pos2::new(rect.left() + 2.0, cy),
        Align2::LEFT_CENTER,
        row.label,
        prop(11.5),
        pal.muted,
    );
    // an old reading is drawn dimmed (the data-age chip and the tooltip say how old)
    let color = if row.dim {
        color.gamma_multiply(0.55)
    } else {
        color
    };
    let vcol = if row.value == "--" { pal.muted } else { color };
    p.text(
        Pos2::new(rect.right() - 2.0, cy),
        Align2::RIGHT_CENTER,
        &row.value,
        mono(12.0),
        vcol,
    );
    let strip = Rect::from_min_max(
        Pos2::new(rect.left() + strip_x, cy - 4.0),
        Pos2::new(
            (rect.right() - value_w - 6.0).max(rect.left() + strip_x + 10.0),
            cy + 4.0,
        ),
    );
    if row.stale {
        led_dashes(p, strip, pal.muted);
    } else {
        led_strip(p, strip, &row.cells, color, pal.dark);
    }
    resp
}

/// Width of the small window's time column (`33h 10m`, `\u{221e}`).
const SMALL_TIME_W: f32 = 56.0;

/// One meter row of the small window: like [`row_widget`] plus the time column, and hover only
/// (a click or drag goes to the window behind it).
fn small_row_widget(
    ui: &mut Ui,
    pal: &Palette,
    row: &view::Row,
    vendor: VendorId,
    value_w: f32,
    strip_x: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let p = ui.painter();
    let color = pal.level(vendor, row.level);
    let cy = rect.center().y;
    p.text(
        Pos2::new(rect.left() + 2.0, cy),
        Align2::LEFT_CENTER,
        row.label,
        prop(11.5),
        pal.muted,
    );
    let color = if row.dim {
        color.gamma_multiply(0.55)
    } else {
        color
    };
    if let Some(t) = &row.time {
        let tcol = if t.warn { pal.red } else { pal.foreground };
        // the infinity sign is a low, small glyph: drawn larger so it stands as tall as the % beside it
        let font = if t.text == view::INFINITY {
            mono(19.0)
        } else {
            mono(11.5)
        };
        p.text(
            Pos2::new(rect.right() - 2.0, cy),
            Align2::RIGHT_CENTER,
            &t.text,
            font,
            tcol,
        );
    }
    let value_right = rect.right() - 2.0 - SMALL_TIME_W;
    let vcol = if row.value == "--" { pal.muted } else { color };
    p.text(
        Pos2::new(value_right, cy),
        Align2::RIGHT_CENTER,
        &row.value,
        mono(12.0),
        vcol,
    );
    let strip = Rect::from_min_max(
        Pos2::new(rect.left() + strip_x, cy - 4.0),
        Pos2::new(
            (value_right - value_w - 4.0).max(rect.left() + strip_x + 10.0),
            cy + 4.0,
        ),
    );
    if row.stale {
        led_dashes(p, strip, pal.muted);
    } else {
        led_strip(p, strip, &row.cells, color, pal.dark);
    }
    resp
}

/// The red warning triangle (drawn, so it does not depend on a font having the glyph).
fn warn_icon(p: &Painter, pal: &Palette, c: Pos2) {
    tri_icon(p, pal, c, pal.red);
}

/// A warning triangle with an exclamation mark, in `color`.
fn tri_icon(p: &Painter, pal: &Palette, c: Pos2, color: Color32) {
    let tri = vec![c + vec2(0.0, -4.5), c + vec2(5.0, 4.0), c + vec2(-5.0, 4.0)];
    p.add(Shape::convex_polygon(tri, color, Stroke::NONE));
    p.line_segment(
        [c + vec2(0.0, -1.5), c + vec2(0.0, 1.0)],
        Stroke::new(1.2_f32, pal.panel),
    );
    p.circle_filled(c + vec2(0.0, 2.7), 0.7, pal.panel);
}

/// A small underlined link; hovering shows `url`. Returns the response (check `clicked()`).
fn link_label(ui: &mut Ui, pal: &Palette, text: &str, url: &str) -> Response {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(prop(11.5))
                .color(pal.blue)
                .underline(),
        )
        .sense(Sense::click()),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .on_hover_text(url)
}

/// The text width tooltips wrap at in a window `screen_w` wide, so that the whole tooltip (text
/// plus its frame, `frame` pt in total) is at most the window minus 12 pt on each side; never
/// wider than reads well.
pub fn tooltip_width(screen_w: f32, frame: f32) -> f32 {
    (screen_w - 24.0 - frame).clamp(120.0, 280.0)
}

fn tooltip(ui: &mut Ui, pal: &Palette, row: &view::Row) {
    ui.set_max_width(260.0_f32.min(ui.ctx().style().spacing.tooltip_width));
    for l in &row.tip {
        let col = match l.tone {
            view::Tone::Normal => pal.foreground,
            view::Tone::Muted => pal.muted,
            view::Tone::Warn => pal.red,
            view::Tone::Caution => pal.orange,
        };
        if l.tone == view::Tone::Caution {
            // the drawn warning triangle, then the line
            ui.horizontal_top(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                tri_icon(ui.painter(), pal, r.center() + vec2(0.0, 1.0), pal.orange);
                ui.add(egui::Label::new(RichText::new(&l.text).font(prop(12.0)).color(col)).wrap());
            });
            continue;
        }
        ui.label(RichText::new(&l.text).font(prop(12.0)).color(col));
    }
}

fn status_bar(ui: &mut Ui, pal: &Palette, s: &view::StatusLine) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    if let Some(full) = &s.full {
        // the bar shows a short summary; the full message is one hover away
        resp.on_hover_ui(|ui| {
            ui.set_max_width(280.0_f32.min(ui.ctx().style().spacing.tooltip_width));
            ui.add(egui::Label::new(RichText::new(full).font(mono(11.0))).wrap());
        });
    }
    let p = ui.painter();
    let c = pal.dot(s.dot);
    let cy = rect.center().y;
    p.circle_filled(
        Pos2::new(rect.left() + 4.0, cy),
        5.0,
        c.gamma_multiply(0.18),
    );
    p.circle_filled(Pos2::new(rect.left() + 4.0, cy), 3.2, c);
    let mut job = egui::text::LayoutJob::simple(
        s.text.clone(),
        mono(11.0),
        pal.muted,
        (rect.width() - 16.0).max(10.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('\u{2026}');
    let g = ui.fonts(|f| f.layout_job(job));
    ui.painter().galley(
        Pos2::new(rect.left() + 16.0, cy - g.size().y / 2.0),
        g,
        pal.muted,
    );
}

/// Dark inset graph with faint grid and a glowing line per limit window.
fn history_chart(
    ui: &mut Ui,
    pal: &Palette,
    data: Option<&ChartData>,
    color: Color32,
    range: Range,
    empty: &str,
) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 96.0), Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    p.rect_filled(rect, 6.0, pal.graph);
    let plot = rect.shrink2(vec2(1.0, 6.0));
    let grid = Stroke::new(1.0_f32, pal.line.gamma_multiply(0.55));
    for i in 1..4 {
        let y = plot.top() + plot.height() * i as f32 / 4.0;
        p.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            grid,
        );
    }
    for i in 1..6 {
        let x = plot.left() + plot.width() * i as f32 / 6.0;
        p.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            grid,
        );
    }
    p.rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0_f32, pal.line),
        StrokeKind::Inside,
    );

    let pts_exist = data.is_some_and(|d| d.segments.iter().any(|s| !s.is_empty()));
    if let (Some(d), true) = (data, pts_exist) {
        let top = if d.y_max > 0.0 { d.y_max } else { 100.0 };
        let map = |(t, pct): (f64, f64)| {
            let x =
                plot.left() + ((t - d.x0) / (d.x1 - d.x0)).clamp(0.0, 1.0) as f32 * plot.width();
            let y = plot.bottom() - (pct / top).clamp(0.0, 1.0) as f32 * plot.height();
            Pos2::new(x, y)
        };
        for seg in &d.segments {
            let pts: Vec<Pos2> = seg.iter().map(|&q| map(q)).collect();
            if pts.len() == 1 {
                // a lone sample: a small dot (no halo that could merge with its neighbours)
                p.circle_filled(pts[0], 1.6, color);
                continue;
            }
            // the glow only where the line is sparse: many points per pixel would stack their
            // translucent strokes into a thick band
            let span = pts[pts.len() - 1].x - pts[0].x;
            let dense = pts.len() as f32 > span.max(1.0) * 0.5;
            if !dense {
                for (w, a) in [(5.0_f32, 0.06_f32), (3.0, 0.12)] {
                    p.add(Shape::line(
                        pts.clone(),
                        Stroke::new(w, color.gamma_multiply(a)),
                    ));
                }
            }
            p.add(Shape::line(pts, Stroke::new(1.4_f32, color)));
        }
    } else {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            empty,
            prop(12.0),
            pal.muted,
        );
    }
    // axis hints
    let left = match range.hours() {
        Some(h) => format!("-{}", view::left(h)),
        None => data
            .and_then(|d| chrono::DateTime::from_timestamp(d.x0 as i64, 0))
            .map(|t| t.with_timezone(&Local).format("%b %d").to_string())
            .unwrap_or_default(),
    };
    p.text(
        Pos2::new(rect.left() + 6.0, rect.bottom() - 3.0),
        Align2::LEFT_BOTTOM,
        left,
        mono(10.0),
        pal.muted,
    );
    p.text(
        Pos2::new(rect.right() - 6.0, rect.bottom() - 3.0),
        Align2::RIGHT_BOTTOM,
        "now",
        mono(10.0),
        pal.muted,
    );
    p.text(
        Pos2::new(rect.right() - 6.0, rect.top() + 3.0),
        Align2::RIGHT_TOP,
        format!(
            "{}%",
            data.map_or(100.0, |d| if d.y_max > 0.0 { d.y_max } else { 100.0 }) as i64
        ),
        mono(10.0),
        pal.muted,
    );
}

#[cfg(all(test, target_os = "linux"))]
mod backend_handle_tests {
    use super::actual_backend;
    use crate::placement::ActualBackend;
    use std::num::NonZeroU32;
    use std::ptr::NonNull;
    use winit::raw_window_handle::{
        RawWindowHandle as H, WaylandWindowHandle, XcbWindowHandle, XlibWindowHandle,
    };

    #[test]
    fn handle_kind_maps_to_backend() {
        let xl = H::Xlib(XlibWindowHandle::new(1));
        let xc = H::Xcb(XcbWindowHandle::new(NonZeroU32::new(1).unwrap()));
        let wl = H::Wayland(WaylandWindowHandle::new(NonNull::dangling()));
        assert_eq!(actual_backend(Ok(xl)), ActualBackend::X11);
        assert_eq!(actual_backend(Ok(xc)), ActualBackend::X11);
        assert_eq!(actual_backend(Ok(wl)), ActualBackend::Wayland);
        assert_eq!(actual_backend(Err(())), ActualBackend::Unknown);
    }
}
