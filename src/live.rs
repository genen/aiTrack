// SPDX-License-Identifier: GPL-3.0-or-later
//! Poll Claude's plan-usage endpoint and record every reading.
//!
//! Uses the OAuth token Claude Code keeps in `~/.claude/.credentials.json` (on macOS, when that
//! file is absent, in the login Keychain). The token is read fresh on every poll (so a refresh by Claude Code is picked up), used for one request, and
//! never stored or logged.

use crate::reading::Kind;
use crate::reading::{Bucket, Reading};
use crate::sources::{self, Env, OauthSource, Observation, Sample, UsageSource};
use crate::store::Store;
use crate::util::{expand_user, home_dir, iso, now_iso, parse_ts};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const BETA_HEADER: &str = "oauth-2025-04-20";
pub const DEFAULT_INTERVAL_MIN: f64 = 15.0;
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct CollectError {
    pub message: String,
    /// Seconds from a `Retry-After` header, if the server sent one.
    pub retry_after: Option<f64>,
}

impl CollectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
        }
    }
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CollectError {}

// ---------------------------------------------------------------- credentials

pub struct Credential {
    pub token: String,
    /// Milliseconds since the epoch, if known.
    pub expires_at_ms: Option<f64>,
}

/// A place a Claude login can be found. Sources are tried in order; add new ones (for example the
/// Claude desktop app's encrypted cache) to [`default_sources`].
pub trait CredentialSource: Send + Sync {
    fn label(&self) -> String;
    /// `None` when this source has no usable login (missing, unreadable or malformed file).
    fn load(&self) -> Option<Credential>;
}

/// `.credentials.json` as written by Claude Code (`claudeAiOauth.accessToken`).
pub struct CredentialsFile {
    pub path: PathBuf,
}

impl CredentialSource for CredentialsFile {
    fn label(&self) -> String {
        self.path.display().to_string()
    }

    fn load(&self) -> Option<Credential> {
        let mut file = std::fs::File::open(&self.path).ok()?;
        let bytes = crate::endpoints::read_capped(&mut file).ok()?;
        parse_credentials(&bytes)
    }
}

/// The `claudeAiOauth` login in the JSON Claude Code stores (file or Keychain item).
pub fn parse_credentials(bytes: &[u8]) -> Option<Credential> {
    let data: Value = serde_json::from_slice(bytes).ok()?;
    let oauth = data.get("claudeAiOauth")?;
    let token = oauth
        .get("accessToken")?
        .as_str()
        .filter(|t| !t.is_empty())?;
    Some(Credential {
        token: token.to_owned(),
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_f64),
    })
}

/// The plan Claude Code saved with its login (`claudeAiOauth.subscriptionType`), as people know it
/// (`max` -> `Max`). Only that field is read; the token is not kept. `rateLimitTier` (5x/20x) is
/// left out: it is known to be wrong on some accounts.
pub fn parse_subscription(bytes: &[u8]) -> Option<String> {
    let data: Value = serde_json::from_slice(bytes).ok()?;
    let plan = data
        .get("claudeAiOauth")?
        .get("subscriptionType")?
        .as_str()?
        .trim();
    // an empty, placeholder or implausibly long value is not a plan name
    if plan.is_empty() || plan.eq_ignore_ascii_case("unknown") || plan.chars().count() > 32 {
        return None;
    }
    Some(match plan.to_ascii_lowercase().as_str() {
        "free" => "Free".into(),
        "pro" => "Pro".into(),
        "max" => "Max".into(),
        "team" => "Team".into(),
        "enterprise" => "Enterprise".into(),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    })
}

/// Keychain service name Claude Code uses on macOS.
pub const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// How long `security` may take (an unanswered Keychain access dialog would otherwise block forever).
pub const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `cmd` (stdin null, stdout piped, stderr null) and collect its output, but give up after
/// `limit`: the child is killed and reaped and a short error is returned. Nothing of the output is
/// logged. The output is read on a helper thread so a full pipe cannot stall the wait.
pub fn run_bounded(
    cmd: &mut std::process::Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.take(1 << 21).read_to_end(&mut buf);
        }
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("timed out after {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait: {e}"));
            }
        }
    };
    let stdout = reader.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// Claude Code's login in the macOS login Keychain (same JSON as `.credentials.json`).
/// UNVERIFIED guess about the owner's Mac: service name and payload shape are assumed, not seen.
/// Only the `security` tool is run; its output is parsed and dropped, never logged or stored.
pub struct KeychainItem;

impl CredentialSource for KeychainItem {
    fn label(&self) -> String {
        format!("macOS Keychain ({KEYCHAIN_SERVICE})")
    }

    #[cfg(target_os = "macos")]
    fn load(&self) -> Option<Credential> {
        let mut cmd = std::process::Command::new("/usr/bin/security");
        cmd.args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]);
        let out = run_bounded(&mut cmd, KEYCHAIN_TIMEOUT).ok()?;
        if !out.status.success() || out.stdout.len() > 1 << 20 {
            return None;
        }
        parse_credentials(&out.stdout)
    }

    #[cfg(not(target_os = "macos"))]
    fn load(&self) -> Option<Credential> {
        None
    }
}

/// `$CLAUDE_CONFIG_DIR/.credentials.json` (if set) then `~/.claude/.credentials.json`, then on
/// macOS the login Keychain.
pub fn default_sources_from(
    config_dir: Option<&str>,
    home: Option<PathBuf>,
) -> Vec<Box<dyn CredentialSource>> {
    let mut v: Vec<Box<dyn CredentialSource>> = credentials_files(config_dir, home)
        .into_iter()
        .map(|path| Box::new(CredentialsFile { path }) as Box<dyn CredentialSource>)
        .collect();
    if cfg!(target_os = "macos") {
        v.push(Box::new(KeychainItem));
    }
    v
}

/// `$CLAUDE_CONFIG_DIR/.credentials.json` (if set), then `~/.claude/.credentials.json`.
pub fn credentials_files(config_dir: Option<&str>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = config_dir.filter(|d| !d.is_empty()) {
        v.push(expand_user(dir).join(".credentials.json"));
    }
    v.push(
        home.unwrap_or_default()
            .join(".claude")
            .join(".credentials.json"),
    );
    v
}

pub fn default_sources() -> Vec<Box<dyn CredentialSource>> {
    default_sources_from(
        std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        home_dir(),
    )
}

/// `now` as Unix seconds (for the token-expiry check).
pub fn unix_secs(now: DateTime<Utc>) -> f64 {
    now.timestamp_millis() as f64 / 1000.0
}

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn read_token(
    sources: &[Box<dyn CredentialSource>],
    now_secs: f64,
) -> Result<String, CollectError> {
    // an expired login in one file must not hide a usable one in the next
    let mut saw_expired = false;
    for s in sources {
        let Some(c) = s.load() else { continue };
        if c.expires_at_ms.is_some_and(|exp| exp / 1000.0 < now_secs) {
            saw_expired = true;
            continue;
        }
        return Ok(c.token);
    }
    if saw_expired {
        return Err(CollectError::new(
            "login token expired; open Claude Code once to refresh it",
        ));
    }
    let tried: Vec<String> = sources.iter().map(|s| s.label()).collect();
    Err(CollectError::new(format!(
        "no Claude login found (looked in: {}). Sign in with Claude Code (run `claude`, then /login); \
         a login that only exists in the Claude desktop app is not supported yet",
        tried.join(", ")
    )))
}

// ---------------------------------------------------------------- HTTP

/// The longest wait a server's `Retry-After` may ask for (one day).
pub const MAX_RETRY_AFTER_SECS: u64 = 24 * 3600;

/// A `Retry-After` header value as seconds: plain whole seconds only (a date, a fraction, `inf`,
/// a negative or garbage value is ignored), capped at [`MAX_RETRY_AFTER_SECS`].
pub fn parse_retry_after(value: &str) -> Option<f64> {
    let secs: u64 = value.trim().parse().ok()?;
    Some(secs.min(MAX_RETRY_AFTER_SECS) as f64)
}

pub fn fetch_usage(url: &str, token: &str, timeout: Duration) -> Result<Value, CollectError> {
    // the bearer goes only to the real endpoint (see `endpoints`), never along a redirect
    crate::endpoints::check(url, crate::endpoints::CLAUDE_HOST).map_err(CollectError::new)?;
    if !crate::endpoints::header_value_ok(token) {
        return Err(CollectError::new(
            "the stored login has an unusable token; sign in to Claude Code again",
        ));
    }
    let mut builder = ureq::AgentBuilder::new().timeout(timeout).redirects(0);
    if url.starts_with("https:") {
        builder = builder.try_proxy_from_env(true);
    }
    let result = builder
        .build()
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", BETA_HEADER)
        .set("Accept", "application/json")
        .set("User-Agent", "aitrack")
        .call();
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(code, resp)) => {
            if code == 401 || code == 403 {
                return Err(CollectError::new(format!(
                    "login rejected (HTTP {code}); sign in to Claude Code again"
                )));
            }
            let retry_after = resp.header("Retry-After").and_then(parse_retry_after);
            return Err(CollectError {
                message: format!("HTTP {code}"),
                retry_after,
            });
        }
        Err(ureq::Error::Transport(t)) => {
            return Err(CollectError::new(format!(
                "network error: {}",
                crate::endpoints::transport_text(&t)
            )))
        }
    };
    if resp.status() >= 300 {
        return Err(CollectError::new(format!(
            "unexpected HTTP {} (redirects are not followed)",
            resp.status()
        )));
    }
    let bytes =
        crate::endpoints::read_capped(&mut resp.into_reader()).map_err(CollectError::new)?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| CollectError::new("unexpected (non-JSON) response"))?;
    if !body.is_object() {
        return Err(CollectError::new("unexpected response shape"));
    }
    Ok(body)
}

// ---------------------------------------------------------------- normalization

fn bucket(b: Option<&Value>) -> Value {
    match b.and_then(|b| Some((b.get("utilization")?.as_f64()?, b))) {
        Some((pct, b)) => {
            json!({"pct": pct, "resets_at": b.get("resets_at").cloned().unwrap_or(Value::Null)})
        }
        None => Value::Null,
    }
}

fn money(a: Option<&Value>) -> Value {
    let a = match a {
        Some(a) => a,
        None => return Value::Null,
    };
    let Some(minor) = a.get("amount_minor").and_then(Value::as_f64) else {
        return Value::Null;
    };
    let exp = a.get("exponent").and_then(Value::as_i64).unwrap_or(0);
    json!(minor / 10f64.powi(exp as i32))
}

/// The response fields aiTrack understands; only these are stored under `raw`.
const RAW_KEYS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_opus",
    "seven_day_sonnet",
    "spend",
    "extra_usage",
    "limits",
];

/// True for a key that names an account, organisation, user or credential (never stored).
fn id_like(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.ends_with("_id")
        || ["uuid", "org", "account", "email", "user", "token", "secret"]
            .iter()
            .any(|w| k.contains(w))
}

/// A copy of `v` without any id-like keys, at any depth.
fn scrub(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !id_like(k))
                .map(|(k, v)| (k.clone(), scrub(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(scrub).collect()),
        other => other.clone(),
    }
}

/// The part of a response that is stored under `raw`: only the known fields, with id-like keys
/// removed, so an organisation or account id a server adds never reaches the disk.
pub fn whitelist_raw(raw: &Value) -> Value {
    let Some(m) = raw.as_object() else {
        return Value::Null;
    };
    Value::Object(
        m.iter()
            .filter(|(k, _)| RAW_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), scrub(v)))
            .collect(),
    )
}

/// Turn the raw endpoint response into the stored record (schema `v`: 1). Only the fields aiTrack
/// understands are kept under `raw` (see [`whitelist_raw`]); older files with a fuller `raw` still load.
pub fn normalize(raw: &Value, ts: &str) -> Value {
    let spend = match raw.get("spend") {
        Some(Value::Object(s)) if !s.is_empty() => json!({
            "enabled": s.get("enabled").map(truthy).unwrap_or(false),
            "used": money(s.get("used")),
            "limit": money(s.get("limit")),
        }),
        _ => Value::Null,
    };
    json!({
        "v": SCHEMA_VERSION,
        "ts": ts,
        "ok": true,
        "session": bucket(raw.get("five_hour")),
        "weekly": bucket(raw.get("seven_day")),
        "spend": spend,
        "limits": raw.get("limits").map(scrub).unwrap_or(Value::Null),
        "raw": whitelist_raw(raw),
    })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn error_record(ts: &str, message: &str) -> Value {
    json!({"v": SCHEMA_VERSION, "ts": ts, "ok": false, "error": message})
}

/// `session 42%, weekly 22%` for a stored record.
pub fn describe(record: &Value) -> String {
    Reading::from_value(record).describe()
}

// ---------------------------------------------------------------- polling

/// Per-source set of already stored sample times (epoch seconds), built once from the store.
#[derive(Default)]
struct Index {
    scanned: bool,
    stored: HashMap<String, HashSet<i64>>,
    /// The newest reset time seen per window kind (session, weekly), from the stored history and
    /// from every source of this run: what a sample without a reset time borrows.
    memo: [Option<ResetMemo>; 2],
}

/// A reset time and the sample it came with.
#[derive(Debug, Clone, Copy)]
struct ResetMemo {
    resets: DateTime<Utc>,
    pct: f64,
    at: DateTime<Utc>,
}

impl Index {
    fn learn(&mut self, kind: usize, resets: DateTime<Utc>, pct: f64, at: DateTime<Utc>) {
        if self.memo[kind].is_none_or(|m| at >= m.at) {
            self.memo[kind] = Some(ResetMemo { resets, pct, at });
        }
    }
}

/// Tries the configured sources in order and records the first fresh reading (or one error that
/// lists why each source failed).
pub struct Poller {
    chain: Vec<Box<dyn UsageSource>>,
    index: Mutex<Index>,
    /// Written into every record as `vendor` (absent for Claude).
    vendor: Option<&'static str>,
}

impl Poller {
    /// A poller that only uses the OAuth endpoint with the given credential sources.
    pub fn new(url: impl Into<String>, credentials: Vec<Box<dyn CredentialSource>>) -> Self {
        Self::with_chain(vec![Box::new(OauthSource {
            url: url.into(),
            credentials,
            timeout: Duration::from_secs(20),
        })])
    }

    /// A poller whose records are tagged `vendor` (and which should write to that vendor's own file).
    pub fn with_chain_for(chain: Vec<Box<dyn UsageSource>>, vendor: &'static str) -> Self {
        let mut p = Self::with_chain(chain);
        p.vendor = Some(vendor);
        p
    }

    /// Grok's chain (live billing, then the CLI log) from the process environment;
    /// `AITRACK_GROK_URL` overrides the billing endpoint in test builds only.
    pub fn grok_from_env() -> Self {
        Self::with_chain_for(crate::grok::build_chain(&Env::from_process()), "grok")
    }

    /// Codex's chain (live usage, then the session logs) from the process environment;
    /// `AITRACK_CODEX_URL` overrides the usage endpoint in test builds only.
    pub fn codex_from_env() -> Self {
        Self::with_chain_for(crate::codex::build_chain(&Env::from_process()), "codex")
    }

    pub fn with_chain(chain: Vec<Box<dyn UsageSource>>) -> Self {
        Self {
            chain,
            index: Mutex::new(Index::default()),
            vendor: None,
        }
    }

    /// Chain for the real world: the real endpoint (`$AITRACK_USAGE_URL` in test builds) and
    /// `$AITRACK_SOURCES` (comma list of `desktop-cache`, `desktop-history`, `claude-json`, `oauth`).
    pub fn from_env() -> Self {
        // the URL override is a test hook (see `endpoints`); real builds ignore the variable
        let url = crate::endpoints::test_env("AITRACK_USAGE_URL");
        let only = std::env::var("AITRACK_SOURCES").ok();
        Self::with_env(
            &Env::from_process(),
            url.as_deref().unwrap_or(DEFAULT_URL),
            only.as_deref(),
        )
    }

    pub fn with_env(env: &Env, url: &str, only: Option<&str>) -> Self {
        Self::with_chain(sources::build_chain(env, url, only))
    }

    pub fn poll_once(&self, store: &dyn Store) -> Result<Value, CollectError> {
        self.poll_at(store, Utc::now())
    }

    /// Take one reading and append it. Errors are recorded too, so gaps are explainable.
    ///
    /// Sources are read in chain order. The first fresh one is the current reading, except that a
    /// later history source replaces it when its newest sample is newer (the Claude app wrote to
    /// its history after the cache entry was fetched). Sources that hold history are always imported
    /// into the store, even when they are not the current reading; a live source (the endpoint) is
    /// only asked when nothing else is current.
    ///
    /// The returned record is the current reading. When it is already stored, nothing is appended
    /// and the returned record carries `"stored": false`.
    pub fn poll_at(&self, store: &dyn Store, now: DateTime<Utc>) -> Result<Value, CollectError> {
        self.poll_inner(store, now, true)
    }

    /// Re-read only the local sources (files; cheap): never asks a live source and records nothing
    /// when none is current, so it can run every minute without writing error rows.
    pub fn poll_local(&self, store: &dyn Store) -> Result<Value, CollectError> {
        self.poll_inner(store, Utc::now(), false)
    }

    pub fn poll_local_at(
        &self,
        store: &dyn Store,
        now: DateTime<Utc>,
    ) -> Result<Value, CollectError> {
        self.poll_inner(store, now, false)
    }

    fn poll_inner(
        &self,
        store: &dyn Store,
        now: DateTime<Utc>,
        network: bool,
    ) -> Result<Value, CollectError> {
        let mut failures: Vec<(&'static str, CollectError)> = Vec::new();
        // (chain index, what the source returned, is it fresh enough)
        let mut seen: Vec<(usize, Observation, bool)> = Vec::new();
        let mut current: Option<usize> = None;
        // a live source (the endpoint) that failed while a local source still served the reading
        let mut live_failure: Option<String> = None;
        for (i, src) in self.chain.iter().enumerate() {
            if current.is_some() && !src.imports_history() {
                continue;
            }
            if !network && src.live() {
                continue;
            }
            let obs = match src.read(now) {
                Ok(o) => o,
                Err(e) => {
                    if src.live() {
                        let msg: String = e.message.chars().take(120).collect();
                        live_failure = Some(format!("{}: {msg}", src.name()));
                    }
                    failures.push((src.name(), e));
                    continue;
                }
            };
            if src.live() {
                let ts = iso(now);
                let mut rec = match &obs.raw {
                    Some(raw) => {
                        let mut r = normalize(raw, &ts);
                        tag(&mut r, src.name(), &ts);
                        r
                    }
                    // sources whose reply is not Claude-shaped (Grok billing): just the sample
                    None => sample_record(&obs.latest, src.name(), false),
                };
                // why the sources ahead of this one did not supply the reading (no ids or tokens)
                let note = Self::skipped_note(&failures);
                if !note.is_empty() {
                    rec["note"] = json!(note);
                }
                self.append(store, &mut rec)?;
                return Ok(rec);
            }
            let age = now - obs.latest.at;
            let mut fresh = src.max_age().is_none_or(|max| age <= max);
            if !fresh {
                failures.push((
                    src.name(),
                    CollectError::new(src.stale_message(age.num_minutes())),
                ));
            } else if let Some(why) = src.reject(&obs, now) {
                fresh = false;
                failures.push((src.name(), CollectError::new(why)));
            }
            seen.push((i, obs, fresh));
            let k = seen.len() - 1;
            if fresh {
                match current {
                    None => current = Some(k),
                    Some(c) if seen[k].1.latest.at > seen[c].1.latest.at => current = Some(k),
                    _ => {}
                }
            }
        }

        // Remember every reset time seen so far (stored history and this poll's sources): a sample
        // from a source that reports percentages only borrows the newest one that still fits.
        {
            let mut idx = self.index.lock().unwrap_or_else(|e| e.into_inner());
            self.ensure_scanned(store, &mut idx);
            for (_, obs, _) in &seen {
                for s in obs.history.iter().chain(std::iter::once(&obs.latest)) {
                    for (i, b) in [(0, &s.session), (1, &s.weekly)] {
                        if let Some(b) = b {
                            if let Some(r) = b.resets_at.as_deref().and_then(parse_ts) {
                                idx.learn(i, r, b.pct, s.at);
                            }
                        }
                    }
                }
            }
        }
        let note = current.map(|c| self.chain_note(&failures, &seen, c));

        // Import everything new from the snapshot sources; only the current reading's newest
        // sample is a regular row, the rest is flagged as backfill.
        let mut current_rec = None;
        for (k, (i, obs, _)) in seen.iter().enumerate() {
            let name = self.chain[*i].name();
            let new = self.new_samples(store, name, obs, crate::store::prune_cutoff(now));
            for s in new.iter() {
                let is_current = current == Some(k) && s.at == obs.latest.at;
                // only the current reading's newest sample is a regular row; every other imported
                // sample is backfill (an older sample must never look like the newest poll)
                let mut rec = snapshot_record(obs, s, name, !is_current);
                self.carry_resets(&mut rec, s.at, now);
                if is_current {
                    if let Some(n) = note.as_ref().filter(|n| !n.is_empty()) {
                        rec["note"] = json!(n);
                    }
                }
                self.append(store, &mut rec)?;
                self.mark_stored(name, s);
                if is_current {
                    current_rec = Some(rec);
                }
            }
        }
        // The live source failed but a local one answered: the poll "succeeds", so without a row the
        // failure would be invisible while the data quietly ages. Record it (it is not a reading).
        if let (true, Some(msg), Some(_)) = (network, &live_failure, current) {
            let mut rec = error_record(&iso(now), msg);
            self.stamp(&mut rec);
            if let Err(e) = store.append(&rec) {
                crate::alerts::log_line(&format!("could not record a failed poll: {e}"));
            }
        }
        if let Some(c) = current {
            let (i, obs, _) = &seen[c];
            return Ok(current_rec.unwrap_or_else(|| {
                let mut rec = snapshot_record(obs, &obs.latest, self.chain[*i].name(), false);
                self.carry_resets(&mut rec, obs.latest.at, now);
                if let Some(n) = note.as_ref().filter(|n| !n.is_empty()) {
                    rec["note"] = json!(n);
                }
                rec["stored"] = Value::Bool(false);
                self.stamp(&mut rec);
                rec
            }));
        }
        let err = combine_failures(failures, self.chain.len());
        if network {
            let mut rec = error_record(&iso(now), &err.message);
            self.stamp(&mut rec);
            // the poll failed and so did recording it: say so instead of leaving a silent gap
            if let Err(e) = store.append(&rec) {
                crate::alerts::log_line(&format!(
                    "could not record a failed poll in {}: {e}",
                    store.describe()
                ));
            }
        }
        Err(err)
    }

    /// "{name} skipped: {reason}" for each failed or stale source, `; `-joined (reasons cut to 70
    /// characters).
    fn skipped_note(failures: &[(&'static str, CollectError)]) -> String {
        failures
            .iter()
            .map(|(n, e)| {
                let msg: String = e.message.chars().take(70).collect();
                format!("{n} skipped: {msg}")
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Why the reading came from the source it did (shown in the diagnostics): the sources ahead of
    /// it in the chain that failed or were skipped, and that a newer sample of a later source beat a
    /// fresh earlier one. No tokens, ids or paths: only source names, ages and short reasons.
    fn chain_note(
        &self,
        failures: &[(&'static str, CollectError)],
        seen: &[(usize, Observation, bool)],
        current: usize,
    ) -> String {
        let cur_idx = seen[current].0;
        let pos = |name: &str| self.chain.iter().position(|s| s.name() == name);
        let mut parts: Vec<String> = failures
            .iter()
            .filter(|(n, _)| pos(n).is_some_and(|p| p < cur_idx))
            .map(|(n, e)| {
                let msg: String = e.message.chars().take(70).collect();
                format!("{n} skipped: {msg}")
            })
            .collect();
        for (k, (i, obs, fresh)) in seen.iter().enumerate() {
            if k != current && *fresh && *i < cur_idx {
                let mins = (seen[current].1.latest.at - obs.latest.at).num_minutes();
                let gap = if mins < 1 {
                    "about the same age as".to_string()
                } else {
                    format!("{mins} min older than")
                };
                parts.push(format!(
                    "{} entry was {gap} the {} sample",
                    self.chain[*i].name(),
                    self.chain[cur_idx].name()
                ));
            }
        }
        parts.join("; ")
    }

    /// Give a sample that reports a percentage but no reset time the newest reset time of the same
    /// window seen before (see [`Index::memo`]), marked `resets_carried`. Only while that time is
    /// still ahead, the sample lies inside the window it defines, and the percentage has not
    /// dropped since (a drop means the window reset). Nothing is invented otherwise.
    fn carry_resets(&self, rec: &mut Value, at: DateTime<Utc>, now: DateTime<Utc>) {
        let idx = self.index.lock().unwrap_or_else(|e| e.into_inner());
        for (i, (key, kind)) in [("session", Kind::Session), ("weekly", Kind::Weekly)]
            .into_iter()
            .enumerate()
        {
            let Some(m) = idx.memo[i] else { continue };
            let Some(b) = rec.get_mut(key).and_then(Value::as_object_mut) else {
                continue;
            };
            if b.get("resets_at").is_some_and(Value::is_string) {
                continue;
            }
            let Some(pct) = b.get("pct").and_then(Value::as_f64) else {
                continue;
            };
            let window = chrono::Duration::minutes((kind.hours() * 60.0) as i64);
            let consistent = m.resets > now
                && m.resets > at
                && at >= m.resets - window - chrono::Duration::minutes(2)
                && !(at > m.at && pct < m.pct - 1.0);
            if consistent {
                b.insert("resets_at".into(), json!(iso(m.resets)));
                b.insert("resets_carried".into(), Value::Bool(true));
            }
        }
    }

    /// Mark a record with this poller's vendor (Claude records carry no `vendor` field).
    fn stamp(&self, rec: &mut Value) {
        if let Some(v) = self.vendor {
            rec["vendor"] = json!(v);
        }
    }

    fn append(&self, store: &dyn Store, rec: &mut Value) -> Result<(), CollectError> {
        self.stamp(rec);
        store
            .append(rec)
            .map_err(|e| CollectError::new(format!("could not write {}: {e}", store.describe())))
    }

    fn ensure_scanned(&self, store: &dyn Store, idx: &mut Index) {
        if idx.scanned {
            return;
        }
        idx.scanned = true;
        let live_names: Vec<&str> = self
            .chain
            .iter()
            .filter(|s| s.live())
            .map(|s| s.name())
            .collect();
        store.read_each(&mut |v| {
            let src = v.get("source").and_then(Value::as_str);
            let at = v
                .get("observed_at")
                .and_then(Value::as_str)
                .and_then(parse_ts);
            let when = at.or_else(|| v.get("ts").and_then(Value::as_str).and_then(parse_ts));
            if let Some(t) = when {
                for (i, key) in ["session", "weekly"].into_iter().enumerate() {
                    let b = v.get(key);
                    let resets = b
                        .and_then(|b| b.get("resets_at"))
                        .and_then(Value::as_str)
                        .and_then(parse_ts);
                    let pct = b.and_then(|b| b.get("pct")).and_then(Value::as_f64);
                    if let (Some(r), Some(p)) = (resets, pct) {
                        idx.learn(i, r, p, t);
                    }
                }
            }
            if let (Some(src), Some(at)) = (src, at) {
                // live sources are not de-duplicated: every poll is a new reading
                if !live_names.contains(&src) {
                    idx.stored
                        .entry(src.to_owned())
                        .or_default()
                        .insert(at.timestamp());
                }
            }
        });
    }

    fn new_samples<'a>(
        &self,
        store: &dyn Store,
        source: &str,
        obs: &'a Observation,
        cutoff: DateTime<Utc>,
    ) -> Vec<&'a Sample> {
        let mut idx = self.index.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_scanned(store, &mut idx);
        let known = idx.stored.get(source);
        // samples past retention are not imported: they were pruned, or would be at once
        let mut out: Vec<&Sample> = obs
            .history
            .iter()
            .filter(|s| s.at >= cutoff)
            .filter(|s| !known.is_some_and(|k| k.contains(&s.at.timestamp())))
            .collect();
        out.sort_by_key(|s| s.at);
        out.dedup_by_key(|s| s.at.timestamp());
        out
    }

    fn mark_stored(&self, source: &str, s: &Sample) {
        let mut idx = self.index.lock().unwrap_or_else(|e| e.into_inner());
        idx.stored
            .entry(source.to_owned())
            .or_default()
            .insert(s.at.timestamp());
    }
}

/// Add the optional v1 fields every record carries now.
fn tag(rec: &mut Value, source: &str, observed_at: &str) {
    rec["source"] = json!(source);
    rec["observed_at"] = json!(observed_at);
}

fn bucket_json(b: &Option<Bucket>) -> Value {
    match b {
        Some(b) => json!({"pct": b.pct, "resets_at": b.resets_at}),
        None => Value::Null,
    }
}

/// Stored record for a snapshot sample (schema v1; `resets_at` is null when the source has none).
/// `ts` is the sample's own time, so history lines up with when usage actually happened.
pub fn sample_record(s: &Sample, source: &str, backfill: bool) -> Value {
    let at = iso(s.at);
    let mut rec = json!({
        "v": SCHEMA_VERSION,
        "ts": at,
        "ok": true,
        "session": bucket_json(&s.session),
        "weekly": bucket_json(&s.weekly),
        "spend": Value::Null,
        "limits": Value::Null,
        "raw": s.raw.clone().unwrap_or(Value::Null),
    });
    // per-model buckets a source put among its extras (Codex) are stored where Claude's are
    if let Some(l) = rec["raw"].as_object_mut().and_then(|r| r.remove("limits")) {
        rec["limits"] = l;
    }
    tag(&mut rec, source, &at);
    if backfill {
        rec["backfill"] = Value::Bool(true);
    }
    rec
}

/// Record for a sample of `obs`. When the source returned a full endpoint-shaped response for its
/// newest sample (the desktop cache), that goes through the same normalisation as the OAuth reply
/// (spend, per-model limits, raw), with the sample's time as `ts`.
fn snapshot_record(obs: &Observation, s: &Sample, source: &str, backfill: bool) -> Value {
    match &obs.raw {
        Some(raw) if s.at == obs.latest.at => {
            let at = iso(s.at);
            let mut rec = normalize(raw, &at);
            tag(&mut rec, source, &at);
            if backfill {
                rec["backfill"] = Value::Bool(true);
            }
            rec
        }
        _ => sample_record(s, source, backfill),
    }
}

/// One error that says why every source failed. A single-source chain keeps its message as is.
fn combine_failures(
    mut failures: Vec<(&'static str, CollectError)>,
    chain_len: usize,
) -> CollectError {
    if failures.is_empty() {
        return CollectError::new("no local usage source configured");
    }
    if chain_len == 1 && failures.len() == 1 {
        return failures.remove(0).1;
    }
    let retry_after = failures
        .iter()
        .find(|(n, _)| *n == sources::OAUTH)
        .and_then(|(_, e)| e.retry_after);
    let parts: Vec<String> = failures
        .into_iter()
        .map(|(n, e)| {
            if n == sources::OAUTH {
                format!("usage API: {}", e.message)
            } else {
                e.message
            }
        })
        .collect();
    CollectError {
        message: parts.join("; "),
        retry_after,
    }
}

/// Headless collector loop. Returns the exit code (0 after a good last reading, 1 after a failure).
pub fn run(
    poller: &Poller,
    store: &dyn Store,
    interval_min: f64,
    once: bool,
    log: &mut dyn FnMut(&str),
) -> i32 {
    log(&format!(
        "aiTrack collector: every {interval_min} min -> {} (Ctrl+C to stop)",
        store.describe()
    ));
    loop {
        let mut wait = interval_min * 60.0;
        let code = match poller.poll_once(store) {
            Ok(rec) => {
                log(&format!(
                    "{}  {}",
                    rec["ts"].as_str().unwrap_or(""),
                    describe(&rec)
                ));
                0
            }
            Err(e) => {
                log(&format!("{}  failed: {e}", now_iso()));
                if let Some(r) = e.retry_after.filter(|r| *r > 0.0) {
                    wait = wait.max(r);
                }
                1
            }
        };
        if once {
            return code;
        }
        std::thread::sleep(Duration::try_from_secs_f64(wait).unwrap_or(Duration::from_secs(60)));
    }
}
