// SPDX-License-Identifier: GPL-3.0-or-later
//! Grok (xAI) weekly credit usage: one pool per billing period, no 5-hour window.
//!
//! Two sources, tried in this order on a full poll (the log alone on the short local tick):
//!
//! 1. **grok-billing**: the CLI's own billing endpoint
//!    (`https://cli-chat-proxy.grok.com/v1/billing?format=credits`) with the xAI sign-in the Grok
//!    CLI stored in `auth.json`. The request shape and the response fields
//!    (`config.creditUsagePercent`, `config.currentPeriod.end`, `subscriptionTier`) follow the
//!    Grok poller of CodeZeno's claude-code-usage-monitor (MIT licensed), which asks the same
//!    endpoint. Only xAI sign-in entries are used, never corporate OIDC tokens or API keys; the
//!    token is read fresh on each poll and is never stored, logged or put into an error message.
//!    This source never spawns a process: an expired login is reported ("run `grok` once"), not
//!    refreshed. (The separate Refresh button, `grok_cli`, does start the CLI, on request.)
//! 2. **grok-log**: `~/.grok/logs/unified.jsonl`, where the CLI logs
//!    `billing: fetched credits config` snapshots. No token, no network; but it is only as fresh as
//!    the last time the CLI fetched billing, so a snapshot older than two hours, or one whose
//!    period has ended, is not "current". (The log layout was learned from a description of the
//!    format; nothing is copied.)
//!
//! xAI omits `creditUsagePercent` when usage is exactly zero (a fresh week): that is 0 %.
//! Readings are stored in `usage-grok.jsonl` (record field `vendor: "grok"`), never in Claude's file.

use crate::live::CollectError;
use crate::reading::Bucket;
use crate::sources::{Env, Observation, Sample, UsageSource};
use crate::util::{iso, parse_ts};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

pub const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// Static marker the CLI proxy uses to route CLI user-token requests.
pub const TOKEN_AUTH: &str = "xai-grok-cli";
pub const CLIENT_MODE: &str = "cli";
/// Sent as `x-grok-client-version` when `GROK_CLIENT_VERSION` is not set (we never ask the CLI).
pub const DEFAULT_CLIENT_VERSION: &str = "1.0.0";
pub const LOG_MESSAGE: &str = "billing: fetched credits config";
pub const SOURCE_BILLING: &str = "grok-billing";
pub const SOURCE_LOG: &str = "grok-log";
/// A log snapshot older than this is not "current".
pub const LOG_MAX_AGE_MIN: i64 = 120;
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TAIL_LINES: usize = 10_000;
const CHUNK: u64 = 64 * 1024;
const MAX_AUTH_BYTES: u64 = 1024 * 1024;
const MAX_SNAPSHOTS: usize = 3000;

/// `$GROK_HOME` (read into [`Env`]) or `~/.grok`.
pub fn grok_home(env: &Env) -> PathBuf {
    env.grok_home
        .clone()
        .unwrap_or_else(|| env.home.clone().unwrap_or_default().join(".grok"))
}

pub fn log_path(env: &Env) -> PathBuf {
    grok_home(env).join("logs").join("unified.jsonl")
}

pub fn auth_path(env: &Env) -> PathBuf {
    grok_home(env).join("auth.json")
}

/// Is Grok installed for this user (its data folder exists)?
pub fn detected(env: &Env) -> bool {
    grok_home(env).is_dir()
}

// ---------------------------------------------------------------- the log

/// One `billing: fetched credits config` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub fetched_at: DateTime<Utc>,
    pub pct: f64,
    pub period_end: DateTime<Utc>,
    pub tier: Option<String>,
}

fn time_value(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => parse_ts(s),
        Value::Number(n) => {
            let secs = n.as_f64()?;
            let secs = if secs > 1e11 { secs / 1000.0 } else { secs };
            (secs > 0.0)
                .then(|| Utc.timestamp_millis_opt((secs * 1000.0) as i64).single())
                .flatten()
        }
        _ => None,
    }
}

/// Parse one log line; `None` for anything that is not a usable billing snapshot.
pub fn parse_snapshot(line: &[u8]) -> Option<Snapshot> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("msg")?.as_str()? != LOG_MESSAGE {
        return None;
    }
    let fetched_at = parse_ts(v.get("ts")?.as_str()?)?;
    let ctx = v.get("ctx")?;
    let config = ctx.get("config")?.as_object()?;
    // absent = exactly zero usage (a freshly reset week)
    let pct = match config.get("creditUsagePercent") {
        None => 0.0,
        Some(p) => p
            .as_f64()
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))?,
    };
    let period_end = time_value(config.get("currentPeriod")?.get("end")?)?;
    let tier = ctx
        .get("subscriptionTier")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    Some(Snapshot {
        fetched_at,
        pct,
        period_end,
        tier,
    })
}

/// Lines from the end of the file, newest first, within the byte and line caps.
pub fn tail_lines(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    let mut f = std::fs::File::open(path)?;
    let mut pos = f.seek(SeekFrom::End(0))?;
    let (mut scanned, mut rest) = (0u64, Vec::<u8>::new());
    let mut lines: Vec<Vec<u8>> = Vec::new();
    while pos > 0 && scanned < MAX_TAIL_BYTES && lines.len() < MAX_TAIL_LINES {
        let n = CHUNK.min(pos).min(MAX_TAIL_BYTES - scanned);
        pos -= n;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; n as usize];
        f.read_exact(&mut chunk)?;
        scanned += n;
        chunk.extend_from_slice(&rest);
        let mut parts: Vec<&[u8]> = chunk.split(|b| *b == b'\n').collect();
        // the first part may be the tail of a line that starts in the previous chunk
        let first = parts.remove(0).to_vec();
        for p in parts.iter().rev() {
            if !p.is_empty() {
                lines.push(p.to_vec());
                if lines.len() >= MAX_TAIL_LINES {
                    return Ok(lines);
                }
            }
        }
        rest = first;
    }
    // a line that begins exactly at the start of the file (or of the window we read)
    if pos == 0 && !rest.is_empty() && lines.len() < MAX_TAIL_LINES {
        lines.push(rest);
    }
    Ok(lines)
}

/// Every usable snapshot in the tail of the log, newest first.
pub fn read_snapshots(path: &Path) -> std::io::Result<Vec<Snapshot>> {
    let mut out: Vec<Snapshot> = tail_lines(path)?
        .iter()
        .filter_map(|l| parse_snapshot(l))
        .take(MAX_SNAPSHOTS)
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.fetched_at));
    Ok(out)
}

fn sample_of(s: &Snapshot) -> Sample {
    Sample {
        at: s.fetched_at,
        session: None,
        weekly: Some(Bucket {
            carried: false,
            pct: s.pct,
            resets_at: Some(iso(s.period_end)),
        }),
        raw: s.tier.as_ref().map(|t| json!({"tier": t})),
    }
}

pub struct GrokLog {
    pub env: Env,
}

impl UsageSource for GrokLog {
    fn name(&self) -> &'static str {
        SOURCE_LOG
    }

    fn max_age(&self) -> Option<Duration> {
        Some(Duration::minutes(LOG_MAX_AGE_MIN))
    }

    fn stale_message(&self, age_min: i64) -> String {
        format!(
            "Grok CLI log is stale (last billing fetch {} ago): run `grok` to refresh",
            crate::sources::age_text(age_min)
        )
    }

    fn read(&self, _now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let path = log_path(&self.env);
        if !path.is_file() {
            return Err(CollectError::new(format!(
                "Grok CLI log not found ({})",
                path.display()
            )));
        }
        let snaps = read_snapshots(&path)
            .map_err(|e| CollectError::new(format!("Grok CLI log unreadable: {e}")))?;
        let Some(newest) = snaps.first() else {
            return Err(CollectError::new(
                "no billing snapshot in the Grok CLI log yet: run `grok` once",
            ));
        };
        let mut history: Vec<Sample> = snaps.iter().map(sample_of).collect();
        history.sort_by_key(|s| s.at);
        Ok(Observation {
            latest: sample_of(newest),
            history,
            raw: None,
        })
    }

    /// A snapshot of a billing period that has already ended is last week's number.
    fn reject(&self, obs: &Observation, now: DateTime<Utc>) -> Option<String> {
        let end = obs
            .latest
            .weekly
            .as_ref()?
            .resets_at
            .as_deref()
            .and_then(parse_ts)?;
        (end < now).then(|| {
            format!(
                "Grok billing period ended and the CLI log has no newer snapshot (last known {}%): run `grok` to refresh",
                obs.latest.weekly.as_ref().map(|b| b.pct.round() as i64).unwrap_or(0)
            )
        })
    }
}

// ---------------------------------------------------------------- the login

/// An xAI sign-in from `auth.json` (never printed; see the module docs).
pub struct Session {
    token: String,
    user_id: String,
    expires_at: Option<DateTime<Utc>>,
    can_refresh: bool,
}

impl Session {
    /// When this login expires, if the file says.
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
}

/// What `auth.json` holds, as counts (safe to print).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AuthSummary {
    pub entries: usize,
    pub xai_signins: usize,
    pub other: usize,
    pub expired_xai: usize,
}

fn is_xai_scope(scope: &str) -> bool {
    scope == "https://accounts.x.ai/sign-in"
        || scope
            .strip_prefix("https://auth.x.ai::")
            .is_some_and(|c| !c.is_empty())
}

/// Pick the xAI sign-in that expires last; also summarise the file (counts only).
pub fn select_session(
    text: &str,
    now: DateTime<Utc>,
) -> Result<(Session, AuthSummary), (String, AuthSummary)> {
    let mut sum = AuthSummary::default();
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
    else {
        return Err(("Grok auth.json is not a JSON object".into(), sum));
    };
    sum.entries = map.len();
    let mut best: Option<Session> = None;
    for (scope, e) in &map {
        if !is_xai_scope(scope) {
            sum.other += 1;
            continue;
        }
        let token = e
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if token.is_empty() {
            continue;
        }
        sum.xai_signins += 1;
        let expires_at = e.get("expires_at").and_then(time_value);
        if expires_at.is_some_and(|t| t < now) {
            sum.expired_xai += 1;
        }
        let s = Session {
            token: token.to_owned(),
            user_id: e
                .get("user_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            expires_at,
            can_refresh: e
                .get("refresh_token")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.trim().is_empty()),
        };
        // a login that is not known to be expired beats an expired one; a missing `expires_at` is
        // "unknown", not worse than a date in the past; then the later known expiry wins
        let rank = |s: &Session| (s.expires_at.is_none_or(|t| t >= now), s.expires_at);
        if best.as_ref().is_none_or(|b| rank(&s) > rank(b)) {
            best = Some(s);
        }
    }
    match best {
        Some(s) => Ok((s, sum)),
        None => Err((
            "no xAI sign-in in the Grok auth.json (only other logins or none): run `grok` and sign in".into(),
            sum,
        )),
    }
}

// ---------------------------------------------------------------- the live endpoint

/// Parse the billing response into a sample (percent, period end, tier).
pub fn parse_billing(body: &Value, now: DateTime<Utc>) -> Result<Sample, String> {
    let config = body
        .get("config")
        .filter(|c| c.is_object())
        .ok_or("unexpected Grok billing response (no config)")?;
    let end = config
        .get("currentPeriod")
        .and_then(|p| p.get("end"))
        .and_then(time_value);
    // protobuf JSON drops zero values: a complete period without creditUsagePercent is 0 % used
    let pct = match config.get("creditUsagePercent").and_then(Value::as_f64) {
        Some(p) if p.is_finite() => p,
        Some(_) => return Err("unexpected Grok billing response (bad percent)".into()),
        None if end.is_some() => 0.0,
        None => return Err("Grok billing response has no credit usage".into()),
    };
    let tier = body
        .get("subscriptionTier")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty());
    Ok(Sample {
        at: now,
        session: None,
        weekly: Some(Bucket {
            carried: false,
            pct: pct.clamp(0.0, 100.0),
            resets_at: end.map(iso),
        }),
        raw: tier.map(|t| json!({"tier": t})),
    })
}

pub struct GrokBilling {
    pub env: Env,
    pub url: String,
    pub timeout: StdDuration,
}

impl GrokBilling {
    pub fn new(env: Env) -> Self {
        // a test hook only (see `endpoints`); real builds always use the real endpoint
        let url = crate::endpoints::test_env("AITRACK_GROK_URL")
            .unwrap_or_else(|| BILLING_URL.to_owned());
        Self {
            env,
            url,
            timeout: StdDuration::from_secs(20),
        }
    }
}

const EXPIRED: &str = "Grok login expired: run `grok` once";

impl UsageSource for GrokBilling {
    fn name(&self) -> &'static str {
        SOURCE_BILLING
    }

    fn live(&self) -> bool {
        true
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        let path = auth_path(&self.env);
        let text = std::fs::metadata(&path)
            .map_err(|_| {
                CollectError::new(format!(
                    "Grok login not found ({}): run `grok` and sign in",
                    path.display()
                ))
            })
            .and_then(|m| {
                if m.len() > MAX_AUTH_BYTES {
                    Err(CollectError::new("Grok auth.json is unexpectedly large"))
                } else {
                    std::fs::read_to_string(&path)
                        .map_err(|e| CollectError::new(format!("Grok auth.json unreadable: {e}")))
                }
            })?;
        let (session, _) = select_session(&text, now).map_err(|(m, _)| CollectError::new(m))?;
        // no process is spawned to refresh it: say so instead
        if session.expires_at.is_some_and(|t| t < now) {
            return Err(CollectError::new(EXPIRED));
        }
        let _ = session.can_refresh;
        crate::endpoints::check(&self.url, crate::endpoints::GROK_HOST)
            .map_err(CollectError::new)?;
        let version = std::env::var("GROK_CLIENT_VERSION")
            .ok()
            .filter(|v| crate::endpoints::header_value_ok(v))
            .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.to_owned());
        if !crate::endpoints::header_value_ok(&session.token)
            || !crate::endpoints::header_value_ok(&session.user_id) && !session.user_id.is_empty()
        {
            return Err(CollectError::new(
                "the Grok login has unusable characters: run `grok` and sign in again",
            ));
        }
        let mut builder = ureq::AgentBuilder::new().timeout(self.timeout).redirects(0);
        if self.url.starts_with("https:") {
            builder = builder.try_proxy_from_env(true);
        }
        let mut req = builder
            .build()
            .get(&self.url)
            .set("Authorization", &format!("Bearer {}", session.token))
            .set("X-XAI-Token-Auth", TOKEN_AUTH)
            .set("x-grok-client-mode", CLIENT_MODE)
            .set("x-grok-client-version", &version)
            .set("Accept", "application/json");
        if !session.user_id.is_empty() {
            req = req.set("x-userid", &session.user_id);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(401 | 403, _)) => return Err(CollectError::new(EXPIRED)),
            Err(ureq::Error::Status(code, resp)) => {
                let retry_after = resp
                    .header("Retry-After")
                    .and_then(crate::live::parse_retry_after);
                return Err(CollectError {
                    message: format!("Grok billing HTTP {code}"),
                    retry_after,
                });
            }
            // the transport error text never contains request headers
            Err(ureq::Error::Transport(t)) => {
                return Err(CollectError::new(format!(
                    "Grok billing network error: {t}"
                )))
            }
        };
        if resp.status() >= 300 {
            return Err(CollectError::new(format!(
                "Grok billing unexpected HTTP {} (redirects are not followed)",
                resp.status()
            )));
        }
        let bytes = crate::endpoints::read_capped(&mut resp.into_reader())
            .map_err(|e| CollectError::new(format!("Grok billing: {e}")))?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| CollectError::new("unexpected (non-JSON) Grok billing response"))?;
        let sample = parse_billing(&body, now).map_err(CollectError::new)?;
        Ok(Observation {
            latest: sample,
            history: vec![],
            raw: None,
        })
    }
}

/// The default Grok chain: live billing first, then the CLI log.
pub fn build_chain(env: &Env) -> Vec<Box<dyn UsageSource>> {
    vec![
        Box::new(GrokBilling::new(env.clone())),
        Box::new(GrokLog { env: env.clone() }),
    ]
}

// ---------------------------------------------------------------- `--sources`

/// Structure-only diagnostic of the Grok sources (no tokens, ids or file contents).
pub fn diagnose(env: &Env, now: DateTime<Utc>) -> String {
    let mut o = String::new();
    let mut line = |s: String| {
        o.push_str(&s);
        o.push('\n');
    };
    let yn = |b: bool| if b { "yes" } else { "no" };
    let home = grok_home(env);
    line("Grok (weekly credits; tried in this order: live billing, then the CLI log)".into());
    line(format!(
        "   grok home: {} [{}]",
        home.display(),
        if home.is_dir() { "exists" } else { "missing" }
    ));
    // --- login
    let auth = auth_path(env);
    line(format!(
        "   auth.json: [{}] {}",
        if auth.is_file() { "exists " } else { "missing" },
        auth.display()
    ));
    if auth.is_file() {
        // the same 1 MiB cap as the live read
        let read = std::fs::File::open(&auth)
            .map_err(|e| e.to_string())
            .and_then(|mut f| crate::endpoints::read_capped(&mut f))
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        match read {
            Err(e) => line(format!("     readable: no ({e})")),
            Ok(t) => {
                let (sum, problem) = match select_session(&t, now) {
                    Ok((_, sum)) => (sum, None),
                    Err((why, sum)) => (sum, Some(why)),
                };
                line(format!(
                    "     entries: {} (xAI sign-in: {}, other logins/keys ignored: {}, xAI expired: {})",
                    sum.entries, sum.xai_signins, sum.other, sum.expired_xai
                ));
                if let Some(why) = problem {
                    line(format!("     {why}"));
                }
            }
        }
    }
    // --- log
    let log = log_path(env);
    line(format!(
        "   unified.jsonl: [{}] {}",
        if log.is_file() { "exists " } else { "missing" },
        log.display()
    ));
    if log.is_file() {
        match read_snapshots(&log) {
            Err(e) => line(format!("     readable: no ({e})")),
            Ok(s) => {
                line(format!(
                    "     billing snapshots in the last 4 MB: {}",
                    s.len()
                ));
                if let Some(n) = s.first() {
                    let age = (now - n.fetched_at).num_minutes();
                    let days = (n.period_end - now).num_minutes() as f64 / 1440.0;
                    line(format!(
                        "     newest snapshot: {} ago, {}% used",
                        crate::sources::age_text(age),
                        n.pct
                    ));
                    line(format!(
                        "     period end: {}",
                        if days >= 0.0 {
                            format!("in {days:.1} days")
                        } else {
                            format!("{:.1} days ago (ended)", -days)
                        }
                    ));
                    line(format!(
                        "     subscription tier known: {}",
                        yn(n.tier.is_some())
                    ));
                    line(format!(
                        "     current: {}",
                        yn(age <= LOG_MAX_AGE_MIN && days >= 0.0)
                    ));
                }
            }
        }
    }
    line("   live billing endpoint: not called by --sources (a token is only used by the running app)".into());
    o
}
