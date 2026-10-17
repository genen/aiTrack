// SPDX-License-Identifier: GPL-3.0-or-later
//! Small shared helpers: time parsing/formatting, home directory, number rounding.

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use std::path::PathBuf;

/// Parse an ISO-8601 timestamp leniently (fractional seconds of any length, `Z` or an offset,
/// or no zone at all, which is taken as UTC).
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z"] {
        if let Ok(d) = DateTime::parse_from_str(s, fmt) {
            return Some(d.with_timezone(&Utc));
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    None
}

/// Current time as written to the store: `2026-10-14T06:40:00+00:00` (seconds precision).
pub fn now_iso() -> String {
    iso(Utc::now())
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Round to `places` decimals, half away from zero.
pub fn round_to(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// JavaScript-style `Math.round` (halves round up).
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub fn home_dir() -> Option<PathBuf> {
    let vars: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME", "USERPROFILE"]
    };
    vars.iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `~/...` expansion for paths taken from environment variables.
pub fn expand_user(p: &str) -> PathBuf {
    if p == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(p));
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

/// `~/.aitrack`, falling back to a relative `.aitrack` when no home directory is known.
pub fn data_dir() -> PathBuf {
    home_dir().unwrap_or_default().join(".aitrack")
}

/// What the user's computer is called in messages: "Mac" on macOS, "PC" elsewhere.
pub fn this_machine() -> &'static str {
    this_machine_for(cfg!(target_os = "macos"))
}

pub fn this_machine_for(mac: bool) -> &'static str {
    if mac {
        "Mac"
    } else {
        "PC"
    }
}
