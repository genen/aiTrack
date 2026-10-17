// SPDX-License-Identifier: GPL-3.0-or-later
//! Print the long-term plan-usage statistics of a usage.jsonl as plain text (no UI):
//!
//!   cargo run --example stats_demo -- C:\Users\you\.aitrack\usage.jsonl [7|30|90|all] [--tz MINUTES]
//!
//! The range defaults to 30 days and the timezone to the system's current UTC offset.

use aitrack::reading::{Kind, Reading};
use aitrack::stats::{
    compute, KindStats, Stats, StatsRange, FAIR_WEEKLY_POINTS, GOOD_WEEKLY_POINTS,
    MIN_WEEKLY_POINTS, RATIO_MIN_SAMPLES, WEEKDAYS,
};
use aitrack::store::{JsonlStore, Store};
use chrono::{DateTime, Local, Utc};

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map_or("-".to_owned(), |x| format!("{x:.1}{unit}"))
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map_or("-".to_owned(), |t| {
        t.format("%Y-%m-%d %H:%M UTC").to_string()
    })
}

fn print_kind(k: &KindStats) {
    let name = match k.kind {
        Kind::Session => "Session (5 h)",
        Kind::Weekly => "Weekly (7 d)",
    };
    println!("\n== {name} ==");
    println!(
        "windows: {} finished ({} cut short by a reset, {} counted)",
        k.windows,
        k.ended_early,
        k.counted()
    );
    match &k.current {
        Some(c) => println!(
            "Current window: {:.0}% used, {:.1} h elapsed (started {}), {} ({} readings) - not in the numbers below",
            c.pct,
            c.elapsed_h,
            when(Some(c.started_at)),
            c.resets_at.map_or("reset time unknown".to_owned(), |r| format!(
                "resets in {:.1} h",
                (r - chrono::Utc::now()).num_minutes() as f64 / 60.0
            )),
            c.readings
        ),
        None => println!("Current window: none in progress"),
    }
    if k.cut_short.is_empty() {
        println!("Cut short by a reset: none");
    } else {
        let peaks: Vec<String> = k
            .cut_short
            .iter()
            .map(|w| {
                format!(
                    "{:.0}% ({} - {}, {})",
                    w.peak,
                    w.first.format("%m-%d %H:%M"),
                    w.last.format("%H:%M"),
                    w.cause.map_or("?".to_owned(), |c| format!("{c:?}"))
                )
            })
            .collect();
        let max = k.cut_short.iter().map(|w| w.peak).fold(0.0, f64::max);
        println!(
            "Cut short by a reset: {} windows, max peak {:.0}%, peaks {}; reached >= 90% before the reset: {}, >= 99.5%: {} (not in the hit rate above)",
            k.cut_short.len(),
            max,
            peaks.join(", "),
            k.hit_before_reset,
            k.limit_before_reset
        );
    }
    println!(
        "peak: avg {}  median {}  max {}",
        opt(k.avg_peak, "%"),
        opt(k.median_peak, "%"),
        opt(k.max_peak, "%")
    );
    println!(
        "hit rate: {} ({} windows)   >= 90%: {} windows",
        opt(k.hit_rate, "%"),
        k.hits,
        k.near_limit
    );
    match &k.trend {
        Some(t) => println!(
            "trend: {} ({:+.1} pts; last {} windows avg {:.1}% vs previous {:.1}%)",
            t.direction.label(),
            t.delta,
            t.windows,
            t.recent_avg,
            t.previous_avg
        ),
        None => println!("trend: not enough complete windows"),
    }
    println!(
        "burn while active: median {}  p90 {}  ({} samples)",
        opt(k.burn.median, " pts/h"),
        opt(k.burn.p90, " pts/h"),
        k.burn.samples
    );
    let h = &k.heatmap;
    if let Some(b) = h.busiest_cell() {
        println!(
            "busiest: {} {:02}:00 ({:.1} pts/h)",
            WEEKDAYS[b.weekday], b.hour, b.pts_per_hour
        );
    }
    if let Some((d, v)) = h.busiest_weekday() {
        println!("busiest weekday: {} ({v:.1} pts/h)", WEEKDAYS[d]);
    }
    if let Some((hr, v)) = h.busiest_hour() {
        println!("busiest hour: {hr:02}:00 ({v:.1} pts/h)");
    }
    if let Some(max) = h.max_avg().filter(|m| *m > 0.0) {
        println!("heat map (local time; digit = 0-9 scaled to the busiest cell, '.' = no data, ' ' = idle)");
        println!("     0         1         2");
        println!("     012345678901234567890123");
        for (d, name) in WEEKDAYS.iter().enumerate() {
            let row: String = (0..24)
                .map(|hr| match h.avg(d, hr) {
                    None => '.',
                    Some(v) if v <= 0.0 => ' ',
                    Some(v) => char::from_digit(((v / max * 9.0).round() as u32).clamp(1, 9), 10)
                        .unwrap_or('9'),
                })
                .collect();
            println!("{name}  {row}");
        }
    }
}

fn print_stats(st: &Stats) {
    println!(
        "Range {} as of {} (timezone UTC{:+}h)",
        st.range.label(),
        when(Some(st.now)),
        st.tz_offset_minutes as f64 / 60.0
    );
    let c = &st.coverage;
    println!("\n== Coverage ==");
    println!(
        "readings: {} ({} live, {} backfilled), failed polls: {}",
        c.readings, c.live_readings, c.backfilled_readings, c.failed_polls
    );
    println!("first: {}   last: {}", when(c.first), when(c.last));
    println!(
        "longest gap: {:.1} h   covered: {} (live only: {})",
        c.longest_gap_hours,
        opt(c.covered_pct, "%"),
        opt(c.live_covered_pct, "%")
    );
    print_kind(&st.session);
    print_kind(&st.weekly);
    match st.session_to_weekly {
        Some(r) => println!(
            "\nSession-to-weekly ratio: a full 5h session ~ {:.0}% of the weekly allowance ({}, {:.0} weekly points over {} windows; range {:.0}-{:.0}%; rough, whole-number percentages)",
            r.points_per_session,
            r.confidence.label(),
            r.weekly_points,
            r.samples,
            r.low,
            r.high
        ),
        None => println!(
            "\nSession-to-weekly ratio: not enough data yet ({:.0} of {:.0} weekly points measured{}, {} of {} windows; \"fair\" from {:.0}, \"good\" from {:.0} points)",
            st.ratio_progress.weekly_points,
            MIN_WEEKLY_POINTS,
            if st.ratio_progress.since_plan_change {
                " since the last plan change"
            } else {
                ""
            },
            st.ratio_progress.windows,
            RATIO_MIN_SAMPLES,
            FAIR_WEEKLY_POINTS,
            GOOD_WEEKLY_POINTS
        ),
    }
    match st.limit_risk {
        Some(r) => println!(
            "\nLimit risk (session): {:.0}% of {} windows hit the limit or would at their active pace",
            r.risk_pct, r.windows
        ),
        None => println!("\nLimit risk (session): not enough data"),
    }
    println!("\n== Early / unscheduled resets ({}) ==", st.resets.len());
    for e in &st.resets {
        println!(
            "{}  {:?}  {:?}  was at {:.0}% (window peak {:.0}%)",
            when(Some(e.at)),
            e.kind,
            e.cause,
            e.before_pct,
            e.peak
        );
    }
}

fn main() {
    let mut path = None;
    let mut range = StatsRange::Last30d;
    let mut tz = Local::now().offset().local_minus_utc() / 60;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "7" => range = StatsRange::Last7d,
            "30" => range = StatsRange::Last30d,
            "90" => range = StatsRange::Last90d,
            "all" => range = StatsRange::All,
            "--tz" => tz = args.next().and_then(|v| v.parse().ok()).unwrap_or(tz),
            _ => path = Some(a),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: stats_demo USAGE.JSONL [7|30|90|all] [--tz MINUTES]");
        std::process::exit(2);
    };
    let readings: Vec<Reading> = JsonlStore::new(&path)
        .read()
        .iter()
        .map(Reading::from_value)
        .collect();
    println!("{} records from {path}", readings.len());
    print_stats(&compute(&readings, range, Utc::now(), tz));
}
