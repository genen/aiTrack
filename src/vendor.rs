// SPDX-License-Identifier: GPL-3.0-or-later
//! Vendors: one block per provider on the main view. Claude, Grok and Codex have real data
//! sources; Antigravity exists only as mock data for screenshots.

use crate::analysis::{Gauge, Plan, WindowStats};
use crate::reading::{Kind, ModelLimit};
use crate::state::State;
use crate::util::{iso, round_to};
use chrono::{DateTime, Duration, Utc};

/// Known providers. Each one has its own colour in `theme.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VendorId {
    Claude,
    Codex,
    Antigravity,
    Grok,
}

impl VendorId {
    /// One-letter glyph for the badge.
    pub fn badge(self) -> char {
        match self {
            VendorId::Claude => 'C',
            VendorId::Codex => 'X',
            VendorId::Antigravity => 'A',
            VendorId::Grok => 'G',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            VendorId::Claude => "Claude",
            VendorId::Codex => "ChatGPT",
            VendorId::Antigravity => "Antigravity",
            VendorId::Grok => "Grok",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credits {
    pub used: f64,
    pub limit: Option<f64>,
}

/// A provider's current readings for the two plan windows, plus optional extras.
#[derive(Debug, Clone, PartialEq)]
pub struct Vendor {
    pub id: VendorId,
    pub name: String,
    pub session: Option<Gauge>,
    pub weekly: Option<Gauge>,
    pub session_stats: Option<WindowStats>,
    pub weekly_stats: Option<WindowStats>,
    /// Per-model limits (shown in the weekly row's tooltip).
    pub models: Vec<ModelLimit>,
    pub credits: Option<Credits>,
    /// Stored history exists, so the detail pane can draw a chart.
    pub history: bool,
    /// Where the newest reading came from (`desktop-history`, ...), for the tooltips.
    pub source: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    /// The newest reading is recent enough to be shown as the present (see `state::reading_is_current`).
    pub current: bool,
    /// One pool per billing period, no 5-hour window (Grok): no Session row.
    pub weekly_only: bool,
    /// Subscription tier, when the source reports one.
    pub tier: Option<String>,
    /// Tracked but nothing usable yet (login problem, no log): shown as dashes with the reason.
    pub placeholder: bool,
    /// Why there is no current data (the collector's last error), for the tooltip.
    pub error: Option<String>,
    /// Grok only: the Grok Bot app's "Weekly Grok Bot Limit", shown as a second bar.
    pub bot: Option<crate::grok_bot::BotReading>,
    /// Window statistics of the Grok Bot's own saved weekly readings.
    pub bot_stats: Option<WindowStats>,
    /// The Grok Bot's weekly gauge from its saved readings: the pace for its row.
    pub bot_gauge: Option<Gauge>,
    /// Codex: the credits line of the weekly tooltip (`Credits  120 left`).
    pub credit_note: Option<String>,
    /// Codex: banked early resets still available.
    pub banked_resets: Option<u64>,
}

impl Vendor {
    pub fn new(id: VendorId) -> Self {
        Vendor {
            id,
            name: id.name().into(),
            session: None,
            weekly: None,
            session_stats: None,
            weekly_stats: None,
            models: vec![],
            credits: None,
            history: false,
            source: None,
            observed_at: None,
            current: true,
            weekly_only: id == VendorId::Grok,
            tier: None,
            placeholder: false,
            error: None,
            bot: None,
            bot_stats: None,
            bot_gauge: None,
            credit_note: None,
            banked_resets: None,
        }
    }

    pub fn gauge(&self, k: Kind) -> Option<&Gauge> {
        match k {
            Kind::Session => self.session.as_ref(),
            Kind::Weekly => self.weekly.as_ref(),
        }
    }

    /// The windows this vendor has rows for.
    pub fn kinds(&self) -> &'static [Kind] {
        if self.weekly_only {
            &[Kind::Weekly]
        } else {
            &Kind::ALL
        }
    }

    pub fn stats(&self, k: Kind) -> Option<&WindowStats> {
        match k {
            Kind::Session => self.session_stats.as_ref(),
            Kind::Weekly => self.weekly_stats.as_ref(),
        }
    }

    /// A vendor is shown only when it has at least one reading.
    pub fn has_data(&self) -> bool {
        self.session.is_some() || self.weekly.is_some() || self.placeholder
    }
}

pub fn claude(plan: &Plan, models: &[ModelLimit]) -> Vendor {
    let mut v = Vendor::new(VendorId::Claude);
    v.session = plan.session.clone();
    v.weekly = plan.weekly.clone();
    v.session_stats = Some(plan.session_stats.clone());
    v.weekly_stats = Some(plan.weekly_stats.clone());
    v.models = models.to_vec();
    v.credits = plan.spend.as_ref().filter(|s| s.enabled).and_then(|s| {
        s.used.filter(|u| *u > 0.0).map(|used| Credits {
            used,
            limit: s.limit.filter(|l| *l > 0.0),
        })
    });
    v.history = true;
    v.source = plan.last_source.clone();
    v.observed_at = crate::util::parse_ts(&plan.last);
    v
}

/// The vendors that currently have data (today: Claude, once a reading exists).
pub fn from_state(s: &State) -> Vec<Vendor> {
    from_state_at(s, Utc::now())
}

pub fn from_state_at(s: &State, now: DateTime<Utc>) -> Vec<Vendor> {
    let mut out = Vec::new();
    if let Some(p) = s.plan.as_ref().filter(|_| s.claude_on) {
        let mut v = claude(p, &s.models);
        v.current = crate::state::reading_is_current(p, &s.settings, now);
        out.push(v);
    }
    if let Some(g) = s.grok.as_ref().filter(|g| g.enabled) {
        let mut v = Vendor::new(VendorId::Grok);
        v.error = g.collector.error.clone();
        v.bot = g.bot;
        v.bot_stats = g.bot_stats.clone();
        v.bot_gauge = g.bot_gauge.clone();
        match &g.plan {
            Some(p) => {
                v.weekly = p.weekly.clone();
                v.weekly_stats = Some(p.weekly_stats.clone());
                v.history = true;
                v.source = p.last_source.clone();
                v.observed_at = crate::util::parse_ts(&p.last);
                v.tier = g.tier.clone();
                // the last known value stays visible however old (dimmed, with the chip)
                v.current = crate::state::grok_reading_is_shown(p, now);
            }
            None => {
                // tracked but nothing yet: a placeholder row with the reason, never a number
                v.placeholder = true;
                v.current = false;
            }
        }
        out.push(v);
    }
    if let Some(c) = s.codex.as_ref().filter(|c| c.enabled) {
        out.push(codex(c, now));
    }
    out.retain(Vendor::has_data);
    out
}

/// The Codex block: two windows like Claude's, kept however old (dimmed, with the chip) like
/// Grok's, because its session logs only change when Codex is used.
pub fn codex(c: &crate::state::CodexState, now: DateTime<Utc>) -> Vendor {
    let mut v = Vendor::new(VendorId::Codex);
    v.error = c.collector.error.clone();
    match &c.plan {
        Some(p) => {
            v.session = p.session.clone();
            v.weekly = p.weekly.clone();
            v.session_stats = Some(p.session_stats.clone());
            v.weekly_stats = Some(p.weekly_stats.clone());
            // a plan with a weekly limit only shows one bar, not a row of dashes
            v.weekly_only = p.session.is_none() && p.weekly.is_some();
            v.models = c.models.clone();
            v.history = true;
            v.source = p.last_source.clone();
            v.observed_at = crate::util::parse_ts(&p.last);
            v.tier = c.tier.clone();
            v.credit_note = c.credits.as_ref().and_then(|b| b.text());
            v.banked_resets = c.banked_resets.filter(|n| *n > 0);
            v.current = crate::state::grok_reading_is_shown(p, now);
        }
        None => {
            // tracked but nothing yet: a placeholder row with the reason, never a number
            v.placeholder = true;
            v.current = false;
        }
    }
    v
}

fn demo_gauge(now: DateTime<Utc>, kind: Kind, pct: f64, hours_left: f64, rate: f64) -> Gauge {
    let projected = round_to(pct.max(pct + rate * hours_left), 1);
    Gauge {
        pct,
        resets_at: Some(iso(now + Duration::seconds((hours_left * 3600.0) as i64))),
        hours_left: Some(hours_left),
        rate_per_hour: Some(rate),
        projected_pct: Some(projected),
        hours_to_limit: (rate > 0.0 && pct < 100.0).then(|| round_to((100.0 - pct) / rate, 1)),
        window_hours: kind.hours(),
        window_id: String::new(),
        rate_method: None,
        resets_carried: false,
        reset_passed: false,
    }
}

/// MOCK data for all four vendors, to preview the intended look (`examples/render`). Not real.
pub fn demo_vendors(now: DateTime<Utc>) -> Vec<Vendor> {
    let mut out = Vec::new();
    for (id, s, sh, sr, w, wh, wr) in [
        (VendorId::Claude, 42.0, 2.2, 9.0, 58.0, 71.0, 0.35),
        (VendorId::Codex, 18.0, 3.8, 3.0, 31.0, 120.0, 0.12),
        (VendorId::Antigravity, 73.0, 1.4, 12.0, 47.0, 40.0, 0.5),
        (VendorId::Grok, 0.0, 0.0, 0.0, 94.0, 60.0, 0.6),
    ] {
        let mut v = Vendor::new(id);
        if !v.weekly_only {
            v.session = Some(demo_gauge(now, Kind::Session, s, sh, sr));
        }
        v.weekly = Some(demo_gauge(now, Kind::Weekly, w, wh, wr));
        if id == VendorId::Codex {
            v.tier = Some("Plus".into());
            v.source = Some("codex-usage".into());
            v.observed_at = Some(now);
        }
        if id == VendorId::Grok {
            v.tier = Some("SuperGrok Heavy".into());
            v.source = Some("grok-billing".into());
            v.observed_at = Some(now);
        }
        if id == VendorId::Claude {
            v.models = vec![
                ModelLimit {
                    name: "Fable".into(),
                    kind: "weekly_scoped".into(),
                    pct: 43.0,
                    resets_at: v.weekly.as_ref().and_then(|g| g.resets_at.clone()),
                },
                ModelLimit {
                    name: "Opus".into(),
                    kind: "weekly_scoped".into(),
                    pct: 12.0,
                    resets_at: None,
                },
            ];
            v.credits = Some(Credits {
                used: 13.59,
                limit: Some(50.0),
            });
        }
        out.push(v);
    }
    out
}
