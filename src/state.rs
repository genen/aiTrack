// SPDX-License-Identifier: GPL-3.0-or-later
//! The data the window needs, computed from the history (no UI types).

use crate::analysis::{analyze, Plan};
use crate::collector::Status;
use crate::reading::{ModelLimit, Reading};
use crate::settings::Values;
use chrono::{DateTime, Utc};

/// How many recent polls the diagnostics list shows.
pub const DIAG_LEN: usize = 25;

#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub ts: String,
    pub ok: bool,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct State {
    pub plan: Option<Plan>,
    pub models: Vec<ModelLimit>,
    pub settings: Values,
    pub collector: Status,
    /// Newest first.
    pub diagnostics: Vec<Diag>,
    /// The history file is still being read.
    pub loading: bool,
    /// Grok (separate history and collector); `None` when the Grok part is not set up.
    pub grok: Option<GrokState>,
    /// Claude's panel is shown (setting, or automatic because a Claude tool was detected).
    pub claude_on: bool,
}

impl State {
    /// No vendor is shown at all: nothing installed (or everything switched off).
    pub fn nothing_to_track(&self) -> bool {
        !self.claude_on && !self.grok.as_ref().is_some_and(|g| g.enabled)
    }
}

#[derive(Debug, Clone)]
pub struct GrokState {
    pub plan: Option<Plan>,
    pub collector: Status,
    /// Newest first (Grok polls only).
    pub diagnostics: Vec<Diag>,
    pub tier: Option<String>,
    /// Tracking is switched on (setting, or automatic because the Grok folder exists).
    pub enabled: bool,
    /// The Grok Bot app's second weekly meter, when its cache file is found (see `grok_bot`).
    pub bot: Option<crate::grok_bot::BotReading>,
}

pub fn build_state(
    readings: &[Reading],
    settings: Values,
    collector: Status,
    now: DateTime<Utc>,
) -> State {
    let plan = analyze(readings, now);
    // newest by time, not by file position (backfilled history is appended after newer records),
    // among the readings that report per-model limits: a newer history sample has none and must not
    // blank the "By model" block
    // and only from the current weekly window: last week's per-model figures are not this week's
    let week_start = plan
        .as_ref()
        .and_then(|p| p.weekly.as_ref())
        .and_then(|w| w.resets_at.as_deref())
        .and_then(crate::util::parse_ts)
        .map(|r| r - chrono::Duration::days(7))
        .unwrap_or(now - chrono::Duration::days(7));
    let models = readings
        .iter()
        .filter(|r| r.ok && !r.limits.is_empty())
        .filter(|r| r.t.is_some_and(|t| t >= week_start && t <= now))
        .max_by_key(|r| r.t)
        .map(|r| r.limits.clone())
        .unwrap_or_default();
    let diagnostics = readings
        .iter()
        .rev()
        .filter(|r| !r.backfill)
        .take(DIAG_LEN)
        .map(|r| Diag {
            ts: r.ts.clone(),
            ok: r.ok,
            text: if r.ok {
                // which source supplied it, and why an earlier one was skipped (no ids or tokens)
                let mut t = r.describe();
                if let Some(src) = &r.source {
                    t.push_str(&format!(" \u{b7} {src}"));
                    if let Some(n) = &r.note {
                        t.push_str(&format!(" ({n})"));
                    }
                }
                t
            } else {
                r.error.clone().unwrap_or_else(|| "failed".into())
            },
        })
        .collect();
    State {
        plan,
        models,
        settings,
        collector,
        diagnostics,
        loading: false,
        grok: None,
        claude_on: true,
    }
}

impl State {
    /// Placeholder while the history is being loaded.
    pub fn loading(settings: Values, collector: Status) -> State {
        State {
            plan: None,
            models: vec![],
            settings,
            collector,
            diagnostics: vec![],
            loading: true,
            grok: None,
            claude_on: true,
        }
    }
}

/// Claude's and Grok's poll rows in one list, newest first, at most [`DIAG_LEN`] (Grok rows start
/// with `Grok:`).
pub fn merged_diagnostics(claude: &[Diag], grok: &[Diag]) -> Vec<Diag> {
    let mut all: Vec<Diag> = claude.iter().chain(grok).cloned().collect();
    // stable: equal times keep Claude first
    all.sort_by_key(|d| std::cmp::Reverse(crate::util::parse_ts(&d.ts)));
    all.truncate(DIAG_LEN);
    all
}

/// Oldest reading still shown as the present for the snapshot sources (the desktop app, claude.json).
pub const SNAPSHOT_MAX_AGE_MIN: f64 = 120.0;

/// Is the newest reading recent enough to be shown as the current state? An old sample (for
/// example a backfilled history row from yesterday) must never pass as "now", least of all a
/// `0%` one.
pub fn reading_is_current(plan: &Plan, settings: &Values, now: DateTime<Utc>) -> bool {
    let Some(t) = crate::util::parse_ts(&plan.last) else {
        return false;
    };
    let age_min = (now - t).num_milliseconds() as f64 / 60000.0;
    let network = SNAPSHOT_MAX_AGE_MIN.max(settings.interval_min * 2.5 + 2.0);
    let limit = match plan.last_source.as_deref() {
        Some("oauth") | Some("grok-billing") | None => network,
        Some(_) => SNAPSHOT_MAX_AGE_MIN,
    };
    // a Grok log snapshot of a billing period that has ended is last week's number
    if plan.last_source.as_deref() == Some("grok-log") && weekly_period_ended(plan, now) {
        return false;
    }
    age_min <= limit
}

/// Is Grok's newest reading worth showing as the last known value? Unlike
/// [`reading_is_current`] there is no age limit: Grok's log only changes when the CLI is used, so a
/// nine-hour-old weekly figure is still the best number there is (the view dims it and adds the
/// data-age chip). Only a figure of a billing period that has ended is never shown (it is last
/// week's), and a reading without a usable time is not shown.
pub fn grok_reading_is_shown(plan: &Plan, now: DateTime<Utc>) -> bool {
    crate::util::parse_ts(&plan.last).is_some() && !weekly_period_ended(plan, now)
}

/// Has the weekly billing period of `plan` really ended (its reset time is in the past)? A reset a
/// few seconds ahead rounds `hours_left` to 0 but has not ended.
fn weekly_period_ended(plan: &Plan, now: DateTime<Utc>) -> bool {
    plan.weekly
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(crate::util::parse_ts)
        .is_some_and(|r| r < now)
}

/// Grok's part of the state, from its own readings.
pub fn build_grok_state(
    readings: &[Reading],
    collector: Status,
    enabled: bool,
    now: DateTime<Utc>,
) -> GrokState {
    let plan = analyze(readings, now);
    let tier = readings
        .iter()
        .filter(|r| r.ok)
        .max_by_key(|r| r.t)
        .and_then(|r| r.tier.clone());
    let diagnostics = readings
        .iter()
        .rev()
        .filter(|r| !r.backfill)
        .take(DIAG_LEN)
        .map(|r| Diag {
            ts: r.ts.clone(),
            ok: r.ok,
            text: format!(
                "Grok: {}",
                if r.ok {
                    // which source supplied it: tells a live reading from a stale log line
                    let src = r
                        .source
                        .as_deref()
                        .map(|s| format!(" \u{b7} {s}"))
                        .unwrap_or_default();
                    r.weekly
                        .as_ref()
                        .map(|w| format!("weekly {}%{src}", crate::reading::fmt0(w.pct)))
                        .unwrap_or_else(|| "no reading".into())
                } else {
                    r.error.clone().unwrap_or_else(|| "failed".into())
                }
            ),
        })
        .collect();
    GrokState {
        plan,
        collector,
        diagnostics,
        tier,
        enabled,
        bot: None,
    }
}
