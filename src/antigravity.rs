// SPDX-License-Identifier: GPL-3.0-or-later
//! Gemini (Google Antigravity) plan usage: the weekly limits Antigravity shows on its Usage page.
//!
//! Since June 2026, free and Google AI Pro / Ultra accounts use Gemini through Google Antigravity
//! (the desktop app and the `agy` CLI); both show the same meter: a **weekly limit per model
//! group** ("Gemini Models": Gemini Flash and Pro; "Claude and GPT models": the other models
//! Antigravity offers). The numbers are not stored on disk: Antigravity fetches them from Google
//! each time it shows them.
//!
//! One source, **agy-usage**: aiTrack runs `agy -p /usage --output-format json`, the CLI's
//! documented non-interactive answer for its read-only `/usage` command (no prompt is sent, no
//! model is used, nothing is written). The reply is
//! `{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"name","buckets":[{"id",
//! "window":"weekly","remaining_fraction","reset_time"}]}]}}}` (checked with agy 1.4.12 on the
//! author's PC, 2026-11-01). aiTrack never reads Antigravity's login (it stays in the system's
//! credential store) and makes no network call of its own for Gemini.
//!
//! The "Gemini Models" group fills the block's bars (its weekly bucket, and a shorter one as the
//! Session row should Antigravity add one); every other group is a per-model line in the
//! tooltips, like Claude's per-model limits. Readings are stored in `usage-antigravity.jsonl`
//! (record field `vendor: "antigravity"`).

use crate::live::CollectError;
use crate::reading::Bucket;
use crate::sources::{Env, Observation, Os, Sample, UsageSource};
use crate::util::iso;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

pub const SOURCE_USAGE: &str = "agy-usage";
/// The group whose limits are the block's bars.
pub const MAIN_GROUP: &str = "Gemini Models";
/// Upper bound for agy's reply (bytes).
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
/// How long one `agy -p /usage` run may take.
const RUN_TIMEOUT: StdDuration = StdDuration::from_secs(60);
/// A bucket shorter than this many hours is a session-type window, otherwise a weekly one.
const WEEKLY_FROM_H: f64 = 24.0;
/// A bucket counts as the weekly one between these lengths (hours); other lengths of a day or
/// more go to the extra limits, not the Weekly bar.
const WEEKLY_RANGE_H: std::ops::RangeInclusive<f64> = 144.0..=192.0;
/// The reply's text when agy answered but listed no limit: retried once (a cold run may answer
/// before the quota is fetched). Seen on 2026-11-01 on an Antigravity Business account (Google
/// Workspace sign-in on Agent Platform): no buckets, its usage is billed in Google Cloud.
pub const EMPTY_REPLY: &str = "No usage data from Antigravity (it listed no limits)";
/// [`EMPTY_REPLY`] when agy's own log of that run says it signed in with a Google Cloud
/// (Antigravity Business) account (`authMethod=gcp`, not `consumer`).
pub const BUSINESS_REPLY: &str =
    "Likely enterprise account: Antigravity reports no weekly limits (Antigravity Business usage is billed in Google Cloud)";
/// How much of the end of agy's newest log is searched for its sign-in type.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// The sign-in type agy's newest log reports (`consumer`, `gcp`, ...): agy writes one log per run
/// (`cli-YYYYMMDD_HHMMSS.log`) with a line `applyAuthResult: ... authMethod=<type> ...`. Only
/// that word is read, never the email or project on the same line.
pub fn auth_method(env: &Env) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let dir = cli_dir(env).join("log");
    let newest = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("cli-") && n.ends_with(".log"))
        })
        // (names carry the start time)
        .max()?;
    let mut f = std::fs::File::open(newest).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    f.take(LOG_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().find_map(|l| {
        let rest = &l[l.find("authMethod=")? + "authMethod=".len()..];
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(24)
            .collect();
        (!word.is_empty()).then(|| word.to_ascii_lowercase())
    })
}
/// The wait before that one retry.
const EMPTY_RETRY_AFTER: StdDuration = StdDuration::from_secs(5);
/// The reply's text when agy used the model (a turn or tokens): aiTrack stops running it.
pub const USED_MODEL: &str = "Antigravity usage: agy sent /usage to the model instead of answering it (that uses quota), so aiTrack stopped running agy until it restarts";

/// The `agy` program names on this platform.
fn exe_names(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &["agy.exe"],
        _ => &["agy"],
    }
}

/// Where the installer puts `agy`, before `PATH` is searched: `%LOCALAPPDATA%\agy\bin` on
/// Windows, `~/.local/bin` elsewhere (plus Homebrew's folders on macOS).
pub fn install_dirs(env: &Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match env.os {
        Os::Windows => {
            if let Some(l) = &env.localappdata {
                out.push(l.join("agy").join("bin"));
            }
        }
        _ => {
            if let Some(h) = &env.home {
                out.push(h.join(".local").join("bin"));
            }
            if env.os == Os::Mac {
                out.push(PathBuf::from("/opt/homebrew/bin"));
            }
            out.push(PathBuf::from("/usr/local/bin"));
        }
    }
    out
}

/// The `agy` program: `$AITRACK_AGY_CMD` (test builds only), the installer's folder, else `PATH`.
pub fn find_agy(env: &Env) -> Option<PathBuf> {
    if let Some(p) = crate::endpoints::test_env("AITRACK_AGY_CMD") {
        return Some(PathBuf::from(p));
    }
    let names = exe_names(env.os);
    let mut dirs = install_dirs(env);
    if let Some(path) = std::env::var_os("PATH") {
        // relative entries ("", ".") would check one folder and run from another
        dirs.extend(std::env::split_paths(&path).filter(|d| d.is_absolute()));
    }
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// Antigravity's CLI folder (`~/.gemini/antigravity-cli`).
pub fn cli_dir(env: &Env) -> PathBuf {
    env.home
        .clone()
        .unwrap_or_default()
        .join(".gemini")
        .join("antigravity-cli")
}

/// Antigravity's desktop app folder (`~/.gemini/antigravity`).
pub fn app_dir(env: &Env) -> PathBuf {
    env.home
        .clone()
        .unwrap_or_default()
        .join(".gemini")
        .join("antigravity")
}

/// Is Antigravity's CLI installed? (the numbers come from it, so the app alone is not enough)
pub fn detected(env: &Env) -> bool {
    find_agy(env).is_some()
}

/// The arguments of the usage run.
pub fn usage_args() -> Vec<OsString> {
    ["-p", "/usage", "--output-format", "json"]
        .iter()
        .map(OsString::from)
        .collect()
}

fn time_value(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(crate::util::parse_ts)
}

/// The window length in hours a bucket stands for (`window` text, else its id).
fn window_hours(b: &serde_json::Map<String, Value>) -> Option<f64> {
    let text = |k: &str| {
        b.get(k)
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    };
    for t in [
        text("window"),
        text("id"),
        text("bucketId"),
        text("bucket_id"),
    ]
    .into_iter()
    .flatten()
    {
        if t.contains("week")
            || t.ends_with("7d")
            || t.contains("7_day")
            || t.contains("7-day")
            || t.contains("7day")
            || t.contains("seven")
        {
            return Some(168.0);
        }
        if t.contains("day") || t.contains("daily") || t.ends_with("24h") || t.ends_with("1d") {
            return Some(24.0);
        }
        if t.contains("month") || t.ends_with("30d") {
            return Some(720.0);
        }
        if let Some(h) = t
            .rsplit(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .and_then(|w| w.strip_suffix('h'))
            .and_then(|n| n.parse::<f64>().ok())
        {
            return Some(h);
        }
    }
    None
}

/// A field under its snake_case name (agy's JSON) or its camelCase one (the RPC's).
fn field<'a>(o: &'a serde_json::Map<String, Value>, snake: &str, camel: &str) -> Option<&'a Value> {
    o.get(snake).or_else(|| o.get(camel))
}

/// One bucket as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub group: String,
    /// Used, 0-100 (from `remaining_fraction`).
    pub pct: f64,
    pub hours: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

fn limits_of(data: &Value) -> Vec<Limit> {
    let mut out = Vec::new();
    for g in data
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = g
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        for b in g
            .get("buckets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(o) = b.as_object() else { continue };
            let Some(rf) = field(o, "remaining_fraction", "remainingFraction")
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite())
            else {
                continue;
            };
            let pct = crate::util::round_to(((1.0 - rf) * 100.0).clamp(0.0, 100.0), 1);
            out.push(Limit {
                group: name.to_owned(),
                pct,
                // a bucket of unknown length is taken as the weekly one (all Antigravity has today)
                hours: window_hours(o).unwrap_or(168.0),
                resets_at: field(o, "reset_time", "resetTime").and_then(time_value),
            });
        }
    }
    out
}

fn is_main(group: &str) -> bool {
    group.eq_ignore_ascii_case(MAIN_GROUP) || group.to_ascii_lowercase().starts_with("gemini")
}

/// Antigravity's `reset_time` is when the bucket will be full again (usage refills gradually, an
/// unused bucket says "now + 7 days"), not the end of a fixed window: it moves on every poll, and
/// analysis and alerts key windows by the reset time. So the bucket has no reset; the time is kept
/// apart (`raw.full_at`) for the tooltip.
fn bucket_of(l: &Limit) -> Bucket {
    Bucket {
        carried: false,
        pct: l.pct,
        resets_at: None,
    }
}

/// A short, safe reason from a reply that did not succeed (no more than one line of 160 chars).
fn failure_text(body: &Value) -> String {
    let raw = ["error", "message", "response"]
        .iter()
        .filter_map(|k| match body.get(*k) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(o)) => o.get("message").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .map(|s| s.trim().to_owned())
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let low = raw.to_ascii_lowercase();
    if low.contains("not logged") || low.contains("not signed") || low.contains("sign in") {
        return "Antigravity is not signed in: run agy once and sign in".into();
    }
    let line: String = raw
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    if line.is_empty() {
        "Antigravity usage: agy did not report success".into()
    } else {
        format!("Antigravity usage: {line}")
    }
}

/// Parse agy's `/usage` JSON reply into a sample.
pub fn parse_usage(body: &Value, now: DateTime<Utc>) -> Result<Sample, String> {
    // a reply that took a model turn or tokens means agy no longer treats /usage as a command
    let turns = body.get("num_turns").and_then(Value::as_f64).unwrap_or(0.0);
    let tokens = body
        .get("usage")
        .and_then(|u| u.get("total_tokens"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if turns > 0.0 || tokens > 0.0 {
        return Err(USED_MODEL.into());
    }
    if let Some(s) = body.get("status").and_then(Value::as_str) {
        if !s.eq_ignore_ascii_case("success") {
            return Err(failure_text(body));
        }
    }
    let command = body
        .get("command")
        .ok_or("unexpected Antigravity usage reply (no command data)")?;
    if let Some(n) = command.get("name").and_then(Value::as_str) {
        if !n.eq_ignore_ascii_case("usage") {
            return Err(format!(
                "unexpected Antigravity usage reply (command {:?})",
                n.chars().take(40).collect::<String>()
            ));
        }
    }
    let data = command
        .get("data")
        .filter(|d| d.is_object())
        .ok_or("unexpected Antigravity usage reply (no command data)")?;
    let mut limits = limits_of(data);
    // "Gemini Models" fills the bars even when another Gemini group (a paid plan's "Gemini 4
    // Argon", say) is listed before it; a stable sort keeps the reply's order otherwise
    limits.sort_by_key(|l| !l.group.eq_ignore_ascii_case(MAIN_GROUP));
    if limits.is_empty() {
        return Err(EMPTY_REPLY.into());
    }
    let (mut session, mut weekly) = (None, None);
    let mut full_at = serde_json::Map::new();
    let mut extra = Vec::new();
    for l in &limits {
        if is_main(&l.group) {
            let slot = if WEEKLY_RANGE_H.contains(&l.hours) {
                Some(("weekly", &mut weekly))
            } else if l.hours < WEEKLY_FROM_H {
                Some(("session", &mut session))
            } else {
                None
            };
            if let Some((kind, slot)) = slot {
                if slot.is_none() {
                    *slot = Some(bucket_of(l));
                    if let Some(t) = l.resets_at {
                        full_at.insert(kind.into(), json!(iso(t)));
                    }
                    continue;
                }
            }
        }
        extra.push(l);
    }
    if session.is_none() && weekly.is_none() {
        return Err("Antigravity usage reply has no Gemini limit".into());
    }
    let mut raw = serde_json::Map::new();
    if !full_at.is_empty() {
        raw.insert("full_at".into(), Value::Object(full_at));
    }
    // a plan name, should agy ever add one to the reply
    if let Some(t) = ["tier", "userTier", "tierDisplayName", "plan"]
        .iter()
        .filter_map(|k| data.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty() && s.len() <= 48)
    {
        raw.insert("tier".into(), json!(t));
    }
    if !extra.is_empty() {
        let arr: Vec<Value> = extra
            .iter()
            .map(|l| {
                // a main-group bucket that is not the bar says which window it is
                let name = if is_main(&l.group) && !WEEKLY_RANGE_H.contains(&l.hours) {
                    let w = match l.hours {
                        h if h < WEEKLY_FROM_H => format!("{}h", crate::reading::fmt0(h)),
                        h if h < 48.0 => "daily".into(),
                        h if h >= 600.0 => "monthly".into(),
                        h => format!("{} days", crate::reading::fmt0(h / 24.0)),
                    };
                    format!("{} ({w})", l.group)
                } else {
                    l.group.clone()
                };
                json!({
                    "name": name,
                    "kind": if l.hours < WEEKLY_FROM_H { "session" } else { "weekly" },
                    "percent": l.pct,
                    // (no reset: see bucket_of)
                })
            })
            .collect();
        raw.insert("limits".into(), Value::Array(arr));
    }
    Ok(Sample {
        at: now,
        session,
        weekly,
        raw: (!raw.is_empty()).then_some(Value::Object(raw)),
    })
}

/// The command for a usage run: no window, no input, output captured (the run points it at a
/// file), started in the home folder
/// (agy treats its working folder as a workspace; home is one the user already trusts).
pub fn command(program: &Path, home: Option<&Path>) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    c.args(usage_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(h) = home.filter(|h| h.is_dir()) {
        c.current_dir(h);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(crate::grok_cli::CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // own process group, so a timeout can end agy and anything it started
        c.process_group(0);
    }
    c
}

/// A new, empty file for agy's output in the temp folder (agy writes to a file, not a pipe: a
/// helper agy starts may keep its output open after agy exits, and a file never blocks).
fn output_file() -> Result<(PathBuf, std::fs::File), String> {
    let dir = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for n in 0..8u32 {
        let p = dir.join(format!(
            "aitrack-agy-{}-{stamp}-{n}.json",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not make a file for agy's output: {e}")),
        }
    }
    Err("could not make a file for agy's output".into())
}

/// Run agy once and return what it printed (at most 1 MiB, at most [`RUN_TIMEOUT`]).
pub fn run_usage(
    program: &Path,
    home: Option<&Path>,
    timeout: StdDuration,
) -> Result<Vec<u8>, String> {
    let (path, file) = output_file()?;
    let result = run_into(program, home, timeout, &path, file);
    // (a helper still holding the file on Windows keeps it until it exits: best effort)
    let _ = std::fs::remove_file(&path);
    result
}

fn run_into(
    program: &Path,
    home: Option<&Path>,
    timeout: StdDuration,
    path: &Path,
    file: std::fs::File,
) -> Result<Vec<u8>, String> {
    let mut child = command(program, home)
        .stdout(std::process::Stdio::from(file))
        .spawn()
        .map_err(|e| format!("could not start agy: {e}"))?;
    let too_big = || std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_OUTPUT_BYTES);
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) => {}
            Err(e) => {
                crate::grok_cli::kill_pid(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not watch agy: {e}"));
            }
        }
        // (agy is still running here, so its pid is still its own)
        if too_big() {
            crate::grok_cli::kill_pid(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err("agy's reply is larger than 1 MiB".into());
        }
        if start.elapsed() >= timeout {
            crate::grok_cli::kill_pid(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("agy did not answer within {} s", timeout.as_secs()));
        }
        std::thread::sleep(StdDuration::from_millis(100));
    };
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut buf))
        .map_err(|e| format!("could not read agy's output: {e}"))?;
    if buf.len() as u64 > MAX_OUTPUT_BYTES {
        return Err("agy's reply is larger than 1 MiB".into());
    }
    if buf.iter().all(u8::is_ascii_whitespace) {
        return Err(match code {
            Some(c) => format!("agy printed nothing (exit code {c})"),
            None => "agy printed nothing".into(),
        });
    }
    Ok(buf)
}

/// The JSON object in agy's output (the last line that parses as one: anything printed before
/// it, such as an update notice, is skipped).
pub fn reply_json(out: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(out);
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if v.is_object() {
            return Some(v);
        }
    }
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(Value::is_object)
}

/// The live source: one `agy -p /usage` run per poll.
pub struct AgyUsage {
    pub env: Env,
    pub timeout: StdDuration,
    /// The wait before retrying a reply that listed no limits.
    pub retry_after: StdDuration,
    /// Set once agy used the model for /usage: no more runs until aiTrack restarts.
    stopped: std::sync::atomic::AtomicBool,
    /// The last poll's reply listed no limits: do not retry again (an account without per-user
    /// limits would otherwise run agy twice on every poll).
    was_empty: std::sync::atomic::AtomicBool,
}

impl AgyUsage {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            timeout: RUN_TIMEOUT,
            retry_after: EMPTY_RETRY_AFTER,
            stopped: std::sync::atomic::AtomicBool::new(false),
            was_empty: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn run_once(&self, program: &Path, now: DateTime<Utc>) -> Result<Sample, String> {
        let out = run_usage(program, self.env.home.as_deref(), self.timeout)
            .map_err(|e| format!("Antigravity usage: {e}"))?;
        let body = reply_json(&out)
            .ok_or_else(|| "unexpected (non-JSON) Antigravity usage reply".to_string())?;
        parse_usage(&body, now)
    }
}

impl UsageSource for AgyUsage {
    fn name(&self) -> &'static str {
        SOURCE_USAGE
    }

    fn live(&self) -> bool {
        true
    }

    fn read(&self, now: DateTime<Utc>) -> Result<Observation, CollectError> {
        use std::sync::atomic::Ordering;
        if self.stopped.load(Ordering::SeqCst) {
            return Err(CollectError::new(USED_MODEL));
        }
        let program = find_agy(&self.env).ok_or_else(|| {
            CollectError::new("Antigravity CLI (agy) not found: install it from antigravity.google")
        })?;
        let mut r = self.run_once(&program, now);
        let empty =
            |r: &Result<Sample, String>| r.as_ref().err().map(String::as_str) == Some(EMPTY_REPLY);
        if empty(&r) && !self.was_empty.load(Ordering::SeqCst) {
            // a cold agy can answer before it has fetched the quota: one more try
            std::thread::sleep(self.retry_after);
            r = self.run_once(&program, now);
        }
        self.was_empty.store(empty(&r), Ordering::SeqCst);
        if empty(&r) && auth_method(&self.env).is_some_and(|m| m != "consumer") {
            r = Err(BUSINESS_REPLY.into());
        }
        if r.as_ref().err().map(String::as_str) == Some(USED_MODEL) {
            self.stopped.store(true, Ordering::SeqCst);
        }
        let sample = r.map_err(CollectError::new)?;
        Ok(Observation {
            latest: sample,
            history: vec![],
            raw: None,
        })
    }
}

/// The Antigravity chain: the one live source.
pub fn build_chain(env: &Env) -> Vec<Box<dyn UsageSource>> {
    vec![Box::new(AgyUsage::new(env.clone()))]
}

/// Structure-only diagnostic of the Antigravity source (no file contents; agy is not run).
pub fn diagnose(env: &Env, _now: DateTime<Utc>) -> String {
    let mut o = String::new();
    let mut line = |s: String| {
        o.push_str(&s);
        o.push('\n');
    };
    line("Gemini (Antigravity: weekly limits per model group, from `agy -p /usage`)".into());
    match find_agy(env) {
        Some(p) => line(format!("   agy: [exists ] {}", p.display())),
        None => line(format!(
            "   agy: [missing] looked in {} and PATH",
            install_dirs(env)
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
    for (label, d) in [("cli folder", cli_dir(env)), ("app folder", app_dir(env))] {
        line(format!(
            "   {label}: [{}] {}",
            if d.is_dir() { "exists " } else { "missing" },
            d.display()
        ));
    }
    line("   usage: not read by --sources (the running app runs agy on each poll)".into());
    o
}
