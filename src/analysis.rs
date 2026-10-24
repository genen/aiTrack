// SPDX-License-Identifier: GPL-3.0-or-later
//! Statistics over the recorded plan-usage snapshots.
//!
//! Readings are grouped into limit windows by their `resets_at` time (5 h session, 7 d weekly).
//! The pace is the least-squares slope of the percentage over the current window.

use crate::reading::{Kind, Reading};
use crate::util::{iso, parse_ts, round_to};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// Percent at which a window counts as having "hit the limit".
pub const HIT: f64 = 99.5;
pub const MAX_SERIES: usize = 3000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gauge {
    pub pct: f64,
    pub resets_at: Option<String>,
    pub hours_left: Option<f64>,
    pub rate_per_hour: Option<f64>,
    /// How `rate_per_hour` was obtained.
    pub rate_method: Option<PaceMethod>,
    pub projected_pct: Option<f64>,
    pub hours_to_limit: Option<f64>,
    pub window_hours: f64,
    /// Stable id of this limit window (reset time, or the start of an inferred window); used to
    /// remember which alerts were already sent for it.
    pub window_id: String,
    /// `resets_at` was carried forward from an earlier sample of the window (an estimate).
    pub resets_carried: bool,
    /// The reset time this reading itself reported has passed: the window is over and the
    /// percentage belongs to the old one (no countdown, pace or projection; wait for a new reading).
    pub reset_passed: bool,
}

/// Where a pace figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PaceMethod {
    /// Regression over the readings of the current window.
    Recent,
    /// Average since the window opened (percent used / hours elapsed).
    Average,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowStats {
    pub windows: usize,
    pub avg_peak: Option<f64>,
    pub max_peak: Option<f64>,
    /// Percent of windows whose peak reached [`HIT`].
    pub hit_rate: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpendInfo {
    pub enabled: bool,
    pub used: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub snapshots: usize,
    pub errors: usize,
    pub first: String,
    pub last: String,
    /// Where the newest reading came from (`desktop-history`, `claude-json`, `oauth`), if recorded.
    pub last_source: Option<String>,
    pub stale_minutes: i64,
    pub session: Option<Gauge>,
    pub weekly: Option<Gauge>,
    pub session_stats: WindowStats,
    pub weekly_stats: WindowStats,
    pub spend: Option<SpendInfo>,
}

impl Plan {
    pub fn gauge(&self, k: Kind) -> Option<&Gauge> {
        match k {
            Kind::Session => self.session.as_ref(),
            Kind::Weekly => self.weekly.as_ref(),
        }
    }
    pub fn stats(&self, k: Kind) -> &WindowStats {
        match k {
            Kind::Session => &self.session_stats,
            Kind::Weekly => &self.weekly_stats,
        }
    }
}

/// Least-squares slope of pct over time, in percentage points per hour. Needs a quarter hour of
/// span; see [`pace_for`] for how much evidence a pace actually needs before it is believed.
pub fn slope_per_hour(points: &[(DateTime<Utc>, f64)]) -> Option<f64> {
    let span = points.last()?.0 - points.first()?.0;
    if span.num_milliseconds() as f64 / 3.6e6 < 0.25 {
        return None;
    }
    raw_slope(points)
}

fn raw_slope(points: &[(DateTime<Utc>, f64)]) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let t0 = points[0].0;
    let xs: Vec<f64> = points
        .iter()
        .map(|(t, _)| (*t - t0).num_milliseconds() as f64 / 3.6e6)
        .collect();
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let den: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if den == 0.0 {
        return None;
    }
    let num: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    Some(num / den)
}

/// Percentages are whole numbers, so one step is 1 point of quantisation noise, not a trend: the
/// recent-trend regression is only believed after at least this many points of gain.
pub const MIN_GAIN_PTS: f64 = 3.0;
/// ... over at least this long, or at least [`MIN_STEPS`] distinct rises (heavy use in a short burst).
pub const MIN_SPAN_H: f64 = 0.25;
pub const MIN_STEPS: usize = 3;
/// A flat window this long means "idle", which is evidence too (pace 0).
pub const IDLE_SPAN_H: f64 = 1.0;
/// The recent pace may exceed the average-since-window-start pace by at most this factor.
pub const BLEND_CAP: f64 = 3.0;

enum Evidence {
    /// Not enough to say anything from the recent readings.
    Insufficient,
    /// Nothing happened for a good while.
    Idle,
    Trend(f64),
}

fn recent_evidence(points: &[(DateTime<Utc>, f64)]) -> Evidence {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return Evidence::Insufficient;
    };
    if points.len() < 2 {
        return Evidence::Insufficient;
    }
    let gain = last.1 - first.1;
    let span_h = (last.0 - first.0).num_milliseconds() as f64 / 3.6e6;
    if gain.abs() < 1e-9 {
        return if span_h >= IDLE_SPAN_H - 1e-9 {
            Evidence::Idle
        } else {
            Evidence::Insufficient
        };
    }
    let steps = points.windows(2).filter(|w| w[1].1 > w[0].1).count();
    if gain < MIN_GAIN_PTS || (span_h < MIN_SPAN_H && steps < MIN_STEPS) {
        return Evidence::Insufficient;
    }
    raw_slope(points)
        .filter(|r| *r > 0.0)
        .map_or(Evidence::Insufficient, Evidence::Trend)
}

/// A weekly window shows no pace until this many hours of it have passed ...
pub const MIN_WEEKLY_ELAPSED_H: f64 = 12.0;
/// ... or this many points are used: one early point is no basis for a pace over seven days.
pub const MIN_WEEKLY_PACE_PCT: f64 = 3.0;

/// Too early in a weekly window to believe a pace (see [`MIN_WEEKLY_ELAPSED_H`]). Needs the reset
/// time; without it the elapsed time is unknown and nothing is held back.
fn weekly_too_early(kind: Kind, pct: f64, hours_left: Option<f64>) -> bool {
    let Some(left) = hours_left.filter(|h| *h > 0.0) else {
        return false;
    };
    kind == Kind::Weekly && pct < MIN_WEEKLY_PACE_PCT && kind.hours() - left < MIN_WEEKLY_ELAPSED_H
}

/// The pace to project with, and where it came from.
///
/// * `points` are the readings of the current evidence window (older ones, before a reset or plan
///   change, must already be left out).
/// * The recent trend is only used with real evidence (see [`MIN_GAIN_PTS`]); without it the
///   average since the window opened is used (needs the reset time: start = reset - length).
/// * With both, the trend is damped: `max(avg, min(trend, 3 x avg))`. A real surge (76 % -> 90 % in
///   six minutes of heavy use) still raises the pace well above the average, but one noisy step
///   cannot make it absurd, and a calm stretch never lowers it below the average.
/// * Nothing reliable: no pace (and so no projection and no warning).
/// * Early in a weekly window (see [`MIN_WEEKLY_ELAPSED_H`], [`MIN_WEEKLY_PACE_PCT`]) there is no
///   pace either, except a flat hour of idleness (pace 0).
pub fn pace_for(
    kind: Kind,
    pct: f64,
    hours_left: Option<f64>,
    points: &[(DateTime<Utc>, f64)],
) -> Option<(f64, PaceMethod)> {
    if weekly_too_early(kind, pct, hours_left) {
        return matches!(recent_evidence(points), Evidence::Idle)
            .then_some((0.0, PaceMethod::Recent));
    }
    let avg = elapsed_pace(kind, pct, hours_left);
    match (recent_evidence(points), avg) {
        (Evidence::Idle, _) => Some((0.0, PaceMethod::Recent)),
        (Evidence::Trend(r), Some(a)) => {
            let rate = a.max(r.min(BLEND_CAP * a));
            Some((
                rate,
                if r > a {
                    PaceMethod::Recent
                } else {
                    PaceMethod::Average
                },
            ))
        }
        (Evidence::Trend(r), None) => Some((r, PaceMethod::Recent)),
        (Evidence::Insufficient, Some(a)) => Some((a, PaceMethod::Average)),
        (Evidence::Insufficient, None) => None,
    }
}

/// Average pace (points per hour) since the window opened, when the reset time is known.
/// Needs a quarter hour of the window to have passed, like the regression does.
pub fn elapsed_pace(kind: Kind, pct: f64, hours_left: Option<f64>) -> Option<f64> {
    let left = hours_left.filter(|h| *h > 0.0)?;
    let elapsed = kind.hours() - left;
    (elapsed >= 0.25).then(|| pct / elapsed)
}

/// Window identity: the reset time truncated to the minute (resets_at jitters by sub-second amounts).
pub fn window_key(reset: DateTime<Utc>) -> i64 {
    reset.timestamp().div_euclid(60)
}

/// Identity of a limit window. Normally the (minute-truncated) reset time; a reading that reports
/// none joins the window of a reset time seen on another reading when it lies inside that window,
/// otherwise (sources that never report resets: Claude desktop history, `~/.claude.json`) it gets an
/// inferred window that starts whenever the percentage drops by [`INFERRED_DROP`] points or more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowKey {
    Reset(i64),
    Inferred(u32),
}

/// A fall of at least this many points between two readings (without a reset time) starts a new window.
pub const INFERRED_DROP: f64 = 10.0;

/// One reading of a limit: `(time, percent, reset time if reported)`.
pub type WindowPoint = (DateTime<Utc>, f64, Option<DateTime<Utc>>);

/// The window of every point, oldest first. Points with
/// a reset time are keyed by it. A point without one is put into the window of the nearest reset
/// time (reported by any point) that still lies ahead of it by at most one window length, so a
/// history sample and a cache sample of the same 5-hour window are one window, not two; if its
/// percentage fell by [`INFERRED_DROP`] or more since the previous point it starts an inferred
/// window instead (the window ended early). Points outside every known window are inferred.
pub fn window_keys(points: &[WindowPoint], kind: Kind) -> Vec<WindowKey> {
    let len = chrono::Duration::minutes((kind.hours() * 60.0) as i64);
    let (slack, tail) = (chrono::Duration::minutes(2), chrono::Duration::minutes(1));
    // every distinct reset time once, ascending: the nearest one ahead of a point is a binary search
    let mut resets: Vec<DateTime<Utc>> = points.iter().filter_map(|p| p.2).collect();
    resets.sort();
    resets.dedup_by_key(|r| window_key(*r));
    let mut out = Vec::with_capacity(points.len());
    let (mut inferred, mut prev_pct): (u32, Option<f64>) = (0, None);
    // when the previous point's window ends (its reset time, reported or found)
    let mut prev_end: Option<DateTime<Utc>> = None;
    // a reset window that ended early (a drop inside it): later unstamped points stay inferred
    let mut ended_early: Option<i64> = None;
    for (t, pct, reset) in points {
        let dropped = prev_pct.is_some_and(|p| p - pct >= INFERRED_DROP);
        // a fall right after the previous window's own reset time is the ordinary boundary, not an
        // early end of the window the point belongs to
        let boundary = prev_end.is_some_and(|e| e <= *t);
        let (key, end) = match reset {
            Some(rs) => (WindowKey::Reset(window_key(*rs)), Some(*rs)),
            None => {
                let at = resets.partition_point(|r| *r < *t - tail);
                let covering = resets.get(at).filter(|r| *t >= **r - len - slack).copied();
                let k = covering.map(window_key);
                match k {
                    Some(k) if !(dropped && !boundary) && ended_early != Some(k) => {
                        (WindowKey::Reset(k), covering)
                    }
                    other => {
                        if dropped {
                            inferred += 1;
                            if !boundary {
                                ended_early = other;
                            }
                        }
                        (WindowKey::Inferred(inferred), None)
                    }
                }
            }
        };
        prev_pct = Some(*pct);
        prev_end = end;
        out.push(key);
    }
    out
}

type Groups = BTreeMap<WindowKey, Vec<(DateTime<Utc>, f64)>>;

/// Group readings of `kind` into windows. Also returns the window of every reading in `ok`
/// (same indexing), `None` where the reading has no bucket of this kind.
fn windows(ok: &[&Reading], kind: Kind) -> (Groups, Vec<Option<WindowKey>>) {
    let mut at: Vec<usize> = Vec::new();
    let mut points = Vec::new();
    for (i, r) in ok.iter().enumerate() {
        if let (Some(b), Some(t)) = (r.bucket(kind), r.t) {
            at.push(i);
            points.push((t, b.pct, b.resets_at.as_deref().and_then(parse_ts)));
        }
    }
    let found = window_keys(&points, kind);
    let mut groups: Groups = BTreeMap::new();
    let mut keys = vec![None; ok.len()];
    for ((i, key), (t, pct, _)) in at.into_iter().zip(found).zip(points) {
        groups.entry(key).or_default().push((t, pct));
        keys[i] = Some(key);
    }
    (groups, keys)
}

/// The reset time to carry forward to `latest` (which reports none for `kind`): that of the newest
/// earlier reading that has one, as long as it is still in the future, `latest` falls inside the
/// window it defines, and the percentage has not dropped since (a drop means the window reset).
/// Nothing is invented: without such a reading there is no reset time.
pub fn carried_reset(
    ok: &[&Reading],
    latest: &Reading,
    kind: Kind,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let (lt, pct) = (latest.t?, latest.bucket(kind)?.pct);
    for r in ok.iter().rev() {
        if std::ptr::eq(*r, latest) {
            continue;
        }
        let (Some(t), Some(b)) = (r.t, r.bucket(kind)) else {
            continue;
        };
        let Some(reset) = b.resets_at.as_deref().and_then(parse_ts) else {
            continue;
        };
        let window = chrono::Duration::minutes((kind.hours() * 60.0) as i64);
        let consistent = reset > now
            && reset > lt
            && lt >= reset - window - chrono::Duration::minutes(2)
            && !(lt > t && pct < b.pct - 1.0);
        return consistent.then_some(reset);
    }
    None
}

/// Readings stamped further ahead of `now` than this are ignored (a wrong clock or a hand-edited
/// date must not become "the latest reading"); a few minutes of skew are tolerated.
pub const MAX_FUTURE_SKEW_MIN: i64 = 5;

/// Is `t` a believable time for a reading, seen at `now`?
pub fn not_in_the_future(t: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    t <= now + chrono::Duration::minutes(MAX_FUTURE_SKEW_MIN)
}

pub fn analyze(readings: &[Reading], now: DateTime<Utc>) -> Option<Plan> {
    let mut ok: Vec<&Reading> = readings
        .iter()
        .filter(|r| r.ok && r.t.is_some_and(|t| not_in_the_future(t, now)))
        .collect();
    ok.sort_by_key(|r| r.t);
    let latest = *ok.last()?;
    let errors = readings.iter().filter(|r| r.failed).count();

    // Evidence boundaries: a fall of 10+ points, or both windows dropping to 0 (a plan change or an
    // early reset), starts a fresh evidence window; older readings must not feed the pace.
    let mut cuts: [Option<DateTime<Utc>>; 2] = [None, None];
    let mut prev: [Option<f64>; 2] = [None, None];
    for r in &ok {
        let Some(t) = r.t else { continue };
        let cur = [
            r.session.as_ref().map(|b| b.pct),
            r.weekly.as_ref().map(|b| b.pct),
        ];
        for i in 0..2 {
            if let (Some(c), Some(p)) = (cur[i], prev[i]) {
                if p - c >= INFERRED_DROP {
                    cuts[i] = Some(t);
                }
            }
        }
        if cur[0] == Some(0.0)
            && cur[1] == Some(0.0)
            && (prev[0] > Some(0.0) || prev[1] > Some(0.0))
        {
            cuts = [Some(t), Some(t)];
        }
        for i in 0..2 {
            if cur[i].is_some() {
                prev[i] = cur[i];
            }
        }
    }

    let mut gauges: [Option<Gauge>; 2] = [None, None];
    let mut stats: Vec<WindowStats> = Vec::new();
    for (i, kind) in Kind::ALL.into_iter().enumerate() {
        let (groups, keys) = windows(&ok, kind);
        let peaks: Vec<f64> = groups
            .values()
            .map(|pts| pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max))
            .collect();
        stats.push(WindowStats {
            windows: peaks.len(),
            avg_peak: (!peaks.is_empty())
                .then(|| round_to(peaks.iter().sum::<f64>() / peaks.len() as f64, 1)),
            max_peak: peaks
                .iter()
                .cloned()
                .reduce(f64::max)
                .map(|m| round_to(m, 1)),
            hit_rate: (!peaks.is_empty()).then(|| {
                (peaks.iter().filter(|p| **p >= HIT).count() as f64 / peaks.len() as f64 * 100.0)
                    .round() as i64
            }),
        });

        let Some(b) = latest.bucket(kind) else {
            continue;
        };
        let mut reset = b.resets_at.as_deref().and_then(parse_ts);
        let mut carried = b.carried;
        if reset.is_none() {
            // this sample has no reset time (a source that reports percentages only): use the
            // newest one an earlier sample of the same window reported
            if let Some(r) = carried_reset(&ok, latest, kind, now) {
                reset = Some(r);
                carried = true;
            }
        }
        // A reset time that passed ended the window its reading belongs to. A reported one is kept
        // (never shown as a countdown); a carried one is dropped (never shown at all). Either way the
        // old percentage is not the current one: wait for a new reading.
        let passed = reset.is_some_and(|r| r <= now);
        let reading_before_reset = reset.is_some_and(|r| latest.t.is_some_and(|t| t < r));
        let reset_passed = passed && (!carried || reading_before_reset);
        if carried && passed {
            reset = None;
        }
        let carried = carried && reset.is_some();
        let hours_left = reset
            .filter(|_| !reset_passed)
            .map(|r| ((r - now).num_milliseconds() as f64 / 3.6e6).max(0.0));
        let latest_key = keys.last().copied().flatten();
        let evidence: Vec<(DateTime<Utc>, f64)> = latest_key
            .and_then(|k| groups.get(&k))
            .map(|pts| {
                pts.iter()
                    .filter(|p| cuts[i].is_none_or(|c| p.0 >= c))
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        let (rate, method) = match pace_for(kind, b.pct, hours_left, &evidence) {
            Some((r, m)) if !reset_passed => (Some(r), Some(m)),
            _ => (None, None),
        };
        let window_id = match latest_key {
            Some(WindowKey::Reset(k)) => format!("r{k}"),
            Some(WindowKey::Inferred(_)) => latest_key
                .and_then(|k| groups.get(&k))
                .and_then(|pts| pts.first())
                .map(|p| format!("i{}", p.0.timestamp()))
                .unwrap_or_default(),
            None => String::new(),
        };
        let projected = match (rate, hours_left) {
            (Some(rate), Some(h)) => Some(round_to(b.pct.max(b.pct + rate * h), 1)),
            _ => None,
        };
        let hours_to_limit = match rate {
            Some(rate) if rate > 0.0 && b.pct < 100.0 => Some(round_to((100.0 - b.pct) / rate, 1)),
            _ => None,
        };
        gauges[i] = Some(Gauge {
            pct: b.pct,
            resets_at: reset.map(iso),
            hours_left: hours_left.map(|h| round_to(h, 2)),
            rate_per_hour: rate.map(|r| round_to(r, 2)),
            rate_method: method,
            projected_pct: projected,
            hours_to_limit,
            window_hours: kind.hours(),
            window_id,
            resets_carried: carried,
            reset_passed,
        });
    }
    let [session, weekly] = gauges;
    let weekly_stats = stats.pop()?;
    let session_stats = stats.pop()?;
    let last_t = latest.t?;
    Some(Plan {
        snapshots: ok.len(),
        errors,
        first: ok[0].ts.clone(),
        last: latest.ts.clone(),
        last_source: latest.source.clone(),
        stale_minutes: (((now - last_t).num_seconds() as f64 / 60.0).round() as i64).max(0),
        session,
        weekly,
        session_stats,
        weekly_stats,
        spend: latest.spend.as_ref().map(|s| SpendInfo {
            enabled: s.enabled,
            used: s.used,
            limit: s.limit,
        }),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesPoint {
    pub t: String,
    pub s: Option<f64>,
    pub w: Option<f64>,
}

/// Down-sampled (<= ~MAX_SERIES points) history of both percentages, always including the newest.
pub fn series(readings: &[Reading]) -> Vec<SeriesPoint> {
    let mut ok: Vec<&Reading> = readings.iter().filter(|r| r.ok && r.t.is_some()).collect();
    ok.sort_by_key(|r| r.t);
    let stride = ok.len().div_ceil(MAX_SERIES).max(1);
    let pt = |r: &Reading| SeriesPoint {
        t: r.ts.clone(),
        s: r.session.as_ref().map(|b| b.pct),
        w: r.weekly.as_ref().map(|b| b.pct),
    };
    let mut out: Vec<SeriesPoint> = ok.iter().step_by(stride).map(|r| pt(r)).collect();
    if let Some(last) = ok.last() {
        if out.last().map(|p| p.t.as_str()) != Some(last.ts.as_str()) {
            out.push(pt(last));
        }
    }
    out
}

/// No "empty soon" warning below this many percent used (5 h session) unless even the average pace
/// since the window opened would exhaust it before the reset.
pub const MIN_WARN_PCT_SESSION: f64 = 10.0;
/// ... the same for the 7-day window.
pub const MIN_WARN_PCT_WEEKLY: f64 = 25.0;
/// The 7-day window's "average pace would exhaust it" exception to [`MIN_WARN_PCT_WEEKLY`] applies only
/// once this many hours of the window have passed: a few hours in, the average is a short burst, not a
/// week of use at that pace.
pub const MIN_WEEKLY_WARN_ELAPSED_H: f64 = 24.0;

/// A window that would run out before it resets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmptyInfo {
    pub empty_in_h: f64,
    pub resets_in_h: f64,
    /// Already at the limit.
    pub at_limit: bool,
}

/// Which window a gauge is for (from its length).
pub fn kind_of(g: &Gauge) -> Kind {
    if g.window_hours > 24.0 {
        Kind::Weekly
    } else {
        Kind::Session
    }
}

/// Would this window run out before it resets, at its current pace? Shared by the header warning and
/// the toast alerts. Low usage never warns on a noisy recent pace alone (see the `MIN_WARN_PCT_*`).
pub fn empty_before_reset(g: &Gauge) -> Option<EmptyInfo> {
    let left = g.hours_left.filter(|h| *h > 0.0)?;
    if g.pct >= 100.0 {
        return Some(EmptyInfo {
            empty_in_h: 0.0,
            resets_in_h: left,
            at_limit: true,
        });
    }
    let rate = g.rate_per_hour.filter(|r| *r > 0.0)?;
    let empty_in = (100.0 - g.pct) / rate;
    if empty_in >= left {
        return None;
    }
    let kind = kind_of(g);
    let min = match kind {
        Kind::Session => MIN_WARN_PCT_SESSION,
        Kind::Weekly => MIN_WARN_PCT_WEEKLY,
    };
    let avg_exhausts = (kind != Kind::Weekly || kind.hours() - left >= MIN_WEEKLY_WARN_ELAPSED_H)
        && elapsed_pace(kind, g.pct, Some(left))
            .is_some_and(|a| a > 0.0 && (100.0 - g.pct) / a < left);
    (g.pct >= min || avg_exhausts).then_some(EmptyInfo {
        empty_in_h: empty_in,
        resets_in_h: left,
        at_limit: false,
    })
}
