// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP server (`aitrack --mcp`): other AI tools ask aiTrack, over the Model Context Protocol, how
//! much of each tracked AI's plan is used and when each one is available again.
//!
//! * Transport: JSON-RPC 2.0 on stdin/stdout, one message per line (the MCP "stdio" transport).
//!   The AI tool starts `aitrack --mcp` itself; there is no port and no listener.
//! * Read-only: every call reads aiTrack's saved history (`~/.aitrack/usage*.jsonl`), the settings
//!   and the Grok Bot app's local cache, exactly what the window reads, and builds the same view.
//!   It makes no network calls, starts no programs and writes nothing. The numbers are as fresh as
//!   the last poll of the aiTrack window (or `--collect`); every answer carries their age.
//! * Nothing secret is returned: no tokens, account ids, emails or paths.
//!
//! Tools: `get_ai_usage` (full detail) and `check_ai_availability` (one short line per AI).

use crate::analysis::{analyze, Gauge};
use crate::collector::Status;
use crate::reading::{Kind, Reading};
use crate::settings::Values;
use crate::sources::Env;
use crate::state::{
    build_antigravity_state, build_codex_state, build_grok_state, build_state, window_ended,
};
use crate::store::{JsonlStore, Store};
use crate::util::{iso, round_to};
use crate::vendor::{Vendor, VendorId};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// A request line longer than this is refused (and skipped) rather than held in memory.
pub const MAX_LINE_BYTES: u64 = 1024 * 1024;

/// A reading older than this is reported as old (the window dims it at the same age).
pub const OLD_AFTER_MIN: f64 = crate::view::STALE_WARN_MIN;

/// Where the data is read from.
#[derive(Debug, Clone)]
pub struct Sources {
    /// Claude's history (`usage.jsonl`); the other vendors' files sit next to it.
    pub data: PathBuf,
    pub settings: PathBuf,
    pub env: Env,
}

impl Sources {
    /// The same locations the window uses.
    pub fn from_process(data: PathBuf) -> Self {
        Sources {
            data,
            settings: crate::settings::default_settings_path(),
            env: Env::from_process(),
        }
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.data.with_file_name(name)
    }
}

fn readings(path: &Path) -> Vec<Reading> {
    JsonlStore::new(path)
        .read()
        .iter()
        .map(Reading::from_value)
        .collect()
}

fn idle() -> Status {
    Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 0,
        sample: None,
        live: false,
    }
}

/// The vendors the window would show right now, built from the saved files only.
pub fn vendors(src: &Sources, now: DateTime<Utc>) -> Vec<Vendor> {
    let settings: Values = crate::settings::Settings::load(&src.settings).get();
    let env = &src.env;
    let mut state = build_state(&readings(&src.data), settings, idle(), now);
    state.claude_on = settings.claude_on(crate::detect::claude_present(env));
    state.tier = state
        .claude_on
        .then(|| crate::sources::claude_plan(env))
        .flatten();

    let mut grok = build_grok_state(
        &readings(&src.sibling("usage-grok.jsonl")),
        idle(),
        settings.grok_on(crate::detect::grok_present(env)),
        now,
    );
    if grok.enabled {
        // the Grok Bot app's cache, read only (the window's tracker would also append to history)
        let scan = crate::grok_bot::persistence_dir(env).map(|d| crate::grok_bot::scan_dir(&d));
        if let Some(scan) = scan {
            grok.bot = scan.reading;
            grok.tier = grok.tier.or(scan.plan);
            grok.shared_pool |= scan.shared;
        }
        let bot_plan = analyze(&readings(&src.sibling("usage-grok-bot.jsonl")), now);
        grok.bot_stats = bot_plan.as_ref().map(|p| p.weekly_stats.clone());
        grok.bot_gauge = bot_plan.and_then(|p| p.weekly);
    }
    state.grok = Some(grok);
    state.codex = Some(build_codex_state(
        &readings(&src.sibling("usage-codex.jsonl")),
        idle(),
        settings.codex_on(crate::detect::codex_present(env)),
        now,
    ));
    state.antigravity = Some(build_antigravity_state(
        &readings(&src.sibling("usage-antigravity.jsonl")),
        idle(),
        settings.antigravity_on(crate::detect::antigravity_present(env)),
        now,
    ));
    crate::vendor::from_state_at(&state, now)
}

/// Stable id used in tool arguments and answers.
pub fn vendor_key(id: VendorId) -> &'static str {
    match id {
        VendorId::Claude => "claude",
        VendorId::Codex => "chatgpt",
        VendorId::Antigravity => "gemini",
        VendorId::Grok => "grok",
    }
}

/// The `ai` argument: a vendor name, or one of its other names. `None` for an unknown name.
pub fn parse_vendor(name: &str) -> Option<VendorId> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some(VendorId::Claude),
        "chatgpt" | "codex" | "openai" | "gpt" => Some(VendorId::Codex),
        "gemini" | "antigravity" | "google" | "agy" => Some(VendorId::Antigravity),
        "grok" | "xai" | "x.ai" | "grok bot" | "grok-bot" => Some(VendorId::Grok),
        _ => None,
    }
}

fn num(x: Option<f64>, places: i32) -> Value {
    x.filter(|v| v.is_finite())
        .map(|v| json!(round_to(v, places)))
        .unwrap_or(Value::Null)
}

fn window_name(k: Kind) -> &'static str {
    match k {
        Kind::Session => "5-hour",
        Kind::Weekly => "weekly",
    }
}

/// One usage window. A window whose period ended since the reading is reported as reset (its old
/// percentage belongs to the previous period), like the window's dashes.
fn window_json(v: &Vendor, k: Kind, g: &Gauge, now: DateTime<Utc>) -> Value {
    let ended = window_ended(g, v.observed_at, now);
    let mut m = Map::new();
    m.insert("window".into(), json!(window_name(k)));
    if ended {
        m.insert("reset_since_reading".into(), json!(true));
        m.insert("used_percent".into(), Value::Null);
        return Value::Object(m);
    }
    let pct = g.pct.clamp(0.0, 100.0);
    m.insert("used_percent".into(), num(Some(pct), 1));
    m.insert("left_percent".into(), num(Some(100.0 - pct), 1));
    m.insert("resets_at".into(), json!(g.resets_at));
    let resets = g.resets_at.as_deref().and_then(crate::util::parse_ts);
    m.insert(
        "hours_until_reset".into(),
        num(
            resets.map(|r| (r - now).num_seconds().max(0) as f64 / 3600.0),
            1,
        ),
    );
    // pace and projection only from a reading that is not old (the window drops them too)
    if v.current && age_min(v, now).is_some_and(|a| a <= OLD_AFTER_MIN) {
        m.insert("pace_percent_per_hour".into(), num(g.rate_per_hour, 2));
        m.insert("projected_percent_at_reset".into(), num(g.projected_pct, 1));
        m.insert("hours_until_limit".into(), num(g.hours_to_limit, 1));
    }
    Value::Object(m)
}

fn age_min(v: &Vendor, now: DateTime<Utc>) -> Option<f64> {
    v.observed_at
        .map(|t| (now - t).num_seconds().max(0) as f64 / 60.0)
}

/// Is the AI usable now, and if not, when again? Every window that is still running and at 100%
/// blocks it; it is available again once the last of them resets.
#[derive(Debug, Clone, PartialEq)]
pub struct Availability {
    /// `None` when there is no number to judge by (tracked, but no reading yet).
    pub available: Option<bool>,
    pub limited_by: Vec<&'static str>,
    pub available_again_at: Option<DateTime<Utc>>,
}

pub fn availability(v: &Vendor, now: DateTime<Utc>) -> Availability {
    let mut out = Availability {
        available: None,
        limited_by: vec![],
        available_again_at: None,
    };
    if v.placeholder {
        return out;
    }
    let mut any = false;
    for &k in v.kinds() {
        let Some(g) = v.gauge(k) else { continue };
        any = true;
        if window_ended(g, v.observed_at, now) || g.pct < 100.0 {
            continue;
        }
        out.limited_by.push(window_name(k));
        let at = g.resets_at.as_deref().and_then(crate::util::parse_ts);
        // Gemini refills gradually: its "full again" time stands in for a missing reset time
        let at = at.or(v.full_at);
        out.available_again_at = match (out.available_again_at, at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    // the shared Grok pool, or a separate Grok Bot meter, is reported by its own fields
    if any {
        out.available = Some(out.limited_by.is_empty());
    }
    out
}

/// Full detail of one AI.
pub fn vendor_json(v: &Vendor, now: DateTime<Utc>) -> Value {
    let mut m = Map::new();
    m.insert("ai".into(), json!(v.name));
    m.insert("id".into(), json!(vendor_key(v.id)));
    m.insert("plan".into(), json!(v.tier));
    let a = availability(v, now);
    m.insert("available".into(), json!(a.available));
    m.insert("limited_by".into(), json!(a.limited_by));
    m.insert(
        "available_again_at".into(),
        json!(a.available_again_at.map(iso)),
    );
    let age = age_min(v, now);
    m.insert("observed_at".into(), json!(v.observed_at.map(iso)));
    m.insert("data_age_minutes".into(), num(age, 0));
    m.insert(
        "data_is_old".into(),
        json!(age.is_none_or(|a| a > OLD_AFTER_MIN)),
    );
    m.insert("source".into(), json!(v.source));
    let windows: Vec<Value> = v
        .kinds()
        .iter()
        .filter_map(|&k| v.gauge(k).map(|g| window_json(v, k, g, now)))
        .collect();
    m.insert("windows".into(), json!(windows));
    if !v.models.is_empty() {
        let models: Vec<Value> = v
            .models
            .iter()
            .map(|l| {
                json!({
                    "name": l.name,
                    "window": l.kind,
                    "used_percent": num(Some(l.pct), 1),
                    "resets_at": l.resets_at,
                })
            })
            .collect();
        m.insert("models".into(), json!(models));
    }
    if let Some(c) = &v.credits {
        m.insert(
            "extra_usage".into(),
            json!({"used": num(Some(c.used), 2), "limit": num(c.limit, 2)}),
        );
    }
    if let Some(n) = &v.credit_note {
        m.insert("credits".into(), json!(n));
    }
    if let Some(n) = v.banked_resets {
        m.insert("banked_resets".into(), json!(n));
    }
    if let Some(t) = v.full_at {
        m.insert("full_again_at".into(), json!(iso(t)));
    }
    if v.id == VendorId::Grok {
        m.insert("shared_pool_with_grok_bot".into(), json!(v.shared_pool));
        if let Some(b) = v.bot.filter(|b| !b.period_ended(now)) {
            let mut bot = json!({
                "used_percent": num(Some(b.pct.clamp(0.0, 100.0)), 1),
                "resets_at": iso(b.resets_at),
                "observed_at": iso(b.read_at),
                "data_age_minutes": num(Some(b.age_min(now)), 0),
            });
            if let Some(g) = v.bot_gauge.as_ref().filter(|_| !b.stale(now)) {
                bot["pace_percent_per_hour"] = num(g.rate_per_hour, 2);
            }
            m.insert("grok_bot".into(), bot);
        }
    }
    if v.placeholder {
        m.insert(
            "note".into(),
            json!(v
                .error
                .clone()
                .unwrap_or_else(|| "tracked, but no reading yet".into())),
        );
    }
    Value::Object(m)
}

/// One short line per AI, for `check_ai_availability`.
pub fn availability_json(v: &Vendor, now: DateTime<Utc>) -> Value {
    let a = availability(v, now);
    let used = |k: Kind| {
        v.gauge(k)
            .filter(|g| !window_ended(g, v.observed_at, now))
            .map(|g| round_to(g.pct.clamp(0.0, 100.0), 1))
    };
    let mut m = Map::new();
    m.insert("ai".into(), json!(v.name));
    m.insert("id".into(), json!(vendor_key(v.id)));
    m.insert("available".into(), json!(a.available));
    m.insert("limited_by".into(), json!(a.limited_by));
    m.insert(
        "available_again_at".into(),
        json!(a.available_again_at.map(iso)),
    );
    for &k in v.kinds() {
        let key = match k {
            Kind::Session => "five_hour_used_percent",
            Kind::Weekly => "weekly_used_percent",
        };
        m.insert(key.into(), json!(used(k)));
    }
    m.insert("data_age_minutes".into(), num(age_min(v, now), 0));
    Value::Object(m)
}

// ---------------------------------------------------------------- the protocol

const SERVER_NAME: &str = "aitrack";

const AI_ARG: &str = "Optional: one AI to ask about: claude, chatgpt (Codex), gemini or grok. \
                      Leave out for every AI tracked on this computer.";

fn tools() -> Value {
    let input = json!({
        "type": "object",
        "properties": {"ai": {"type": "string", "description": AI_ARG}},
        "additionalProperties": false
    });
    json!([
        {
            "name": "check_ai_availability",
            "title": "Check AI availability",
            "description": "Which AI assistants tracked by aiTrack on this computer (Claude, \
                ChatGPT/Codex, Gemini, Grok) can be used right now. Per AI: available (false = a \
                usage limit is at 100%), which limit blocks it, when it is available again, the \
                used percentages and how old the numbers are.",
            "inputSchema": input,
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "get_ai_usage",
            "title": "Get AI plan usage",
            "description": "Full plan usage of the AI assistants tracked by aiTrack on this \
                computer: plan name, each usage window (5-hour, weekly) with percent used and \
                left, reset time, pace per hour and projected percent at reset, per-model limits, \
                credits, and the age and source of the reading. Times are UTC (ISO 8601).",
            "inputSchema": input,
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }
    ])
}

fn instructions() -> String {
    "aiTrack reports how much of each AI assistant's plan is used on this computer (Claude, \
     ChatGPT/Codex, Gemini, Grok) and when each is available again. Numbers come from the aiTrack \
     window's last poll; check data_age_minutes, and treat data_is_old readings as estimates."
        .into()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn tool_result(payload: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
    let mut r = json!({"content": [{"type": "text", "text": text}], "isError": is_error});
    if !is_error {
        r["structuredContent"] = payload;
    }
    r
}

fn call_tool(params: &Value, src: &Sources, now: DateTime<Utc>) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let full = match name {
        "get_ai_usage" => true,
        "check_ai_availability" => false,
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    let wanted = match args.get("ai") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => match parse_vendor(s) {
            Some(id) => Some(id),
            None => {
                return Ok(tool_result(
                    json!({"error": format!(
                        "unknown AI {s:?}; use claude, chatgpt, gemini or grok"
                    )}),
                    true,
                ))
            }
        },
        Some(_) => return Err((-32602, "ai must be a string".into())),
    };
    let all = vendors(src, now);
    let list: Vec<Value> = all
        .iter()
        .filter(|v| wanted.is_none_or(|w| v.id == w))
        .map(|v| {
            if full {
                vendor_json(v, now)
            } else {
                availability_json(v, now)
            }
        })
        .collect();
    let mut payload = json!({"now": iso(now), "ais": list});
    if list_is_empty(&payload) {
        payload["note"] = json!(match wanted {
            Some(w) => format!(
                "{} is not tracked on this computer, or aiTrack has no reading of it yet",
                w.name()
            ),
            None => "aiTrack tracks no AI on this computer yet, or has no readings".into(),
        });
    }
    Ok(tool_result(payload, false))
}

fn list_is_empty(p: &Value) -> bool {
    p["ais"].as_array().is_none_or(Vec::is_empty)
}

/// Answer one message. `None` for a notification (no reply is sent).
pub fn handle(msg: &Value, src: &Sources, now: DateTime<Utc>) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    };
    let method = obj.get("method").and_then(Value::as_str);
    let Some(id) = obj.get("id").cloned() else {
        // notifications (initialized, cancelled, ...) and stray replies get no answer
        return None;
    };
    let Some(method) = method else {
        // a reply to a request we never sent
        return None;
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|v| PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "title": "aiTrack",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": instructions()
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": tools()})),
        "tools/call" => match call_tool(&params, src, now) {
            Ok(r) => rpc_result(id, r),
            Err((code, text)) => rpc_error(id, code, &text),
        },
        "resources/list" => rpc_result(id, json!({"resources": []})),
        "prompts/list" => rpc_result(id, json!({"prompts": []})),
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    })
}

/// Read one line of at most [`MAX_LINE_BYTES`] into `buf`. `Ok(None)` at the end of input,
/// `Ok(Some(true))` for a line, `Ok(Some(false))` for an over-long line (the rest of it is skipped).
fn read_line<R: BufRead>(input: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    buf.clear();
    let n = Read::take(&mut *input, MAX_LINE_BYTES + 1).read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() as u64 > MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
        // skip the rest of the long line
        let mut sink = Vec::new();
        loop {
            sink.clear();
            let m = Read::take(&mut *input, MAX_LINE_BYTES).read_until(b'\n', &mut sink)?;
            if m == 0 || sink.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(Some(false));
    }
    Ok(Some(true))
}

/// Serve until the input ends. Every reply is one line of JSON on `output`; nothing else is
/// written there (stdout belongs to the protocol).
pub fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    src: &Sources,
    now: impl Fn() -> DateTime<Utc>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    while let Some(fits) = read_line(&mut input, &mut buf)? {
        let reply = if !fits {
            Some(rpc_error(Value::Null, -32600, "message too long"))
        } else {
            let text = String::from_utf8_lossy(&buf);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(text) {
                Ok(msg) => handle(&msg, src, now()),
                Err(_) => Some(rpc_error(Value::Null, -32700, "parse error")),
            }
        };
        if let Some(r) = reply {
            serde_json::to_writer(&mut output, &r)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
