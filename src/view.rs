// SPDX-License-Identifier: GPL-3.0-or-later
//! Presentation logic shared by the window: text formatting, bar segments, rows. No UI types here,
//! so everything is unit-testable; `ui.rs` only maps these onto egui widgets and `theme.rs` colours.

use crate::analysis::Gauge;
use crate::reading::Kind;
use crate::state::{Diag, State};
use crate::util::{js_round, parse_ts};
use crate::vendor::{Vendor, VendorId};
use chrono::{DateTime, Local, Utc};

pub const SEGMENTS: usize = 20;
pub const PRESETS: [(f64, &str); 4] = [
    (1.0, "1 min"),
    (5.0, "5 min"),
    (15.0, "15 min"),
    (60.0, "1 hour"),
];

/// Colour state of a bar/value: the vendor colour, or red from 90 %.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    Bad,
}

pub fn level(pct: f64) -> Level {
    if pct >= 90.0 {
        Level::Bad
    } else {
        Level::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    Lit,
    /// Faint: where the pace projects to by the reset.
    Projected,
    Off,
}

/// 20 segments; a segment is lit when its midpoint is below the value. The projection is only
/// drawn in "used" mode.
pub fn bar_cells(pct: f64, projected: Option<f64>, remaining: bool) -> [Cell; SEGMENTS] {
    let v = if remaining { 100.0 - pct } else { pct };
    let mut out = [Cell::Off; SEGMENTS];
    for (i, c) in out.iter_mut().enumerate() {
        let mid = (i + 1) as f64 / SEGMENTS as f64 * 100.0 - 2.5;
        *c = if mid < v {
            Cell::Lit
        } else if !remaining && projected.is_some_and(|p| mid < p) {
            Cell::Projected
        } else {
            Cell::Off
        };
    }
    out
}

/// Compact duration: `12m`, `3.5h`, `14h`, `5d`.
pub fn left(hours: f64) -> String {
    if hours < 1.0 {
        format!("{}m", js_round(hours * 60.0).max(1.0) as i64)
    } else if hours < 48.0 {
        if hours < 10.0 {
            format!("{hours:.1}h")
        } else {
            format!("{}h", js_round(hours) as i64)
        }
    } else {
        format!("{}d", js_round(hours / 24.0) as i64)
    }
}

/// `just now`, `12 min ago`, `2.5 h ago`.
pub fn ago(minutes: f64) -> String {
    let m = js_round(minutes);
    if m < 1.0 {
        "just now".into()
    } else if m < 90.0 {
        format!("{} min ago", m as i64)
    } else {
        format!("{:.1} h ago", m / 60.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub text: String,
    /// The "limit at this pace" warning.
    pub warn: bool,
}

pub fn pace_note(g: &Gauge) -> Option<Note> {
    if let (Some(htl), Some(hl)) = (g.hours_to_limit, g.hours_left) {
        if htl < hl {
            return Some(Note {
                text: format!("Limit in ~{} at this pace", left(htl)),
                warn: true,
            });
        }
    }
    // No reset time (desktop/claude.json sources): there is no countdown to project against.
    if g.hours_left.is_none() {
        return match g.rate_per_hour {
            Some(r) if r > 0.0 => Some(Note {
                text: match g.hours_to_limit {
                    Some(h) => format!("Rising ~{r:.1} pts/h \u{b7} limit in ~{}", left(h)),
                    None => format!("Rising ~{r:.1} pts/h"),
                },
                warn: false,
            }),
            Some(_) => Some(Note {
                text: "Steady, no recent use".into(),
                warn: false,
            }),
            None => None,
        };
    }
    match (g.projected_pct, g.rate_per_hour) {
        (Some(p), Some(r)) if r > 0.0 => Some(Note {
            text: format!("On pace for ~{}% by reset", js_round(p) as i64),
            warn: false,
        }),
        (_, Some(_)) => Some(Note {
            text: "Steady, no recent use".into(),
            warn: false,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    Ok,
    Stale,
    Err,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusLine {
    pub dot: Dot,
    pub text: String,
    /// The full, unabridged text (shown as a hover tooltip) when `text` is a summary.
    pub full: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Empty {
    pub title: String,
    pub body: String,
    /// Offer the "Open help" button ([`crate::launch::HELP_URL`]).
    pub help: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    /// "Limit in ~3h at this pace".
    Warn,
    /// The data behind the reading is old (amber, with a warning triangle).
    Caution,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TipLine {
    pub text: String,
    pub tone: Tone,
}

fn tip(text: impl Into<String>, tone: Tone) -> TipLine {
    TipLine {
        text: text.into(),
        tone,
    }
}

/// One of a vendor's two rows ("Session" / "Weekly").
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub label: &'static str,
    /// `42%`, `58% left`, or `--` when not reported.
    pub value: String,
    pub cells: [Cell; SEGMENTS],
    pub level: Level,
    /// No current reading: the strip is drawn as dashes and the value is `--`.
    pub stale: bool,
    /// The reading is shown but old (see [`STALE_WARN_MIN`]): drawn dimmed.
    pub dim: bool,
    /// Hover tooltip: reset time, pace, and (weekly) per-model limits and credits.
    pub tip: Vec<TipLine>,
    /// An extra meter under the vendor's own rows (Grok Bot). It is not one of the vendor's
    /// [`Kind`] windows: clicking it opens its own detail pane ([`build_bot_detail`]) charting its
    /// own history, and the UI tracks it separately from `expanded`.
    pub secondary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VendorView {
    pub id: VendorId,
    pub name: String,
    pub rows: Vec<Row>,
    /// Shown right after the vendor name when a window would run out before it resets.
    pub warn: Option<HeaderWarn>,
    /// Right-aligned amber chip when the reading shown is older than its source's threshold.
    pub age_chip: Option<AgeChip>,
}

/// Sources that read a file on this PC (an app or CLI has to write it).
const LOCAL_SOURCES: [&str; 4] = [
    "desktop-cache",
    "desktop-history",
    "claude-json",
    "grok-log",
];
/// Sources that ask a service over the network.
const NETWORK_SOURCES: [&str; 2] = ["oauth", "grok-billing"];

/// A reading from a local-file source older than this (minutes) gets the data-age chip.
pub const CHIP_LOCAL_MIN: f64 = 15.0;
/// The Grok Bot app rewrites its cache only every 30 to 40 minutes while it is open (observed on a Mac
/// and a PC), so its chip waits for the same 60 minutes as the dimming.
pub const CHIP_BOT_MIN: f64 = 60.0;
/// A network source gets it when older than this many poll intervals ...
pub const CHIP_NETWORK_INTERVALS: f64 = 2.0;
/// ... but never before this many minutes.
pub const CHIP_NETWORK_MIN: f64 = 10.0;
/// The row tooltip carries a warning line when the data is older than this (minutes).
pub const STALE_WARN_MIN: f64 = 60.0;

/// Minutes after which a reading from `source` counts as old enough for the chip (`None` for a
/// source we know nothing about).
pub fn chip_threshold_min(source: &str, interval_min: f64) -> Option<f64> {
    if LOCAL_SOURCES.contains(&source) {
        Some(CHIP_LOCAL_MIN)
    } else if NETWORK_SOURCES.contains(&source) {
        Some((CHIP_NETWORK_INTERVALS * interval_min).max(CHIP_NETWORK_MIN))
    } else {
        None
    }
}

/// The "data is old" chip in a vendor header: an amber triangle, the age and an info icon whose
/// tooltip says where the data comes from and what to do.
#[derive(Debug, Clone, PartialEq)]
pub struct AgeChip {
    /// `about 38 minutes ago`.
    pub long: String,
    /// `38 min`.
    pub compact: String,
    /// Two short sentences (what the data is, what to do), with the age.
    pub tooltip: String,
    pub age_min: f64,
}

/// `about 38 minutes ago` / `about 2 hours ago` / `about 3 days ago`.
pub fn age_long(age_min: f64) -> String {
    let m = js_round(age_min).max(1.0) as i64;
    let (n, unit) = if m < 60 {
        (m, "minute")
    } else if m < 48 * 60 {
        (js_round(age_min / 60.0) as i64, "hour")
    } else {
        (js_round(age_min / 1440.0) as i64, "day")
    };
    format!("about {n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// `38 min` / `2 h` / `3 d`.
pub fn age_compact(age_min: f64) -> String {
    let m = js_round(age_min).max(1.0) as i64;
    if m < 60 {
        format!("{m} min")
    } else if m < 48 * 60 {
        format!("{} h", js_round(age_min / 60.0) as i64)
    } else {
        format!("{} d", js_round(age_min / 1440.0) as i64)
    }
}

/// `about 38 min` / `about 2 h` (for the sentence in the tooltip warning).
fn age_short(age_min: f64) -> String {
    format!("about {}", age_compact(age_min))
}

fn age_of(v: &Vendor, now: DateTime<Utc>) -> Option<f64> {
    Some((now - v.observed_at?).num_milliseconds() as f64 / 60000.0)
}

/// The chip for `v`, when its reading is current but older than its source's threshold. Never for
/// "No current data" (dashes have their own status), placeholders or unknown sources.
pub fn age_chip(v: &Vendor, interval_min: f64, now: DateTime<Utc>) -> Option<AgeChip> {
    if !v.current || v.placeholder {
        return None;
    }
    let src = v.source.as_deref()?;
    let age = age_of(v, now)?;
    if age <= chip_threshold_min(src, interval_min)? {
        return None;
    }
    let long = age_long(age);
    let tooltip = match src {
        "grok-log" => format!(
            "Live billing failed, so this comes from the Grok CLI's local log, last updated {long}. Run a grok command or press refresh to update."
        ),
        "grok-billing" => format!(
            "Grok billing has not answered since {long}. Check your connection or press refresh."
        ),
        "oauth" => format!(
            "The usage service has not answered since {long}. Check your connection or press refresh."
        ),
        "claude-json" => format!(
            "Data comes from Claude Code's local settings file, last updated {long}. Run Claude Code or press refresh for it to update."
        ),
        _ => format!(
            "Data comes from the Claude desktop app's local cache, last updated {long}. Open the Claude app or press refresh for it to update."
        ),
    };
    Some(AgeChip {
        compact: age_compact(age),
        long,
        tooltip,
        age_min: age,
    })
}

/// Is the reading shown older than [`STALE_WARN_MIN`]? An old reading keeps its plain value but
/// gets no projection ("Empty in ...", pace, "At reset"): the pace of an old sample says nothing
/// about now.
pub fn is_old(v: &Vendor, now: DateTime<Utc>) -> bool {
    age_of(v, now).is_some_and(|a| a > STALE_WARN_MIN)
}

/// The amber line at the end of a row tooltip when the data is older than [`STALE_WARN_MIN`].
/// The age is that of the sample itself (`observed_at`), not of the last poll.
pub fn stale_warning(v: &Vendor, now: DateTime<Utc>) -> Option<String> {
    if !v.current {
        return None;
    }
    let age = age_of(v, now)?;
    if age <= STALE_WARN_MIN {
        return None;
    }
    let how = match v.source.as_deref()? {
        "grok-log" => {
            "Grok figures come from the CLI's local log, which only updates when you use the CLI."
        }
        "grok-billing" => "Grok billing has not answered recently.",
        "oauth" => "the usage service has not answered recently.",
        "claude-json" => "Claude Code has not refreshed its saved usage.",
        "desktop-cache" | "desktop-history" => "the Claude app has not refreshed its usage cache.",
        _ => return None,
    };
    Some(format!("Data is {} old: {how}", age_short(age)))
}

/// The warning in the vendor header line (the window draws the warning triangle itself).
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderWarn {
    /// The window that would run out first.
    pub kind: Kind,
    /// `Empty in ~38m · resets in 2h 59m` (weekly: `Weekly: empty in ~2d 3h · resets in 5d`).
    pub full: String,
    /// Same without the reset part, for narrow windows.
    pub compact: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeView {
    pub empty: Option<Empty>,
    pub vendors: Vec<VendorView>,
    pub status: StatusLine,
}

/// The pane that opens under a vendor block when one of its rows is clicked.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub title: String,
    pub vendor: VendorId,
    pub level: Level,
    /// Big number, e.g. `42`.
    pub value: String,
    /// `%` / `% left`.
    pub unit: &'static str,
    /// `Resets in 3.2h`.
    pub resets: Option<String>,
    pub note: Option<Note>,
    /// Label/value rows (pace, projection, window statistics).
    pub rows: Vec<(String, String)>,
    /// Plan window whose history line can be charted.
    pub chart: Option<Kind>,
    /// The pane is the Grok Bot weekly meter: its chart reads the Grok Bot's own history
    /// (`usage-grok-bot.jsonl`), not the vendor's.
    pub bot: bool,
}

fn row_label(k: Kind) -> &'static str {
    match k {
        Kind::Session => "Session",
        Kind::Weekly => "Weekly",
    }
}

/// `2h 14m`, `45m`, `3d 4h`: the longer form used in tooltips.
pub fn left_long(hours: f64) -> String {
    let mins = js_round(hours.max(0.0) * 60.0) as i64;
    if mins < 60 {
        format!("{}m", mins.max(1))
    } else if mins < 24 * 60 {
        let (h, m) = (mins / 60, mins % 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        let (d, h) = (mins / 1440, (mins % 1440) / 60);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d {h}h")
        }
    }
}

/// Wall-clock text for a reset: `3:42 PM`, or `Sat 3:42 PM` when it is a day or more away.
pub fn clock_text<Tz: chrono::TimeZone>(t: DateTime<Tz>, days_away: bool) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if days_away {
        t.format("%a %-I:%M %p").to_string()
    } else {
        t.format("%-I:%M %p").to_string()
    }
}

fn reset_line(g: &Gauge, label: &str) -> String {
    let Some(h) = g.hours_left else {
        return format!("{label} reset time unknown");
    };
    let clock = g
        .resets_at
        .as_deref()
        .and_then(parse_ts)
        .map(|t| format!(" ({})", clock_text(t.with_timezone(&Local), h >= 20.0)))
        .unwrap_or_default();
    if g.resets_carried {
        // not reported with this sample: carried forward from an earlier one of the same window
        return format!(
            "{label} resets in ~{} (carried from earlier sample)",
            left_long(h)
        );
    }
    format!("{label} resets in {}{clock}", left_long(h))
}

fn pct_text_for(pct: f64, remaining: bool) -> String {
    if remaining {
        format!("{}% left", js_round((100.0 - pct).max(0.0)) as i64)
    } else {
        format!("{}%", js_round(pct) as i64)
    }
}

/// When the pace would empty the window before it resets: `(hours until empty, hours until reset,
/// already at the limit)`.
fn empty_info(g: &Gauge) -> Option<(f64, f64, bool)> {
    let e = crate::analysis::empty_before_reset(g)?;
    Some((e.empty_in_h, e.resets_in_h, e.at_limit))
}

/// `Empty in ~1h 22m · resets in 3h 20m`, only when the pace would empty the window before it resets.
pub fn empty_warning(g: &Gauge) -> Option<String> {
    let (e, left, at_limit) = empty_info(g)?;
    Some(if at_limit {
        format!("At the limit \u{b7} resets in {}", left_long(left))
    } else {
        format!(
            "Empty in ~{} \u{b7} resets in {}",
            left_long(e),
            left_long(left)
        )
    })
}

/// The header warning of a vendor: the most urgent window that would run out before its reset.
/// The weekly window is named (`Weekly: empty in ~2d 3h`); the session is the default.
pub fn header_warning(v: &Vendor) -> Option<HeaderWarn> {
    Kind::ALL
        .iter()
        .filter_map(|k| {
            let (e, left, at_limit) = empty_info(v.gauge(*k)?)?;
            Some((*k, e, left, at_limit))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(kind, e, left, at_limit)| {
            let head = if at_limit {
                "at the limit".to_string()
            } else {
                format!("empty in ~{}", left_long(e))
            };
            let (head, tail) = (head, format!(" \u{b7} resets in {}", left_long(left)));
            // a vendor with only one window needs no prefix
            let compact = match kind {
                Kind::Weekly if !v.weekly_only => format!("Weekly: {head}"),
                _ => capitalise(&head),
            };
            HeaderWarn {
                kind,
                full: format!("{compact}{tail}"),
                compact,
            }
        })
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

fn row_for(v: &Vendor, kind: Kind, remaining: bool, now: DateTime<Utc>) -> Row {
    let label = row_label(kind);
    let Some(g) = v.gauge(kind) else {
        // tracked but nothing yet (Grok without a login / log): dashes and the reason
        let tip_text = match (&v.error, v.placeholder) {
            (Some(e), true) => format!("{label}: no current data. {e}"),
            (None, true) => format!("{label}: no reading yet"),
            _ => format!("{label} not reported"),
        };
        return Row {
            kind,
            label,
            value: "--".into(),
            cells: [Cell::Off; SEGMENTS],
            level: Level::Normal,
            stale: v.placeholder,
            dim: false,
            tip: vec![tip(
                tip_text,
                if v.placeholder {
                    Tone::Warn
                } else {
                    Tone::Muted
                },
            )],
            secondary: false,
        };
    };
    if g.reset_passed && v.current {
        // The reset time this reading reported has passed: its percentage belongs to the window
        // that just ended. No countdown, no clock time in the past, no pace; wait for a new reading.
        let last = v
            .observed_at
            .map(|t| {
                format!(
                    " (last reading {}: {}%)",
                    ago((now - t).num_milliseconds() as f64 / 60000.0),
                    js_round(g.pct) as i64
                )
            })
            .unwrap_or_default();
        return Row {
            kind,
            label,
            value: "--".into(),
            cells: [Cell::Off; SEGMENTS],
            level: Level::Normal,
            stale: true,
            dim: false,
            tip: vec![tip(
                format!("{label}: its reset time has passed, waiting for a new reading{last}"),
                Tone::Warn,
            )],
            secondary: false,
        };
    }
    if !v.current {
        // An old reading must not pass as the present (and never as a lit 0%).
        let age = v
            .observed_at
            .map(|t| {
                format!(
                    " (last reading {}: {}%)",
                    ago((now - t).num_milliseconds() as f64 / 60000.0),
                    js_round(g.pct) as i64
                )
            })
            .unwrap_or_default();
        let mut tipv = vec![tip(format!("{label}: no current data{age}"), Tone::Warn)];
        if let Some(e) = &v.error {
            tipv.push(tip(e.clone(), Tone::Muted));
        }
        return Row {
            kind,
            label,
            value: "--".into(),
            cells: [Cell::Off; SEGMENTS],
            level: Level::Normal,
            stale: true,
            dim: false,
            tip: tipv,
            secondary: false,
        };
    }
    let old = is_old(v, now);
    let mut t = vec![tip(reset_line(g, label), Tone::Normal)];
    if let Some(n) = pace_note(g).filter(|_| !old) {
        t.push(tip(n.text, if n.warn { Tone::Warn } else { Tone::Muted }));
    }
    if let (Some(r), Some(m), false) = (g.rate_per_hour, g.rate_method, old) {
        let how = match m {
            crate::analysis::PaceMethod::Recent => "recent",
            crate::analysis::PaceMethod::Average => "avg since window start",
        };
        t.push(tip(format!("Pace: {r:+.1} pts/h ({how})"), Tone::Muted));
    }
    if let Some(tier) = &v.tier {
        t.push(tip(format!("Plan: {tier}"), Tone::Muted));
    }
    if v.source.as_deref() == Some("grok-billing") {
        t.push(tip("Live from Grok billing", Tone::Muted));
    } else if let Some(label) = v.source.as_deref().and_then(source_label) {
        let age = v
            .observed_at
            .map(|t| {
                format!(
                    ", sample {}",
                    ago((now - t).num_milliseconds() as f64 / 60000.0)
                )
            })
            .unwrap_or_default();
        t.push(tip(format!("From {label}{age}"), Tone::Muted));
    }
    if kind == Kind::Weekly {
        // a model whose own reset time has passed belongs to the window that just ended
        let models: Vec<&crate::reading::ModelLimit> = v
            .models
            .iter()
            .filter(|m| {
                m.resets_at
                    .as_deref()
                    .and_then(parse_ts)
                    .is_none_or(|r| r > now)
            })
            .collect();
        if !models.is_empty() {
            t.push(tip("By model", Tone::Muted));
            for m in models {
                let reset = resets_in(m.resets_at.as_deref(), now)
                    .map(|r| format!(" \u{b7} resets in {r}"))
                    .unwrap_or_default();
                t.push(tip(
                    format!("{}  {}{reset}", m.name, pct_text_for(m.pct, remaining)),
                    Tone::Normal,
                ));
            }
        }
        if let Some(c) = &v.credits {
            let text = match c.limit {
                Some(l) => format!("Credits  ${:.2} of ${l:.2}", c.used),
                None => format!("Credits  ${:.2}", c.used),
            };
            t.push(tip(text, Tone::Normal));
        }
    }
    if let Some(w) = stale_warning(v, now) {
        t.push(tip(w, Tone::Caution));
    }
    Row {
        kind,
        label,
        value: pct_text_for(g.pct, remaining),
        cells: bar_cells(g.pct, g.projected_pct.filter(|_| !old), remaining),
        level: level(g.pct),
        stale: false,
        dim: old,
        tip: t,
        secondary: false,
    }
}

/// Label of the Grok Bot row.
pub const BOT_LABEL: &str = "Grok Bot";

/// The Grok Bot app's weekly meter as a second bar. The reading is a cached file the app writes
/// only while it is open, so it follows the staleness rules: kept however old, dimmed after
/// [`STALE_WARN_MIN`] with a caution line, no pace or projection, and dashes only once its weekly
/// period has ended.
pub fn bot_row(b: &crate::grok_bot::BotReading, remaining: bool, now: DateTime<Utc>) -> Row {
    let age = b.age_min(now);
    let from = "Read from the Grok Bot app's cache; the app rewrites it only every half hour or so, and only while it is open";
    if b.period_ended(now) {
        return Row {
            kind: Kind::Weekly,
            label: BOT_LABEL,
            value: "--".into(),
            cells: [Cell::Off; SEGMENTS],
            level: Level::Normal,
            stale: true,
            dim: false,
            tip: vec![
                tip(
                    format!(
                        "Grok Bot: its weekly period has ended, waiting for a new reading (last reading {}: {}%)",
                        ago(age),
                        js_round(b.pct) as i64
                    ),
                    Tone::Warn,
                ),
                tip(from, Tone::Muted),
            ],
            secondary: true,
        };
    }
    let hours = (b.resets_at - now).num_milliseconds() as f64 / 3.6e6;
    let clock = clock_text(b.resets_at.with_timezone(&Local), hours >= 20.0);
    let stale = b.stale(now);
    let mut t = vec![
        tip(
            format!("Grok Bot resets in {} ({clock})", left_long(hours)),
            Tone::Normal,
        ),
        tip(format!("Reading {}", ago(age)), Tone::Muted),
        tip(from, Tone::Muted),
    ];
    if stale {
        t.push(tip(
            format!(
                "This reading is {} old: open the Grok Bot app to update it",
                age_short(age).trim_start_matches("about ")
            ),
            Tone::Caution,
        ));
    }
    Row {
        kind: Kind::Weekly,
        label: BOT_LABEL,
        value: pct_text_for(b.pct, remaining),
        cells: bar_cells(b.pct, None, remaining),
        level: level(b.pct),
        stale: false,
        dim: stale,
        tip: t,
        secondary: true,
    }
}

/// The header chip for a Grok block whose own reading is fresh but whose Grok Bot reading is old
/// (the vendor chip, when there is one, speaks for the main reading).
fn bot_chip(v: &Vendor, now: DateTime<Utc>) -> Option<AgeChip> {
    let b = v.bot.as_ref().filter(|b| !b.period_ended(now))?;
    let age = b.age_min(now);
    if age <= CHIP_BOT_MIN {
        return None;
    }
    let long = age_long(age);
    Some(AgeChip {
        compact: age_compact(age),
        tooltip: format!(
            "The Grok Bot bar comes from the Grok Bot app's local cache, last updated {long}. The app rewrites it only every half hour or so, and only while it is open."
        ),
        long,
        age_min: age,
    })
}

/// Main view for the vendors that have data (today: Claude).
pub fn build_home(s: &State, now: DateTime<Utc>) -> HomeView {
    build_home_for(s, &crate::vendor::from_state_at(s, now), now)
}

/// Like [`build_home`] but for an explicit vendor list (mock data, future providers).
pub fn build_home_for(s: &State, vendors: &[Vendor], now: DateTime<Utc>) -> HomeView {
    let remaining = s.settings.remaining();
    let mut v = HomeView {
        empty: None,
        vendors: vec![],
        status: status_line(s, now),
    };
    if vendors.is_empty() {
        v.empty = Some(if s.nothing_to_track() {
            Empty {
                title: "Nothing to track yet".into(),
                body: "aiTrack reads usage from the Claude desktop app or Claude Code, and from the Grok CLI. Install and sign in to one of them, then press refresh.".into(),
                help: true,
            }
        } else if s.loading {
            Empty {
                title: "Loading history".into(),
                body: "Reading saved usage...".into(),
                help: false,
            }
        } else if let Some(e) = &s.collector.error {
            Empty {
                title: "No reading yet".into(),
                body: e.clone(),
                help: false,
            }
        } else {
            Empty {
                title: "Taking first reading...".into(),
                body: "This takes a few seconds.".into(),
                help: false,
            }
        });
        return v;
    }
    for ven in vendors.iter().filter(|x| x.has_data()) {
        v.vendors.push(VendorView {
            id: ven.id,
            name: ven.name.clone(),
            rows: ven
                .kinds()
                .iter()
                .map(|k| row_for(ven, *k, remaining, now))
                .chain(ven.bot.as_ref().map(|b| bot_row(b, remaining, now)))
                .collect(),
            warn: header_warning(ven).filter(|_| ven.current && !is_old(ven, now)),
            age_chip: age_chip(ven, s.settings.interval_min, now).or_else(|| bot_chip(ven, now)),
        });
    }
    v
}

fn resets_in(resets_at: Option<&str>, now: DateTime<Utc>) -> Option<String> {
    let r = parse_ts(resets_at?)?;
    Some(left((r - now).num_milliseconds() as f64 / 3.6e6))
}

fn opt(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map(f).unwrap_or_else(|| "-".into())
}

/// Detail pane for one row of a vendor.
pub fn build_detail(v: &Vendor, kind: Kind, remaining: bool) -> Detail {
    build_detail_at(v, kind, remaining, Utc::now())
}

/// [`build_detail`] at an explicit time (an old reading gets no pace or projection).
pub fn build_detail_at(v: &Vendor, kind: Kind, remaining: bool, now: DateTime<Utc>) -> Detail {
    let title = match kind {
        Kind::Session => "5-hour session",
        Kind::Weekly => "7-day weekly",
    };
    let mut d = Detail {
        title: title.into(),
        vendor: v.id,
        level: Level::Normal,
        value: "--".into(),
        unit: "",
        resets: None,
        note: None,
        rows: vec![],
        chart: v.history.then_some(kind),
        bot: false,
    };
    if let (Some(_), false) = (v.gauge(kind), v.current) {
        d.resets = Some("No current data".into());
    } else if v.gauge(kind).is_some_and(|g| g.reset_passed) {
        d.resets = Some("Reset passed, waiting for a new reading".into());
    } else if let Some(g) = v.gauge(kind) {
        let shown = if remaining {
            (100.0 - g.pct).max(0.0)
        } else {
            g.pct
        };
        d.level = level(g.pct);
        d.value = format!("{}", js_round(shown) as i64);
        d.unit = if remaining { "% left" } else { "%" };
        d.resets = Some(match g.hours_left {
            Some(h) => format!("Resets in {}", left(h)),
            None => "Reset time unknown".into(),
        });
        if !is_old(v, now) {
            d.note = pace_note(g);
            d.rows.push((
                "Pace".into(),
                opt(g.rate_per_hour, |r| format!("{r:+.1} pts/h")),
            ));
            d.rows.push((
                "At reset".into(),
                opt(g.projected_pct, |x| format!("{}%", js_round(x) as i64)),
            ));
            d.rows
                .push(("To limit".into(), opt(g.hours_to_limit, left)));
        }
    }
    if let Some(st) = v.stats(kind) {
        d.rows.push(("Windows".into(), st.windows.to_string()));
        d.rows
            .push(("Avg peak".into(), opt(st.avg_peak, |x| format!("{x:.0}%"))));
        d.rows
            .push(("Max peak".into(), opt(st.max_peak, |x| format!("{x:.0}%"))));
        d.rows.push((
            "Hit rate".into(),
            st.hit_rate
                .map(|h| format!("{h}%"))
                .unwrap_or_else(|| "-".into()),
        ));
    }
    d
}

/// Detail pane for the Grok Bot weekly meter. Same layout as [`build_detail_at`]; the reading is a
/// cached file, so it follows the staleness rules: kept however old, a caution note after
/// [`STALE_WARN_MIN`], no pace or projection, dashes once its weekly period has ended. The chart
/// is always offered (it says "No history yet" when nothing is stored).
pub fn build_bot_detail(
    b: &crate::grok_bot::BotReading,
    remaining: bool,
    now: DateTime<Utc>,
) -> Detail {
    let age = b.age_min(now);
    let mut d = Detail {
        title: "Grok Bot weekly".into(),
        vendor: VendorId::Grok,
        level: Level::Normal,
        value: "--".into(),
        unit: "",
        resets: None,
        note: None,
        rows: vec![("Reading".into(), ago(age))],
        chart: Some(Kind::Weekly),
        bot: true,
    };
    if b.period_ended(now) {
        d.resets = Some("Reset passed, waiting for a new reading".into());
        d.rows
            .push(("Last value".into(), format!("{}%", js_round(b.pct) as i64)));
        return d;
    }
    let shown = if remaining {
        (100.0 - b.pct).max(0.0)
    } else {
        b.pct
    };
    let hours = (b.resets_at - now).num_milliseconds() as f64 / 3.6e6;
    d.level = level(b.pct);
    d.value = format!("{}", js_round(shown) as i64);
    d.unit = if remaining { "% left" } else { "%" };
    d.resets = Some(format!("Resets in {}", left(hours)));
    if b.stale(now) {
        d.note = Some(Note {
            text: format!(
                "Reading is {} old: open the Grok Bot app to update it",
                age_short(age).trim_start_matches("about ")
            ),
            warn: false,
        });
    }
    d
}

/// A sample from the desktop app / `~/.claude.json` older than this is called out in the status line.
const SNAPSHOT_STALE_MIN: f64 = 20.0;

/// Human name of a usage source (`None` for the live endpoint, which needs no remark).
pub fn source_label(source: &str) -> Option<&'static str> {
    match source {
        "desktop-cache" | "desktop-history" => Some("Claude desktop app"),
        "grok-log" => Some("Grok CLI log"),
        "claude-json" => Some("~/.claude.json cache"),
        _ => None,
    }
}

pub fn status_line(s: &State, now: DateTime<Utc>) -> StatusLine {
    // Show Claude = Off: the line speaks about what is shown (Grok), not about hidden Claude data
    if !s.claude_on {
        if let Some(g) = s.grok.as_ref().filter(|g| g.enabled) {
            let mut grok_view = s.clone();
            grok_view.plan = g.plan.clone();
            grok_view.collector = g.collector.clone();
            grok_view.claude_on = true;
            return status_line_of(&grok_view, now, true);
        }
    }
    status_line_of(s, now, false)
}

/// The line for the plan and collector of `s`; `grok` says they are Grok's (its last value stays
/// shown however old, until its billing period ends).
fn status_line_of(s: &State, now: DateTime<Utc>, grok: bool) -> StatusLine {
    if s.nothing_to_track() {
        return StatusLine {
            dot: Dot::Stale,
            text: "No supported tools found".into(),
            full: Some(format!(
                "Neither the Claude desktop app / Claude Code nor the Grok CLI was found on this {}.",
                crate::util::this_machine()
            )),
        };
    }
    let c = &s.collector;
    let plan = s.plan.as_ref();
    let source = c
        .sample
        .as_ref()
        .map(|i| i.source.as_str())
        .or_else(|| plan.and_then(|p| p.last_source.as_deref()));
    let snapshot = source.is_some_and(|x| x != "oauth" && x != "grok-billing");
    let observed = c
        .sample
        .as_ref()
        .and_then(|i| i.observed_at)
        .or_else(|| plan.and_then(|p| parse_ts(&p.last)));
    let age_min = observed.map(|t| (now - t).num_milliseconds() as f64 / 60000.0);

    let current = plan.is_some_and(|p| {
        if grok {
            crate::state::grok_reading_is_shown(p, now)
        } else {
            crate::state::reading_is_current(p, &s.settings, now)
        }
    });
    // No current reading at all (everything failed, or the newest one is too old): say so plainly,
    // never leave an old number to pass as the present.
    if (plan.is_some() && !current) || (plan.is_none() && c.error.is_some()) {
        let reason = match (&c.error, age_min) {
            (Some(e), _) => short_reason(e),
            (None, Some(m)) => format!("last reading {}", ago(m)),
            (None, None) => "no readings".into(),
        };
        let full = c
            .error
            .clone()
            .or_else(|| age_min.map(|m| format!("The newest reading is {}.", ago(m))));
        return StatusLine {
            dot: if c.error.is_some() {
                Dot::Err
            } else {
                Dot::Stale
            },
            text: format!("No current data \u{b7} {reason}"),
            full,
        };
    }

    let stale = if snapshot {
        age_min.is_some_and(|m| m >= SNAPSHOT_STALE_MIN)
    } else {
        plan.is_some_and(|p| p.stale_minutes as f64 > s.settings.interval_min * 2.5 + 2.0)
    };
    let dot = if c.error.is_some() {
        Dot::Err
    } else if stale {
        Dot::Stale
    } else {
        Dot::Ok
    };
    let app_closed =
        stale && source.is_some_and(|x| x.starts_with("desktop-")) && c.error.is_none();
    let mut text = match plan {
        Some(p) if snapshot => {
            let m = age_min.unwrap_or(p.stale_minutes as f64);
            let hint = if app_closed {
                " (app not running?)"
            } else {
                ""
            };
            format!("Last sample {}{hint}", ago(m))
        }
        Some(p) => format!("Updated {}", ago(age_min.unwrap_or(p.stale_minutes as f64))),
        None => String::new(),
    };
    // current data, but the latest poll failed: keep the number, add the short reason
    if let Some(e) = &c.error {
        text = format!("{text} \u{b7} {}", short_reason(e));
        return StatusLine {
            dot,
            text,
            full: Some(e.clone()),
        };
    }
    // (the hint already fills the narrow status bar, so the countdown is left out then)
    if c.live && snapshot && !app_closed {
        // the local files are re-read every minute or so: a countdown would mean nothing
        text = if text.is_empty() {
            "live".into()
        } else {
            format!("{text} \u{b7} live")
        };
    } else if let Some(n) = c.next_in.filter(|_| !app_closed) {
        let next = if n < 90.0 {
            format!("next in {}s", n.round() as i64)
        } else {
            format!("next in {} min", js_round(n / 60.0) as i64)
        };
        if text.is_empty() {
            text = next;
        } else {
            text = format!("{text} \u{b7} {next}");
        }
    }
    StatusLine {
        dot,
        text,
        full: None,
    }
}

/// A short human summary of a (possibly very long, multi-source) error message.
pub fn short_reason(err: &str) -> String {
    let e = err.to_ascii_lowercase();
    let has = |p: &str| e.contains(p);
    let r = if has("open the claude app") {
        "Claude app closed?"
    } else if has("429") {
        "rate limited"
    } else if has("login rejected") {
        "login rejected"
    } else if has("token expired") {
        "login expired"
    } else if has("network error") {
        "offline?"
    } else if has("no claude login")
        && (has("not found") || has("no usable") || has("no usage entry"))
    {
        "no Claude data found"
    } else if has("no claude login") {
        "no Claude login"
    } else if has("no usage entry") || has("not found") {
        "no Claude app data"
    } else {
        let first = err.split(['(', ';', ':']).next().unwrap_or(err).trim();
        return if first.chars().count() > 36 {
            format!("{}\u{2026}", first.chars().take(35).collect::<String>())
        } else {
            first.to_string()
        };
    };
    r.to_string()
}

// ---------------------------------------------------------------- settings view

#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub minutes: f64,
    pub label: &'static str,
    pub selected: bool,
}

pub fn presets(interval_min: f64) -> Vec<Preset> {
    PRESETS
        .iter()
        .map(|&(m, label)| Preset {
            minutes: m,
            label,
            selected: interval_min == m,
        })
        .collect()
}

/// Warn under the presets when polling every minute.
pub fn fast_warning(interval_min: f64) -> bool {
    interval_min <= 1.0
}

/// Local-time text for a stored timestamp (falls back to the raw text).
pub fn local_time(ts: &str) -> String {
    match parse_ts(ts) {
        Some(t) => t
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => ts.to_owned(),
    }
}

/// One diagnostics line as shown on screen.
pub fn diag_line(d: &Diag) -> String {
    format!(
        "{}  {} {}",
        local_time(&d.ts),
        if d.ok { "\u{2713}" } else { "\u{2717}" },
        d.text
    )
}

/// The text "Copy log" puts on the clipboard.
pub fn copy_log(diags: &[Diag]) -> String {
    std::iter::once(crate::buildinfo::line())
        .chain(
            diags
                .iter()
                .map(|d| format!("{} {} {}", d.ts, if d.ok { "ok" } else { "FAIL" }, d.text)),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- history chart

/// Time range of the history chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Month,
    All,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Day, Range::Week, Range::Month, Range::All];

    pub fn label(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::All => "All",
        }
    }

    pub fn hours(self) -> Option<f64> {
        match self {
            Range::Day => Some(24.0),
            Range::Week => Some(168.0),
            Range::Month => Some(720.0),
            Range::All => None,
        }
    }
}

/// Percentages over time for one plan window, split into one segment per limit window so the
/// line does not draw a diagonal across a reset.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChartData {
    /// `(unix seconds, pct)` points per segment, oldest first.
    pub segments: Vec<Vec<(f64, f64)>>,
    pub x0: f64,
    pub x1: f64,
    /// Top of the y axis in percent (see [`chart_y_max`]); `0` (no data) means 100.
    pub y_max: f64,
}

/// The range the history chart opens with when the user has not picked one: the shortest of
/// 24h / 7d / 30d / All that covers everything stored for `kind` (the span from the first stored
/// reading to `now`). Without data: 24h.
pub fn default_range(
    readings: &[crate::reading::Reading],
    kind: Kind,
    now: DateTime<Utc>,
) -> Range {
    let first = readings
        .iter()
        .filter(|r| r.ok && r.bucket(kind).is_some())
        .filter_map(|r| r.t)
        .min();
    let Some(first) = first else {
        return Range::Day;
    };
    let hours = (now - first).num_seconds() as f64 / 3600.0;
    [Range::Day, Range::Week, Range::Month]
        .into_iter()
        .find(|r| r.hours().is_some_and(|h| hours < h))
        .unwrap_or(Range::All)
}

/// At most this many points are kept (evenly thinned) for drawing.
pub const CHART_MAX_POINTS: usize = 3000;

/// A line is cut where two neighbouring samples are further apart than this (the app was closed or
/// the PC asleep). Ordinary spacing (5-15 minutes, uneven) stays one connected line.
pub const CHART_GAP_SECS: f64 = 2.0 * 3600.0;

/// Data whose peak is at most this many percent gets a zoomed y axis.
pub const CHART_ZOOM_BELOW: f64 = 25.0;

/// Top of the y axis for data peaking at `peak` percent: 100 when the data is high, otherwise the
/// smallest of 10 / 20 / 25 / 50 that leaves a quarter of headroom above the peak.
pub fn chart_y_max(peak: f64) -> f64 {
    if peak > CHART_ZOOM_BELOW {
        return 100.0;
    }
    [10.0, 20.0, 25.0, 50.0]
        .into_iter()
        .find(|s| *s >= peak.max(0.0) * 1.25)
        .unwrap_or(100.0)
}

pub fn chart_data(
    readings: &[crate::reading::Reading],
    kind: Kind,
    range: Range,
    now: DateTime<Utc>,
) -> ChartData {
    let now_s = now.timestamp() as f64;
    let from = range.hours().map(|h| now_s - h * 3600.0);
    let mut pts: Vec<(f64, f64, Option<DateTime<Utc>>)> = readings
        .iter()
        .filter(|r| r.ok)
        .filter_map(|r| {
            let (t, b) = (r.t?, r.bucket(kind)?);
            let reset = b.resets_at.as_deref().and_then(parse_ts);
            Some((t.timestamp() as f64, b.pct, reset))
        })
        .filter(|(t, _, _)| from.is_none_or(|f| *t >= f))
        .filter(|(t, _, _)| *t <= now_s + (crate::analysis::MAX_FUTURE_SKEW_MIN * 60) as f64)
        .collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    // the same window rule as the analysis: samples with and without a reset time (cache and
    // history) of one window are one window, so they stay one line
    let window_points: Vec<crate::analysis::WindowPoint> = pts
        .iter()
        .map(|(t, pct, reset)| {
            (
                DateTime::from_timestamp(*t as i64, 0).unwrap_or(now),
                *pct,
                *reset,
            )
        })
        .collect();
    let keys = crate::analysis::window_keys(&window_points, kind);
    let peak = pts.iter().map(|p| p.1).fold(0.0_f64, f64::max);
    let keyed: Vec<(f64, f64, crate::analysis::WindowKey)> = pts
        .iter()
        .zip(keys)
        .map(|((t, pct, _), k)| (*t, *pct, k))
        .collect();
    let stride = keyed.len().div_ceil(CHART_MAX_POINTS).max(1);
    let last = keyed.last().copied();
    let mut thinned: Vec<_> = keyed.iter().copied().step_by(stride).collect();
    if let (Some(l), Some(t)) = (last, thinned.last()) {
        if t.0 != l.0 {
            thinned.push(l);
        }
    }
    let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut prev: Option<(f64, crate::analysis::WindowKey)> = None;
    for (t, pct, key) in thinned.iter() {
        let cut = prev.is_none_or(|(pt, pk)| pk != *key || *t - pt > CHART_GAP_SECS);
        match segments.last_mut() {
            Some(seg) if !cut => seg.push((*t, *pct)),
            _ => segments.push(vec![(*t, *pct)]),
        }
        prev = Some((*t, *key));
    }
    let x0 = from.unwrap_or_else(|| thinned.first().map(|p| p.0).unwrap_or(now_s - 3600.0));
    ChartData {
        segments,
        x0,
        x1: now_s.max(x0 + 1.0),
        y_max: if thinned.is_empty() {
            100.0
        } else {
            chart_y_max(peak)
        },
    }
}

// ---------------------------------------------------------------- window height

/// Shortest the auto-fitted window gets (logical points).
pub const MIN_WINDOW_H: f32 = 120.0;
/// Longest the Settings / Stats views auto-fit to (logical points), whatever the monitor.
pub const SCROLL_VIEW_MAX_PT: f32 = 640.0;
/// ... and never more than this share of the monitor's height.
pub const SCROLL_VIEW_MONITOR_SHARE: f32 = 0.6;
/// Smallest cap for the scrolling views (tiny monitors).
pub const SCROLL_VIEW_MIN_CAP: f32 = 240.0;

/// The tallest the window may auto-fit to, in logical points. `monitor_h` is the monitor's height in
/// logical points (physical pixels / scale factor), `None` when unknown.
///
/// The home view fits its content up to 85 % of the monitor. The Settings and Stats views (long,
/// scrolling) stop at `min(0.6 x monitor, 640 pt)`: the cap does not grow with a bigger monitor, so
/// on a 4K screen they stay a sensible size and the rest scrolls.
pub fn height_cap(scrolling_view: bool, monitor_h: Option<f32>) -> f32 {
    if scrolling_view {
        monitor_h
            .map_or(SCROLL_VIEW_MAX_PT, |m| {
                (m * SCROLL_VIEW_MONITOR_SHARE).min(SCROLL_VIEW_MAX_PT)
            })
            .max(SCROLL_VIEW_MIN_CAP)
    } else {
        monitor_h.map_or(720.0, |m| m * 0.85).max(300.0)
    }
}

// ---------------------------------------------------------------- window auto-fit decision

/// How long after a size request the window is judged (milliseconds).
pub const FIT_SETTLE_MS: u128 = 800;
/// Times a request the OS ignored outright is repeated before giving up (never marks "by hand").
pub const FIT_MAX_RETRIES: u8 = 3;

/// What the auto-fit remembers between frames. Heights are logical points of the inner (content) area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FitState {
    /// The last height requested.
    pub sent: Option<f32>,
    /// The window height when that request was made.
    pub baseline: f32,
    /// Milliseconds since the request, `None` before any.
    pub age_ms: Option<u128>,
    /// Requests repeated because the OS left the size alone.
    pub retries: u8,
    /// The user resized the window by hand: no more auto-fit for this view.
    pub user_sized: bool,
    /// The OS ignored every request: stop asking (not a user resize).
    pub gave_up: bool,
}

/// What to do this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FitAction {
    /// Nothing.
    Keep,
    /// Ask for this inner height.
    Request(f32),
    /// The window is at a size we did not ask for: treat it as the user's, stop fitting.
    UserSized,
    /// The OS ignored every request: stop asking.
    GiveUp,
}

/// The pure auto-fit decision. `desired` is the content height (panels included, already clamped to
/// the cap), `actual` the window's current inner height. A request is judged once it is older than
/// `FIT_SETTLE_MS`: a window still at its old height means the OS ignored it (macOS can drop or
/// defer a resize made before the window is shown), so it is repeated a few times and never counted
/// as a hand resize; a window at some other height was changed by the user.
pub fn fit_decide(st: &FitState, desired: f32, actual: f32) -> FitAction {
    if st.user_sized || st.gave_up {
        return FitAction::Keep;
    }
    if let (Some(sent), Some(age)) = (st.sent, st.age_ms) {
        if age > FIT_SETTLE_MS && (actual - sent).abs() > 3.0 {
            if (actual - st.baseline).abs() <= 3.0 {
                return if st.retries < FIT_MAX_RETRIES {
                    FitAction::Request(desired)
                } else {
                    FitAction::GiveUp
                };
            }
            return FitAction::UserSized;
        }
    }
    match st.sent {
        Some(h) if (h - desired).abs() <= 1.0 => FitAction::Keep,
        _ => FitAction::Request(desired),
    }
}

impl FitState {
    /// Record `action` (as returned by `fit_decide`); `actual` is the window height it was given.
    pub fn apply(&mut self, action: FitAction, actual: f32) {
        match action {
            FitAction::Keep => {}
            FitAction::UserSized => self.user_sized = true,
            FitAction::GiveUp => self.gave_up = true,
            FitAction::Request(h) => {
                if self.sent.is_some() && self.age_ms > Some(FIT_SETTLE_MS) {
                    self.retries += 1; // a repeat of an ignored request
                } else {
                    self.baseline = actual;
                    self.retries = 0;
                }
                self.sent = Some(h);
                self.age_ms = Some(0);
            }
        }
    }
}

/// The inner window height that fits the content, measured from the layout itself.
///
/// `window_h` is the current inner height, `panels_top` the y where the pinned bottom panels start
/// (`ctx.available_rect().bottom()` once they are laid out), `content_bottom` the y where the
/// content ends and `margin` the gap kept below it. The empty strip between content and panels is
/// `panels_top - (content_bottom + margin)`; removing it (or adding the missing part when it is
/// negative) gives the fit, whatever else (title bar, safe-area inset, panel frames) the platform
/// adds. On a platform where `window_h` is exactly content + panels this equals
/// `content_bottom + margin + panels_h`. Clamped to `[MIN_WINDOW_H, cap]`.
pub fn fit_height(
    window_h: f32,
    panels_top: f32,
    content_bottom: f32,
    margin: f32,
    min_h: f32,
    cap: f32,
) -> f32 {
    let slack = panels_top - (content_bottom + margin);
    (window_h - slack).clamp(min_h, cap.max(min_h))
}

/// Starting window (inner size, logical points) on Windows and Linux.
pub const DEFAULT_INNER_PT: (f32, f32) = (310.0, 240.0);
/// Starting window on macOS: the owner's chosen size, the home view with the 5-hour detail pane
/// open. Estimated from a Retina screenshot, unverified; retune here only. Not a minimum.
pub const DEFAULT_INNER_MACOS_PT: (f32, f32) = (376.0, 500.0);

/// The starting inner size (width, height) in points for the platform this is built for. A saved
/// window width wins over the width; the user may drag the window smaller.
pub fn default_inner_size_pt() -> (f32, f32) {
    if cfg!(target_os = "macos") {
        DEFAULT_INNER_MACOS_PT
    } else {
        DEFAULT_INNER_PT
    }
}
