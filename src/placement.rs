// SPDX-License-Identifier: GPL-3.0-or-later
//! Remembering where the window was left (UI-free, so the rules are unit-testable).
//!
//! The window's outer position and width are saved in `settings.json` under `window`:
//! `{"x": 1210.0, "y": 96.0, "width": 310.0, "scale": 1.5}`, in egui's logical points plus the
//! native scale factor they were measured with (physical pixels = points x scale). The height is
//! never saved: it always fits the content.
//!
//! * [`Tracker`] decides *when* to save: only when the position or width changed and then stayed
//!   put for [`SETTLE`] (so dragging does not write on every frame), never while minimised or when
//!   the position is the Windows "minimised" sentinel (about -32000, -32000), and once more on exit.
//! * [`restore`] decides *whether* a saved place is still usable, given the monitors' work areas
//!   in physical pixels: it must show at least [`MIN_VISIBLE_W`] x [`MIN_VISIBLE_H`] pixels on some
//!   monitor (title bar included); a window that is only partly outside is nudged back inside;
//!   anything else (an unplugged monitor, a smaller resolution) gives `None` = default placement.
//! * [`monitors`] lists the monitors (Windows: `EnumDisplayMonitors` + `GetDpiForMonitor`, in
//!   physical pixels; macOS: `NSScreen`, in points; other platforms return nothing, so nothing is
//!   restored there).
//! * macOS works in logical points everywhere (winit and eframe report and accept points whose
//!   origin is the top-left of the primary display), so [`restore_points`] checks the saved point
//!   position against monitor rectangles in points and nothing is scaled. [`try_place`] picks the
//!   right rule and says why a saved place was dropped (for `notify.log`).

use crate::settings::WindowPos;
use std::time::{Duration, Instant};

/// A saved position must keep at least this much of the window visible (physical pixels).
pub const MIN_VISIBLE_W: f64 = 80.0;
pub const MIN_VISIBLE_H: f64 = 40.0;
/// Narrowest window (matches the viewport's minimum inner width).
pub const MIN_WIDTH: f64 = 270.0;
pub const MAX_WIDTH: f64 = 2000.0;
pub const DEFAULT_WIDTH: f64 = 310.0;
/// Height assumed when checking visibility (the real height is auto-fitted later).
pub const ASSUMED_HEIGHT: f64 = 240.0;
/// How long a changed position must stay unchanged before it is saved.
pub const SETTLE: Duration = Duration::from_millis(900);
/// Where "Reset window position" puts the window (logical points).
pub const RESET_POS: (f32, f32) = (80.0, 80.0);

/// A monitor's work area in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Native scale factor (1.0 = 96 dpi).
    pub scale: f64,
    pub primary: bool,
}

/// A restored place, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
}

/// Windows reports a minimised window at about (-32000, -32000); that is not a place.
pub fn is_sentinel(x: f64, y: f64) -> bool {
    !x.is_finite()
        || !y.is_finite()
        || x <= -30000.0
        || y <= -30000.0
        || x >= 30000.0
        || y >= 30000.0
}

fn overlap(a: (f64, f64, f64, f64), m: &Monitor) -> (f64, f64) {
    let w = (a.0 + a.2).min(m.x + m.w) - a.0.max(m.x);
    let h = (a.1 + a.3).min(m.y + m.h) - a.1.max(m.y);
    (w.max(0.0), h.max(0.0))
}

/// Is the saved place still on a screen? `None` = use the default placement.
pub fn restore(saved: &WindowPos, monitors: &[Monitor]) -> Option<Placement> {
    if is_sentinel(saved.x, saved.y) {
        return None;
    }
    let scale = saved
        .scale
        .filter(|s| s.is_finite() && *s > 0.0 && *s <= 8.0)
        .unwrap_or(1.0);
    let width_pts = saved
        .width
        .filter(|w| w.is_finite())
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH);
    let (x, y) = (saved.x * scale, saved.y * scale);
    let (w, h) = (width_pts * scale, ASSUMED_HEIGHT * scale);
    // the monitor that shows the most of the window
    let (best, (ow, oh)) = monitors
        .iter()
        .map(|m| (m, overlap((x, y, w, h), m)))
        .max_by(|a, b| (a.1 .0 * a.1 .1).total_cmp(&(b.1 .0 * b.1 .1)))?;
    if ow < MIN_VISIBLE_W || oh < MIN_VISIBLE_H {
        return None;
    }
    // nudge back inside (the title bar must stay reachable: never above the work area)
    let clamp = |v: f64, lo: f64, hi: f64| if hi < lo { lo } else { v.clamp(lo, hi) };
    Some(Placement {
        x: clamp(x, best.x, best.x + best.w - w),
        y: clamp(y, best.y, best.y + best.h - h),
        width: w,
    })
}

/// Position and width for the viewport builder, in egui points. The builder multiplies points by
/// the *primary* monitor's scale, so divide by that to land on the physical pixels wanted.
pub fn builder_points(p: &Placement, monitors: &[Monitor]) -> ((f32, f32), f32) {
    let s = monitors
        .iter()
        .find(|m| m.primary)
        .or(monitors.first())
        .map_or(1.0, |m| m.scale)
        .max(0.1);
    (
        (((p.x / s) as f32), ((p.y / s) as f32)),
        (p.width / s) as f32,
    )
}

/// Restore in logical points (macOS): `saved` and the monitors are in the same point space, so
/// nothing is scaled. Same visibility rule and nudging as [`restore`]; the result is in points.
pub fn restore_points(saved: &WindowPos, monitors: &[Monitor]) -> Option<Placement> {
    if is_sentinel(saved.x, saved.y) {
        return None;
    }
    let width = saved
        .width
        .filter(|w| w.is_finite())
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH);
    let (x, y, h) = (saved.x, saved.y, ASSUMED_HEIGHT);
    let (best, (ow, oh)) = monitors
        .iter()
        .map(|m| (m, overlap((x, y, width, h), m)))
        .max_by(|a, b| (a.1 .0 * a.1 .1).total_cmp(&(b.1 .0 * b.1 .1)))?;
    if ow < MIN_VISIBLE_W || oh < MIN_VISIBLE_H {
        return None;
    }
    let clamp = |v: f64, lo: f64, hi: f64| if hi < lo { lo } else { v.clamp(lo, hi) };
    Some(Placement {
        x: clamp(x, best.x, best.x + best.w - width),
        y: clamp(y, best.y, best.y + best.h - h),
        width,
    })
}

/// Why a saved place was not used (goes to `notify.log`, so it carries no secrets: only numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// The stored position is the minimised sentinel or not a finite number.
    Sentinel,
    /// The monitor list is empty (unknown platform or the query failed), so nothing can be checked.
    NoMonitors,
    /// Less than the minimum would be visible on any connected monitor.
    OffScreen,
}

impl Discard {
    pub fn text(self) -> &'static str {
        match self {
            Discard::Sentinel => "not a usable position",
            Discard::NoMonitors => "no monitor list available on this platform",
            Discard::OffScreen => "not on any connected screen",
        }
    }
}

/// Decide the viewport builder's position and width (egui points) for a saved place, or why not.
/// `points_space` = the monitors are in logical points (macOS) rather than physical pixels
/// (Windows).
pub fn try_place(
    saved: &WindowPos,
    monitors: &[Monitor],
    points_space: bool,
) -> Result<((f32, f32), f32), Discard> {
    if is_sentinel(saved.x, saved.y) {
        return Err(Discard::Sentinel);
    }
    if monitors.is_empty() {
        return Err(Discard::NoMonitors);
    }
    if points_space {
        let p = restore_points(saved, monitors).ok_or(Discard::OffScreen)?;
        Ok(((p.x as f32, p.y as f32), p.width as f32))
    } else {
        let p = restore(saved, monitors).ok_or(Discard::OffScreen)?;
        Ok(builder_points(&p, monitors))
    }
}

/// One-line, secret-free description of the outcome for `notify.log`.
pub fn describe(saved: Option<&WindowPos>, outcome: &Result<((f32, f32), f32), Discard>) -> String {
    let Some(s) = saved else {
        return "window: no saved position, using the default place".into();
    };
    match outcome {
        Ok(((x, y), w)) => format!(
            "window: restored saved position ({:.0}, {:.0}) -> ({x:.0}, {y:.0}), width {w:.0}",
            s.x, s.y
        ),
        Err(d) => format!(
            "window: discarded saved position ({:.0}, {:.0}): {}",
            s.x,
            s.y,
            d.text()
        ),
    }
}

// ---- Linux: display backend and what position handling each one allows ----------------------

/// Which display backend the window should start under (Linux only; the decision is pure so it
/// can be tested, [`crate::ui::run`] applies it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// Not a Wayland session (or nothing to choose between): let winit pick.
    Auto,
    /// Wayland session, X display available: start through XWayland, so "always on top" and
    /// remembering the window position work (the default).
    XWayland,
    /// Native Wayland: no always-on-top, no window position (size only).
    NativeWayland,
}

/// The environment values the backend choice depends on (`None` or empty = not set).
#[derive(Debug, Clone, Copy, Default)]
pub struct DisplayEnv<'a> {
    pub wayland_display: Option<&'a str>,
    pub xdg_session_type: Option<&'a str>,
    pub display: Option<&'a str>,
    /// `AITRACK_WAYLAND`: `native` (or `wayland`, `1`) asks for native Wayland.
    pub aitrack_wayland: Option<&'a str>,
}

fn set(v: Option<&str>) -> bool {
    v.is_some_and(|s| !s.trim().is_empty())
}

fn wants_native(e: &DisplayEnv) -> bool {
    e.aitrack_wayland.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "native" | "wayland" | "1" | "true"
        )
    })
}

/// Pick the backend. Wayland session + X display = XWayland unless `AITRACK_WAYLAND=native`.
pub fn choose_backend(e: &DisplayEnv) -> BackendChoice {
    let wayland_session = set(e.wayland_display)
        || e.xdg_session_type
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("wayland"));
    if !wayland_session {
        return BackendChoice::Auto;
    }
    let wants_native = wants_native(e);
    if !set(e.wayland_display) {
        // no Wayland socket to connect to: only an X display can work
        return BackendChoice::Auto;
    }
    if wants_native || !set(e.display) {
        BackendChoice::NativeWayland
    } else {
        BackendChoice::XWayland
    }
}

/// Socket file of a local X display (`:0`, `:1.0`, `unix:0`); `None` for a remote host or an
/// unparsable value (then the display is assumed to work and winit finds out).
pub fn x_socket_path(display: &str) -> Option<std::path::PathBuf> {
    let d = display.trim();
    let d = d.strip_prefix("unix").unwrap_or(d);
    let rest = d.strip_prefix(':')?;
    let num = rest.split('.').next()?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(std::path::PathBuf::from(format!("/tmp/.X11-unix/X{num}")))
}

/// Display numbers found in socket file names (`X0`, `X1024`; anything else is ignored), lowest first.
pub fn x_display_numbers<S: AsRef<str>>(names: &[S]) -> Vec<u32> {
    let mut v: Vec<u32> = names
        .iter()
        .filter_map(|n| n.as_ref().strip_prefix('X'))
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|d| d.parse().ok())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// How the backend was decided, for the log and Settings. Never contains values of variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDecision {
    pub choice: BackendChoice,
    /// `DISPLAY` to export before the window opens (set only when it was unset and a socket was found).
    pub display: Option<String>,
    pub why: &'static str,
}

/// Decide the backend. `scan` lists the names in `/tmp/.X11-unix`; `reach` really connects to an X
/// display (`:0`) and says whether it answered. Both are injected so the logic is testable.
/// - an X display that does not answer falls back to native Wayland;
/// - `DISPLAY` unset in a Wayland session: the first reachable display found by `scan` is used.
pub fn decide_backend(
    e: &DisplayEnv,
    scan: &dyn Fn() -> Vec<String>,
    reach: &dyn Fn(&str) -> bool,
) -> BackendDecision {
    let base = choose_backend(e);
    let done = |choice, display, why| BackendDecision {
        choice,
        display,
        why,
    };
    match base {
        BackendChoice::Auto => done(base, None, "not a Wayland session"),
        BackendChoice::XWayland => {
            let d = e.display.unwrap_or_default();
            if reach(d) {
                done(base, None, "X display answered")
            } else {
                done(
                    BackendChoice::NativeWayland,
                    None,
                    "X display did not answer",
                )
            }
        }
        BackendChoice::NativeWayland if wants_native(e) => {
            done(base, None, "AITRACK_WAYLAND asks for native Wayland")
        }
        BackendChoice::NativeWayland => {
            // DISPLAY unset: look for a running X server
            let found = x_display_numbers(&scan())
                .into_iter()
                .map(|n| format!(":{n}"))
                .find(|d| reach(d));
            match found {
                Some(d) => done(
                    BackendChoice::XWayland,
                    Some(d),
                    "DISPLAY was unset; found an X display",
                ),
                None => done(base, None, "no X display found"),
            }
        }
    }
}

/// `x11 (XWayland)` / `wayland (native)` / `auto (library default)`.
pub fn backend_name(b: BackendChoice) -> &'static str {
    match b {
        BackendChoice::XWayland => "x11 (XWayland)",
        BackendChoice::NativeWayland => "wayland (native)",
        BackendChoice::Auto => "auto (library default)",
    }
}

/// What the window really runs on, read from its window handle after creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualBackend {
    X11,
    Wayland,
    Unknown,
}

impl ActualBackend {
    pub fn text(self) -> &'static str {
        match self {
            ActualBackend::X11 => "x11",
            ActualBackend::Wayland => "wayland",
            ActualBackend::Unknown => "unknown",
        }
    }
}

/// A warning when the window runs on another backend than the one chosen (`None` = agreement, or
/// nothing to compare: `Auto` or an unreadable handle).
pub fn backend_mismatch(chosen: BackendChoice, actual: ActualBackend) -> Option<String> {
    let want = match chosen {
        BackendChoice::XWayland => ActualBackend::X11,
        BackendChoice::NativeWayland => ActualBackend::Wayland,
        BackendChoice::Auto => return None,
    };
    (actual != ActualBackend::Unknown && actual != want).then(|| {
        format!(
            "window: WARNING chose {} but the window runs on {}",
            backend_name(chosen),
            actual.text()
        )
    })
}

/// The `display backend: ...` line for the log, `--sources` and Settings. `actual` is known only
/// once the window exists.
pub fn backend_line(chosen: BackendChoice, actual: Option<ActualBackend>) -> String {
    match actual {
        Some(a) if a != ActualBackend::Unknown => format!(
            "display backend: {} (chosen: {})",
            match a {
                ActualBackend::X11 => "x11 (running)",
                _ => "wayland (running)",
            },
            backend_name(chosen),
        ),
        _ => format!("display backend: {} (chosen)", backend_name(chosen)),
    }
}

/// Hint appended to a window start-up error so the owner knows what to try.
pub fn failure_hint(chosen: BackendChoice) -> &'static str {
    match chosen {
        BackendChoice::XWayland => {
            "it was started through XWayland and that failed; try AITRACK_WAYLAND=native aitrack"
        }
        BackendChoice::NativeWayland => {
            "it was started on native Wayland and that failed; check that DISPLAY points at a running X server and that AITRACK_WAYLAND is not set"
        }
        BackendChoice::Auto => "check that DISPLAY or WAYLAND_DISPLAY points at a running display",
    }
}

/// Connect to the X server's socket, retrying (Xwayland may be started lazily by the compositor).
/// `true` as soon as one connect works.
#[cfg(unix)]
pub fn probe_x_socket(path: &std::path::Path, attempts: u32, delay: std::time::Duration) -> bool {
    for i in 0..attempts.max(1) {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return true;
        }
        if i + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    false
}

/// Real-world `reach`: a local display is probed (5 tries, 200 ms apart, about 1 s); a display
/// that is not a local socket is assumed to work and winit finds out.
#[cfg(unix)]
pub fn reach_x_display(display: &str) -> bool {
    match x_socket_path(display) {
        Some(p) => probe_x_socket(&p, 5, std::time::Duration::from_millis(200)),
        None => !display.trim().is_empty(),
    }
}

/// Names in `/tmp/.X11-unix`.
#[cfg(unix)]
pub fn scan_x_sockets() -> Vec<String> {
    std::fs::read_dir("/tmp/.X11-unix")
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The decision for this process from the real environment and X sockets (Linux/unix; reads the
/// environment and may wait about a second for a lazily started Xwayland). Changes nothing.
#[cfg(unix)]
pub fn current_decision() -> BackendDecision {
    let var = |k: &str| std::env::var(k).ok();
    let (wd, st, d, aw) = (
        var("WAYLAND_DISPLAY"),
        var("XDG_SESSION_TYPE"),
        var("DISPLAY"),
        var("AITRACK_WAYLAND"),
    );
    decide_backend(
        &DisplayEnv {
            wayland_display: wd.as_deref(),
            xdg_session_type: st.as_deref(),
            display: d.as_deref(),
            aitrack_wayland: aw.as_deref(),
        },
        &scan_x_sockets,
        &reach_x_display,
    )
}

static BACKEND_REPORT: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();

/// Remember the current `display backend: ...` line for Settings (Linux only; empty elsewhere).
pub fn set_backend_report(line: String) {
    let m = BACKEND_REPORT.get_or_init(Default::default);
    if let Ok(mut g) = m.lock() {
        *g = line;
    }
}

pub fn backend_report() -> String {
    BACKEND_REPORT
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

/// How much of the window place can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosMode {
    /// Windows and macOS: position and width, checked against the monitors.
    Full,
    /// X11 / XWayland: trust a sane saved point, the window manager clamps it.
    Trust,
    /// Native Wayland: clients cannot read or set their position; only the width is kept.
    WidthOnly,
}

impl PosMode {
    pub fn of(b: BackendChoice) -> Self {
        match b {
            BackendChoice::NativeWayland => PosMode::WidthOnly,
            _ => PosMode::Trust,
        }
    }
}

static MODE: std::sync::OnceLock<PosMode> = std::sync::OnceLock::new();

/// Record the mode for this run (first call wins). Not set = [`PosMode::Full`].
pub fn set_mode(m: PosMode) {
    let _ = MODE.set(m);
}

pub fn mode() -> PosMode {
    MODE.get().copied().unwrap_or(PosMode::Full)
}

/// Native Wayland cannot keep a window above others; the Settings row says so.
pub fn on_top_unavailable() -> bool {
    mode() == PosMode::WidthOnly
}

/// A saved point on X11 is trusted inside these generous bounds (points); the window manager
/// moves it further if needed.
const TRUST_X: (f64, f64) = (-4000.0, 16000.0);
const TRUST_Y: (f64, f64) = (-100.0, 16000.0);

/// The place to ask for when reopening on Linux: a position (X11 only) and the width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinuxPlace {
    pub pos: Option<(f32, f32)>,
    pub width: f32,
}

/// Linux restore rule. `Trust`: a non-sentinel point inside sane bounds is used as saved, an
/// absurd one keeps only the width. `WidthOnly`: x/y are never applied.
pub fn linux_place(saved: &WindowPos, mode: PosMode) -> Result<LinuxPlace, Discard> {
    let width = saved
        .width
        .filter(|w| w.is_finite())
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH) as f32;
    match mode {
        PosMode::WidthOnly => Ok(LinuxPlace { pos: None, width }),
        _ => {
            if is_sentinel(saved.x, saved.y) {
                return Err(Discard::Sentinel);
            }
            let sane = (TRUST_X.0..=TRUST_X.1).contains(&saved.x)
                && (TRUST_Y.0..=TRUST_Y.1).contains(&saved.y);
            // exactly (0, 0) is what native Wayland stores (`sample_for`, `WidthOnly`): it means
            // "no position", not a window in the corner, so it is never applied under X11
            let wayland_stand_in = saved.x == 0.0 && saved.y == 0.0;
            Ok(LinuxPlace {
                pos: (sane && !wayland_stand_in).then_some((saved.x as f32, saved.y as f32)),
                width,
            })
        }
    }
}

/// One-line description for `notify.log` (numbers only).
pub fn describe_linux(saved: &WindowPos, r: &Result<LinuxPlace, Discard>) -> String {
    match r {
        Ok(LinuxPlace {
            pos: Some((x, y)),
            width,
        }) => {
            format!("window: restored saved position ({x:.0}, {y:.0}), width {width:.0}")
        }
        Ok(LinuxPlace { pos: None, width }) => {
            format!("window: restored width {width:.0} only (no usable position on this display)")
        }
        Err(d) => format!(
            "window: discarded saved position ({:.0}, {:.0}): {}",
            saved.x,
            saved.y,
            d.text()
        ),
    }
}

/// Build the per-frame sample for the tracker. `Full` needs the outer position (as before);
/// `Trust` the same; `WidthOnly` takes the width alone and stores a constant (0, 0), because the
/// position is not known or meaningful there.
pub fn sample_for(
    mode: PosMode,
    outer_min: Option<(f64, f64)>,
    inner_width: Option<f64>,
    scale: f64,
    minimized: bool,
) -> Option<Sample> {
    let width = inner_width?;
    let (x, y) = match mode {
        PosMode::WidthOnly => (0.0, 0.0),
        _ => outer_min?,
    };
    Some(Sample {
        x,
        y,
        width,
        scale,
        minimized,
    })
}

/// A Cocoa screen as `NSScreen` reports it: points, origin at the bottom-left of the primary
/// screen, y growing upwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CocoaScreen {
    /// Height of the whole frame (the first screen's height is the flip reference).
    pub frame_h: f64,
    /// Visible frame (menu bar, Dock and notch area excluded): x, y (bottom edge), width, height.
    pub visible: (f64, f64, f64, f64),
    /// `backingScaleFactor` (2.0 on Retina).
    pub scale: f64,
}

/// Convert Cocoa screens (the first is the primary one, the one with the menu bar) into monitors
/// in the top-left-origin point space that winit and eframe use.
pub fn cocoa_monitors(screens: &[CocoaScreen]) -> Vec<Monitor> {
    let Some(primary_h) = screens.first().map(|s| s.frame_h) else {
        return Vec::new();
    };
    screens
        .iter()
        .enumerate()
        .filter(|(_, s)| s.visible.2 > 0.0 && s.visible.3 > 0.0)
        .map(|(i, s)| {
            let (x, y_bottom, w, h) = s.visible;
            Monitor {
                x,
                y: primary_h - (y_bottom + h),
                w,
                h,
                scale: if s.scale.is_finite() && s.scale > 0.0 {
                    s.scale
                } else {
                    1.0
                },
                primary: i == 0,
            }
        })
        .collect()
}

/// What the window looked like in one frame (points + the native scale factor).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub scale: f64,
    pub minimized: bool,
}

/// Decides when the window place is worth saving (see the module docs).
#[derive(Debug, Default)]
pub struct Tracker {
    saved: Option<Key>,
    pending: Option<(Sample, Instant)>,
    last: Option<Sample>,
}

/// Whole-point key: sub-point jitter is not a move.
type Key = (i64, i64, i64);

fn key(s: &Sample) -> Key {
    (
        s.x.round() as i64,
        s.y.round() as i64,
        s.width.round() as i64,
    )
}

fn usable(s: &Sample) -> bool {
    !s.minimized && !is_sentinel(s.x, s.y) && s.width.is_finite() && s.width > 0.0
}

impl Tracker {
    /// The place already stored (from settings at start-up), so it is not saved again.
    pub fn with_saved(saved: Option<&WindowPos>) -> Self {
        Self {
            saved: saved.map(|p| {
                (
                    p.x.round() as i64,
                    p.y.round() as i64,
                    p.width.unwrap_or(DEFAULT_WIDTH).round() as i64,
                )
            }),
            ..Self::default()
        }
    }

    /// Record the place stored by something else (the reset button) without saving it.
    pub fn mark_saved(&mut self, s: Option<Sample>) {
        self.saved = s.map(|s| key(&s));
        self.pending = None;
    }

    /// Is a change waiting to settle? (The window schedules a repaint to flush it.)
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Feed one frame. Returns the place to save when a change has stayed put for [`SETTLE`].
    pub fn observe(&mut self, now: Instant, sample: Option<Sample>) -> Option<Sample> {
        let s = sample.filter(usable)?;
        self.last = Some(s);
        if Some(key(&s)) == self.saved {
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((p, since)) if key(&p) == key(&s) => {
                if now.duration_since(since) >= SETTLE {
                    self.saved = Some(key(&s));
                    self.pending = None;
                    return Some(s);
                }
            }
            _ => self.pending = Some((s, now)),
        }
        None
    }

    /// On exit: the last usable place, if it differs from what is stored.
    pub fn flush(&mut self) -> Option<Sample> {
        let s = self.last.filter(usable)?;
        if Some(key(&s)) == self.saved {
            return None;
        }
        self.saved = Some(key(&s));
        self.pending = None;
        Some(s)
    }
}

impl Sample {
    pub fn to_pos(self) -> WindowPos {
        WindowPos {
            x: self.x,
            y: self.y,
            width: Some(self.width),
            scale: Some(self.scale),
        }
    }
}

/// The monitors' work areas in physical pixels (empty when unknown: nothing is restored then).
pub fn monitors() -> Vec<Monitor> {
    sys::monitors()
}

#[cfg(windows)]
mod sys {
    use super::Monitor;
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    type EnumProc = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut Rect, isize) -> i32;

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayMonitors(
            hdc: *mut c_void,
            clip: *const Rect,
            f: EnumProc,
            data: isize,
        ) -> i32;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    }

    #[link(name = "shcore")]
    extern "system" {
        fn GetDpiForMonitor(monitor: *mut c_void, kind: i32, x: *mut u32, y: *mut u32) -> i32;
    }

    unsafe extern "system" fn collect(
        monitor: *mut c_void,
        _hdc: *mut c_void,
        _rect: *mut Rect,
        data: isize,
    ) -> i32 {
        // SAFETY: `data` is the address of the Vec passed by `monitors()` below, alive for the call.
        let out = &mut *(data as *mut Vec<Monitor>);
        let mut info = MonitorInfo {
            size: std::mem::size_of::<MonitorInfo>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            let (mut dx, mut dy) = (96u32, 96u32);
            // MDT_EFFECTIVE_DPI = 0; on failure keep 96 dpi
            if GetDpiForMonitor(monitor, 0, &mut dx, &mut dy) != 0 || dx == 0 {
                dx = 96;
            }
            out.push(Monitor {
                x: info.work.left as f64,
                y: info.work.top as f64,
                w: (info.work.right - info.work.left) as f64,
                h: (info.work.bottom - info.work.top) as f64,
                scale: dx as f64 / 96.0,
                primary: info.flags & 1 != 0, // MONITORINFOF_PRIMARY
            });
        }
        1
    }

    pub fn monitors() -> Vec<Monitor> {
        let mut out: Vec<Monitor> = Vec::new();
        // SAFETY: plain Win32 call; the callback only touches `out` through the pointer given here.
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                collect,
                &mut out as *mut Vec<Monitor> as isize,
            );
        }
        out
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::{cocoa_monitors, CocoaScreen, Monitor};
    use objc2_app_kit::NSScreen;
    use objc2_foundation_02::MainThreadMarker;

    /// Needs the main thread (AppKit); anywhere else, or on failure, the list is empty.
    pub fn monitors() -> Vec<Monitor> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Vec::new();
        };
        let screens = NSScreen::screens(mtm);
        let mut list = Vec::new();
        for i in 0..screens.len() {
            if let Some(s) = screens.get(i) {
                let frame = s.frame();
                let v = s.visibleFrame();
                list.push(CocoaScreen {
                    frame_h: frame.size.height,
                    visible: (v.origin.x, v.origin.y, v.size.width, v.size.height),
                    scale: s.backingScaleFactor(),
                });
            }
        }
        cocoa_monitors(&list)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use super::Monitor;

    pub fn monitors() -> Vec<Monitor> {
        Vec::new()
    }
}

#[cfg(test)]
mod linux_tests {
    use super::*;

    fn env<'a>(
        w: Option<&'a str>,
        t: Option<&'a str>,
        d: Option<&'a str>,
        a: Option<&'a str>,
    ) -> DisplayEnv<'a> {
        DisplayEnv {
            wayland_display: w,
            xdg_session_type: t,
            display: d,
            aitrack_wayland: a,
        }
    }

    #[test]
    fn wayland_with_x_display_defaults_to_xwayland() {
        let e = env(Some("wayland-0"), Some("wayland"), Some(":0"), None);
        assert_eq!(choose_backend(&e), BackendChoice::XWayland);
    }

    #[test]
    fn native_switch_wins() {
        for v in ["native", "Native", "wayland", "1"] {
            let e = env(Some("wayland-0"), Some("wayland"), Some(":0"), Some(v));
            assert_eq!(choose_backend(&e), BackendChoice::NativeWayland, "{v}");
        }
        let e = env(Some("wayland-0"), Some("wayland"), Some(":0"), Some("x11"));
        assert_eq!(choose_backend(&e), BackendChoice::XWayland);
    }

    #[test]
    fn wayland_without_x_display_is_native() {
        let e = env(Some("wayland-0"), Some("wayland"), None, None);
        assert_eq!(choose_backend(&e), BackendChoice::NativeWayland);
        let e = env(Some("wayland-0"), None, Some(""), None);
        assert_eq!(choose_backend(&e), BackendChoice::NativeWayland);
    }

    #[test]
    fn non_wayland_sessions_are_left_to_winit() {
        assert_eq!(
            choose_backend(&env(None, Some("x11"), Some(":0"), None)),
            BackendChoice::Auto
        );
        assert_eq!(
            choose_backend(&env(None, None, None, None)),
            BackendChoice::Auto
        );
        // session says wayland but there is no socket to connect to
        assert_eq!(
            choose_backend(&env(None, Some("wayland"), Some(":0"), None)),
            BackendChoice::Auto
        );
        assert_eq!(
            choose_backend(&env(Some(" "), None, Some(":0"), Some("native"))),
            BackendChoice::Auto
        );
    }

    #[test]
    fn x_socket_paths() {
        let p = |d: &str| x_socket_path(d).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(p(":0").as_deref(), Some("/tmp/.X11-unix/X0"));
        assert_eq!(p(":12.1").as_deref(), Some("/tmp/.X11-unix/X12"));
        assert_eq!(p("unix:1").as_deref(), Some("/tmp/.X11-unix/X1"));
        assert_eq!(p("host:0"), None);
        assert_eq!(p(":"), None);
        assert_eq!(p(":a"), None);
    }

    fn saved(x: f64, y: f64, w: Option<f64>) -> WindowPos {
        WindowPos {
            x,
            y,
            width: w,
            scale: Some(1.0),
        }
    }

    #[test]
    fn trust_mode_uses_sane_points_and_keeps_width() {
        let r = linux_place(&saved(1210.0, 96.0, Some(330.0)), PosMode::Trust).unwrap();
        assert_eq!(
            r,
            LinuxPlace {
                pos: Some((1210.0, 96.0)),
                width: 330.0
            }
        );
        // second monitor to the left is fine
        let r = linux_place(&saved(-1500.0, 40.0, None), PosMode::Trust).unwrap();
        assert_eq!(r.pos, Some((-1500.0, 40.0)));
        assert_eq!(r.width, DEFAULT_WIDTH as f32);
    }

    #[test]
    fn trust_mode_drops_absurd_points_but_keeps_width() {
        let r = linux_place(&saved(20000.0, 10.0, Some(300.0)), PosMode::Trust).unwrap();
        assert_eq!(r.pos, None);
        assert_eq!(r.width, 300.0);
        let r = linux_place(&saved(10.0, -5000.0, Some(300.0)), PosMode::Trust).unwrap();
        assert_eq!(r.pos, None);
    }

    #[test]
    fn trust_mode_rejects_the_sentinel() {
        assert_eq!(
            linux_place(&saved(-32000.0, -32000.0, Some(300.0)), PosMode::Trust),
            Err(Discard::Sentinel)
        );
        assert_eq!(
            linux_place(&saved(f64::NAN, 1.0, None), PosMode::Trust),
            Err(Discard::Sentinel)
        );
    }

    #[test]
    fn the_wayland_stand_in_origin_is_not_applied_under_x11() {
        for mode in [PosMode::Trust, PosMode::Full] {
            let r = linux_place(&saved(0.0, 0.0, Some(300.0)), mode).unwrap();
            assert_eq!(r.pos, None, "{mode:?}");
            assert_eq!(r.width, 300.0);
            let r = linux_place(&saved(0.0, 40.0, Some(300.0)), mode).unwrap();
            assert_eq!(r.pos, Some((0.0, 40.0)), "{mode:?}");
        }
    }

    #[test]
    fn width_only_never_applies_a_position() {
        let r = linux_place(&saved(1210.0, 96.0, Some(5000.0)), PosMode::WidthOnly).unwrap();
        assert_eq!(r.pos, None);
        assert_eq!(r.width, MAX_WIDTH as f32);
        let r = linux_place(&saved(0.0, 0.0, Some(100.0)), PosMode::WidthOnly).unwrap();
        assert_eq!(r.width, MIN_WIDTH as f32);
    }

    #[test]
    fn samples_per_mode() {
        let outer = Some((10.0, 20.0));
        let s = sample_for(PosMode::Full, outer, Some(310.0), 1.0, false).unwrap();
        assert_eq!((s.x, s.y, s.width), (10.0, 20.0, 310.0));
        assert!(sample_for(PosMode::Trust, None, Some(310.0), 1.0, false).is_none());
        let s = sample_for(PosMode::WidthOnly, None, Some(310.0), 1.0, false).unwrap();
        assert_eq!((s.x, s.y, s.width), (0.0, 0.0, 310.0));
        assert!(sample_for(PosMode::WidthOnly, outer, None, 1.0, false).is_none());
    }

    #[test]
    fn pos_mode_follows_backend() {
        assert_eq!(
            PosMode::of(BackendChoice::NativeWayland),
            PosMode::WidthOnly
        );
        assert_eq!(PosMode::of(BackendChoice::XWayland), PosMode::Trust);
        assert_eq!(PosMode::of(BackendChoice::Auto), PosMode::Trust);
    }

    #[test]
    fn describe_has_no_surprises() {
        let sv = saved(1.0, 2.0, Some(300.0));
        let r = linux_place(&sv, PosMode::WidthOnly);
        assert!(describe_linux(&sv, &r).contains("width 300 only"));
    }
}
