// SPDX-License-Identifier: GPL-3.0-or-later
//! A compact, typed view of one stored record (see `live::normalize` for the stored schema).

use crate::util::parse_ts;
use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub pct: f64,
    pub resets_at: Option<String>,
    /// The reset time was not reported by the source for this sample: it was carried forward from
    /// an earlier sample of the same window (see `live::Poller`), so it is an estimate.
    pub carried: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    pub enabled: bool,
    pub used: Option<f64>,
    pub limit: Option<f64>,
}

/// One per-model entry from the response's `limits` array.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelLimit {
    pub name: String,
    pub kind: String,
    pub pct: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Timestamp text exactly as stored (empty if missing).
    pub ts: String,
    pub t: Option<DateTime<Utc>>,
    /// Truthy `ok`.
    pub ok: bool,
    /// `ok` was explicitly `false` (a recorded failed poll).
    pub failed: bool,
    pub error: Option<String>,
    pub session: Option<Bucket>,
    pub weekly: Option<Bucket>,
    pub spend: Option<Spend>,
    pub limits: Vec<ModelLimit>,
    /// `desktop-history`, `claude-json` or `oauth` (absent in records from older versions).
    pub source: Option<String>,
    /// Imported in bulk from a source's history rather than polled live.
    pub backfill: bool,
    /// Why this source supplied the reading (sources skipped ahead of it, ...), for the diagnostics.
    pub note: Option<String>,
    /// Subscription tier when the source reported one (`raw.tier`, Grok).
    pub tier: Option<String>,
}

fn bucket(v: Option<&Value>) -> Option<Bucket> {
    let o = v?.as_object()?;
    Some(Bucket {
        carried: o.get("resets_carried").and_then(Value::as_bool) == Some(true),
        pct: o.get("pct")?.as_f64()?,
        resets_at: o
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Extract per-model limits from a `limits` array (accepts `percent` or `utilization`).
pub fn model_limits(limits: &Value) -> Vec<ModelLimit> {
    let Some(arr) = limits.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for lim in arr {
        let Some(lim) = lim.as_object() else { continue };
        let model = lim
            .get("scope")
            .and_then(|s| s.get("model"))
            .and_then(Value::as_object);
        let name = model
            .and_then(|m| {
                ["display_name", "id"]
                    .iter()
                    .filter_map(|k| m.get(*k).and_then(Value::as_str))
                    .find(|s| !s.is_empty())
            })
            .map(str::to_owned);
        let pct = match lim.get("percent") {
            Some(p) => p.as_f64(),
            None => lim.get("utilization").and_then(Value::as_f64),
        };
        if let (Some(name), Some(pct)) = (name, pct) {
            out.push(ModelLimit {
                name,
                kind: lim
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                pct,
                resets_at: lim
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    out
}

impl Reading {
    pub fn from_value(v: &Value) -> Self {
        let ts = v.get("ts").and_then(Value::as_str).unwrap_or("").to_owned();
        let spend = v.get("spend").and_then(Value::as_object).map(|s| Spend {
            enabled: s.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            used: s.get("used").and_then(Value::as_f64),
            limit: s.get("limit").and_then(Value::as_f64),
        });
        Reading {
            t: parse_ts(&ts),
            ts,
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(false),
            failed: v.get("ok").and_then(Value::as_bool) == Some(false),
            error: v.get("error").and_then(Value::as_str).map(str::to_owned),
            session: bucket(v.get("session")),
            // Grok Bot records keep their weekly meter under `bot_weekly`
            weekly: bucket(v.get("weekly")).or_else(|| bucket(v.get("bot_weekly"))),
            spend,
            limits: v.get("limits").map(model_limits).unwrap_or_default(),
            source: v.get("source").and_then(Value::as_str).map(str::to_owned),
            backfill: v.get("backfill").and_then(Value::as_bool).unwrap_or(false),
            note: v.get("note").and_then(Value::as_str).map(str::to_owned),
            tier: v
                .get("raw")
                .and_then(|r| r.get("tier"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    pub fn bucket(&self, kind: Kind) -> Option<&Bucket> {
        match kind {
            Kind::Session => self.session.as_ref(),
            Kind::Weekly => self.weekly.as_ref(),
        }
    }

    /// Short text for logs and the diagnostics list: `session 42%, weekly 22%`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = &self.session {
            parts.push(format!("session {}%", fmt0(s.pct)));
        }
        if let Some(w) = &self.weekly {
            parts.push(format!("weekly {}%", fmt0(w.pct)));
        }
        if parts.is_empty() {
            "no limits reported".into()
        } else {
            parts.join(", ")
        }
    }
}

/// The two plan windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Session,
    Weekly,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Session, Kind::Weekly];
    pub fn hours(self) -> f64 {
        match self {
            Kind::Session => 5.0,
            Kind::Weekly => 168.0,
        }
    }
}

/// Format with no decimals, rounding halves to even (like C's `%.0f`).
pub fn fmt0(x: f64) -> String {
    let fl = x.floor();
    let r = if (x - fl - 0.5).abs() < 1e-12 {
        if fl % 2.0 == 0.0 {
            fl
        } else {
            fl + 1.0
        }
    } else {
        x.round()
    };
    format!("{}", r as i64)
}
