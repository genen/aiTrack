// SPDX-License-Identifier: GPL-3.0-or-later
//! Long-term plan-usage statistics over the stored readings (pure functions, no UI).
//!
//! [`compute`] turns the readings of the last 7 / 30 / 90 days (or all of them) into
//!
//! * per window kind (5 h session, 7 d weekly): windows seen, peak per window, average / median /
//!   max peak, hit rate, windows near the limit and a trend ([`KindStats`]);
//! * early or unscheduled resets (plan changes, manual resets), kept out of the window averages
//!   ([`ResetEvent`]);
//! * a weekday x hour heat map of when usage happens ([`Heatmap`]);
//! * burn rates while active and a simple "limit risk" ([`BurnStats`], [`LimitRisk`]);
//! * how complete the data is ([`Coverage`]), so a UI can say how far to trust the numbers.
//!
//! Everything is deterministic: the caller passes `now` and the local UTC offset (the system
//! timezone is never consulted).
//!
//! # Windows
//!
//! Readings are grouped into limit windows exactly as [`crate::analysis`] does: by the
//! minute-truncated `resets_at` when present ([`window_key`]), otherwise a new window starts
//! whenever the percentage falls by [`INFERRED_DROP`] points or more. Only successful readings
//! (`ok`) with a parseable timestamp are used. Backfilled rows take part in everything; they are
//! only counted separately in [`Coverage`]. A window belongs to the range when its *last* reading
//! does, and its peak is taken over all of its readings, so a window straddling the start of the
//! range is not under-counted.
//!
//! # The window in progress
//!
//! The newest window of a kind is *in progress* when it was not cut short and either its scheduled
//! reset (`resets_at`) is after `now`, or, when the source reports no reset time, its last reading
//! is within a grace of `now`: twice the longest gap between two readings inside that window, at
//! least [`MIN_GRACE_MIN`] minutes. Only the window holding the latest reading of the kind can be
//! in progress. It is reported on its own ([`KindStats::current`], [`CurrentWindow`]) and is left
//! out of everything that describes *finished* windows: [`KindStats::windows`], the peak average /
//! median / max, the hit rate, the near-limit count, the trend, the limit risk and the
//! session-to-weekly ratio. (A window that has only just started sits at a few percent; averaging
//! it in would make every trend look like a collapse, and it has not had its chance to peak.) It
//! still appears in [`KindStats::peaks`] with `in_progress` set, for charts.
//!
//! # Reset events
//!
//! Between two consecutive readings of one kind, a fall of at least [`INFERRED_DROP`] points while
//! the earlier reading's `resets_at` is still more than [`EARLY_RESET_LEAD_MIN`] minutes after the
//! later reading is an [`ResetCause::Early`] reset. If both session and weekly fall by that much
//! to about zero ([`ZERO_PCT`]) at the same time, and neither had a scheduled reset due, it is a
//! [`ResetCause::Together`] reset (covers sources that report no `resets_at`). The window that
//! was cut short is flagged `ended_early` and left out of that kind's averages, hit rate and
//! trend.
//!
//! A cut-short window is not a normal window: it did not get to run its 5 h / 7 d, so its peak says
//! nothing about how a full window usually ends and would drag the averages down. But it is not
//! nothing either: if it reached 90 % before the reset, the limit *was* in reach. So such windows
//! are listed in [`KindStats::cut_short`] (start, end, peak, cause), [`ResetEvent::peak`] carries
//! the window's highest reading (not just the last one before the fall), and
//! [`KindStats::hit_before_reset`] (peak at least [`NEAR_LIMIT`]) and
//! [`KindStats::limit_before_reset`] (peak at least [`HIT`]) count them. These counters are always
//! reported next to, and never mixed into, the normal hit rate and near-limit count.
//!
//! # Session-to-weekly ratio
//!
//! [`Stats::session_to_weekly`] answers "how much of the weekly allowance does a full session cost?".
//! For every finished (complete, not cut short) session window with at least
//! [`RATIO_MIN_SESSION_PTS`] session points gained, the positive session gain and the weekly gain over
//! the same consecutive reading pairs (both buckets present, weekly not falling) are summed. The
//! ratio is `sum(weekly gain) / sum(session gain) * 100` weekly points per full (100-point)
//! session. Sums, not a median of per-window ratios, because percentages are whole numbers: a window
//! of `+12` session and `+2` weekly points is really anywhere between about 1 and 3 weekly points.
//! Spread: each window's two totals are known to within a point (rounding of the first and last
//! reading), so with `n` windows `low = max(0, W - n) / (S + n)` and `high = (W + n) / (S - n)`,
//! scaled the same way. It needs [`RATIO_MIN_SAMPLES`] windows *and* [`MIN_WEEKLY_POINTS`] weekly points
//! in total, and carries a [`Confidence`] from the points measured; until then
//! [`Stats::ratio_progress`] says how far along it is.
//!
//! *Plan eras.* A plan change (Pro to Max, say) resets both windows at the same moment and changes what
//! a session costs in weekly points, so the ratio is computed only from windows entirely inside the
//! most recent era, never across them. An era starts at every moment where both the session and the
//! weekly window had an early or together reset ([`Stats::era_start`]). *Same-instant pairs.* Only
//! reading pairs where one stored record carries both buckets are used, so session and weekly gains are
//! measured at the same instants. Records imported from a source's history (`backfill`) are sparse;
//! they count, but when less than half of the weekly points come from live readings the confidence is
//! capped at [`Confidence::Rough`].
//!
//! # Heat map
//!
//! For every pair of consecutive readings of one kind inside one window and at most
//! [`HEATMAP_MAX_GAP_HOURS`] apart, the positive gain is spread evenly over the interval and split
//! across the local-hour cells it covers; the interval's length goes into the cell's exposure
//! hours whatever the gain. A cell's value is gain / exposure, in percentage points per hour.
//! Longer gaps are skipped because the usage cannot be placed in time.
//!
//! # Burn rate and limit risk
//!
//! A burn sample is the gain / hours of a pair of readings in one window that are at most
//! [`ACTIVE_GAP_MIN`] minutes apart and gained something ("active"). The limit risk looks at
//! session windows: a window counts as *at risk* if it hit the limit or if its own active pace
//! (total active gain / total active hours, needing a quarter hour of activity) sustained for the
//! whole 5 h would reach 100 %. The risk is the share of such windows. It is a heuristic about
//! intensity, not a forecast.
//!
//! # Trend
//!
//! The average peak of the last `n` windows against the `n` before them (`n` = [`TREND_WINDOWS`]
//! for the session, [`TREND_WINDOWS_WEEKLY`] for the weekly window, or half of what exists if
//! that is fewer, at least 2). A change smaller than [`TREND_FLAT_PTS`] points is "flat".

use crate::analysis::{window_keys, WindowKey, WindowPoint, HIT, INFERRED_DROP};
use crate::reading::{Kind, Reading};
use crate::util::parse_ts;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// Two readings further apart than this do not make an "active" pair for burn rates.
pub const ACTIVE_GAP_MIN: i64 = 30;
/// A fall is an early reset when the old reset time is still this many minutes ahead.
pub const EARLY_RESET_LEAD_MIN: i64 = 30;
/// "Near the limit" threshold, percent.
pub const NEAR_LIMIT: f64 = 90.0;
/// Both windows at or below this after a drop count as "dropped to 0 together".
pub const ZERO_PCT: f64 = 1.0;
/// Longest interval whose gain is placed on the heat map.
pub const HEATMAP_MAX_GAP_HOURS: f64 = 2.0;
/// A heat-map cell/row/column needs this much observed time to be called busiest.
pub const BUSIEST_MIN_HOURS: f64 = 1.0;
/// Windows compared per side for the session trend.
pub const TREND_WINDOWS: usize = 5;
/// Windows compared per side for the weekly trend.
pub const TREND_WINDOWS_WEEKLY: usize = 3;
/// Average-peak changes below this many points are "flat".
pub const TREND_FLAT_PTS: f64 = 5.0;
/// Shortest grace after the last reading for which a window without a reset time still counts as in
/// progress (the real grace is twice the longest gap seen inside that window).
pub const MIN_GRACE_MIN: i64 = 20;
/// A session window needs this many session points gained to take part in the ratio.
pub const RATIO_MIN_SESSION_PTS: f64 = 5.0;
/// Windows needed for [`Stats::session_to_weekly`].
pub const RATIO_MIN_SAMPLES: usize = 3;
/// Weekly points that must have actually been measured (summed over the windows used) before
/// [`Stats::session_to_weekly`] is shown: with whole-number percentages a smaller total is mostly
/// rounding noise. Both this and [`RATIO_MIN_SAMPLES`] must hold.
pub const MIN_WEEKLY_POINTS: f64 = 5.0;
/// From this many measured weekly points the ratio is [`Confidence::Fair`] (below: `Rough`).
pub const FAIR_WEEKLY_POINTS: f64 = 10.0;
/// From this many measured weekly points the ratio is [`Confidence::Good`].
pub const GOOD_WEEKLY_POINTS: f64 = 20.0;
/// Active time a window needs before its pace is used for the limit risk.
pub const MIN_ACTIVE_HOURS: f64 = 0.25;
/// Each reading covers this long after it when computing the covered share of time.
pub const COVER_MIN: i64 = 30;

pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// How much history to look at. (Not `view::Range`, which is the chart's own selector.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsRange {
    Last7d,
    Last30d,
    Last90d,
    All,
}

impl StatsRange {
    pub const ALL: [StatsRange; 4] = [
        StatsRange::Last7d,
        StatsRange::Last30d,
        StatsRange::Last90d,
        StatsRange::All,
    ];

    pub fn days(self) -> Option<i64> {
        match self {
            StatsRange::Last7d => Some(7),
            StatsRange::Last30d => Some(30),
            StatsRange::Last90d => Some(90),
            StatsRange::All => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StatsRange::Last7d => "7d",
            StatsRange::Last30d => "30d",
            StatsRange::Last90d => "90d",
            StatsRange::All => "All",
        }
    }

    /// Start of the range (`None` = unbounded).
    pub fn start(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.days().map(|d| now - Duration::days(d))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Flat,
    Down,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Flat => "flat",
            Direction::Down => "down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trend {
    pub direction: Direction,
    /// Recent average peak minus the previous one, in percentage points.
    pub delta: f64,
    pub recent_avg: f64,
    pub previous_avg: f64,
    /// Windows on each side.
    pub windows: usize,
}

/// One limit window and its peak.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowPeak {
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub peak: f64,
    /// Reset time if the source reported one.
    pub resets_at: Option<DateTime<Utc>>,
    /// The window was cut short by a reset event; not part of the aggregates.
    pub ended_early: bool,
    /// Why it was cut short (`ended_early` only).
    pub cause: Option<ResetCause>,
    /// The window in progress (see the module docs); not part of the aggregates.
    pub in_progress: bool,
}

/// The window in progress, reported separately from the finished ones.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentWindow {
    /// Latest reading of the window.
    pub pct: f64,
    /// Start of the window: `resets_at` minus the window length when a reset time is known,
    /// otherwise the first reading.
    pub started_at: DateTime<Utc>,
    pub resets_at: Option<DateTime<Utc>>,
    /// Hours from `started_at` to `now` (never negative).
    pub elapsed_h: f64,
    /// Readings seen in the window.
    pub readings: usize,
}

/// How far [`Stats::session_to_weekly`] is from being shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatioProgress {
    /// Weekly points gained over the usable session windows so far (needs [`MIN_WEEKLY_POINTS`]).
    pub weekly_points: f64,
    /// Usable session windows so far (needs [`RATIO_MIN_SAMPLES`]).
    pub windows: usize,
    /// A plan change was seen, so the points were measured since the last one.
    pub since_plan_change: bool,
}

/// How much to trust the ratio. Percentages are whole numbers, so the weekly total of each window
/// is off by up to a point; the more weekly points were measured in all, the smaller that error is
/// relative to the total ([`FAIR_WEEKLY_POINTS`], [`GOOD_WEEKLY_POINTS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Rough,
    Fair,
    Good,
}

impl Confidence {
    pub fn of(weekly_points: f64) -> Self {
        if weekly_points >= GOOD_WEEKLY_POINTS {
            Confidence::Good
        } else if weekly_points >= FAIR_WEEKLY_POINTS {
            Confidence::Fair
        } else {
            Confidence::Rough
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Confidence::Rough => "rough",
            Confidence::Fair => "fair",
            Confidence::Good => "good",
        }
    }
}

/// Weekly points per full session, see the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ratio {
    /// Weekly points gained per 100 session points ("a full session costs this much of the week").
    pub points_per_session: f64,
    /// Session windows that took part.
    pub samples: usize,
    /// The measured sum of weekly points the ratio rests on.
    pub weekly_points: f64,
    pub confidence: Confidence,
    /// Rough bounds from the whole-number rounding of the percentages.
    pub low: f64,
    pub high: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurnStats {
    /// Active reading pairs.
    pub samples: usize,
    /// Percentage points per hour while active.
    pub median: Option<f64>,
    pub p90: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Heatmap {
    /// Percentage points gained per cell, `[weekday Mon=0][local hour]`.
    pub gain: [[f64; 24]; 7],
    /// Observed hours per cell.
    pub hours: [[f64; 24]; 7],
}

impl Default for Heatmap {
    fn default() -> Self {
        Self {
            gain: [[0.0; 24]; 7],
            hours: [[0.0; 24]; 7],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Busiest {
    pub weekday: usize,
    pub hour: usize,
    pub pts_per_hour: f64,
}

impl Heatmap {
    /// Average points per hour in one cell, `None` when never observed.
    pub fn avg(&self, weekday: usize, hour: usize) -> Option<f64> {
        let h = self.hours[weekday][hour];
        (h > 0.0).then(|| self.gain[weekday][hour] / h)
    }

    /// Largest value in the grid, for scaling colours.
    pub fn max_avg(&self) -> Option<f64> {
        (0..7)
            .flat_map(|d| (0..24).map(move |h| (d, h)))
            .filter_map(|(d, h)| self.avg(d, h))
            .reduce(f64::max)
    }

    /// Cell with the highest average among cells observed for at least [`BUSIEST_MIN_HOURS`].
    pub fn busiest_cell(&self) -> Option<Busiest> {
        let mut best: Option<Busiest> = None;
        for d in 0..7 {
            for h in 0..24 {
                if self.hours[d][h] < BUSIEST_MIN_HOURS {
                    continue;
                }
                let v = self.gain[d][h] / self.hours[d][h];
                if v > 0.0 && best.is_none_or(|b| v > b.pts_per_hour) {
                    best = Some(Busiest {
                        weekday: d,
                        hour: h,
                        pts_per_hour: v,
                    });
                }
            }
        }
        best
    }

    /// `(weekday, pts/h)` with the highest average over its whole day.
    pub fn busiest_weekday(&self) -> Option<(usize, f64)> {
        best_index(7, |d| {
            let hours: f64 = self.hours[d].iter().sum();
            (hours >= BUSIEST_MIN_HOURS).then(|| self.gain[d].iter().sum::<f64>() / hours)
        })
    }

    /// `(local hour, pts/h)` with the highest average over all weekdays.
    pub fn busiest_hour(&self) -> Option<(usize, f64)> {
        best_index(24, |h| {
            let hours: f64 = (0..7).map(|d| self.hours[d][h]).sum();
            (hours >= BUSIEST_MIN_HOURS)
                .then(|| (0..7).map(|d| self.gain[d][h]).sum::<f64>() / hours)
        })
    }
}

fn best_index(n: usize, f: impl Fn(usize) -> Option<f64>) -> Option<(usize, f64)> {
    (0..n)
        .filter_map(|i| f(i).map(|v| (i, v)))
        .filter(|(_, v)| *v > 0.0)
        .fold(None, |best, (i, v)| match best {
            Some((_, bv)) if bv >= v => best,
            _ => Some((i, v)),
        })
}

#[derive(Debug, Clone, PartialEq)]
pub struct KindStats {
    pub kind: Kind,
    /// Finished windows seen in the range, including those cut short by a reset but not the window
    /// in progress.
    pub windows: usize,
    /// Of those, the ones cut short (not in the aggregates below).
    pub ended_early: usize,
    pub avg_peak: Option<f64>,
    pub median_peak: Option<f64>,
    pub max_peak: Option<f64>,
    /// Percent of counted windows whose peak reached [`HIT`].
    pub hit_rate: Option<f64>,
    pub hits: usize,
    /// Counted windows with a peak of at least [`NEAR_LIMIT`].
    pub near_limit: usize,
    pub trend: Option<Trend>,
    /// All windows in the range, oldest first (for charts), the one in progress included.
    pub peaks: Vec<WindowPeak>,
    /// The window in progress, if any.
    pub current: Option<CurrentWindow>,
    /// Windows cut short by a reset, oldest first. Never part of the aggregates above.
    pub cut_short: Vec<WindowPeak>,
    /// Cut-short windows whose peak reached [`NEAR_LIMIT`] before the reset (kept apart from `near_limit`).
    pub hit_before_reset: usize,
    /// Cut-short windows whose peak reached [`HIT`] before the reset (kept apart from `hits`).
    pub limit_before_reset: usize,
    pub burn: BurnStats,
    pub heatmap: Heatmap,
}

impl KindStats {
    /// Windows in the aggregates (`windows - ended_early`).
    pub fn counted(&self) -> usize {
        self.windows - self.ended_early
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCause {
    /// A fall while the scheduled reset was still far away.
    Early,
    /// Session and weekly both fell to about zero together.
    Together,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResetEvent {
    /// Time of the reading that showed the fall.
    pub at: DateTime<Utc>,
    pub kind: Kind,
    /// Percentage just before the fall.
    pub before_pct: f64,
    /// Highest reading of the window that was cut short (at least `before_pct`).
    pub peak: f64,
    pub cause: ResetCause,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitRisk {
    /// Counted session windows that could be judged.
    pub windows: usize,
    /// Of those, windows that hit the limit or would have at their sustained active pace.
    pub at_risk: usize,
    /// `at_risk / windows`, percent.
    pub risk_pct: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    /// Successful readings in the range.
    pub readings: usize,
    /// Of those, polled live / imported from a source's history.
    pub live_readings: usize,
    pub backfilled_readings: usize,
    pub failed_polls: usize,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
    /// Longest stretch without a reading, including the one up to `now`.
    pub longest_gap_hours: f64,
    /// Percent of the time from the first reading (or range start) to `now` that is within
    /// [`COVER_MIN`] minutes after a reading, backfilled readings included.
    pub covered_pct: Option<f64>,
    /// The same counting only live readings.
    pub live_covered_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub range: StatsRange,
    pub now: DateTime<Utc>,
    pub tz_offset_minutes: i32,
    pub session: KindStats,
    pub weekly: KindStats,
    /// Oldest first.
    pub resets: Vec<ResetEvent>,
    /// Session windows only.
    pub limit_risk: Option<LimitRisk>,
    /// Weekly cost of a full session; `None` with fewer than [`RATIO_MIN_SAMPLES`] usable windows.
    pub session_to_weekly: Option<Ratio>,
    /// What the ratio has to go on so far (always set, also when the ratio itself is shown).
    pub ratio_progress: RatioProgress,
    /// Start of the most recent plan era (both windows reset together), if there was one.
    pub era_start: Option<DateTime<Utc>>,
    pub coverage: Coverage,
}

impl Stats {
    pub fn kind(&self, k: Kind) -> &KindStats {
        match k {
            Kind::Session => &self.session,
            Kind::Weekly => &self.weekly,
        }
    }
}

// ---- internals ------------------------------------------------------------------------------

type Point<'a> = (&'a Reading, DateTime<Utc>);

struct Win {
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    peak: f64,
    reset: Option<DateTime<Utc>>,
    early: bool,
    cause: Option<ResetCause>,
    in_progress: bool,
    last_pct: f64,
    readings: usize,
    /// Longest gap between two readings of the window, hours.
    max_gap_h: f64,
    active_hours: f64,
    active_gain: f64,
    /// Session / weekly points gained over the reading pairs used for the ratio.
    pair_s: f64,
    pair_w: f64,
    /// Part of `pair_w` measured between two live (not backfilled) readings.
    pair_w_live: f64,
}

impl Win {
    fn cut(&mut self, cause: ResetCause) {
        self.early = true;
        self.cause.get_or_insert(cause);
    }
}

/// Same grouping rule as `analysis::windows` ([`window_keys`]); returns the windows oldest first
/// and the window index of every point.
fn build_windows(ok: &[Point], kind: Kind) -> (Vec<Win>, Vec<Option<usize>>) {
    let mut wins: Vec<Win> = Vec::new();
    let mut by_key: HashMap<WindowKey, usize> = HashMap::new();
    let mut idx = Vec::with_capacity(ok.len());
    let points: Vec<WindowPoint> = ok
        .iter()
        .filter_map(|(r, t)| {
            r.bucket(kind)
                .map(|b| (*t, b.pct, b.resets_at.as_deref().and_then(parse_ts)))
        })
        .collect();
    let mut keys = window_keys(&points, kind).into_iter();
    for (r, t) in ok {
        let Some(b) = r.bucket(kind) else {
            idx.push(None);
            continue;
        };
        let reset = b.resets_at.as_deref().and_then(parse_ts);
        let Some(key) = keys.next() else {
            idx.push(None);
            continue;
        };
        let i = *by_key.entry(key).or_insert_with(|| {
            wins.push(Win {
                first: *t,
                last: *t,
                peak: b.pct,
                reset,
                early: false,
                cause: None,
                in_progress: false,
                last_pct: b.pct,
                readings: 0,
                max_gap_h: 0.0,
                active_hours: 0.0,
                active_gain: 0.0,
                pair_s: 0.0,
                pair_w: 0.0,
                pair_w_live: 0.0,
            });
            wins.len() - 1
        });
        let w = &mut wins[i];
        w.readings += 1;
        w.max_gap_h = w.max_gap_h.max(hours_between(w.last, *t));
        w.last = *t;
        w.last_pct = b.pct;
        w.peak = w.peak.max(b.pct);
        w.reset = w.reset.or(reset);
        idx.push(Some(i));
    }
    (wins, idx)
}

fn hours_between(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (b - a).num_milliseconds() as f64 / 3.6e6
}

/// Linear-interpolated percentile of an ascending slice, `q` in 0..=1.
fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64))
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.total_cmp(b));
    v
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// Spread `gain` over the local hours between `t0` and `t1`.
fn add_interval(h: &mut Heatmap, t0: DateTime<Utc>, t1: DateTime<Utc>, gain: f64, tz_min: i32) {
    let secs = |t: DateTime<Utc>| t.timestamp_millis() as f64 / 1000.0 + tz_min as f64 * 60.0;
    let (a, b) = (secs(t0), secs(t1));
    let total = b - a;
    if total <= 0.0 {
        return;
    }
    let mut cur = a;
    while cur < b {
        let hour_idx = (cur / 3600.0).floor() as i64;
        let end = (((hour_idx + 1) * 3600) as f64).min(b);
        let part = end - cur;
        // 1970-01-01 was a Thursday; Monday = 0.
        let day = (hour_idx.div_euclid(24) + 3).rem_euclid(7) as usize;
        let hour = hour_idx.rem_euclid(24) as usize;
        h.hours[day][hour] += part / 3600.0;
        h.gain[day][hour] += gain * part / total;
        cur = end;
    }
}

fn trend(peaks: &[f64], n_max: usize) -> Option<Trend> {
    let n = n_max.min(peaks.len() / 2);
    if n < 2 {
        return None;
    }
    let recent = mean(&peaks[peaks.len() - n..])?;
    let previous = mean(&peaks[peaks.len() - 2 * n..peaks.len() - n])?;
    let delta = recent - previous;
    let direction = if delta >= TREND_FLAT_PTS {
        Direction::Up
    } else if delta <= -TREND_FLAT_PTS {
        Direction::Down
    } else {
        Direction::Flat
    };
    Some(Trend {
        direction,
        delta,
        recent_avg: recent,
        previous_avg: previous,
        windows: n,
    })
}

/// Compute the statistics of `records` over `range`, as seen at `now`. `tz_offset_minutes` is the
/// local offset from UTC (east positive) used to bucket the heat map by local weekday and hour.
/// Records need not be sorted; readings after `now` are ignored.
pub fn compute(
    records: &[Reading],
    range: StatsRange,
    now: DateTime<Utc>,
    tz_offset_minutes: i32,
) -> Stats {
    let start = range.start(now);
    let in_range = |t: DateTime<Utc>| start.is_none_or(|s| t >= s);

    let mut ok: Vec<Point> = records
        .iter()
        .filter(|r| r.ok)
        .filter_map(|r| r.t.filter(|t| *t <= now).map(|t| (r, t)))
        .collect();
    ok.sort_by_key(|p| p.1);

    let mut wins: [Vec<Win>; 2] = [Vec::new(), Vec::new()];
    let mut idx: [Vec<Option<usize>>; 2] = [Vec::new(), Vec::new()];
    for (i, kind) in Kind::ALL.into_iter().enumerate() {
        (wins[i], idx[i]) = build_windows(&ok, kind);
    }

    // ---- reset events ----
    let mut resets: Vec<ResetEvent> = Vec::new();
    for (ki, kind) in Kind::ALL.into_iter().enumerate() {
        let mut prev: Option<usize> = None; // index into `ok` of the last reading with this bucket
        for (i, (r, t)) in ok.iter().enumerate() {
            let Some(b) = r.bucket(kind) else { continue };
            if let Some(p) = prev {
                let (pr, _) = ok[p];
                if let Some(pb) = pr.bucket(kind) {
                    let scheduled_far = pb
                        .resets_at
                        .as_deref()
                        .and_then(parse_ts)
                        .is_some_and(|rs| rs - *t > Duration::minutes(EARLY_RESET_LEAD_MIN));
                    if pb.pct - b.pct >= INFERRED_DROP && scheduled_far {
                        let mut peak = pb.pct;
                        if let Some(w) = idx[ki][p] {
                            peak = peak.max(wins[ki][w].peak);
                            wins[ki][w].cut(ResetCause::Early);
                        }
                        resets.push(ResetEvent {
                            at: *t,
                            kind,
                            before_pct: pb.pct,
                            peak,
                            cause: ResetCause::Early,
                        });
                    }
                }
            }
            prev = Some(i);
        }
    }
    let mut prev: Option<usize> = None; // last reading with both buckets
    for (i, (r, t)) in ok.iter().enumerate() {
        let (Some(s), Some(w)) = (&r.session, &r.weekly) else {
            continue;
        };
        if let Some(p) = prev {
            if let (Some(ps), Some(pw)) = (&ok[p].0.session, &ok[p].0.weekly) {
                let due_soon = |reset: &Option<String>| {
                    reset
                        .as_deref()
                        .and_then(parse_ts)
                        .is_some_and(|rs| rs - *t <= Duration::minutes(EARLY_RESET_LEAD_MIN))
                };
                if ps.pct - s.pct >= INFERRED_DROP
                    && pw.pct - w.pct >= INFERRED_DROP
                    && s.pct <= ZERO_PCT
                    && w.pct <= ZERO_PCT
                    && !due_soon(&ps.resets_at)
                    && !due_soon(&pw.resets_at)
                {
                    for (ki, kind, before) in
                        [(0, Kind::Session, ps.pct), (1, Kind::Weekly, pw.pct)]
                    {
                        if resets.iter().any(|e| e.at == *t && e.kind == kind) {
                            continue;
                        }
                        let mut peak = before;
                        if let Some(wi) = idx[ki][p] {
                            peak = peak.max(wins[ki][wi].peak);
                            wins[ki][wi].cut(ResetCause::Together);
                        }
                        resets.push(ResetEvent {
                            at: *t,
                            kind,
                            before_pct: before,
                            peak,
                            cause: ResetCause::Together,
                        });
                    }
                }
            }
        }
        prev = Some(i);
    }
    resets.sort_by_key(|e| (e.at, e.kind == Kind::Weekly));
    // a plan era starts where both windows reset at the same reading
    let era_start = resets
        .iter()
        .filter(|e| e.kind == Kind::Session)
        .filter(|e| {
            resets
                .iter()
                .any(|o| o.kind == Kind::Weekly && o.at == e.at)
        })
        .map(|e| e.at)
        .max();
    resets.retain(|e| in_range(e.at));

    // ---- the window in progress (newest window of each kind) ----
    for ki in 0..2 {
        let Some(wi) = idx[ki].iter().rev().flatten().next().copied() else {
            continue;
        };
        let w = &mut wins[ki][wi];
        w.in_progress = !w.early
            && match w.reset {
                Some(rs) => rs > now,
                None => {
                    let grace = (w.max_gap_h * 2.0).max(MIN_GRACE_MIN as f64 / 60.0);
                    hours_between(w.last, now) <= grace
                }
            };
    }

    // ---- session / weekly gain over the same reading pairs (for the ratio) ----
    {
        // session window, session pct, weekly pct, backfilled
        let mut prev: Option<(usize, f64, f64, bool)> = None;
        for (i, (r, _)) in ok.iter().enumerate() {
            let (Some(s), Some(wk), Some(sw)) = (&r.session, &r.weekly, idx[0][i]) else {
                continue;
            };
            if let Some((pw, ps, pwk, pb)) = prev {
                let (ds, dw) = (s.pct - ps, wk.pct - pwk);
                if pw == sw && ds >= 0.0 && dw >= 0.0 {
                    wins[0][sw].pair_s += ds;
                    wins[0][sw].pair_w += dw;
                    if !pb && !r.backfill {
                        wins[0][sw].pair_w_live += dw;
                    }
                }
            }
            prev = Some((sw, s.pct, wk.pct, r.backfill));
        }
    }

    // ---- per kind: burn, heat map, aggregates ----
    let mut kinds: Vec<KindStats> = Vec::new();
    for (ki, kind) in Kind::ALL.into_iter().enumerate() {
        let mut heat = Heatmap::default();
        let mut burn: Vec<f64> = Vec::new();
        let mut prev: Option<(usize, DateTime<Utc>, f64)> = None;
        for (i, (r, t)) in ok.iter().enumerate() {
            let (Some(b), Some(w)) = (r.bucket(kind), idx[ki][i]) else {
                continue;
            };
            if let Some((pw, pt, pp)) = prev {
                let dt = hours_between(pt, *t);
                if pw == w && dt > 0.0 {
                    let gain = b.pct - pp;
                    let active = gain > 0.0 && dt <= ACTIVE_GAP_MIN as f64 / 60.0;
                    if active {
                        wins[ki][w].active_hours += dt;
                        wins[ki][w].active_gain += gain;
                    }
                    if in_range(*t) {
                        if active {
                            burn.push(gain / dt);
                        }
                        if dt <= HEATMAP_MAX_GAP_HOURS {
                            add_interval(&mut heat, pt, *t, gain.max(0.0), tz_offset_minutes);
                        }
                    }
                }
            }
            prev = Some((w, *t, b.pct));
        }

        let in_win: Vec<&Win> = wins[ki]
            .iter()
            .filter(|w| in_range(w.last) && !w.in_progress)
            .collect();
        let peak_of = |w: &Win| WindowPeak {
            first: w.first,
            last: w.last,
            peak: w.peak,
            resets_at: w.reset,
            ended_early: w.early,
            cause: w.cause,
            in_progress: w.in_progress,
        };
        let current = wins[ki]
            .iter()
            .find(|w| w.in_progress && in_range(w.last))
            .map(|w| {
                let started_at = w.reset.map_or(w.first, |rs| {
                    rs - Duration::minutes((kind.hours() * 60.0) as i64)
                });
                CurrentWindow {
                    pct: w.last_pct,
                    started_at,
                    resets_at: w.reset,
                    elapsed_h: hours_between(started_at, now).max(0.0),
                    readings: w.readings,
                }
            });
        let cut_short: Vec<WindowPeak> = in_win
            .iter()
            .filter(|w| w.early)
            .map(|w| peak_of(w))
            .collect();
        let counted: Vec<f64> = in_win.iter().filter(|w| !w.early).map(|w| w.peak).collect();
        let sorted_peaks = sorted(counted.clone());
        let hits = counted.iter().filter(|p| **p >= HIT).count();
        let burn = sorted(burn);
        kinds.push(KindStats {
            kind,
            windows: in_win.len(),
            ended_early: in_win.iter().filter(|w| w.early).count(),
            avg_peak: mean(&counted),
            median_peak: percentile(&sorted_peaks, 0.5),
            max_peak: sorted_peaks.last().copied(),
            hit_rate: (!counted.is_empty()).then(|| hits as f64 / counted.len() as f64 * 100.0),
            hits,
            near_limit: counted.iter().filter(|p| **p >= NEAR_LIMIT).count(),
            trend: trend(
                &counted,
                match kind {
                    Kind::Session => TREND_WINDOWS,
                    Kind::Weekly => TREND_WINDOWS_WEEKLY,
                },
            ),
            peaks: wins[ki]
                .iter()
                .filter(|w| in_range(w.last))
                .map(peak_of)
                .collect(),
            current,
            hit_before_reset: cut_short.iter().filter(|w| w.peak >= NEAR_LIMIT).count(),
            limit_before_reset: cut_short.iter().filter(|w| w.peak >= HIT).count(),
            cut_short,
            burn: BurnStats {
                samples: burn.len(),
                median: percentile(&burn, 0.5),
                p90: percentile(&burn, 0.9),
            },
            heatmap: heat,
        });
    }

    // ---- limit risk (session windows) ----
    let (mut judged, mut at_risk) = (0usize, 0usize);
    for w in wins[0]
        .iter()
        .filter(|w| in_range(w.last) && !w.early && !w.in_progress)
    {
        let hit = w.peak >= HIT;
        let sustained = (w.active_hours >= MIN_ACTIVE_HOURS)
            .then(|| w.active_gain / w.active_hours * Kind::Session.hours() >= 100.0);
        if hit || sustained.is_some() {
            judged += 1;
            at_risk += usize::from(hit || sustained == Some(true));
        }
    }
    let limit_risk = (judged > 0).then(|| LimitRisk {
        windows: judged,
        at_risk,
        risk_pct: at_risk as f64 / judged as f64 * 100.0,
    });

    // ---- session-to-weekly ratio ----
    let used: Vec<&Win> = wins[0]
        .iter()
        .filter(|w| {
            in_range(w.last)
                && !w.early
                && !w.in_progress
                && w.pair_s >= RATIO_MIN_SESSION_PTS
                && era_start.is_none_or(|e| w.first >= e)
        })
        .collect();
    let (s_sum, w_sum) = used
        .iter()
        .fold((0.0, 0.0), |(s, w), x| (s + x.pair_s, w + x.pair_w));
    let live_w: f64 = used.iter().map(|x| x.pair_w_live).sum();
    let ratio_progress = RatioProgress {
        weekly_points: w_sum,
        windows: used.len(),
        since_plan_change: era_start.is_some(),
    };
    let session_to_weekly =
        (used.len() >= RATIO_MIN_SAMPLES && w_sum >= MIN_WEEKLY_POINTS).then(|| {
            let n = used.len() as f64;
            Ratio {
                points_per_session: w_sum / s_sum * 100.0,
                samples: used.len(),
                weekly_points: w_sum,
                confidence: if live_w * 2.0 < w_sum {
                    Confidence::Rough // mostly sparse imported history
                } else {
                    Confidence::of(w_sum)
                },
                low: (w_sum - n).max(0.0) / (s_sum + n) * 100.0,
                high: (w_sum + n) / (s_sum - n).max(1.0) * 100.0,
            }
        });

    // ---- coverage ----
    let range_ok: Vec<Point> = ok.iter().filter(|p| in_range(p.1)).copied().collect();
    let failed_polls = records
        .iter()
        .filter(|r| r.failed)
        .filter(|r| r.t.is_some_and(|t| t <= now && in_range(t)))
        .count();
    let backfilled = range_ok.iter().filter(|p| p.0.backfill).count();
    let cover = |pts: &[Point]| -> Option<f64> {
        let first = pts.first()?.1;
        let from = start.map_or(first, |s| s.max(first));
        let span = hours_between(from, now);
        if span <= 0.0 {
            return Some(100.0);
        }
        let cap = COVER_MIN as f64 / 60.0;
        let mut covered = 0.0;
        for (i, (_, t)) in pts.iter().enumerate() {
            let next = pts.get(i + 1).map_or(now, |n| n.1);
            covered += hours_between(*t, next).min(cap);
        }
        Some((covered / span * 100.0).clamp(0.0, 100.0))
    };
    let live: Vec<Point> = range_ok.iter().filter(|p| !p.0.backfill).copied().collect();
    let longest_gap = range_ok
        .windows(2)
        .map(|w| hours_between(w[0].1, w[1].1))
        .chain(range_ok.last().map(|l| hours_between(l.1, now)))
        .fold(0.0, f64::max);

    let [session, weekly]: [KindStats; 2] = kinds.try_into().expect("two kinds");
    Stats {
        range,
        now,
        tz_offset_minutes,
        session,
        weekly,
        resets,
        limit_risk,
        session_to_weekly,
        ratio_progress,
        era_start,
        coverage: Coverage {
            readings: range_ok.len(),
            live_readings: live.len(),
            backfilled_readings: backfilled,
            failed_polls,
            first: range_ok.first().map(|p| p.1),
            last: range_ok.last().map(|p| p.1),
            longest_gap_hours: longest_gap,
            covered_pct: cover(&range_ok),
            live_covered_pct: cover(&live),
        },
    }
}
