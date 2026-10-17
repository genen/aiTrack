// SPDX-License-Identifier: GPL-3.0-or-later
//! Long-term statistics over synthetic readings. All times are offsets from a fixed `now`.

use aitrack::reading::{Kind, Reading};
use aitrack::stats::*;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

/// Wednesday 2026-10-14 12:00 UTC.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// `mins` minutes after `now` (negative = before).
fn at(mins: i64) -> DateTime<Utc> {
    now() + Duration::minutes(mins)
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

fn bucket(pct: f64, reset: Option<DateTime<Utc>>) -> Value {
    json!({"pct": pct, "resets_at": reset.map(|r| r.to_rfc3339())})
}

struct R {
    t: DateTime<Utc>,
    s: Option<(f64, Option<DateTime<Utc>>)>,
    w: Option<(f64, Option<DateTime<Utc>>)>,
    backfill: bool,
}

impl R {
    fn reading(&self) -> Reading {
        let mut v = json!({"ok": true, "ts": self.t.to_rfc3339(), "backfill": self.backfill});
        if let Some((p, r)) = self.s {
            v["session"] = bucket(p, r);
        }
        if let Some((p, r)) = self.w {
            v["weekly"] = bucket(p, r);
        }
        Reading::from_value(&v)
    }
}

fn far() -> DateTime<Utc> {
    now() + Duration::days(3)
}

/// Session-only readings every 15 minutes from `start`, one per entry of `pcts`, all in the window
/// that resets at `reset` (`None` = the source reports no reset time).
fn window(start: DateTime<Utc>, reset: Option<DateTime<Utc>>, pcts: &[f64]) -> Vec<Reading> {
    pcts.iter()
        .enumerate()
        .map(|(i, p)| {
            R {
                t: start + Duration::minutes(15 * i as i64),
                s: Some((*p, reset)),
                w: None,
                backfill: false,
            }
            .reading()
        })
        .collect()
}

/// One session window per entry of `peaks`, 6 h apart, each ending well before `now`.
fn windows_with_peaks(first_start_mins: i64, peaks: &[f64]) -> Vec<Reading> {
    let mut out = Vec::new();
    for (i, peak) in peaks.iter().enumerate() {
        let start = at(first_start_mins + 360 * i as i64);
        out.extend(window(
            start,
            Some(start + Duration::hours(5)),
            &[1.0, *peak / 2.0, *peak],
        ));
    }
    out
}

fn compute7(recs: &[Reading]) -> Stats {
    compute(recs, StatsRange::Last7d, now(), 0)
}

#[test]
fn ordinary_windows() {
    let recs = windows_with_peaks(-3000, &[50.0, 92.0, 100.0]);
    let st = compute7(&recs);
    let s = &st.session;
    assert_eq!((s.windows, s.ended_early, s.counted()), (3, 0, 3));
    assert!(near(s.avg_peak.unwrap(), 242.0 / 3.0));
    assert_eq!(s.median_peak, Some(92.0));
    assert_eq!(s.max_peak, Some(100.0));
    assert_eq!((s.hits, s.near_limit), (1, 2));
    assert!(near(s.hit_rate.unwrap(), 100.0 / 3.0));
    assert_eq!(s.peaks.len(), 3);
    assert!(s.peaks.windows(2).all(|w| w[0].first < w[1].first));
    assert!(s.peaks[0].resets_at.is_some());
    assert!(st.resets.is_empty());
    // no weekly buckets in these records
    assert_eq!(st.weekly.windows, 0);
    assert_eq!(st.weekly.avg_peak, None);
    // order of the input does not matter
    let mut rev = recs.clone();
    rev.reverse();
    assert_eq!(compute7(&rev), st);
}

#[test]
fn median_of_an_even_count_interpolates() {
    let st = compute7(&windows_with_peaks(-3000, &[10.0, 20.0, 40.0, 80.0]));
    assert_eq!(st.session.median_peak, Some(30.0));
}

#[test]
fn hit_rate_threshold_is_99_5() {
    let st = compute7(&windows_with_peaks(-3000, &[100.0, 99.5, 99.4, 50.0]));
    assert_eq!(st.session.hits, 2);
    assert!(near(st.session.hit_rate.unwrap(), 50.0));
    assert_eq!(st.session.near_limit, 3);
}

#[test]
fn windows_without_reset_times_are_inferred_from_drops() {
    // 10 -> 60, then a 55 point fall starts a new window, then a 9 point dip does not
    let mut recs = window(
        at(-300),
        None,
        &[10.0, 35.0, 60.0, 5.0, 20.0, 40.0, 31.0, 38.0],
    );
    // a third window after a 40 point fall
    recs.extend(window(at(-170), None, &[0.0, 15.0]));
    let st = compute7(&recs);
    assert_eq!(st.session.windows, 3);
    let peaks: Vec<f64> = st.session.peaks.iter().map(|p| p.peak).collect();
    assert_eq!(peaks, [60.0, 40.0, 15.0]);
    assert!(st.session.peaks.iter().all(|p| p.resets_at.is_none()));
    // a lone fall is a window boundary, not a reset event
    assert!(st.resets.is_empty());
}

#[test]
fn plan_change_reset_is_reported_and_excluded() {
    let reset_a = at(-1000);
    let reset_b = at(-700);
    let (reset_c, reset_w) = (at(-400), far());
    let mut recs = window(at(-1200), Some(reset_a), &[10.0, 40.0]);
    // window B is at 70% with 4 h to go, then the next reading shows 2% in a brand new window
    let b_start = at(-940);
    recs.extend(window(
        b_start,
        Some(reset_b + Duration::hours(4)),
        &[30.0, 70.0],
    ));
    recs.extend(window(at(-880), Some(reset_c), &[2.0, 20.0]));
    // weekly follows: 50% -> 3% at the same time
    for (t, w, rs) in [
        (b_start + Duration::minutes(15), 50.0, far()),
        (at(-880), 3.0, reset_w),
    ] {
        recs.push(
            R {
                t: t + Duration::seconds(1),
                s: None,
                w: Some((w, Some(rs))),
                backfill: false,
            }
            .reading(),
        );
    }
    let st = compute7(&recs);
    let kinds: Vec<_> = st.resets.iter().map(|e| (e.kind, e.cause)).collect();
    assert!(
        kinds.contains(&(Kind::Session, ResetCause::Early)),
        "{:?}",
        st.resets
    );
    let ev = st.resets.iter().find(|e| e.kind == Kind::Session).unwrap();
    assert_eq!(ev.at, at(-880));
    assert_eq!(ev.before_pct, 70.0);
    assert!(st.resets.windows(2).all(|w| w[0].at <= w[1].at));

    let s = &st.session;
    assert_eq!((s.windows, s.ended_early, s.counted()), (3, 1, 2));
    // window B (peak 70) is out of the averages
    assert!(near(s.avg_peak.unwrap(), (40.0 + 20.0) / 2.0));
    assert_eq!(s.max_peak, Some(40.0));
    assert!(s.peaks.iter().any(|p| p.ended_early && p.peak == 70.0));
}

#[test]
fn a_drop_after_the_scheduled_reset_is_not_an_event() {
    // reset due at -90, next reading at -80 shows the fall: normal
    let mut recs = window(at(-300), Some(at(-90)), &[40.0, 90.0]);
    recs.extend(window(at(-80), Some(at(210)), &[3.0, 10.0]));
    // 20 minutes before the scheduled reset (-400): still normal
    let mut early_but_close = window(at(-600), Some(at(-400)), &[40.0, 90.0]);
    early_but_close.extend(window(at(-420), Some(at(-100)), &[3.0]));
    assert!(compute7(&recs).resets.is_empty());
    assert!(compute7(&early_but_close).resets.is_empty());
}

#[test]
fn both_windows_dropping_to_zero_together_is_a_reset() {
    let mk = |t: i64, s: f64, w: f64| {
        R {
            t: at(t),
            s: Some((s, None)),
            w: Some((w, None)),
            backfill: false,
        }
        .reading()
    };
    let recs = vec![
        mk(-120, 20.0, 30.0),
        mk(-105, 50.0, 31.0),
        mk(-90, 0.0, 0.0), // plan change
        mk(-75, 4.0, 1.0),
        mk(-60, 40.0, 2.0),
        mk(-45, 5.0, 3.0), // an ordinary session roll-over: weekly keeps counting
    ];
    let st = compute7(&recs);
    assert_eq!(st.resets.len(), 2, "{:?}", st.resets);
    assert!(st
        .resets
        .iter()
        .all(|e| e.at == at(-90) && e.cause == ResetCause::Together));
    let by = |k| st.resets.iter().find(|e| e.kind == k).unwrap().before_pct;
    assert_eq!((by(Kind::Session), by(Kind::Weekly)), (50.0, 31.0));
    // windows: session 20,50 | cut | 0,4,40 | 5; weekly 30,31 | 0,1,2,3
    assert_eq!(st.session.windows, 3);
    assert_eq!(st.session.ended_early, 1);
    assert_eq!(st.weekly.windows, 2);
    assert_eq!(st.weekly.ended_early, 1);
    assert_eq!(st.weekly.max_peak, Some(3.0));
}

#[test]
fn empty_and_single_reading() {
    let st = compute(&[], StatsRange::All, now(), 0);
    assert_eq!((st.session.windows, st.weekly.windows), (0, 0));
    assert_eq!(st.session.avg_peak, None);
    assert_eq!(st.session.median_peak, None);
    assert_eq!(st.session.hit_rate, None);
    assert_eq!(st.session.trend, None);
    assert_eq!(st.session.burn.median, None);
    assert_eq!(st.limit_risk, None);
    assert!(st.resets.is_empty());
    let c = &st.coverage;
    assert_eq!((c.readings, c.failed_polls), (0, 0));
    assert_eq!(
        (c.first, c.last, c.covered_pct, c.longest_gap_hours),
        (None, None, None, 0.0)
    );
    assert_eq!(st.session.heatmap.busiest_cell(), None);
    assert_eq!(st.session.heatmap.max_avg(), None);

    // a finished window (its reset time has passed)
    let one = window(at(-10), Some(at(-5)), &[33.0]);
    let st = compute(&one, StatsRange::Last30d, now(), 0);
    assert_eq!(st.session.windows, 1);
    assert_eq!(st.session.avg_peak, Some(33.0));
    assert_eq!(st.session.median_peak, Some(33.0));
    assert_eq!(st.session.trend, None);
    assert_eq!(st.session.burn.samples, 0);
    assert_eq!(st.session.heatmap.busiest_cell(), None);
    assert_eq!(st.coverage.readings, 1);
    assert_eq!(st.coverage.first, Some(at(-10)));
    assert!(near(st.coverage.longest_gap_hours, 10.0 / 60.0));
    assert!(near(st.coverage.covered_pct.unwrap(), 100.0));

    // failed polls alone leave no data but are counted
    let failed =
        Reading::from_value(&json!({"ok": false, "ts": at(-5).to_rfc3339(), "error": "x"}));
    let st = compute(&[failed], StatsRange::All, now(), 0);
    assert_eq!((st.coverage.readings, st.coverage.failed_polls), (0, 1));
    assert_eq!(st.session.windows, 0);
}

#[test]
fn readings_without_a_timestamp_or_in_the_future_are_ignored() {
    let mut recs = window(at(-60), Some(at(-30)), &[10.0, 20.0]);
    recs.push(Reading::from_value(
        &json!({"ok": true, "session": {"pct": 99.0}}),
    ));
    recs.push(Reading::from_value(
        &json!({"ok": true, "ts": "garbage", "session": {"pct": 99.0}}),
    ));
    recs.extend(window(at(30), Some(at(100)), &[88.0]));
    let st = compute7(&recs);
    assert_eq!(st.session.max_peak, Some(20.0));
    assert_eq!(st.coverage.readings, 2);
}

#[test]
fn backfilled_rows_count_in_stats_but_separately_in_coverage() {
    let mut recs = Vec::new();
    // imported history: one hourly reading for 6 hours, 60% peak
    for i in 0..6 {
        recs.push(
            R {
                t: at(-720 + 60 * i),
                s: Some((10.0 * (i + 1) as f64, None)),
                w: None,
                backfill: true,
            }
            .reading(),
        );
    }
    // live: 90 minutes of 15-minute readings ending 30 minutes ago
    recs.extend(window(
        at(-120),
        Some(at(-10)),
        &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
    ));
    let st = compute7(&recs);
    assert_eq!(st.session.windows, 2); // the backfilled window and the live one
    assert_eq!(st.session.max_peak, Some(60.0));
    let c = &st.coverage;
    assert_eq!(
        (c.readings, c.live_readings, c.backfilled_readings),
        (13, 7, 6)
    );
    assert_eq!(c.first, Some(at(-720)));
    // all: 6 backfilled readings x 30 min + live (six 15 min gaps + 30 min) = 5 h of the 12 h span
    assert!(near(c.covered_pct.unwrap(), 5.0 / 12.0 * 100.0));
    // live only: six 15 min gaps + the last one capped at 30 min = 2 h of the 2 h span
    assert!(near(c.live_covered_pct.unwrap(), 100.0));
}

#[test]
fn coverage_gaps_and_failed_polls() {
    let mut recs = window(
        at(-420),
        Some(at(-100)),
        &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
    );
    for m in [-410, -300] {
        recs.push(Reading::from_value(
            &json!({"ok": false, "ts": at(m).to_rfc3339(), "error": "HTTP 429"}),
        ));
    }
    // outside the range: not counted
    recs.push(Reading::from_value(
        &json!({"ok": false, "ts": at(-60 * 24 * 20).to_rfc3339()}),
    ));
    let st = compute7(&recs);
    let c = &st.coverage;
    assert_eq!((c.readings, c.failed_polls), (9, 2));
    assert_eq!((c.first, c.last), (Some(at(-420)), Some(at(-300))));
    // 9 readings, 15 min apart: 2 h covered + 30 min after the last; the span is 7 h up to now
    assert!(near(c.covered_pct.unwrap(), 2.5 / 7.0 * 100.0));
    assert!(near(c.longest_gap_hours, 5.0));
}

#[test]
fn range_selection() {
    // one window per day for 100 days: readings at 06:00 and 07:00 UTC
    let mut recs = Vec::new();
    for d in 0..100 {
        let start = now() - Duration::days(d) - Duration::hours(6);
        recs.extend(window(
            start,
            Some(start + Duration::hours(5)),
            &[10.0, 50.0 + (d % 3) as f64],
        ));
    }
    let n = |r| compute(&recs, r, now(), 0).session.windows;
    assert_eq!(n(StatsRange::Last7d), 7);
    assert_eq!(n(StatsRange::Last30d), 30);
    assert_eq!(n(StatsRange::Last90d), 90);
    assert_eq!(n(StatsRange::All), 100);
    assert_eq!(
        compute(&recs, StatsRange::Last7d, now(), 0)
            .coverage
            .readings,
        14
    );
    assert_eq!(
        StatsRange::ALL.map(StatsRange::label),
        ["7d", "30d", "90d", "All"]
    );
}

#[test]
fn a_window_straddling_the_range_start_keeps_its_full_peak() {
    let reset = at(-60 * 24 * 7 + 120);
    let mut recs = window(at(-60 * 24 * 7 - 120), Some(reset), &[90.0, 95.0]);
    recs.extend(window(at(-60 * 24 * 7 + 30), Some(reset), &[96.0]));
    let st = compute7(&recs);
    assert_eq!(st.session.windows, 1);
    assert_eq!(st.session.max_peak, Some(96.0));
    // the pre-range readings themselves are not part of the coverage
    assert_eq!(st.coverage.readings, 1);
}

#[test]
fn trend_up_down_flat() {
    let peaks = |v: &[f64]| compute7(&windows_with_peaks(-5400, v)).session.trend;

    let up = peaks(&[20.0, 20.0, 20.0, 50.0, 50.0, 50.0]).unwrap();
    assert_eq!(up.direction, Direction::Up);
    assert_eq!(up.windows, 3);
    assert!(near(up.delta, 30.0));
    assert!(near(up.recent_avg, 50.0) && near(up.previous_avg, 20.0));

    let down = peaks(&[90.0, 80.0, 70.0, 40.0, 30.0, 20.0]).unwrap();
    assert_eq!(down.direction, Direction::Down);
    assert!(near(down.delta, -50.0));

    let flat = peaks(&[50.0, 52.0, 51.0, 53.0]).unwrap();
    assert_eq!((flat.direction, flat.windows), (Direction::Flat, 2));
    assert!(near(flat.delta, 1.0));
    // exactly 5 points counts as a change
    let edge = peaks(&[50.0, 50.0, 55.0, 55.0]).unwrap();
    assert_eq!(edge.direction, Direction::Up);

    // only the newest 5 are compared with the 5 before them
    let long = peaks(&[
        99.0, 99.0, 99.0, 30.0, 30.0, 30.0, 30.0, 30.0, 60.0, 60.0, 60.0, 60.0, 60.0,
    ])
    .unwrap();
    assert_eq!(
        (long.windows, long.previous_avg, long.recent_avg),
        (5, 30.0, 60.0)
    );
    // too few windows
    assert_eq!(peaks(&[10.0, 90.0, 40.0]), None);
    assert_eq!(peaks(&[10.0]), None);
}

#[test]
fn burn_rate_and_limit_risk() {
    // window A: gains 2, 4, 6, 8 per 15 min (8, 16, 24, 32 pts/h), then a flat reading,
    // then a 45 minute gap with a gain (not "active")
    let a_start = at(-1000);
    let mut recs = window(
        a_start,
        Some(a_start + Duration::hours(5)),
        &[0.0, 2.0, 6.0, 12.0, 20.0, 20.0],
    );
    recs.push(
        R {
            t: a_start + Duration::minutes(75 + 45),
            s: Some((30.0, Some(a_start + Duration::hours(5)))),
            w: None,
            backfill: false,
        }
        .reading(),
    );
    // window B: gentle, 1 point per 15 minutes -> 4 pts/h, 20 over a whole window
    let b_start = at(-600);
    recs.extend(window(
        b_start,
        Some(b_start + Duration::hours(5)),
        &[0.0, 1.0, 2.0, 3.0, 4.0],
    ));
    // window C: hits the limit without a sustained active pace we can measure
    let c_start = at(-300);
    recs.extend(window(
        c_start,
        Some(c_start + Duration::hours(5)),
        &[90.0, 100.0],
    ));
    let st = compute7(&recs);
    let b = &st.session.burn;
    // samples: A 8, 16, 24, 32 ; B 4, 4, 4, 4 ; C 40
    assert_eq!(b.samples, 9);
    assert!(near(b.median.unwrap(), 8.0), "{b:?}");
    // sorted: 4,4,4,4,8,16,24,32,40 -> p90 at position 7.2 = 32 + 0.2 * 8
    assert!(near(b.p90.unwrap(), 33.6));
    // A: 20 points over 1 h active = 20/h * 5 h = 100 -> at risk. B: 4/h * 5 = 20 -> not. C hit.
    let risk = st.limit_risk.unwrap();
    assert_eq!((risk.windows, risk.at_risk), (3, 2));
    assert!(near(risk.risk_pct, 200.0 / 3.0));
}

#[test]
fn limit_risk_ignores_windows_it_cannot_judge() {
    // a single reading: no activity measured, did not hit
    let recs = window(at(-300), Some(at(0)), &[50.0]);
    assert_eq!(compute7(&recs).limit_risk, None);
}

/// Wednesday 10:00-11:00 UTC, +4 points every 15 minutes.
fn wednesday_morning() -> Vec<Reading> {
    let start = Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap();
    // `now()` is 12:00 that day
    window(
        start,
        Some(start + Duration::hours(5)),
        &[10.0, 14.0, 18.0, 22.0, 26.0],
    )
}

#[test]
fn heatmap_buckets_by_local_weekday_and_hour() {
    let recs = wednesday_morning();
    let cell = |tz: i32| {
        let st = compute(&recs, StatsRange::Last7d, now(), tz);
        let h = &st.session.heatmap;
        let b = h.busiest_cell().unwrap();
        assert!(near(h.gain.iter().flatten().sum::<f64>(), 16.0));
        assert!(near(h.hours.iter().flatten().sum::<f64>(), 1.0));
        (b.weekday, b.hour, b.pts_per_hour)
    };
    assert_eq!(cell(0), (2, 10, 16.0)); // Wednesday 10:00 UTC
    assert_eq!(cell(120), (2, 12, 16.0)); // UTC+2
    assert_eq!(cell(-600), (2, 0, 16.0)); // UTC-10: Wednesday 00:00
    assert_eq!(cell(840), (3, 0, 16.0)); // UTC+14 rolls over into Thursday 00:00
                                         // UTC+5:30 puts the hour across two local hours; UTC-10:01 across Tuesday 23:xx / Wednesday 00:xx
    let st = compute(&recs, StatsRange::Last7d, now(), 330);
    let h = &st.session.heatmap;
    assert!(near(h.gain[2][15], 8.0) && near(h.gain[2][16], 8.0));
    let st = compute(&recs, StatsRange::Last7d, now(), -601);
    let h = &st.session.heatmap;
    assert!(near(h.hours[1][23], 1.0 / 60.0) && near(h.hours[2][0], 59.0 / 60.0));
}

#[test]
fn heatmap_splits_an_interval_across_hours() {
    let start = Utc.with_ymd_and_hms(2026, 10, 14, 10, 45, 0).unwrap();
    // 10:45 -> 11:15, +6 points: half the interval in hour 10, half in hour 11
    let recs = window(start, Some(start + Duration::hours(5)), &[10.0]);
    let mut recs = recs;
    recs.extend(window(
        start + Duration::minutes(30),
        Some(start + Duration::hours(5)),
        &[16.0],
    ));
    let st = compute(&recs, StatsRange::Last7d, now(), 0);
    let h = &st.session.heatmap;
    assert!(near(h.gain[2][10], 3.0) && near(h.hours[2][10], 0.25));
    assert!(near(h.gain[2][11], 3.0) && near(h.hours[2][11], 0.25));
    assert!(near(h.avg(2, 10).unwrap(), 12.0));
    assert_eq!(h.avg(2, 9), None);
    assert_eq!(h.avg(3, 10), None);
    // half an hour of exposure is below the "busiest" threshold
    assert_eq!(h.busiest_cell(), None);
    assert!(near(h.max_avg().unwrap(), 12.0));
}

#[test]
fn heatmap_skips_resets_negative_deltas_and_long_gaps() {
    let start = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
    let reset = start + Duration::hours(5);
    let mut recs = window(start, Some(reset), &[10.0, 30.0]); // +20 in hour 6
                                                              // a small dip (< 10 points) inside the window: exposure but no gain
    recs.extend(window(start + Duration::minutes(30), Some(reset), &[27.0]));
    // a 3 hour gap with a gain: not placed on the map
    recs.extend(window(start + Duration::minutes(210), Some(reset), &[60.0]));
    // a new window (different reset) is not connected to the old one
    recs.extend(window(
        start + Duration::minutes(225),
        Some(reset + Duration::hours(5)),
        &[1.0],
    ));
    let st = compute(&recs, StatsRange::Last7d, now(), 0);
    let h = &st.session.heatmap;
    assert!(near(h.gain.iter().flatten().sum::<f64>(), 20.0));
    assert!(near(h.hours[2][6], 0.5)); // 06:00-06:15 and 06:15-06:30
    assert!(near(h.hours.iter().flatten().sum::<f64>(), 0.5));
}

#[test]
fn busiest_weekday_and_hour() {
    // Wednesday 10:00-11:00 at 16 pts/h, and Monday the 12th 20:00-21:00 at 40 pts/h
    let mut recs = wednesday_morning();
    let mon = Utc.with_ymd_and_hms(2026, 10, 12, 20, 0, 0).unwrap();
    recs.extend(window(
        mon,
        Some(mon + Duration::hours(5)),
        &[0.0, 10.0, 20.0, 30.0, 40.0],
    ));
    let st = compute7(&recs);
    let h = &st.session.heatmap;
    let cell = h.busiest_cell().unwrap();
    assert_eq!((cell.weekday, cell.hour), (0, 20));
    assert!(near(cell.pts_per_hour, 40.0));
    let (wd, v) = h.busiest_weekday().unwrap();
    assert_eq!(wd, 0);
    assert!(near(v, 40.0));
    let (hr, v) = h.busiest_hour().unwrap();
    assert_eq!(hr, 20);
    assert!(near(v, 40.0));
    assert_eq!(WEEKDAYS[wd], "Mon");
}

#[test]
fn kind_accessor_and_weekly_windows() {
    let reset_w = at(-10);
    let mut recs = Vec::new();
    for (i, pct) in [10.0, 20.0, 30.0].into_iter().enumerate() {
        recs.push(
            R {
                t: at(-60 + 15 * i as i64),
                s: None,
                w: Some((pct, Some(reset_w))),
                backfill: false,
            }
            .reading(),
        );
    }
    let st = compute7(&recs);
    assert_eq!(st.kind(Kind::Weekly).windows, 1);
    assert_eq!(st.kind(Kind::Session).windows, 0);
    assert_eq!(st.weekly.kind, Kind::Weekly);
    assert_eq!(st.weekly.max_peak, Some(30.0));
    assert!(st.weekly.burn.samples == 2 && near(st.weekly.burn.median.unwrap(), 40.0));
}

// ---- the window in progress, early-reset peaks, session-to-weekly ratio ----------------------

/// A window whose reset is still ahead, started an hour ago, now at 5%.
fn in_progress_window() -> Vec<Reading> {
    window(at(-60), Some(at(240)), &[1.0, 2.0, 3.0, 4.0, 5.0])
}

#[test]
fn the_window_in_progress_is_reported_apart_and_left_out_of_the_aggregates() {
    let mut recs = windows_with_peaks(-3000, &[38.0, 38.0, 38.0, 38.0]);
    recs.extend(in_progress_window());
    let st = compute7(&recs);
    let s = &st.session;
    assert_eq!((s.windows, s.ended_early, s.counted()), (4, 0, 4));
    assert_eq!(s.avg_peak, Some(38.0));
    assert_eq!(s.median_peak, Some(38.0));
    assert_eq!(s.max_peak, Some(38.0));
    assert_eq!((s.hits, s.near_limit), (0, 0));
    // an in-progress window at 3-5% must not look like a collapse
    let t = s.trend.expect("4 complete windows give a trend");
    assert_eq!(t.direction, Direction::Flat);
    assert!(near(t.delta, 0.0) && t.windows == 2);
    let c = s.current.as_ref().expect("current window");
    assert!(near(c.pct, 5.0));
    assert_eq!(c.readings, 5);
    assert_eq!(c.resets_at, Some(at(240)));
    assert_eq!(c.started_at, at(-60)); // resets_at - 5 h
    assert!(near(c.elapsed_h, 1.0));
    // still listed for charts, flagged
    assert_eq!(s.peaks.len(), 5);
    assert!(s.peaks.last().unwrap().in_progress);
    assert!(s.peaks[..4].iter().all(|p| !p.in_progress));
    assert_eq!(st.weekly.current, None);
}

#[test]
fn too_few_complete_windows_means_no_trend_even_with_one_in_progress() {
    let mut recs = windows_with_peaks(-3000, &[60.0, 60.0, 60.0]);
    recs.extend(in_progress_window());
    let s = compute7(&recs).session;
    assert_eq!(s.counted(), 3);
    assert_eq!(s.trend, None); // 3 complete windows: one per side is not enough
    assert!(s.current.is_some());
    // only the window in progress: nothing finished at all
    let s = compute7(&in_progress_window()).session;
    assert_eq!(
        (s.windows, s.avg_peak, s.hit_rate, s.trend),
        (0, None, None, None)
    );
    assert!(s.current.is_some());
}

#[test]
fn trend_up_down_flat_still_work_with_a_window_in_progress() {
    for (peaks, dir) in [
        ([20.0, 20.0, 60.0, 60.0], Direction::Up),
        ([60.0, 60.0, 20.0, 20.0], Direction::Down),
        ([40.0, 42.0, 41.0, 40.0], Direction::Flat),
    ] {
        let mut recs = windows_with_peaks(-3000, &peaks);
        recs.extend(in_progress_window());
        assert_eq!(
            compute7(&recs).session.trend.unwrap().direction,
            dir,
            "{peaks:?}"
        );
    }
}

#[test]
fn a_finished_window_at_the_limit_is_not_in_progress_and_the_in_progress_one_is_not_a_risk() {
    let mut recs = windows_with_peaks(-3000, &[100.0]);
    // in progress and already at 95%: neither a hit nor a risk until it has finished
    recs.extend(window(at(-60), Some(at(240)), &[50.0, 80.0, 95.0]));
    let st = compute7(&recs);
    assert_eq!((st.session.hits, st.session.near_limit), (1, 1));
    assert_eq!(st.session.windows, 1);
    assert_eq!(st.limit_risk.map(|r| r.windows), Some(1));
    assert!(near(st.session.current.as_ref().unwrap().pct, 95.0));
}

#[test]
fn windows_without_a_reset_time_are_in_progress_within_twice_their_longest_gap() {
    let cur = |recs: Vec<Reading>| compute7(&recs).session.current;
    // 15 min gaps -> grace 30 min
    assert!(cur(window(at(-55), None, &[1.0, 2.0, 3.0])).is_some()); // last reading 25 min ago
    assert!(cur(window(at(-61), None, &[1.0, 2.0, 3.0])).is_none()); // 31 min ago
                                                                     // a single reading has no gap: the 20 minute minimum applies
    assert!(cur(window(at(-19), None, &[5.0])).is_some());
    assert!(cur(window(at(-21), None, &[5.0])).is_none());
    // a wider polling gap widens the grace: 60 min gaps -> 2 h, last reading 100 min ago
    let slow: Vec<Reading> = [(-220, 1.0), (-160, 2.0), (-100, 3.0)]
        .into_iter()
        .map(|(m, p)| {
            R {
                t: at(m),
                s: Some((p, None)),
                w: None,
                backfill: false,
            }
            .reading()
        })
        .collect();
    assert!(near(cur(slow).expect("within 2 x 60 min").pct, 3.0));
}

#[test]
fn a_stale_window_without_a_reset_time_is_complete_and_counted() {
    let s = compute7(&window(at(-600), None, &[10.0, 40.0])).session;
    assert!(s.current.is_none());
    assert_eq!((s.windows, s.max_peak), (1, Some(40.0)));
}

#[test]
fn null_reset_current_window_starts_at_its_first_reading() {
    let c = compute7(&window(at(-45), None, &[1.0, 2.0, 3.0, 4.0]))
        .session
        .current
        .unwrap();
    assert_eq!(c.started_at, at(-45));
    assert!(near(c.elapsed_h, 0.75));
    assert_eq!((c.resets_at, c.readings), (None, 4));
}

/// Three ordinary windows, then a window that reached 90% and was cut short by a plan change
/// (its reset time was still hours away), then the new window.
fn plan_change() -> Vec<Reading> {
    let mut recs = windows_with_peaks(-3000, &[30.0, 40.0, 50.0]);
    recs.extend(window(at(-240), Some(at(60)), &[20.0, 90.0, 85.0]));
    recs.extend(window(at(-195), Some(at(200)), &[3.0, 4.0]));
    recs
}

#[test]
fn a_cut_short_window_peaking_at_90_is_listed_not_averaged() {
    let st = compute7(&plan_change());
    let s = &st.session;
    assert_eq!((s.windows, s.ended_early, s.counted()), (4, 1, 3));
    // normal aggregates ignore it ...
    assert_eq!(s.avg_peak, Some(40.0));
    assert_eq!(s.max_peak, Some(50.0));
    assert_eq!((s.hit_rate, s.hits, s.near_limit), (Some(0.0), 0, 0));
    // ... but it is visible
    assert_eq!(s.cut_short.len(), 1);
    let c = &s.cut_short[0];
    assert!(c.ended_early && near(c.peak, 90.0));
    assert_eq!(c.cause, Some(ResetCause::Early));
    assert_eq!((c.first, c.last), (at(-240), at(-210)));
    assert_eq!((s.hit_before_reset, s.limit_before_reset), (1, 0));
    // the event carries the window's peak, not only the last reading before the fall
    assert_eq!(st.resets.len(), 1);
    let e = &st.resets[0];
    assert!(near(e.before_pct, 85.0) && near(e.peak, 90.0));
    assert_eq!(e.cause, ResetCause::Early);
    // the new window is the one in progress
    assert!(near(s.current.as_ref().unwrap().pct, 4.0));
    assert!(s.cut_short.iter().all(|w| !w.in_progress));
}

#[test]
fn hitting_the_limit_before_a_reset_counts_separately_at_99_5() {
    let mut recs = windows_with_peaks(-3000, &[30.0, 40.0]);
    recs.extend(window(at(-240), Some(at(60)), &[20.0, 99.5]));
    recs.extend(window(at(-225), Some(at(200)), &[3.0]));
    let s = compute7(&recs).session;
    assert_eq!((s.hit_before_reset, s.limit_before_reset), (1, 1));
    assert_eq!((s.hits, s.hit_rate), (0, Some(0.0)));
}

#[test]
fn together_resets_also_carry_the_peak() {
    let mut recs = Vec::new();
    for (m, sp, wp) in [
        (-90, 20.0, 40.0),
        (-75, 80.0, 45.0),
        (-60, 75.0, 46.0),
        (-45, 0.0, 0.0),
    ] {
        recs.push(
            R {
                t: at(m),
                s: Some((sp, None)),
                w: Some((wp, None)),
                backfill: false,
            }
            .reading(),
        );
    }
    let st = compute7(&recs);
    let e = st.resets.iter().find(|e| e.kind == Kind::Session).unwrap();
    assert_eq!(e.cause, ResetCause::Together);
    assert!(near(e.before_pct, 75.0) && near(e.peak, 80.0));
    assert_eq!(st.session.cut_short[0].cause, Some(ResetCause::Together));
}

// ratio

/// One finished session window per `(session pts, weekly pts)` entry: two readings 15 min apart
/// (session 0 -> s, weekly 10 -> 10 + w).
fn ratio_windows(gains: &[(f64, f64)]) -> Vec<Reading> {
    let mut out = Vec::new();
    for (i, (s, w)) in gains.iter().enumerate() {
        let start = at(-3000 + 360 * i as i64);
        let reset = Some(start + Duration::hours(5));
        for (k, (sp, wp)) in [(0.0, 10.0), (*s, 10.0 + *w)].into_iter().enumerate() {
            out.push(
                R {
                    t: start + Duration::minutes(15 * k as i64),
                    s: Some((sp, reset)),
                    w: Some((wp, Some(far()))),
                    backfill: false,
                }
                .reading(),
            );
        }
    }
    out
}

fn ratio_of(gains: &[(f64, f64)]) -> Stats {
    compute7(&ratio_windows(gains))
}

#[test]
fn ratio_math_with_the_users_real_examples() {
    // +12 session / +2 weekly, +14 / +1, plus one more window: 5 weekly points over 39 session points
    let st = ratio_of(&[(12.0, 2.0), (14.0, 1.0), (13.0, 2.0)]);
    let r = st.session_to_weekly.expect("5 weekly points, 3 windows");
    assert_eq!(r.samples, 3);
    assert!(near(r.weekly_points, 5.0));
    assert!(near(r.points_per_session, 5.0 / 39.0 * 100.0));
    // rounding spread: (W -/+ n) / (S +/- n)
    assert!(near(r.low, 2.0 / 42.0 * 100.0));
    assert!(near(r.high, 8.0 / 36.0 * 100.0));
    assert!(r.low < r.points_per_session && r.points_per_session < r.high);
    assert_eq!(r.confidence, Confidence::Rough);
}

#[test]
fn the_two_real_examples_alone_stay_hidden() {
    let st = ratio_of(&[(12.0, 2.0), (14.0, 1.0)]);
    assert_eq!(st.session_to_weekly, None);
    // 3 weekly points over 2 windows: short on both counts
    assert_eq!(st.ratio_progress.windows, 2);
    assert!(near(st.ratio_progress.weekly_points, 3.0));
}

#[test]
fn enough_windows_but_too_few_weekly_points_is_hidden() {
    let st = ratio_of(&[(12.0, 2.0), (14.0, 1.0), (13.0, 1.0)]);
    assert_eq!(st.session_to_weekly, None);
    assert_eq!(st.ratio_progress.windows, 3);
    assert!(near(st.ratio_progress.weekly_points, 4.0));
}

#[test]
fn enough_weekly_points_but_too_few_windows_is_hidden() {
    let st = ratio_of(&[(30.0, 6.0), (30.0, 6.0)]);
    assert_eq!(st.session_to_weekly, None);
    assert_eq!(st.ratio_progress.windows, 2);
    assert!(near(st.ratio_progress.weekly_points, 12.0));
}

#[test]
fn ratio_confidence_tiers_and_their_boundaries() {
    let total = |w: f64| ratio_of(&[(12.0, 1.0), (12.0, 1.0), (12.0, w - 2.0)]).session_to_weekly;
    assert_eq!(total(4.9), None);
    for (w, c) in [
        (5.0, Confidence::Rough),
        (9.9, Confidence::Rough),
        (10.0, Confidence::Fair),
        (19.9, Confidence::Fair),
        (20.0, Confidence::Good),
    ] {
        assert_eq!(
            total(w).unwrap_or_else(|| panic!("{w} hidden")).confidence,
            c,
            "{w}"
        );
    }
    assert_eq!(Confidence::Rough.label(), "rough");
    assert_eq!(
        (MIN_WEEKLY_POINTS, FAIR_WEEKLY_POINTS, GOOD_WEEKLY_POINTS),
        (5.0, 10.0, 20.0)
    );
}

#[test]
fn ratio_ignores_tiny_unfinished_and_cut_short_windows() {
    let mut recs = ratio_windows(&[(12.0, 2.0), (14.0, 1.0), (13.0, 2.0), (3.0, 3.0)]); // last: < 5 session pts
                                                                                        // a window in progress with a big gain, and its weekly cost
    recs.push(
        R {
            t: at(-30),
            s: Some((0.0, Some(at(240)))),
            w: Some((20.0, Some(far()))),
            backfill: false,
        }
        .reading(),
    );
    recs.push(
        R {
            t: at(-15),
            s: Some((40.0, Some(at(240)))),
            w: Some((30.0, Some(far()))),
            backfill: false,
        }
        .reading(),
    );
    let st = compute7(&recs);
    let r = st.session_to_weekly.unwrap();
    assert_eq!((r.samples, near(r.weekly_points, 5.0)), (3, true));
}

#[test]
fn a_weekly_reset_inside_a_session_window_does_not_make_a_negative_cost() {
    let mut recs = ratio_windows(&[(12.0, 2.0), (14.0, 1.0), (13.0, 2.0)]);
    // a window where the weekly counter fell (it reset): that pair is skipped
    let start = at(-3000 + 360 * 3);
    for (k, (sp, wp)) in [(0.0, 90.0), (20.0, 2.0)].into_iter().enumerate() {
        recs.push(
            R {
                t: start + Duration::minutes(15 * k as i64),
                s: Some((sp, Some(start + Duration::hours(5)))),
                w: Some((wp, Some(far()))),
                backfill: false,
            }
            .reading(),
        );
    }
    let st = compute7(&recs);
    assert!(near(st.ratio_progress.weekly_points, 5.0));
    assert!(st.session_to_weekly.unwrap().points_per_session > 0.0);
}

// ---- plan eras: the ratio never crosses a plan change ------------------------------------------

/// Finished session windows at `starts` minutes (relative to `now`), each with two readings 15
/// minutes apart: session 0 -> s, weekly `base` -> `base + w`.
fn ratio_block(starts: &[i64], gains: &[(f64, f64)], base: f64, backfill: bool) -> Vec<Reading> {
    let mut out = Vec::new();
    for (start, (s, w)) in starts.iter().zip(gains) {
        let t0 = at(*start);
        let reset = Some(t0 + Duration::hours(5));
        for (k, (sp, wp)) in [(0.0, base), (*s, base + *w)].into_iter().enumerate() {
            out.push(
                R {
                    t: t0 + Duration::minutes(15 * k as i64),
                    s: Some((sp, reset)),
                    w: Some((wp, Some(far()))),
                    backfill,
                }
                .reading(),
            );
        }
    }
    out
}

/// Pre-change windows, the plan change (both windows fall at 985 minutes ago, with their reset
/// times still far away), then post-change windows.
fn plan_change_fixture(
    pre: &[(f64, f64)],
    pre_backfill: bool,
    post: &[(f64, f64)],
    post_backfill: bool,
) -> Vec<Reading> {
    let mut recs = ratio_block(&[-3000, -2640, -2280], pre, 10.0, pre_backfill);
    let rd = |m: i64, sp: f64, sr: i64, wp: f64| {
        R {
            t: at(m),
            s: Some((sp, Some(at(sr)))),
            w: Some((wp, Some(far()))),
            backfill: false,
        }
        .reading()
    };
    recs.push(rd(-1000, 40.0, 100, 50.0));
    recs.push(rd(-985, 0.0, 315, 0.0));
    recs.extend(ratio_block(&[-960, -680, -400], post, 0.0, post_backfill));
    recs
}

#[test]
fn a_fixture_shaped_like_the_users_stays_hidden_after_a_plan_change() {
    // 20 weekly points from sparse imported windows before the change, 3 measured since
    let recs = plan_change_fixture(
        &[(12.0, 6.0), (12.0, 7.0), (12.0, 7.0)],
        true,
        &[(12.0, 1.0), (14.0, 1.0), (13.0, 1.0)],
        false,
    );
    let st = compute7(&recs);
    assert_eq!(st.era_start, Some(at(-985)));
    assert_eq!(st.session_to_weekly, None);
    let p = st.ratio_progress;
    assert!(near(p.weekly_points, 3.0) && p.windows == 3 && p.since_plan_change);
    // both windows' resets are reported as events
    assert_eq!(st.resets.len(), 2);
}

#[test]
fn an_era_with_enough_points_shows_and_excludes_the_windows_before_the_change() {
    let recs = plan_change_fixture(
        &[(12.0, 6.0), (12.0, 7.0), (12.0, 7.0)],
        true,
        &[(12.0, 2.0), (14.0, 2.0), (13.0, 2.0)],
        false,
    );
    let r = compute7(&recs)
        .session_to_weekly
        .expect("6 points since the change");
    assert_eq!(r.samples, 3);
    assert!(near(r.weekly_points, 6.0));
    assert!(near(r.points_per_session, 6.0 / 39.0 * 100.0));
}

#[test]
fn a_backfill_only_era_is_capped_at_rough() {
    let post = [(12.0, 10.0), (12.0, 10.0), (12.0, 10.0)]; // 30 points: "good" if measured live
    let live = compute7(&plan_change_fixture(&[], false, &post, false));
    assert_eq!(live.session_to_weekly.unwrap().confidence, Confidence::Good);
    let imported = compute7(&plan_change_fixture(&[], false, &post, true));
    let r = imported.session_to_weekly.unwrap();
    assert_eq!(r.confidence, Confidence::Rough);
    assert!(near(r.weekly_points, 30.0));
}

#[test]
fn without_a_plan_change_nothing_is_cut_off() {
    let st = ratio_of(&[(12.0, 2.0), (14.0, 1.0), (13.0, 2.0)]);
    assert_eq!(st.era_start, None);
    assert!(!st.ratio_progress.since_plan_change);
    assert!(st.session_to_weekly.is_some());
}

#[test]
fn only_one_window_resetting_early_does_not_start_an_era() {
    // a session-only early reset (the weekly counter keeps rising) is not a plan change
    let mut recs = ratio_windows(&[(12.0, 2.0), (14.0, 1.0), (13.0, 2.0)]);
    for (m, sp, sr) in [(-1000, 40.0, 100), (-985, 0.0, 315)] {
        recs.push(
            R {
                t: at(m),
                s: Some((sp, Some(at(sr)))),
                w: Some((20.0 + (m + 1000) as f64 / 100.0, Some(far()))),
                backfill: false,
            }
            .reading(),
        );
    }
    let st = compute7(&recs);
    assert!(st.resets.iter().any(|e| e.kind == Kind::Session));
    assert_eq!(st.era_start, None);
}
