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
        dirs.extend(std::env::split_paths(&path));
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
        if t.contains("week") || t.ends_with("7d") {
            return Some(168.0);
        }
        if t.contains("day") || t.ends_with("24h") || t.ends_with("1d") {
            return Some(24.0);
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

fn bucket_of(l: &Limit) -> Bucket {
    Bucket {
        carried: false,
        pct: l.pct,
        resets_at: l.resets_at.map(iso),
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
    if let Some(s) = body.get("status").and_then(Value::as_str) {
        if !s.eq_ignore_ascii_case("success") {
            return Err(failure_text(body));
        }
    }
    let data = body
        .get("command")
        .and_then(|c| c.get("data"))
        .filter(|d| d.is_object())
        .ok_or("unexpected Antigravity usage reply (no command data)")?;
    let limits = limits_of(data);
    let (mut session, mut weekly) = (None, None);
    let mut extra = Vec::new();
    for l in &limits {
        if is_main(&l.group) {
            let slot = if l.hours >= WEEKLY_FROM_H {
                &mut weekly
            } else {
                &mut session
            };
            if slot.is_none() {
                *slot = Some(bucket_of(l));
                continue;
            }
        }
        extra.push(l);
    }
    if session.is_none() && weekly.is_none() {
        return Err("Antigravity usage reply has no Gemini limit".into());
    }
    let mut raw = serde_json::Map::new();
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
                json!({
                    "name": l.group,
                    "kind": if l.hours >= WEEKLY_FROM_H { "weekly" } else { "session" },
                    "percent": l.pct,
                    "resets_at": l.resets_at.map(iso),
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

/// The command for a usage run: no window, no input, output captured, started in the home folder
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

/// Run agy once and return what it printed (at most 1 MiB, at most [`RUN_TIMEOUT`]).
pub fn run_usage(
    program: &Path,
    home: Option<&Path>,
    timeout: StdDuration,
) -> Result<Vec<u8>, String> {
    let mut child = command(program, home)
        .spawn()
        .map_err(|e| format!("could not start agy: {e}"))?;
    let mut out = child.stdout.take().ok_or("could not read agy's output")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = (&mut out)
            .take(MAX_OUTPUT_BYTES + 1)
            .read_to_end(&mut buf)
            .map(|_| buf);
        // drain the rest so agy never blocks on a full pipe
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        r
    });
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) => {}
            Err(e) => {
                crate::grok_cli::kill_pid(child.id());
                let _ = child.kill();
                return Err(format!("could not watch agy: {e}"));
            }
        }
        if start.elapsed() >= timeout {
            crate::grok_cli::kill_pid(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("agy did not answer within {} s", timeout.as_secs()));
        }
        std::thread::sleep(StdDuration::from_millis(100));
    };
    let buf = reader
        .join()
        .map_err(|_| "could not read agy's output".to_string())?
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
}

impl AgyUsage {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            timeout: RUN_TIMEOUT,
        }
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
        let program = find_agy(&self.env).ok_or_else(|| {
            CollectError::new("Antigravity CLI (agy) not found: install it from antigravity.google")
        })?;
        let out = run_usage(&program, self.env.home.as_deref(), self.timeout)
            .map_err(|e| CollectError::new(format!("Antigravity usage: {e}")))?;
        let body = reply_json(&out)
            .ok_or_else(|| CollectError::new("unexpected (non-JSON) Antigravity usage reply"))?;
        let sample = parse_usage(&body, now).map_err(CollectError::new)?;
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
