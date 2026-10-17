// SPDX-License-Identifier: GPL-3.0-or-later
//! Where Claude usage readings come from, tried in order on every poll:
//!
//! 1. **desktop-cache**: the Claude desktop app's Chromium HTTP cache holds the last response of
//!    `claude.ai/api/organizations/<org>/usage`: fresh percentages *and* reset times, no login.
//! 2. **desktop-history**: the app also keeps `plan-usage-history.json` (samples of the 5-hour and
//!    7-day used percentages; no reset times, appended only now and then). It is always imported
//!    into the store (backfill) and is the current reading only when newer than the cache.
//! 3. **claude-json**: Claude Code caches the last usage reading in `~/.claude.json`
//!    (`cachedUsageUtilization`).
//! 4. **oauth**: the usage endpoint with Claude Code's OAuth token (last resort).
//!
//! The snapshot sources only hold a past reading, so one older than [`MAX_AGE_MIN`] is rejected and
//! the next source is tried. The history file is parsed tolerantly: a surprise in its shape produces a
//! descriptive error, never a panic.

use crate::live::{self, CollectError, CredentialSource};
use crate::reading::Bucket;
use crate::util::{iso, parse_ts};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

pub const HISTORY_NAME: &str = "plan-usage-history.json";
/// A desktop-history / claude.json sample older than this is not used as the current reading.
pub const MAX_AGE_MIN: i64 = 120;
/// Refuse absurdly large files instead of loading them.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

pub const DESKTOP_CACHE: &str = "desktop-cache";
pub const DESKTOP: &str = "desktop-history";
pub const CLAUDE_JSON: &str = "claude-json";
pub const OAUTH: &str = "oauth";

// ---------------------------------------------------------------- locations

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

/// Everything path discovery depends on, so it can be tested with a temp directory tree.
#[derive(Debug, Clone)]
pub struct Env {
    pub os: Os,
    pub home: Option<PathBuf>,
    /// `%APPDATA%`
    pub appdata: Option<PathBuf>,
    /// `%LOCALAPPDATA%`
    pub localappdata: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub claude_config_dir: Option<PathBuf>,
    /// `$GROK_HOME`
    pub grok_home: Option<PathBuf>,
    /// `$AITRACK_GROK_BOT_DIR`: the Grok Bot app's `sand-client-persistence` folder, for tests and
    /// odd setups (used on any platform; without it only macOS has a default, see `grok_bot`).
    pub grok_bot_dir: Option<PathBuf>,
}

impl Env {
    pub fn new(os: Os) -> Env {
        Env {
            os,
            home: None,
            appdata: None,
            localappdata: None,
            xdg_config_home: None,
            claude_config_dir: None,
            grok_home: None,
            grok_bot_dir: None,
        }
    }

    pub fn from_process() -> Env {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Env {
            os: Os::current(),
            home: crate::util::home_dir(),
            appdata: var("APPDATA"),
            localappdata: var("LOCALAPPDATA"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| crate::util::expand_user(&v)),
            grok_home: std::env::var("GROK_HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| crate::util::expand_user(&v)),
            grok_bot_dir: std::env::var("AITRACK_GROK_BOT_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| crate::util::expand_user(&v)),
        }
    }
}

/// Candidate `plan-usage-history.json` paths for this platform, most specific first.
///
/// Mac: `~/Library/Application Support/Claude/`. Linux: `$XDG_CONFIG_HOME` (else `~/.config`)
/// `/Claude/`. Windows: `%APPDATA%\Claude\` (classic installer) and every
/// `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\` (Microsoft Store / MSIX install,
/// where the package id suffix is not known in advance).
pub fn desktop_history_candidates(env: &Env) -> Vec<PathBuf> {
    let home = env.home.clone().unwrap_or_default();
    match env.os {
        Os::Windows => {
            let roaming = env
                .appdata
                .clone()
                .unwrap_or_else(|| home.join("AppData").join("Roaming"));
            let local = env
                .localappdata
                .clone()
                .unwrap_or_else(|| home.join("AppData").join("Local"));
            let mut v = vec![roaming.join("Claude").join(HISTORY_NAME)];
            let mut packages: Vec<PathBuf> = std::fs::read_dir(local.join("Packages"))
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .starts_with("claude_")
                        })
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default();
            packages.sort();
            for p in packages {
                v.push(
                    p.join("LocalCache")
                        .join("Roaming")
                        .join("Claude")
                        .join(HISTORY_NAME),
                );
            }
            v
        }
        Os::Mac => vec![home
            .join("Library")
            .join("Application Support")
            .join("Claude")
            .join(HISTORY_NAME)],
        Os::Linux => {
            let cfg = env
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| home.join(".config"));
            vec![cfg.join("Claude").join(HISTORY_NAME)]
        }
    }
}

/// `$CLAUDE_CONFIG_DIR/.claude.json` (if set), then `~/.claude.json`.
pub fn claude_json_candidates(env: &Env) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = &env.claude_config_dir {
        v.push(d.join(".claude.json"));
    }
    v.push(env.home.clone().unwrap_or_default().join(".claude.json"));
    v
}

fn credential_sources(env: &Env) -> Vec<Box<dyn CredentialSource>> {
    live::default_sources_from(
        env.claude_config_dir.as_ref().and_then(|p| p.to_str()),
        env.home.clone(),
    )
}

// ---------------------------------------------------------------- the source abstraction

/// One reading of the two plan windows at a point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub at: DateTime<Utc>,
    pub session: Option<Bucket>,
    pub weekly: Option<Bucket>,
    /// Source-specific extras kept in the stored record's `raw` (never credentials or org ids).
    pub raw: Option<Value>,
}

/// What a source returned: the newest sample, all older samples it holds (for backfill), and for
/// the live endpoint the full response.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub latest: Sample,
    /// Every sample the source knows about, ascending by time, including `latest`.
    pub history: Vec<Sample>,
    /// Full endpoint response (oauth only).
    pub raw: Option<Value>,
}

pub trait UsageSource: Send + Sync {
    /// Stable id written to records: `desktop-history`, `claude-json`, `oauth`.
    fn name(&self) -> &'static str;
    /// Oldest acceptable sample; `None` for a live source.
    fn max_age(&self) -> Option<Duration> {
        None
    }
    /// A live source is queried every time and always yields a new record.
    fn live(&self) -> bool {
        false
    }
    /// Holds past samples that should be imported into the store even when another source already
    /// supplied the current reading.
    fn imports_history(&self) -> bool {
        false
    }
    /// Reject an otherwise fresh observation (for example a billing period that has ended).
    fn reject(&self, _obs: &Observation, _now: DateTime<Utc>) -> Option<String> {
        None
    }
    /// Message when the newest sample is older than `max_age`.
    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "{} data is stale (last sample {} ago)",
            self.name(),
            age_text(age_min)
        )
    }
    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError>;
}

/// `47 min`, `3 h`, `2 d`.
pub fn age_text(minutes: i64) -> String {
    let m = minutes.max(0);
    if m < 90 {
        format!("{m} min")
    } else if m < 48 * 60 {
        format!("{} h", (m as f64 / 60.0).round() as i64)
    } else {
        format!("{} d", (m as f64 / 1440.0).round() as i64)
    }
}

// ---------------------------------------------------------------- shared parsing helpers

fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// A used-percent value: finite and within 0..=100.
fn percent(v: Option<&Value>) -> Option<f64> {
    number(v).filter(|n| (0.0..=100.0).contains(n))
}

/// Epoch milliseconds; values that are clearly seconds are scaled up.
fn epoch_ms(v: Option<&Value>) -> Option<i64> {
    let n = number(v)?;
    let ms = if n.abs() < 1e11 { n * 1000.0 } else { n };
    (ms.abs() < 4e15).then_some(ms as i64)
}

fn at_ms(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn read_capped(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("file is unexpectedly large ({} bytes)", meta.len()));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    // tolerate a UTF-8 BOM (some Windows writers add one)
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

// ---------------------------------------------------------------- desktop history

#[derive(Debug, Clone, PartialEq)]
pub struct HistorySample {
    pub t_ms: i64,
    pub fh: Option<f64>,
    pub sd: Option<f64>,
    /// The sample's `u` object, kept verbatim in the stored record.
    pub u: Value,
}

/// A parsed `plan-usage-history.json` (structure facts + the usable samples of the newest org).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryFile {
    pub version: Option<i64>,
    /// Entries in `samples` (any shape).
    pub total: usize,
    /// Entries with a usable `t`.
    pub timed: usize,
    pub orgs: usize,
    pub newest_t_ms: Option<i64>,
    /// Organisation of the newest sample (used to pair it with the right cache entries; never print it).
    pub newest_org: Option<String>,
    pub newest_fields: Vec<String>,
    pub newest_u_fields: Vec<String>,
    pub newest_fh: Option<f64>,
    pub newest_sd: Option<f64>,
    /// Samples of the newest sample's organisation with at least one valid percentage,
    /// ascending by time, one per timestamp. Never mixes organisations.
    pub samples: Vec<HistorySample>,
}

impl HistoryFile {
    /// The newest sample, if the newest entry itself carries a usable percentage.
    pub fn latest(&self) -> Option<&HistorySample> {
        self.samples
            .last()
            .filter(|s| Some(s.t_ms) == self.newest_t_ms)
    }
}

/// Parse the file tolerantly. Only a structure that cannot be a history file at all is an error.
pub fn parse_desktop_history(text: &str, now: DateTime<Utc>) -> Result<HistoryFile, String> {
    let data: Value = serde_json::from_str(text).map_err(|_| "not valid JSON".to_string())?;
    let obj = data
        .as_object()
        .ok_or("unexpected shape (top level is not an object)")?;
    let list = obj
        .get("samples")
        .and_then(Value::as_array)
        .ok_or("unexpected shape (no `samples` list)")?;
    let mut f = HistoryFile {
        version: obj.get("version").and_then(Value::as_i64),
        total: list.len(),
        ..Default::default()
    };
    // a sample from the future is clock skew or garbage
    let horizon = now.timestamp_millis() + 5 * 60_000;
    let timed: Vec<(i64, &Value)> = list
        .iter()
        .filter(|s| s.is_object())
        .filter_map(|s| epoch_ms(s.get("t")).map(|t| (t, s)))
        .filter(|(t, _)| *t <= horizon)
        .collect();
    f.timed = timed.len();
    let org_of = |s: &Value| s.get("org").and_then(Value::as_str).map(str::to_owned);
    f.orgs = timed
        .iter()
        .map(|(_, s)| org_of(s))
        .collect::<HashSet<_>>()
        .len();
    // later entries win ties
    let Some((newest_t, newest)) = timed
        .iter()
        .copied()
        .reduce(|a, b| if b.0 >= a.0 { b } else { a })
    else {
        return Ok(f);
    };
    f.newest_t_ms = Some(newest_t);
    f.newest_fields = keys(newest);
    let u = newest.get("u");
    f.newest_u_fields = u.map(keys).unwrap_or_default();
    f.newest_fh = percent(u.and_then(|u| u.get("fh")));
    f.newest_sd = percent(u.and_then(|u| u.get("sd")));
    let org = org_of(newest);
    f.newest_org = org.clone();
    let mut samples: Vec<HistorySample> = timed
        .iter()
        .filter(|(_, s)| org_of(s) == org)
        .filter_map(|(t, s)| {
            let u = s.get("u").filter(|u| u.is_object())?;
            let (fh, sd) = (percent(u.get("fh")), percent(u.get("sd")));
            (fh.is_some() || sd.is_some()).then(|| HistorySample {
                t_ms: *t,
                fh,
                sd,
                u: u.clone(),
            })
        })
        .collect();
    samples.sort_by_key(|s| s.t_ms); // stable: for equal t the later entry stays last
    samples.reverse();
    samples.dedup_by_key(|s| s.t_ms); // keeps the first of the reversed run = the later entry
    samples.reverse();
    f.samples = samples;
    Ok(f)
}

fn history_sample(s: &HistorySample) -> Option<Sample> {
    let bucket = |p: Option<f64>| {
        p.map(|pct| Bucket {
            carried: false,
            pct,
            resets_at: None,
        })
    };
    Some(Sample {
        at: at_ms(s.t_ms)?,
        session: bucket(s.fh),
        weekly: bucket(s.sd),
        raw: Some(serde_json::json!({"t": s.t_ms, "u": s.u})),
    })
}

pub struct DesktopHistory {
    pub env: Env,
}

impl UsageSource for DesktopHistory {
    fn name(&self) -> &'static str {
        DESKTOP
    }

    fn imports_history(&self) -> bool {
        true
    }

    fn max_age(&self) -> Option<Duration> {
        Some(Duration::minutes(MAX_AGE_MIN))
    }

    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "Claude desktop usage is stale (last sample {} ago): open the Claude app",
            age_text(age_min)
        )
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let candidates = desktop_history_candidates(&self.env);
        let existing: Vec<&PathBuf> = candidates.iter().filter(|p| p.is_file()).collect();
        if existing.is_empty() {
            return Err(CollectError::new(format!(
                "Claude desktop history not found (checked {} location{})",
                candidates.len(),
                if candidates.len() == 1 { "" } else { "s" }
            )));
        }
        let mut best: Option<HistoryFile> = None;
        let mut first_err = None;
        for p in existing {
            match read_capped(p).and_then(|t| parse_desktop_history(&t, now)) {
                Ok(f) => {
                    if best.as_ref().is_none_or(|b| f.newest_t_ms > b.newest_t_ms) {
                        best = Some(f);
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        let Some(file) = best else {
            return Err(CollectError::new(format!(
                "Claude desktop history is unreadable: {}",
                first_err.unwrap_or_default()
            )));
        };
        if file.newest_t_ms.is_none() {
            return Err(CollectError::new(
                "Claude desktop history has no usable samples",
            ));
        }
        let history: Vec<Sample> = file.samples.iter().filter_map(history_sample).collect();
        match file.latest().and_then(history_sample) {
            Some(latest) => Ok(Observation {
                latest,
                history,
                raw: None,
            }),
            None => Err(CollectError::new(
                "newest Claude desktop sample has no usable 5h/7d percentages",
            )),
        }
    }
}

// ---------------------------------------------------------------- ~/.claude.json

#[derive(Debug, Clone, PartialEq)]
pub struct JsonCache {
    pub fetched_ms: i64,
    pub session: Option<Bucket>,
    pub weekly: Option<Bucket>,
    /// `Some(false)` when the cache belongs to a different account than `oauthAccount`.
    pub account_matches: Option<bool>,
    pub fields: Vec<String>,
}

fn json_bucket(w: Option<&Value>) -> Option<Bucket> {
    let w = w?;
    let pct = percent(w.get("utilization"))?;
    let resets_at = w
        .get("resets_at")
        .and_then(Value::as_str)
        .filter(|s| parse_ts(s).is_some());
    Some(Bucket {
        carried: false,
        pct,
        resets_at: resets_at.map(str::to_owned),
    })
}

pub fn parse_claude_json(text: &str) -> Result<JsonCache, String> {
    let data: Value = serde_json::from_str(text).map_err(|_| "not valid JSON".to_string())?;
    let cache = data
        .get("cachedUsageUtilization")
        .ok_or("no cachedUsageUtilization")?;
    if !cache.is_object() {
        return Err("cachedUsageUtilization is not an object".into());
    }
    let fetched_ms =
        epoch_ms(cache.get("fetchedAtMs")).ok_or("cachedUsageUtilization has no fetchedAtMs")?;
    let util = cache.get("utilization");
    let (session, weekly) = (
        json_bucket(util.and_then(|u| u.get("five_hour"))),
        json_bucket(util.and_then(|u| u.get("seven_day"))),
    );
    let cached_acct = cache.get("accountUuid").and_then(Value::as_str);
    let own_acct = data
        .get("oauthAccount")
        .and_then(|o| o.get("accountUuid"))
        .and_then(Value::as_str);
    let account_matches = match (cached_acct, own_acct) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    if account_matches == Some(false) {
        return Err("cached usage belongs to a different account".into());
    }
    if session.is_none() && weekly.is_none() {
        return Err("cached usage has no five_hour/seven_day utilization".into());
    }
    Ok(JsonCache {
        fetched_ms,
        session,
        weekly,
        account_matches,
        fields: keys(cache),
    })
}

pub struct ClaudeJson {
    pub env: Env,
}

impl UsageSource for ClaudeJson {
    fn name(&self) -> &'static str {
        CLAUDE_JSON
    }

    fn max_age(&self) -> Option<Duration> {
        Some(Duration::minutes(MAX_AGE_MIN))
    }

    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "~/.claude.json usage cache is stale (fetched {} ago)",
            age_text(age_min)
        )
    }

    fn read(&self, _now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let mut why = None;
        let mut best: Option<JsonCache> = None;
        for p in claude_json_candidates(&self.env)
            .iter()
            .filter(|p| p.is_file())
        {
            match read_capped(p).and_then(|t| parse_claude_json(&t)) {
                Ok(c) => {
                    if best.as_ref().is_none_or(|b| c.fetched_ms > b.fetched_ms) {
                        best = Some(c);
                    }
                }
                Err(e) => {
                    why.get_or_insert(e);
                }
            }
        }
        match best {
            Some(c) => {
                let at = at_ms(c.fetched_ms)
                    .ok_or_else(|| CollectError::new("~/.claude.json cache has an invalid time"))?;
                let sample = Sample {
                    at,
                    session: c.session,
                    weekly: c.weekly,
                    raw: None,
                };
                Ok(Observation {
                    latest: sample.clone(),
                    history: vec![sample],
                    raw: None,
                })
            }
            None => Err(CollectError::new(format!(
                "~/.claude.json has no usable usage cache ({})",
                why.unwrap_or_else(|| "file not found".into())
            ))),
        }
    }
}

// ---------------------------------------------------------------- desktop HTTP cache

/// The Claude desktop app's data directories (the parents of the candidate history files).
pub fn desktop_data_dirs(env: &Env) -> Vec<PathBuf> {
    desktop_history_candidates(env)
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect()
}

/// `<data dir>/Cache/Cache_Data`, where Electron keeps its Chromium HTTP cache.
pub fn cache_dir_of(data_dir: &Path) -> PathBuf {
    data_dir.join("Cache").join("Cache_Data")
}

/// Organisation of the newest sample in this data dir's history file, if it has a readable one.
fn history_org(data_dir: &Path, now: DateTime<Utc>) -> Option<String> {
    let text = read_capped(&data_dir.join(HISTORY_NAME)).ok()?;
    parse_desktop_history(&text, now).ok()?.newest_org
}

/// Percentages (and reset times) from a usage response body, as the endpoint returns them.
fn usage_buckets(body: &Value) -> (Option<Bucket>, Option<Bucket>) {
    let b = |k: &str| {
        let w = body.get(k)?;
        let pct = percent(w.get("utilization"))?;
        let resets_at = w
            .get("resets_at")
            .and_then(Value::as_str)
            .filter(|s| parse_ts(s).is_some());
        Some(Bucket {
            carried: false,
            pct,
            resets_at: resets_at.map(str::to_owned),
        })
    };
    (b("five_hour"), b("seven_day"))
}

/// The newest usage response in each data dir's cache (matching the org of that dir's history, if
/// known), the best across dirs. Organisations are never mixed.
pub struct DesktopCache {
    pub env: Env,
}

impl UsageSource for DesktopCache {
    fn name(&self) -> &'static str {
        DESKTOP_CACHE
    }

    fn max_age(&self) -> Option<Duration> {
        Some(Duration::minutes(MAX_AGE_MIN))
    }

    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "Claude desktop cache is stale (last response {} ago): open the Claude app",
            age_text(age_min)
        )
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let dirs = desktop_data_dirs(&self.env);
        let mut scanned = 0;
        let mut found_dir = false;
        let mut notes: Vec<String> = Vec::new();
        let mut candidates: Vec<crate::cache::UsageEntry> = Vec::new();
        for d in &dirs {
            // every call reads the files afresh (see `cache::scan_usage_entries_cached`)
            let scan = crate::cache::scan_usage_entries_cached_at(&cache_dir_of(d), now);
            found_dir |= scan.dir_exists;
            scanned += scan.files;
            notes.extend(scan.notes);
            let org = history_org(d, now);
            candidates.extend(
                scan.entries
                    .into_iter()
                    .filter(|e| org.as_ref().is_none_or(|o| *o == e.org)),
            );
        }
        let best = crate::cache::pick_best(&candidates).map(|p| (p.entry.clone(), p.reason));
        if !found_dir {
            return Err(CollectError::new(format!(
                "Claude desktop cache not found (no Cache_Data folder in {} location{})",
                dirs.len(),
                if dirs.len() == 1 { "" } else { "s" }
            )));
        }
        let Some((entry, reason)) = best else {
            let extra = if notes.is_empty() {
                String::new()
            } else {
                format!("; {}", notes.join("; "))
            };
            return Err(CollectError::new(format!(
                "no usage entry in the Claude desktop cache (scanned {scanned} files{extra})"
            )));
        };
        let body = entry
            .body
            .map_err(|e| CollectError::new(format!("Claude desktop cache entry unusable: {e}")))?;
        let (session, weekly) = usage_buckets(&body);
        if session.is_none() && weekly.is_none() {
            return Err(CollectError::new(
                "Claude desktop cache entry has no five_hour/seven_day utilization",
            ));
        }
        let at = entry
            .observed
            .ok_or_else(|| CollectError::new("Claude desktop cache entry has no usable time"))?;
        self_check(now, at, &reason, candidates.len());
        let latest = Sample {
            at,
            session,
            weekly,
            raw: None,
        };
        Ok(Observation {
            latest: latest.clone(),
            history: vec![latest],
            raw: Some(body),
        })
    }
}

/// Self-check for the "the window lags the app" class of problems: when the newest usage entry in
/// the cache is older than [`SELF_CHECK_MIN`] minutes, say so in `notify.log` (at most every 30
/// minutes) with the selection rule that applied, so a stale reading is explainable afterwards.
pub const SELF_CHECK_MIN: i64 = 10;

fn self_check(now: DateTime<Utc>, at: DateTime<Utc>, reason: &str, candidates: usize) {
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let age = (now - at).num_minutes();
    if age <= SELF_CHECK_MIN {
        return;
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(30 * 60)) {
        return;
    }
    *last = Some(Instant::now());
    crate::alerts::log_line(&format!(
        "desktop-cache self-check: the newest usage entry in the Claude app's cache is {} old ({candidates} matching entr{}; chosen: {reason}). If the Claude app is running and showing newer numbers, run `aitrack --sources`",
        age_text(age),
        if candidates == 1 { "y" } else { "ies" }
    ));
}

// ---------------------------------------------------------------- oauth endpoint

pub struct OauthSource {
    pub url: String,
    pub credentials: Vec<Box<dyn CredentialSource>>,
    pub timeout: StdDuration,
}

impl UsageSource for OauthSource {
    fn name(&self) -> &'static str {
        OAUTH
    }

    fn live(&self) -> bool {
        true
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let token = live::read_token(&self.credentials, live::unix_secs(now))?;
        let raw = live::fetch_usage(&self.url, &token, self.timeout)?;
        let bucket = |k: &str| {
            let b = raw.get(k)?;
            Some(Bucket {
                carried: false,
                pct: b.get("utilization")?.as_f64()?,
                resets_at: b
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        };
        let latest = Sample {
            at: now,
            session: bucket("five_hour"),
            weekly: bucket("seven_day"),
            raw: None,
        };
        Ok(Observation {
            latest,
            history: vec![],
            raw: Some(raw),
        })
    }
}

/// The default chain: desktop cache, desktop history, `~/.claude.json`, then the endpoint. `only` (a comma list of
/// source names, from `$AITRACK_SOURCES`) restricts and reorders it.
pub fn build_chain(env: &Env, url: &str, only: Option<&str>) -> Vec<Box<dyn UsageSource>> {
    let make = |name: &str| -> Option<Box<dyn UsageSource>> {
        match name {
            DESKTOP_CACHE => Some(Box::new(DesktopCache { env: env.clone() })),
            DESKTOP => Some(Box::new(DesktopHistory { env: env.clone() })),
            CLAUDE_JSON => Some(Box::new(ClaudeJson { env: env.clone() })),
            OAUTH => Some(Box::new(OauthSource {
                url: url.to_owned(),
                credentials: credential_sources(env),
                timeout: StdDuration::from_secs(20),
            })),
            _ => None,
        }
    };
    let wanted: Vec<&str> = match only {
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec![],
    };
    let mut chain: Vec<Box<dyn UsageSource>> = wanted.iter().filter_map(|n| make(n)).collect();
    if chain.is_empty() {
        chain = [DESKTOP_CACHE, DESKTOP, CLAUDE_JSON, OAUTH]
            .iter()
            .filter_map(|n| make(n))
            .collect();
    }
    chain
}

// ---------------------------------------------------------------- `aitrack --sources`

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn fmt_pct(p: Option<f64>) -> String {
    p.map(|v| format!("{v}")).unwrap_or_else(|| "-".into())
}

/// Human-readable diagnostic for `--sources`. Prints structure only: file paths, counts, field
/// names, ages and the two percentages. Never organisation ids, tokens or other file contents.
pub fn diagnose(env: &Env, now: DateTime<Utc>) -> String {
    let mut o = String::new();
    let mut line = |s: String| {
        o.push_str(&s);
        o.push('\n');
    };
    line(format!(
        "aiTrack sources (tried in this order; a sample must be at most {MAX_AGE_MIN} min old)"
    ));
    line(format!("platform: {:?}", env.os));
    line(String::new());

    // --- desktop cache
    line(format!(
        "1. {DESKTOP_CACHE}  (Claude desktop app's HTTP cache: Cache/Cache_Data, usage response)"
    ));
    let mut any_dir = false;
    for d in desktop_data_dirs(env) {
        let cd = cache_dir_of(&d);
        line(format!(
            "   [{}] {}",
            if cd.is_dir() { "exists " } else { "missing" },
            cd.display()
        ));
        if !cd.is_dir() {
            continue;
        }
        any_dir = true;
        let scan = crate::cache::scan_usage_entries_at(&cd, now);
        let org = history_org(&d, now);
        let matching: Vec<_> = scan
            .entries
            .iter()
            .filter(|e| org.as_ref().is_none_or(|o| *o == e.org))
            .collect();
        line(format!("     cache files scanned: {}", scan.files));
        line(format!(
            "     usage entries found: {} ({} for the org of the newest history sample{})",
            scan.entries.len(),
            matching.len(),
            if org.is_none() {
                "; no history org known, any org accepted"
            } else {
                ""
            }
        ));
        for n in &scan.notes {
            line(format!("     note: {n}"));
        }
        let rel = |t: Option<DateTime<Utc>>| {
            t.map(|t| age_text((now - t).num_minutes()))
                .unwrap_or_else(|| "n/a".into())
        };
        let describe = |e: &crate::cache::UsageEntry| -> String {
            match &e.body {
                Err(why) => format!("unusable ({why})"),
                Ok(body) => {
                    let (s5, s7) = usage_buckets(body);
                    let one = |b: Option<Bucket>| match b {
                        None => "not reported".to_string(),
                        Some(b) => format!(
                            "{}% used{}",
                            b.pct,
                            b.resets_at
                                .as_deref()
                                .and_then(parse_ts)
                                .map(|t| format!(
                                    ", resets in {}",
                                    age_text((t - now).num_minutes())
                                ))
                                .unwrap_or_default()
                        ),
                    };
                    format!("five_hour {}; seven_day {}", one(s5), one(s7))
                }
            }
        };
        for (i, e) in matching.iter().enumerate() {
            line(format!(
                "     entry {}: {} in {}, key {}",
                i + 1,
                e.storage.label(),
                e.file,
                e.key_shape
            ));
            line(format!(
                "       Date header: {} | creation time: {} | file mtime: {} | used as observed: {}",
                rel(e.date_header),
                rel(e.created),
                rel(e.file_time),
                rel(e.observed)
            ));
            line(format!(
                "       content-encoding: {} | {}",
                e.encoding.as_deref().unwrap_or("(none)"),
                describe(e)
            ));
        }
        let pick = crate::cache::pick_best(matching.iter().copied());
        match pick.map(|p| (p.entry, p.reason)) {
            None => line("     matching usage entry: none".into()),
            Some((e, reason)) => {
                let age = e.observed.map(|t| (now - t).num_minutes());
                line("     matching usage entry: yes".into());
                line(format!(
                    "     selected: entry {} ({reason})",
                    matching
                        .iter()
                        .position(|m| std::ptr::eq(*m, e))
                        .map_or(0, |i| i + 1)
                ));
                line(format!(
                    "     entry age: {}",
                    age.map(age_text).unwrap_or_else(|| "unknown".into())
                ));
                line(format!(
                    "     content-encoding: {}",
                    e.encoding.as_deref().unwrap_or("(none)")
                ));
                match &e.body {
                    Err(why) => line(format!("     parsed: no ({why})")),
                    Ok(body) => {
                        line("     parsed: yes".into());
                        let (s5, s7) = usage_buckets(body);
                        for (label, b) in [("five_hour", s5), ("seven_day", s7)] {
                            let text = match b {
                                None => "not reported".to_string(),
                                Some(b) => {
                                    let reset = b
                                        .resets_at
                                        .as_deref()
                                        .and_then(parse_ts)
                                        .map(|t| {
                                            format!(
                                                "resets in {}",
                                                age_text((t - now).num_minutes())
                                            )
                                        })
                                        .unwrap_or_else(|| "no reset time".into());
                                    format!("{}% used, {reset}", b.pct)
                                }
                            };
                            line(format!("     {label}: {text}"));
                        }
                        line(format!(
                            "     fresh enough: {}",
                            yes_no(age.is_some_and(|a| a <= MAX_AGE_MIN))
                        ));
                    }
                }
            }
        }
    }
    if !any_dir {
        line("   no Cache_Data folder found".into());
    }
    line(String::new());

    // --- desktop history
    line(format!(
        "2. {DESKTOP}  (Claude desktop app: {HISTORY_NAME})"
    ));
    let cands = desktop_history_candidates(env);
    for p in &cands {
        line(format!(
            "   [{}] {}",
            if p.is_file() { "exists " } else { "missing" },
            p.display()
        ));
    }
    let mut any = false;
    for p in cands.iter().filter(|p| p.is_file()) {
        any = true;
        line(format!("   file: {}", p.display()));
        match read_capped(p).and_then(|t| parse_desktop_history(&t, now)) {
            Err(e) => line(format!("     parsed: no ({e})")),
            Ok(f) => {
                line("     parsed: yes".into());
                line(format!(
                    "     version: {}",
                    f.version
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(missing)".into())
                ));
                line(format!(
                    "     samples: {} entries, {} with a usable time, {} usable for the newest org",
                    f.total,
                    f.timed,
                    f.samples.len()
                ));
                line(format!("     distinct orgs: {}", f.orgs));
                match f.newest_t_ms.and_then(at_ms) {
                    None => line("     newest sample: none".into()),
                    Some(t) => {
                        let age = (now - t).num_minutes();
                        line(format!(
                            "     newest sample fields: {}",
                            f.newest_fields.join(", ")
                        ));
                        line(format!(
                            "     newest sample `u` fields: {}",
                            f.newest_u_fields.join(", ")
                        ));
                        line(format!(
                            "     newest sample age: {} (fh={}, sd={})",
                            age_text(age),
                            fmt_pct(f.newest_fh),
                            fmt_pct(f.newest_sd)
                        ));
                        line(format!("     fresh enough: {}", yes_no(age <= MAX_AGE_MIN)));
                    }
                }
            }
        }
    }
    if !any {
        line("   no history file found".into());
    }
    line(String::new());

    // --- which of the two the chain would use right now
    {
        let cache = DesktopCache { env: env.clone() }.read(now);
        let hist = DesktopHistory { env: env.clone() }.read(now);
        let fresh = |r: &Result<Observation, CollectError>, max: i64| {
            r.as_ref()
                .ok()
                .filter(|o| (now - o.latest.at).num_minutes() <= max)
                .map(|o| o.latest.at)
        };
        let (c, h) = (fresh(&cache, MAX_AGE_MIN), fresh(&hist, MAX_AGE_MIN));
        let verdict = match (c, h) {
            (Some(c), Some(h)) if h > c => {
                format!("{DESKTOP} (its newest sample is newer than the cache entry)")
            }
            (Some(_), _) => DESKTOP_CACHE.to_string(),
            (None, Some(_)) => format!("{DESKTOP} (no fresh cache entry)"),
            (None, None) => {
                "neither is fresh: the chain falls through to .claude.json / the endpoint".into()
            }
        };
        let age = |t: Option<DateTime<Utc>>| {
            t.map(|t| age_text((now - t).num_minutes()))
                .unwrap_or_else(|| "none".into())
        };
        line(format!(
            "   chain verdict: cache newest {} / history newest {} -> would use {verdict}",
            age(cache.as_ref().ok().map(|o| o.latest.at)),
            age(hist.as_ref().ok().map(|o| o.latest.at)),
        ));
        line(String::new());
    }

    // --- ~/.claude.json
    line(format!(
        "3. {CLAUDE_JSON}  (cachedUsageUtilization in .claude.json)"
    ));
    let mut any = false;
    for p in claude_json_candidates(env) {
        line(format!(
            "   [{}] {}",
            if p.is_file() { "exists " } else { "missing" },
            p.display()
        ));
        if !p.is_file() {
            continue;
        }
        any = true;
        match read_capped(&p).and_then(|t| parse_claude_json(&t)) {
            Err(e) => line(format!("     usable: no ({e})")),
            Ok(c) => {
                let age = at_ms(c.fetched_ms).map(|t| (now - t).num_minutes());
                line("     usable: yes".into());
                line(format!("     cache fields: {}", c.fields.join(", ")));
                line(format!(
                    "     age: {}",
                    age.map(age_text).unwrap_or_else(|| "unknown".into())
                ));
                line(format!(
                    "     five_hour={} seven_day={} (reset times {})",
                    fmt_pct(c.session.as_ref().map(|b| b.pct)),
                    fmt_pct(c.weekly.as_ref().map(|b| b.pct)),
                    yes_no(c.session.as_ref().is_some_and(|b| b.resets_at.is_some()))
                ));
                line(format!(
                    "     account matches oauthAccount: {}",
                    match c.account_matches {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => "unknown",
                    }
                ));
                line(format!(
                    "     fresh enough: {}",
                    yes_no(age.is_some_and(|a| a <= MAX_AGE_MIN))
                ));
            }
        }
    }
    if !any {
        line("   no .claude.json found".into());
    }
    line(String::new());

    // --- oauth
    line(format!(
        "4. {OAUTH}  (usage endpoint with Claude Code's login token; last resort)"
    ));
    for s in credential_sources(env) {
        let (exists, state) = match s.load() {
            Some(c) => {
                let expired = c
                    .expires_at_ms
                    .is_some_and(|e| e / 1000.0 < live::unix_now());
                (true, format!("token present, expired: {}", yes_no(expired)))
            }
            None => (false, "no usable token".into()),
        };
        line(format!(
            "   [{}] {}: {state}",
            if exists { "exists " } else { "missing" },
            s.label()
        ));
    }
    line(String::new());
    o.push_str(&crate::grok::diagnose(env, now));
    o.push_str(&crate::grok_bot::diagnose(env, now));
    o
}

/// Used by tests and `--sources`: ISO text of a sample time.
pub fn sample_iso(s: &Sample) -> String {
    iso(s.at)
}
