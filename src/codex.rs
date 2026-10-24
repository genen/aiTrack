// SPDX-License-Identifier: GPL-3.0-or-later
//! Codex (OpenAI) plan usage: the 5-hour and weekly limits of a ChatGPT plan's Codex / Work
//! allowance. Ordinary ChatGPT chat message caps are not exposed anywhere and are not tracked.
//!
//! Two sources, tried in this order on a full poll (the log alone on the short local tick):
//!
//! 1. **codex-usage**: `GET https://chatgpt.com/backend-api/wham/usage` with the ChatGPT sign-in
//!    Codex stored in `~/.codex/auth.json` (`tokens.access_token` as the bearer,
//!    `tokens.account_id` as `ChatGPT-Account-Id`). The reply has `plan_type` and
//!    `rate_limit.primary_window` / `secondary_window` (`used_percent`, `limit_window_seconds`,
//!    `reset_at` in Unix seconds, `reset_after_seconds`), optional `additional_rate_limits[]`
//!    (per-model buckets), `credits` and `rate_limit_reset_credits.available_count` (banked
//!    resets). The endpoint is undocumented (the same one the Codex app and several trackers ask);
//!    the shape was checked on the author's Mac on 2026-10-24. The token is read fresh on each
//!    poll and is never stored, logged or put into an error message; an expired login is reported
//!    ("open Codex once"), never refreshed, and nothing is written back to `auth.json`.
//! 2. **codex-log**: Codex's session logs, `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (and
//!    `archived_sessions/`, plain or `.jsonl.zst`). Each model turn writes an `event_msg` line of
//!    `payload.type: "token_count"` whose `payload.rate_limits` holds `limit_id` (`codex` for the
//!    main allowance), `primary` / `secondary` (`used_percent`, `window_minutes`, `resets_at` in
//!    Unix seconds; older builds wrote `resets_in_seconds`), `plan_type` and `credits`. It is only
//!    as fresh as the last Codex turn.
//!
//! Windows are classified by their length, never by slot: `primary` is not always the 5-hour one
//! (a plan with a weekly limit only puts it in `primary`). `CODEX_HOME` replaces `~/.codex`.
//! Readings are stored in `usage-codex.jsonl` (record field `vendor: "codex"`).

use crate::live::CollectError;
use crate::reading::Bucket;
use crate::sources::{Env, Observation, Sample, UsageSource};
use crate::util::{iso, parse_ts};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const SOURCE_USAGE: &str = "codex-usage";
pub const SOURCE_LOG: &str = "codex-log";
/// The `limit_id` of the main Codex allowance.
pub const MAIN_LIMIT: &str = "codex";
/// A log reading older than this is not "current" (the last known value stays on screen, dimmed).
pub const LOG_MAX_AGE_MIN: i64 = 120;
/// How many of the newest session files are read for usage lines.
pub const MAX_LOG_FILES: usize = 12;
/// A window this long or longer is the weekly one (shorter ones are the 5-hour session).
const WEEKLY_FROM_MIN: f64 = 24.0 * 60.0;
const MAX_AUTH_BYTES: u64 = 1024 * 1024;
/// Largest decompressed `.jsonl.zst` session file read.
const MAX_ZST_BYTES: u64 = 64 * 1024 * 1024;
/// Log samples older than this are not imported as history.
const HISTORY_DAYS: i64 = 30;

/// `$CODEX_HOME` (read into [`Env`]) or `~/.codex`.
pub fn codex_home(env: &Env) -> PathBuf {
    env.codex_home
        .clone()
        .unwrap_or_else(|| env.home.clone().unwrap_or_default().join(".codex"))
}

pub fn auth_path(env: &Env) -> PathBuf {
    codex_home(env).join("auth.json")
}

pub fn sessions_dir(env: &Env) -> PathBuf {
    codex_home(env).join("sessions")
}

pub fn archived_dir(env: &Env) -> PathBuf {
    codex_home(env).join("archived_sessions")
}

/// Is Codex in use for this user (its data folder holds a sign-in or session logs)?
pub fn detected(env: &Env) -> bool {
    auth_path(env).is_file() || sessions_dir(env).is_dir() || archived_dir(env).is_dir()
}

// ---------------------------------------------------------------- shared parsing

/// A time from epoch seconds (or milliseconds), or an ISO string.
fn time_value(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => parse_ts(s).or_else(|| s.trim().parse::<f64>().ok().and_then(epoch)),
        Value::Number(n) => epoch(n.as_f64()?),
        _ => None,
    }
}

fn epoch(secs: f64) -> Option<DateTime<Utc>> {
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    let secs = if secs > 1e11 { secs / 1000.0 } else { secs };
    Utc.timestamp_millis_opt((secs * 1000.0) as i64).single()
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// Which plan window a window of `minutes` is.
pub fn kind_of(minutes: f64) -> crate::reading::Kind {
    if minutes >= WEEKLY_FROM_MIN {
        crate::reading::Kind::Weekly
    } else {
        crate::reading::Kind::Session
    }
}

/// One limit window as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub pct: f64,
    pub minutes: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// A log window: `used_percent`, `window_minutes`, `resets_at` (or `resets_in_seconds` after `at`).
fn log_window(v: &Value, at: DateTime<Utc>) -> Option<Window> {
    let o = v.as_object()?;
    let pct = number(o.get("used_percent"))?;
    let minutes = number(o.get("window_minutes"))?;
    let resets_at = o.get("resets_at").and_then(time_value).or_else(|| {
        number(o.get("resets_in_seconds")).map(|s| at + Duration::milliseconds((s * 1000.0) as i64))
    });
    Some(Window {
        pct: pct.clamp(0.0, 100.0),
        minutes,
        resets_at,
    })
}

/// A live window: `used_percent`, `limit_window_seconds`, `reset_at` (or `reset_after_seconds`).
fn live_window(v: &Value, now: DateTime<Utc>) -> Option<Window> {
    let o = v.as_object()?;
    let pct = number(o.get("used_percent"))?;
    let minutes = number(o.get("limit_window_seconds"))? / 60.0;
    let resets_at = o.get("reset_at").and_then(time_value).or_else(|| {
        number(o.get("reset_after_seconds"))
            .map(|s| now + Duration::milliseconds((s * 1000.0) as i64))
    });
    Some(Window {
        pct: pct.clamp(0.0, 100.0),
        minutes,
        resets_at,
    })
}

fn bucket_of(w: &Window) -> Bucket {
    Bucket {
        carried: false,
        pct: w.pct,
        resets_at: w.resets_at.map(iso),
    }
}

/// The plan's name as people know it: `plus` -> `Plus`, `pro` -> `Pro`, `team` -> `Business`.
pub fn plan_label(plan: &str) -> Option<String> {
    let p = plan.trim();
    if p.is_empty() || p.eq_ignore_ascii_case("unknown") {
        return None;
    }
    Some(match p.to_ascii_lowercase().as_str() {
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" | "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "edu" | "education" => "Edu".into(),
        "free" => "Free".into(),
        "go" => "Go".into(),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    })
}

/// The credits balance as stored: `{"unlimited": bool, "has_credits": bool, "balance": number}`.
fn credits_json(v: Option<&Value>) -> Option<Value> {
    let o = v?.as_object()?;
    let unlimited = o.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
    let has = o
        .get("has_credits")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let balance = number(o.get("balance"));
    Some(json!({"unlimited": unlimited, "has_credits": has, "balance": balance}))
}

/// The parts of one usage report, before they become a [`Sample`].
#[derive(Debug, Clone, Default, PartialEq)]
struct Report {
    session: Option<Window>,
    weekly: Option<Window>,
    plan: Option<String>,
    credits: Option<Value>,
    banked: Option<u64>,
    /// Extra buckets: (name, window), named by the bucket.
    extra: Vec<(String, Window)>,
}

impl Report {
    /// Put each window in its place by length (the first of each kind wins).
    fn place(&mut self, w: Window) {
        let slot = match kind_of(w.minutes) {
            crate::reading::Kind::Session => &mut self.session,
            crate::reading::Kind::Weekly => &mut self.weekly,
        };
        if slot.is_none() {
            *slot = Some(w);
        }
    }

    fn sample(&self, at: DateTime<Utc>) -> Sample {
        let mut raw = serde_json::Map::new();
        if let Some(p) = &self.plan {
            if let Some(label) = plan_label(p) {
                raw.insert("tier".into(), json!(label));
            }
            raw.insert("plan_type".into(), json!(p));
        }
        if let Some(c) = &self.credits {
            raw.insert("credits".into(), c.clone());
        }
        if let Some(b) = self.banked {
            raw.insert("banked_resets".into(), json!(b));
        }
        if !self.extra.is_empty() {
            let limits: Vec<Value> = self
                .extra
                .iter()
                .map(|(name, w)| {
                    let kind = match kind_of(w.minutes) {
                        crate::reading::Kind::Session => "session",
                        crate::reading::Kind::Weekly => "weekly",
                    };
                    json!({
                        "name": name,
                        "kind": kind,
                        "percent": w.pct,
                        "resets_at": w.resets_at.map(iso),
                    })
                })
                .collect();
            raw.insert("limits".into(), Value::Array(limits));
        }
        Sample {
            at,
            session: self.session.as_ref().map(bucket_of),
            weekly: self.weekly.as_ref().map(bucket_of),
            raw: (!raw.is_empty()).then_some(Value::Object(raw)),
        }
    }
}

// ---------------------------------------------------------------- the live endpoint

/// A readable name for an extra bucket: its `limit_name`, else its id.
fn bucket_name(o: &serde_json::Map<String, Value>) -> Option<String> {
    [
        "limit_name",
        "display_name",
        "name",
        "metered_feature",
        "limit_id",
    ]
    .iter()
    .filter_map(|k| o.get(*k).and_then(Value::as_str))
    .map(str::trim)
    .find(|s| !s.is_empty())
    .map(str::to_owned)
}

/// Parse the `wham/usage` reply into a sample.
pub fn parse_usage(body: &Value, now: DateTime<Utc>) -> Result<Sample, String> {
    let rl = body
        .get("rate_limit")
        .filter(|r| r.is_object())
        .ok_or("unexpected ChatGPT usage response (no rate_limit)")?;
    let mut r = Report::default();
    for key in ["primary_window", "secondary_window"] {
        if let Some(w) = rl.get(key).and_then(|w| live_window(w, now)) {
            r.place(w);
        }
    }
    if r.session.is_none() && r.weekly.is_none() {
        return Err("ChatGPT usage response has no limit windows".into());
    }
    r.plan = body
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    r.credits = credits_json(body.get("credits"));
    r.banked = body
        .get("rate_limit_reset_credits")
        .and_then(|c| c.get("available_count"))
        .and_then(Value::as_u64);
    if let Some(arr) = body.get("additional_rate_limits").and_then(Value::as_array) {
        for e in arr {
            let Some(o) = e.as_object() else { continue };
            let Some(name) = bucket_name(o) else { continue };
            let inner = o.get("rate_limit").unwrap_or(e);
            for key in ["primary_window", "secondary_window"] {
                if let Some(w) = inner.get(key).and_then(|w| live_window(w, now)) {
                    r.extra.push((name.clone(), w));
                }
            }
        }
    }
    Ok(r.sample(now))
}

// ---------------------------------------------------------------- the login

/// What `auth.json` holds, as yes/no facts (safe to print).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AuthSummary {
    pub mode: Option<String>,
    pub access_token: bool,
    pub account_id: bool,
    pub api_key_only: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A ChatGPT sign-in from `auth.json` (never printed).
pub struct Login {
    token: String,
    account_id: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

impl Login {
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The `exp` claim of a JWT (the only claim read; nothing else of the token is looked at).
pub fn jwt_expiry(token: &str) -> Option<DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let bytes = b64url_decode(payload)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(time_value)
}

/// Read the sign-in from the text of `auth.json`.
pub fn parse_auth(text: &str) -> Result<(Login, AuthSummary), (String, AuthSummary)> {
    let mut sum = AuthSummary::default();
    let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
        return Err(("Codex auth.json is not valid JSON".into(), sum));
    };
    sum.mode = v
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tokens = v.get("tokens").filter(|t| t.is_object());
    let token = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let account = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty());
    sum.access_token = token.is_some();
    sum.account_id = account.is_some();
    sum.api_key_only = token.is_none()
        && v.get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|k| !k.trim().is_empty());
    let Some(token) = token else {
        let why = if sum.api_key_only {
            "Codex is signed in with an API key, which has no plan limits: sign in with ChatGPT in Codex"
        } else {
            "no ChatGPT sign-in in the Codex auth.json: open Codex and sign in"
        };
        return Err((why.into(), sum));
    };
    sum.expires_at = jwt_expiry(token);
    Ok((
        Login {
            token: token.to_owned(),
            account_id: account.map(str::to_owned),
            expires_at: sum.expires_at,
        },
        sum,
    ))
}

const EXPIRED: &str = "Codex login expired: open Codex once";

pub struct CodexUsage {
    pub env: Env,
    pub url: String,
    pub timeout: StdDuration,
}

impl CodexUsage {
    pub fn new(env: Env) -> Self {
        // a test hook only (see `endpoints`); real builds always use the real endpoint
        let url =
            crate::endpoints::test_env("AITRACK_CODEX_URL").unwrap_or_else(|| USAGE_URL.to_owned());
        Self {
            env,
            url,
            timeout: StdDuration::from_secs(20),
        }
    }

    fn login(&self, now: DateTime<Utc>) -> Result<Login, CollectError> {
        let path = auth_path(&self.env);
        let meta = std::fs::metadata(&path).map_err(|_| {
            CollectError::new(format!(
                "Codex login not found ({}): open Codex and sign in",
                path.display()
            ))
        })?;
        if meta.len() > MAX_AUTH_BYTES {
            return Err(CollectError::new("Codex auth.json is unexpectedly large"));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| CollectError::new(format!("Codex auth.json unreadable: {e}")))?;
        let (login, _) = parse_auth(&text).map_err(|(m, _)| CollectError::new(m))?;
        // no process is started to refresh it: say so instead
        if login.expires_at.is_some_and(|t| t < now) {
            return Err(CollectError::new(EXPIRED));
        }
        Ok(login)
    }
}

impl UsageSource for CodexUsage {
    fn name(&self) -> &'static str {
        SOURCE_USAGE
    }

    fn live(&self) -> bool {
        true
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let login = self.login(now)?;
        crate::endpoints::check(&self.url, crate::endpoints::CODEX_HOST)
            .map_err(CollectError::new)?;
        if !crate::endpoints::header_value_ok(&login.token)
            || login
                .account_id
                .as_deref()
                .is_some_and(|a| !crate::endpoints::header_value_ok(a))
        {
            return Err(CollectError::new(
                "the Codex login has unusable characters: open Codex and sign in again",
            ));
        }
        let mut builder = ureq::AgentBuilder::new().timeout(self.timeout).redirects(0);
        if self.url.starts_with("https:") {
            builder = builder.try_proxy_from_env(true);
        }
        let mut req = builder
            .build()
            .get(&self.url)
            .set("Authorization", &format!("Bearer {}", login.token))
            .set("Accept", "application/json")
            .set("User-Agent", "aiTrack");
        if let Some(a) = &login.account_id {
            req = req.set("ChatGPT-Account-Id", a);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(401, _)) => return Err(CollectError::new(EXPIRED)),
            Err(ureq::Error::Status(403, _)) => {
                return Err(CollectError::new(
                    "ChatGPT usage refused (HTTP 403): the service did not accept the request",
                ))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let retry_after = resp
                    .header("Retry-After")
                    .and_then(crate::live::parse_retry_after);
                return Err(CollectError {
                    message: format!("ChatGPT usage HTTP {code}"),
                    retry_after,
                });
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(CollectError::new(format!(
                    "ChatGPT usage network error: {}",
                    crate::endpoints::transport_text(&t)
                )))
            }
        };
        if resp.status() >= 300 {
            return Err(CollectError::new(format!(
                "ChatGPT usage unexpected HTTP {} (redirects are not followed)",
                resp.status()
            )));
        }
        let bytes = crate::endpoints::read_capped(&mut resp.into_reader())
            .map_err(|e| CollectError::new(format!("ChatGPT usage: {e}")))?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| CollectError::new("unexpected (non-JSON) ChatGPT usage response"))?;
        let sample = parse_usage(&body, now).map_err(CollectError::new)?;
        Ok(Observation {
            latest: sample,
            history: vec![],
            raw: None,
        })
    }
}

// ---------------------------------------------------------------- the session logs

/// One `rate_limits` report from a session log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub at: DateTime<Utc>,
    /// `codex` for the main allowance; other ids are extra (per-model) buckets.
    pub limit_id: String,
    pub name: Option<String>,
    pub windows: Vec<Window>,
    pub plan: Option<String>,
    pub credits: Option<Value>,
}

impl LogEntry {
    pub fn main(&self) -> bool {
        self.limit_id == MAIN_LIMIT
    }
}

/// Parse one session-log line; `None` for anything that is not a usable `rate_limits` report.
pub fn parse_log_line(line: &[u8]) -> Option<LogEntry> {
    // cheap prefilter: most lines are messages and tool calls
    if !line.windows(13).any(|w| w == b"\"rate_limits\"") {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    let payload = v.get("payload")?;
    let rl = payload.get("rate_limits").filter(|r| r.is_object())?;
    let at = v.get("timestamp").and_then(time_value)?;
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| rl.get(*k).and_then(|w| log_window(w, at)))
        .collect();
    if windows.is_empty() {
        return None;
    }
    let limit_id = rl
        .get("limit_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(MAIN_LIMIT)
        .to_owned();
    Some(LogEntry {
        at,
        name: rl
            .get("limit_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        limit_id,
        windows,
        plan: rl
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        credits: credits_json(rl.get("credits")),
    })
}

/// A session file found on disk, with its modification time.
#[derive(Debug, Clone, PartialEq)]
pub struct LogFile {
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
}

fn is_session_file(name: &str) -> bool {
    name.starts_with("rollout-") && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
}

fn files_in(dir: &Path, out: &mut Vec<LogFile>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_session_file(&name) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_file() {
            out.push(LogFile {
                path: e.path(),
                modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            });
        }
    }
}

/// Sub-folders of `dir` whose names are all digits, newest (largest) first.
fn dated_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, p)| p).collect()
}

/// The newest session files (at most `max`), newest first: the dated `sessions` tree is walked
/// from the newest day back until enough are found, then the archived ones are added.
pub fn newest_log_files(env: &Env, max: usize) -> Vec<LogFile> {
    let mut out = Vec::new();
    'walk: for y in dated_dirs(&sessions_dir(env)) {
        for m in dated_dirs(&y) {
            for d in dated_dirs(&m) {
                files_in(&d, &mut out);
                if out.len() >= max {
                    break 'walk;
                }
            }
        }
    }
    files_in(&archived_dir(env), &mut out);
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.path.cmp(&a.path)));
    out.truncate(max);
    out
}

/// The usage reports of one session file (a plain file's last 4 MB, or a whole `.zst` one).
pub fn read_log_file(path: &Path) -> std::io::Result<Vec<LogEntry>> {
    let zst = path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("zst"));
    if !zst {
        return Ok(crate::grok::tail_lines(path)?
            .iter()
            .filter_map(|l| parse_log_line(l))
            .collect());
    }
    let f = std::fs::File::open(path)?;
    let mut d = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
        std::io::BufReader::new(f),
        8 * 1024 * 1024,
    )
    .map_err(|e| std::io::Error::other(format!("invalid zstd file: {e}")))?;
    let mut buf = Vec::new();
    (&mut d).take(MAX_ZST_BYTES).read_to_end(&mut buf)?;
    Ok(buf
        .split(|b| *b == b'\n')
        .filter_map(parse_log_line)
        .collect())
}

/// Every usage report in the newest session files, oldest first (one per time and bucket).
pub fn read_log_entries(env: &Env) -> std::io::Result<Vec<LogEntry>> {
    let files = newest_log_files(env, MAX_LOG_FILES);
    let mut out = Vec::new();
    let mut last_err = None;
    let mut any_ok = false;
    for f in &files {
        match read_log_file(&f.path) {
            Ok(v) => {
                any_ok = true;
                out.extend(v);
            }
            Err(e) => last_err = Some(e),
        }
    }
    if let (false, Some(e)) = (any_ok, last_err) {
        return Err(e);
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.limit_id.cmp(&b.limit_id)));
    out.dedup_by(|a, b| a.at == b.at && a.limit_id == b.limit_id);
    Ok(out)
}

/// Turn log reports (oldest first) into samples of the main allowance, each carrying the newest
/// extra buckets seen up to its time.
pub fn log_samples(entries: &[LogEntry]) -> Vec<Sample> {
    let mut extras: Vec<(String, LogEntry)> = Vec::new();
    let mut out = Vec::new();
    for e in entries {
        if !e.main() {
            match extras.iter_mut().find(|(id, _)| *id == e.limit_id) {
                Some(slot) => slot.1 = e.clone(),
                None => extras.push((e.limit_id.clone(), e.clone())),
            }
            continue;
        }
        let mut r = Report {
            plan: e.plan.clone(),
            credits: e.credits.clone(),
            ..Default::default()
        };
        for w in &e.windows {
            r.place(w.clone());
        }
        for (id, x) in &extras {
            let name = x.name.clone().unwrap_or_else(|| id.clone());
            for w in &x.windows {
                // a bucket whose window has reset since belongs to an earlier window
                if w.resets_at.is_none_or(|t| t > e.at) {
                    r.extra.push((name.clone(), w.clone()));
                }
            }
        }
        out.push(r.sample(e.at));
    }
    out
}

pub struct CodexLog {
    pub env: Env,
}

impl UsageSource for CodexLog {
    fn name(&self) -> &'static str {
        SOURCE_LOG
    }

    fn max_age(&self) -> Option<Duration> {
        Some(Duration::minutes(LOG_MAX_AGE_MIN))
    }

    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "Codex session log is stale (last usage line {} ago): use Codex to refresh",
            crate::sources::age_text(age_min)
        )
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let dir = sessions_dir(&self.env);
        if !dir.is_dir() && !archived_dir(&self.env).is_dir() {
            return Err(CollectError::new(format!(
                "Codex session logs not found ({}): run a Codex task once",
                dir.display()
            )));
        }
        let entries = read_log_entries(&self.env)
            .map_err(|e| CollectError::new(format!("Codex session log unreadable: {e}")))?;
        let horizon = now + Duration::minutes(5);
        let history: Vec<Sample> = log_samples(&entries)
            .into_iter()
            .filter(|s| s.at <= horizon && s.at >= now - Duration::days(HISTORY_DAYS))
            .collect();
        let Some(latest) = history.last().cloned() else {
            return Err(CollectError::new(
                "no usage line in the Codex session logs yet: run a Codex task once",
            ));
        };
        Ok(Observation {
            latest,
            history,
            raw: None,
        })
    }

    /// A reading of a weekly window that has already reset is last week's number.
    fn reject(&self, obs: &Observation, now: DateTime<Utc>) -> Option<String> {
        let w = obs.latest.weekly.as_ref()?;
        let end = w.resets_at.as_deref().and_then(parse_ts)?;
        (end < now).then(|| {
            format!(
                "Codex weekly window ended and the session logs have no newer usage line (last known {}%): use Codex to refresh",
                w.pct.round() as i64
            )
        })
    }
}

/// The default Codex chain: the live usage endpoint first, then the session logs.
pub fn build_chain(env: &Env) -> Vec<Box<dyn UsageSource>> {
    vec![
        Box::new(CodexUsage::new(env.clone())),
        Box::new(CodexLog { env: env.clone() }),
    ]
}

// ---------------------------------------------------------------- `--sources`

/// Structure-only diagnostic of the Codex sources (no tokens, ids or file contents).
pub fn diagnose(env: &Env, now: DateTime<Utc>) -> String {
    let mut o = String::new();
    let mut line = |s: String| {
        o.push_str(&s);
        o.push('\n');
    };
    let yn = |b: bool| if b { "yes" } else { "no" };
    let home = codex_home(env);
    line(
        "ChatGPT (Codex: 5-hour and weekly limits; tried in this order: live usage, then the session logs)"
            .into(),
    );
    line(format!(
        "   codex home: {} [{}]",
        home.display(),
        if home.is_dir() { "exists" } else { "missing" }
    ));
    let auth = auth_path(env);
    line(format!(
        "   auth.json: [{}] {}",
        if auth.is_file() { "exists " } else { "missing" },
        auth.display()
    ));
    if auth.is_file() {
        let read = std::fs::File::open(&auth)
            .map_err(|e| e.to_string())
            .and_then(|mut f| crate::endpoints::read_capped(&mut f))
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        match read {
            Err(e) => line(format!("     readable: no ({e})")),
            Ok(t) => {
                let (sum, problem) = match parse_auth(&t) {
                    Ok((_, s)) => (s, None),
                    Err((why, s)) => (s, Some(why)),
                };
                line(format!(
                    "     sign-in mode: {}; ChatGPT token: {}; account id: {}",
                    sum.mode.as_deref().unwrap_or("not stated"),
                    yn(sum.access_token),
                    yn(sum.account_id)
                ));
                if let Some(t) = sum.expires_at {
                    let days = (t - now).num_minutes() as f64 / 1440.0;
                    line(format!(
                        "     token expires: {}",
                        if days >= 0.0 {
                            format!("in {days:.1} days")
                        } else {
                            format!("{:.1} days ago (expired: open Codex once)", -days)
                        }
                    ));
                }
                if let Some(why) = problem {
                    line(format!("     {why}"));
                }
            }
        }
    }
    let dir = sessions_dir(env);
    line(format!(
        "   sessions: [{}] {}",
        if dir.is_dir() { "exists " } else { "missing" },
        dir.display()
    ));
    let files = newest_log_files(env, MAX_LOG_FILES);
    line(format!(
        "     newest session files read: {} (of at most {MAX_LOG_FILES})",
        files.len()
    ));
    match read_log_entries(env) {
        Err(e) => line(format!("     readable: no ({e})")),
        Ok(entries) => {
            let main: Vec<&LogEntry> = entries.iter().filter(|e| e.main()).collect();
            let extra = entries.len() - main.len();
            line(format!(
                "     usage lines: {} (extra buckets: {extra})",
                main.len()
            ));
            if let Some(n) = main.last() {
                let age = (now - n.at).num_minutes();
                let ws: Vec<String> = n
                    .windows
                    .iter()
                    .map(|w| {
                        format!(
                            "{} min window {}%{}",
                            w.minutes.round() as i64,
                            w.pct,
                            w.resets_at
                                .map(|t| if t >= now {
                                    format!(
                                        ", resets in {}",
                                        crate::sources::age_text((t - now).num_minutes())
                                    )
                                } else {
                                    " (reset passed)".into()
                                })
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                line(format!(
                    "     newest usage line: {} ago: {}",
                    crate::sources::age_text(age),
                    ws.join("; ")
                ));
                line(format!(
                    "     plan known: {}; credits reported: {}",
                    yn(n.plan.is_some()),
                    yn(n.credits.is_some())
                ));
                line(format!("     current: {}", yn(age <= LOG_MAX_AGE_MIN)));
            }
        }
    }
    line(
        "   live usage endpoint: not called by --sources (a token is only used by the running app)"
            .into(),
    );
    o
}
