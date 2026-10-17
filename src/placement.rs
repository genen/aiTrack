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
