// SPDX-License-Identifier: GPL-3.0-or-later
//! Usage alerts (toast notifications): when a limit window crosses 75 % (testing: `AITRACK_ALERT_INFO_PCT`) / 90 % / 100 %, is on
//! course to run out within 30 minutes, or recovers after a reset.
//!
//! Each event fires at most once per limit window. What was already sent is kept in `alerts.json`
//! (keyed by window id, so a restart does not repeat toasts) and old windows are pruned. Delivery
//! goes through the [`Notifier`] trait: Windows toasts in `toast.rs`, a logging no-op elsewhere,
//! a recording fake in the tests.

use crate::analysis::{Gauge, Plan};
use crate::reading::Kind;
use crate::util::{js_round, parse_ts};
use crate::view::left_long;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Percent used at which the first (informational) alert fires.
pub const INFO_PCT: f64 = 75.0;
/// Testing override of [`INFO_PCT`]: `AITRACK_ALERT_INFO_PCT=25` makes the informational alert fire at
/// 25 % used (to see a real toast without waiting for 75 %). Clamped to [`INFO_PCT_MIN`]..=[`INFO_PCT_MAX`].
pub const INFO_PCT_ENV: &str = "AITRACK_ALERT_INFO_PCT";
pub const INFO_PCT_MIN: f64 = 1.0;
pub const INFO_PCT_MAX: f64 = 89.0;
/// ... the warning ("10% left").
pub const WARN_PCT: f64 = 90.0;
/// Predictive alert: projected to run out before the reset, and empty within this many minutes.
pub const PREDICT_MIN: f64 = 30.0;
/// Fired-event records older than this are dropped.
pub const PRUNE_DAYS: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Info,
    Warning,
    Limit,
    Predictive,
    Recovery,
}

impl AlertKind {
    /// Warning, limit and "empty soon" alerts are the ones that must not be missed: they play the
    /// reminder sound. Info and recovery stay quiet.
    pub fn important(self) -> bool {
        matches!(
            self,
            AlertKind::Warning | AlertKind::Limit | AlertKind::Predictive
        )
    }

    /// The usage alerts (75 %, 90 %, limit) also bring the aiTrack window to the front.
    pub fn raises_window(self) -> bool {
        matches!(
            self,
            AlertKind::Info | AlertKind::Warning | AlertKind::Limit
        )
    }
}

/// Wraps a notifier: a usage alert (see [`AlertKind::raises_window`]) also sets `raise`, which the
/// window reads to come to the front. Set whether or not the toast could be shown.
pub struct Raising<'a> {
    pub inner: &'a dyn Notifier,
    pub raise: &'a AtomicBool,
}

impl Notifier for Raising<'_> {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        if n.kind.raises_window() {
            self.raise.store(true, Ordering::Relaxed);
        }
        self.inner.notify(n)
    }
}

/// How important alerts are presented (the Alert sounds setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertOptions {
    /// Those toasts play the reminder sound (Info is always silent).
    pub sounds: bool,
}

impl Default for AlertOptions {
    fn default() -> Self {
        Self { sounds: true }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub kind: AlertKind,
    /// Which vendor's alert (`Claude`, `Grok`): picks the toast logo. Empty = none.
    pub vendor: String,
    pub options: AlertOptions,
}

impl Notification {
    pub fn new(title: impl Into<String>, body: impl Into<String>, kind: AlertKind) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            kind,
            vendor: String::new(),
            options: AlertOptions::default(),
        }
    }
}

/// Prefix of a [`Notifier`] error meaning "not shown yet, try again at the next poll" (for example
/// macOS notification permission not decided yet). Any other error is final: the alert stays
/// recorded as sent and is not repeated every minute. A notifier that deliberately shows nothing
/// (the user turned its notifications off) returns `Ok`.
pub const RETRY_LATER: &str = "[retry later] ";

/// An error for [`Notifier::notify`] that makes the alert be offered again at the next poll.
pub fn retry_later(why: &str) -> String {
    format!("{RETRY_LATER}{why}")
}

/// Is `err` a [`retry_later`] error?
pub fn is_retry_later(err: &str) -> bool {
    err.starts_with(RETRY_LATER)
}

/// Delivers a notification to the user. `Ok` means shown, or deliberately not shown (opt-out);
/// `Err` means it could not be shown (see [`RETRY_LATER`] for the retried kind).
pub trait Notifier: Send + Sync {
    fn notify(&self, n: &Notification) -> Result<(), String>;
}

/// Logs to stderr and the notification log instead of showing anything (non-Windows, headless).
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        log_line(&format!(
            "notification (not shown on this platform): {} | {}",
            n.title, n.body
        ));
        Ok(())
    }
}

/// Append a line to `~/.aitrack/notify.log` (best effort; used for failures that must be silent).
pub fn log_line(msg: &str) {
    use std::io::Write;
    let path = match std::env::var_os("AITRACK_NOTIFY_LOG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::util::data_dir().join("notify.log"),
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}  {msg}", crate::util::now_iso());
    }
}

// ---------------------------------------------------------------- persisted state

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct WindowAlerts {
    fired: Vec<String>,
    /// Last time this window was evaluated (for pruning).
    seen: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AlertState {
    #[serde(default)]
    windows: BTreeMap<String, WindowAlerts>,
    /// Latest window key per kind, to notice resets.
    #[serde(default)]
    last: BTreeMap<String, String>,
}

pub struct Alerts {
    path: PathBuf,
    state: Mutex<AlertState>,
    options: Mutex<AlertOptions>,
}

/// The override from [`INFO_PCT_ENV`] when it is set to a number (clamped).
pub fn info_pct_override() -> Option<f64> {
    std::env::var(INFO_PCT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(INFO_PCT_MIN, INFO_PCT_MAX))
}

/// The percent used at which the informational alert fires right now.
pub fn info_pct() -> f64 {
    info_pct_override().unwrap_or(INFO_PCT)
}

/// Fired-event key of the informational alert for a threshold: `p75` by default, `p25` for 25. The
/// key follows the number, so a `p75` recorded earlier never hides a `p25` alert and a stale `p25`
/// never hides a later real `p75`.
pub fn info_tag(pct: f64) -> String {
    if pct.fract() == 0.0 {
        format!("p{}", pct as i64)
    } else {
        format!("p{pct}")
    }
}

/// Is the window id `old` (`i<first reading, unix seconds>`) the same window as `new`
/// (`r<reset time, unix minutes>`)? True when the first reading lies inside the 5 h / 7 d span that
/// ends at that reset time.
fn renamed_window(old: Option<&str>, new: &str, kind: Kind) -> bool {
    let first: Option<i64> = old
        .and_then(|o| o.strip_prefix('i'))
        .and_then(|t| t.parse().ok());
    let reset: Option<i64> = new.strip_prefix('r').and_then(|t| t.parse().ok());
    match (first, reset) {
        (Some(first), Some(reset_min)) => {
            let end = reset_min.saturating_mul(60);
            let start = end.saturating_sub((kind.hours() * 3600.0) as i64);
            first > start && first <= end
        }
        _ => false,
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Session => "session",
        Kind::Weekly => "weekly",
    }
}

fn resets_body(g: &Gauge) -> String {
    match g.hours_left {
        Some(h) => format!("Resets in {}", left_long(h)),
        None => "Reset time unknown".into(),
    }
}

impl Alerts {
    /// Load the fired-event state (an unreadable file just starts empty).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<AlertState>(&t).ok())
            .unwrap_or_default();
        if let Some(p) = info_pct_override() {
            log_line(&format!(
                "alert threshold override active: informational alert at {p}% used ({INFO_PCT_ENV}), default is {INFO_PCT}%"
            ));
        }
        Alerts {
            path,
            state: Mutex::new(state),
            options: Mutex::new(AlertOptions::default()),
        }
    }

    /// How the notifications sent from now on are presented.
    pub fn set_options(&self, o: AlertOptions) {
        *self.options.lock().unwrap_or_else(|e| e.into_inner()) = o;
    }

    fn save(&self, st: &AlertState) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(text) =
            serde_json::to_string_pretty(&serde_json::to_value(st).unwrap_or(Value::Null))
        {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    /// Decide what to tell the user about `plan` (the newest reading, which the caller has checked
    /// to be current), deliver it through `notifier`, and remember it. Returns what was sent.
    pub fn evaluate(
        &self,
        vendor: &str,
        plan: &Plan,
        now: DateTime<Utc>,
        notifier: &dyn Notifier,
    ) -> Vec<Notification> {
        self.evaluate_with(vendor, plan, now, notifier, info_pct())
    }

    /// [`Alerts::evaluate`] with an explicit informational threshold (tests, and the env override).
    pub fn evaluate_with(
        &self,
        vendor: &str,
        plan: &Plan,
        now: DateTime<Utc>,
        notifier: &dyn Notifier,
        info: f64,
    ) -> Vec<Notification> {
        let info_key = info_tag(info);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = serde_json::to_string(&*st).unwrap_or_default();
        let mut sent = Vec::new();
        // per alert in `sent`: its window key and the `fired` tags it set (undone if it must be retried)
        let mut undo: Vec<(String, Vec<String>)> = Vec::new();
        for kind in Kind::ALL {
            let Some(g) = plan.gauge(kind) else { continue };
            if g.window_id.is_empty() {
                continue;
            }
            let name = kind_name(kind);
            // Claude keys stay as they were; other vendors are namespaced (`grok:weekly:r123`)
            let ns = if vendor == "Claude" {
                String::new()
            } else {
                format!("{}:", vendor.to_ascii_lowercase())
            };
            let key = format!("{ns}{name}:{}", g.window_id);
            let last_key = format!("{ns}{name}");
            let label = format!("{vendor} {name}");
            let mut out: Vec<Notification> = Vec::new();
            let mut out_tags: Vec<Vec<String>> = Vec::new();

            // recovery: the previous window of this kind ended while it was in the red
            let prev = st.last.get(&last_key).cloned();
            // A window known only by its first reading (`i<ts>`: a source without reset times) that
            // then gets a reset time (`r<minute>`) is the same window under a new id: its
            // announcements carry over instead of firing again.
            let same_window = prev.as_deref().is_some_and(|p| {
                p != key
                    && renamed_window(p.strip_prefix(&format!("{ns}{name}:")), &g.window_id, kind)
            });
            if same_window {
                let old = prev.as_deref().and_then(|p| st.windows.get(p)).cloned();
                if let Some(old) = old {
                    let entry = st.windows.entry(key.clone()).or_default();
                    for f in old.fired {
                        if !entry.fired.contains(&f) {
                            entry.fired.push(f);
                        }
                    }
                }
            }
            let new_window = !same_window && prev.as_deref() != Some(key.as_str());
            let was_red = prev
                .as_deref()
                .filter(|p| *p != key)
                .and_then(|p| st.windows.get(p))
                .is_some_and(|w| w.fired.iter().any(|f| f == "p90" || f == "p100"));
            let entry = st.windows.entry(key.clone()).or_default();
            entry.seen = crate::util::iso(now);
            let fired = |e: &WindowAlerts, f: &str| e.fired.iter().any(|x| x == f);

            if new_window && was_red && g.pct < WARN_PCT && !fired(entry, "rec") {
                entry.fired.push("rec".into());
                out_tags.push(vec!["rec".into()]);
                out.push(Notification::new(
                    format!("{label}: reset"),
                    format!("Back to {}% used", js_round(g.pct) as i64),
                    AlertKind::Recovery,
                ));
            }

            // thresholds: only the highest one crossed is announced; lower ones are marked done
            let top = if g.pct >= 100.0 {
                Some(("p100", AlertKind::Limit))
            } else if g.pct >= WARN_PCT {
                Some(("p90", AlertKind::Warning))
            } else if g.pct >= info {
                Some((info_key.as_str(), AlertKind::Info))
            } else {
                None
            };
            if let Some((tag, ak)) = top {
                if !fired(entry, tag) {
                    let mut tags = Vec::new();
                    for t in [info_key.as_str(), "p90", "p100"] {
                        if !fired(entry, t) {
                            entry.fired.push(t.to_string());
                            tags.push(t.to_string());
                        }
                        if t == tag {
                            break;
                        }
                    }
                    let title = match tag {
                        "p100" => format!("{label}: limit reached"),
                        "p90" => format!(
                            "{label}: {}% left",
                            js_round((100.0 - g.pct).max(0.0)) as i64
                        ),
                        _ => format!("{label}: {}% used", js_round(g.pct) as i64),
                    };
                    out_tags.push(tags);
                    out.push(Notification::new(title, resets_body(g), ak));
                }
            }

            // predictive: on course to run out before the reset, and soon (same rule as the header
            // warning, including its guard against noisy low-usage paces)
            if let Some(e) = crate::analysis::empty_before_reset(g).filter(|e| !e.at_limit) {
                if e.empty_in_h * 60.0 < PREDICT_MIN && !fired(entry, "pred") {
                    entry.fired.push("pred".into());
                    out_tags.push(vec!["pred".into()]);
                    out.push(Notification::new(
                        format!("{label}: empty in ~{}", left_long(e.empty_in_h)),
                        resets_body(g),
                        AlertKind::Predictive,
                    ));
                }
            }
            st.last.insert(last_key, key.clone());
            undo.extend(out_tags.into_iter().map(|t| (key.clone(), t)));
            sent.extend(out);
        }

        // forget windows we have not seen for a long time
        let cutoff = now - Duration::days(PRUNE_DAYS);
        st.windows
            .retain(|_, w| parse_ts(&w.seen).is_none_or(|t| t >= cutoff));
        if serde_json::to_string(&*st).unwrap_or_default() != before {
            self.save(&st);
        }
        drop(st);

        let options = *self.options.lock().unwrap_or_else(|e| e.into_inner());
        for n in &mut sent {
            n.vendor = vendor.to_owned();
            n.options = options;
        }
        // An alert that could not be shown yet is not left marked as sent: it is offered again at
        // the next poll. Other failures are logged and final.
        let mut delivered = Vec::new();
        let mut retry: Vec<(String, Vec<String>)> = Vec::new();
        for (n, u) in sent.into_iter().zip(undo) {
            match notifier.notify(&n) {
                Ok(()) => delivered.push(n),
                Err(e) if is_retry_later(&e) => {
                    log_line(&format!("notification will be retried: {e}"));
                    retry.push(u);
                }
                Err(e) => {
                    log_line(&format!("notification failed: {e}"));
                    delivered.push(n);
                }
            }
        }
        if !retry.is_empty() {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            for (key, tags) in retry {
                if let Some(w) = st.windows.get_mut(&key) {
                    w.fired.retain(|f| !tags.contains(f));
                }
            }
            self.save(&st);
        }
        delivered
    }
}
