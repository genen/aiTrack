// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-project token and cost usage from local transcripts (no UI).
//!
//! # Sources
//!
//! * **Claude Code**: `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`, every `*.jsonl`
//!   below it (including `<session>/subagents/*.jsonl`). Lines of `type: assistant` with
//!   `message.usage` are requests: `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
//!   `cache_creation_input_tokens` (split into 5 minute / 1 hour writes by
//!   `cache_creation.ephemeral_*_input_tokens` when present), `message.id`, `requestId`,
//!   `message.model`, `timestamp`, `cwd`, `sessionId`, `isSidechain`. Streaming writes repeat a
//!   message, so requests are de-duplicated by (`message.id`, `requestId`), keeping the **last**
//!   chunk (it carries the final token counts); this also removes copies in resumed sessions.
//!   Synthetic models (`<synthetic>`) are skipped. The project is the last component of `cwd`
//!   (either slash style), else derived from the folder name. A sidechain
//!   flag (`isSidechain` or a file under `subagents/`) is kept for a later per-agent view.
//! * **Grok CLI** (format taken from the reference loader, reimplemented): `~/.grok` (or
//!   `$GROK_HOME`). `sessions/<url-encoded cwd>/<session>/updates.jsonl` holds, per turn, a
//!   `session/update` (or `_x.ai/session/update`) message whose `params.update.usage` has
//!   `costUsdTicks` and per-model `modelUsage` (`inputTokens` including `cachedReadTokens`,
//!   `outputTokens`, `cacheCreationTokens`, `costUsdTicks`). One tick is **1e-10 US dollars**
//!   ([`COST_USD_PER_TICK`]), the reference's constant. These are the exact figures and win.
//!   For a session with no such updates, `logs/unified.jsonl` (its last [`GROK_LOG_TAIL_BYTES`])
//!   is used instead: `shell.turn.inference_done` events (`prompt_tokens`, `cached_prompt_tokens`,
//!   `completion_tokens`), the model from the last `model changed` event of the session (else the
//!   `[models] default` of `config.toml`), the folder from `session created`. That path has tokens
//!   only: **cost unknown** (never guessed from Claude prices).
//! * **Codex** (OpenAI): `~/.codex` (or `$CODEX_HOME`), `sessions/YYYY/MM/DD/rollout-*.jsonl` and
//!   `archived_sessions/` (plain or `.jsonl.zst`). The first line (`session_meta`) has the session
//!   `id` and `cwd`; `turn_context` lines name the `model` (and `cwd`); every model turn writes an
//!   `event_msg` of `payload.type: "token_count"` whose `info.total_token_usage` is the session's
//!   running total (`input_tokens` including `cached_input_tokens`, `output_tokens` including
//!   reasoning). A request is the growth of that total since the previous line, so repeated lines
//!   add nothing; it is priced at the API list price of its model, like Claude.
//!
//! Anything unreadable or oddly shaped is skipped, never fatal: lines are read with a size cap,
//! huge files and runaway directory trees are bounded, nothing panics. Message content is never
//! kept, only counts, ids, model names and folder names.
//!
//! # Cheap rescans
//!
//! [`Scanner`] remembers per file `(mtime, size)` and the requests found in it; a rescan only
//! stats the files and re-reads the ones that changed. The summaries can be persisted
//! (`<data dir>/transcript-index.json`, a compact string-table format) so the first scan after a
//! restart is also quick. A file whose content changed without changing size and mtime is not
//! noticed (the usual trade-off).
//!
//! # Aggregation
//!
//! [`aggregate`] de-duplicates, prices (Claude and Codex through [`PriceTable`], Grok from its
//! ticks) and
//! groups by (vendor, project) for Today (local calendar day), 7 days, 30 days and all time.

use crate::pricing::{PriceTable, TokenUsage};
use crate::sources::Env;
use crate::util::parse_ts;
use crate::vendor::VendorId;
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Longest line read; longer ones are skipped.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Files larger than this are skipped.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// At most this many files are indexed per source.
pub const MAX_FILES: usize = 50_000;
/// Directory depth below the projects folder.
pub const MAX_DEPTH: usize = 8;
/// How much of the end of `unified.jsonl` is read.
pub const GROK_LOG_TAIL_BYTES: u64 = 16 * 1024 * 1024;
/// US dollars per Grok cost tick.
pub const COST_USD_PER_TICK: f64 = 1e-10;
/// Project name when a request carries no folder.
pub const NO_PROJECT: &str = "(no folder)";
const INDEX_VERSION: u64 = 1;

// ---- requests -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// A Claude Code transcript.
    Claude,
    /// A Grok `updates.jsonl` turn (exact cost).
    GrokUpdate,
    /// A Grok `unified.jsonl` inference event (tokens only).
    GrokLog,
    /// A Codex session-log turn (priced like Claude).
    Codex,
}

impl Origin {
    pub fn vendor(self) -> VendorId {
        match self {
            Origin::Claude => VendorId::Claude,
            Origin::Codex => VendorId::Codex,
            _ => VendorId::Grok,
        }
    }

    fn code(self) -> u64 {
        match self {
            Origin::Claude => 0,
            Origin::GrokUpdate => 1,
            Origin::GrokLog => 2,
            Origin::Codex => 3,
        }
    }

    fn from_code(c: u64) -> Option<Self> {
        Some(match c {
            0 => Origin::Claude,
            1 => Origin::GrokUpdate,
            2 => Origin::GrokLog,
            3 => Origin::Codex,
            _ => return None,
        })
    }
}

/// One model request: what the aggregation works on. No message content.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Unix seconds.
    pub at: i64,
    pub origin: Origin,
    pub project: String,
    pub session: String,
    pub model: String,
    pub usage: TokenUsage,
    /// Grok's recorded cost in ticks, when it has one.
    pub cost_ticks: Option<u64>,
    /// Made by a subagent / sidechain.
    pub sidechain: bool,
    /// De-duplication key (hash of message id and request id); 0 = never a duplicate.
    pub key: u64,
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h.max(1)
}

// ---- reading helpers ------------------------------------------------------------------------

enum Line {
    Eof,
    Ok,
    TooLong,
}

/// Read one line into `buf` (without the newline), keeping at most `max` bytes: a longer line is
/// consumed entirely and reported as [`Line::TooLong`].
fn next_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> std::io::Result<Line> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let (done, used) = {
            let avail = match r.fill_buf() {
                Ok(a) => a,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                return Ok(if any {
                    if too_long {
                        Line::TooLong
                    } else {
                        Line::Ok
                    }
                } else {
                    Line::Eof
                });
            }
            any = true;
            match avail.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if !too_long && buf.len() + i <= max {
                        buf.extend_from_slice(&avail[..i]);
                    } else {
                        too_long = true;
                    }
                    (true, i + 1)
                }
                None => {
                    if !too_long && buf.len() + avail.len() <= max {
                        buf.extend_from_slice(avail);
                    } else {
                        too_long = true;
                    }
                    (false, avail.len())
                }
            }
        };
        r.consume(used);
        if done {
            return Ok(if too_long { Line::TooLong } else { Line::Ok });
        }
    }
}

fn text_of(line: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(line)
}

/// Call `f` for every complete line of `path` (starting `from` bytes in, skipping the partial line
/// that offset may land in), stopping at [`MAX_FILE_BYTES`]. Unreadable files give no lines.
fn for_each_line(path: &Path, from: u64, mut f: impl FnMut(&[u8])) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    if from > 0 && file.seek(SeekFrom::Start(from)).is_err() {
        return;
    }
    let mut r = BufReader::with_capacity(256 * 1024, file.take(MAX_FILE_BYTES));
    let mut buf = Vec::new();
    let mut first = from > 0;
    loop {
        match next_line(&mut r, &mut buf, MAX_LINE_BYTES) {
            Ok(Line::Eof) | Err(_) => break,
            Ok(Line::TooLong) => {
                first = false;
            }
            Ok(Line::Ok) => {
                if first {
                    first = false; // the line the offset landed in is partial
                    continue;
                }
                f(&buf)
            }
        }
    }
}

fn as_u64(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite() && *f > 0.0)
                    .map(|f| f as u64)
            })
            .unwrap_or(0),
        _ => 0,
    }
}

/// A non-negative whole number from a JSON number or numeric string (costs, tick counts).
fn as_nonneg(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Last path component, either slash style (`C:\Users\me\proj` -> `proj`).
pub fn project_name(cwd: &str) -> String {
    let t = cwd.trim().trim_end_matches(['/', '\\']);
    let name = t.rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() {
        if t.is_empty() {
            NO_PROJECT.to_owned()
        } else {
            t.to_owned()
        }
    } else {
        name.to_owned()
    }
}

/// The project name derived from the folder name: `-home-user-aiTrack` -> `aiTrack`.
fn folder_fallback(dir: &str) -> String {
    let d = dir.trim_start_matches('-');
    d.rsplit('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(d)
        .to_owned()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- Claude Code transcripts ----------------------------------------------------------------

fn parse_claude_file(path: &Path, fallback_project: &str, subagent: bool) -> Vec<Request> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out: Vec<Request> = Vec::new();
    let mut by_key: HashMap<u64, usize> = HashMap::new();
    for_each_line(path, 0, |line| {
        let text = text_of(line);
        // cheap prefilter: only assistant lines with usage matter, and most lines are neither
        if !text.contains("\"usage\"") || !text.contains("\"assistant\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(msg) = v.get("message") else { return };
        let Some(usage) = msg.get("usage").filter(|u| u.is_object()) else {
            return;
        };
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
        if model.is_empty() || model.starts_with('<') {
            return;
        }
        let Some(at) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts)
        else {
            return;
        };
        let key = msg
            .get("id")
            .and_then(Value::as_str)
            .map(|id| {
                fnv(&format!(
                    "{id}:{}",
                    v.get("requestId").and_then(Value::as_str).unwrap_or("")
                ))
            })
            .unwrap_or(0);
        let project = v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(project_name)
            .unwrap_or_else(|| fallback_project.to_owned());
        let r = Request {
            at: at.timestamp(),
            origin: Origin::Claude,
            project,
            session: v
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or(&stem)
                .to_owned(),
            model: model.to_owned(),
            usage: TokenUsage::from_usage(usage),
            cost_ticks: None,
            sidechain: v.get("isSidechain").and_then(Value::as_bool) == Some(true) || subagent,
            key,
        };
        match by_key.get(&key).filter(|_| key != 0) {
            // a later chunk of the same message replaces the earlier one
            Some(i) => out[*i] = r,
            None => {
                if key != 0 {
                    by_key.insert(key, out.len());
                }
                out.push(r);
            }
        }
    });
    out
}

// ---- Grok -----------------------------------------------------------------------------------

fn parse_update_time(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => parse_ts(s).map(|t| t.timestamp()),
        Value::Number(n) => {
            let f = n.as_f64().filter(|f| f.is_finite() && *f > 0.0)?;
            // seconds, or milliseconds
            Some(if f > 1e11 {
                (f / 1000.0) as i64
            } else {
                f as i64
            })
        }
        _ => None,
    }
}

fn normalize_grok_model(m: &str) -> String {
    m.strip_suffix("-build").unwrap_or(m).to_owned()
}

fn parse_grok_updates(path: &Path, cwd: &str, sid: &str) -> Vec<Request> {
    let project = if cwd.is_empty() {
        NO_PROJECT.to_owned()
    } else {
        project_name(cwd)
    };
    let mut out = Vec::new();
    for_each_line(path, 0, |line| {
        let text = text_of(line);
        if !text.contains("costUsdTicks") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if !matches!(
            v.get("method").and_then(Value::as_str),
            Some("session/update" | "_x.ai/session/update")
        ) {
            return;
        }
        let Some(at) = parse_update_time(v.get("timestamp")) else {
            return;
        };
        let Some(usage) = v.pointer("/params/update/usage").filter(|u| u.is_object()) else {
            return;
        };
        let Some(ticks) = usage.get("costUsdTicks").and_then(as_nonneg) else {
            return;
        };
        let make = |model: String, tokens: TokenUsage, cost: Option<u64>| Request {
            at,
            origin: Origin::GrokUpdate,
            project: project.clone(),
            session: sid.to_owned(),
            model,
            usage: tokens,
            cost_ticks: cost,
            sidechain: false,
            key: 0,
        };
        let models = usage
            .get("modelUsage")
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty());
        let Some(models) = models else {
            // no per-model detail: keep the cost, tokens unknown
            out.push(make("unknown".into(), TokenUsage::default(), Some(ticks)));
            return;
        };
        let single = models.len() == 1;
        let first_row = out.len();
        for (name, mu) in models {
            if name.is_empty() || !mu.is_object() {
                continue;
            }
            let cached = mu.get("cachedReadTokens").map(as_u64).unwrap_or(0);
            let input = mu.get("inputTokens").map(as_u64).unwrap_or(0);
            let tokens = TokenUsage {
                input: input.saturating_sub(cached),
                output: mu.get("outputTokens").map(as_u64).unwrap_or(0),
                cache_read: cached,
                cache_write_5m: mu.get("cacheCreationTokens").map(as_u64).unwrap_or(0),
                cache_write_1h: 0,
            };
            let cost = mu.get("costUsdTicks").and_then(as_nonneg).or(if single {
                Some(ticks)
            } else {
                None
            });
            out.push(make(normalize_grok_model(name), tokens, cost));
        }
        // several models and only the turn's total recorded: keep that exact cost on one row
        // (the models are sorted by name by the JSON map) instead of calling the turn unpriced
        let rows = &mut out[first_row..];
        if rows.is_empty() {
            out.push(make("unknown".into(), TokenUsage::default(), Some(ticks)));
        } else if rows.iter().all(|r| r.cost_ticks.is_none()) {
            // the first row carries the total, the others are part of it (0 extra, not unpriced)
            for (i, r) in rows.iter_mut().enumerate() {
                r.cost_ticks = Some(if i == 0 { ticks } else { 0 });
            }
        }
    });
    out
}

/// `[models] default = "..."` of Grok's config.toml (a tiny reader for just that key).
pub fn grok_default_model(config: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(config)
        .ok()?
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    let mut in_models = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_models = l == "[models]";
        } else if in_models {
            if let Some((k, v)) = l.split_once('=') {
                if k.trim() == "default" {
                    let v = v.trim().trim_matches(['"', '\'']);
                    if !v.is_empty() {
                        return Some(v.to_owned());
                    }
                }
            }
        }
    }
    None
}

fn parse_grok_unified(path: &Path, config: &Path) -> Vec<Request> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let from = meta.len().saturating_sub(GROK_LOG_TAIL_BYTES);
    enum Ev {
        Model(String),
        Created(String),
        Infer { loop_index: i64, usage: TokenUsage },
    }
    let mut events: Vec<(i64, usize, String, Ev)> = Vec::new();
    for_each_line(path, from, |line| {
        let text = text_of(line);
        let wanted = [
            "shell.turn.inference_done",
            "model changed",
            "session created",
        ];
        if !wanted.iter().any(|w| text.contains(w)) {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let (Some(msg), Some(sid), Some(at), Some(ctx)) = (
            v.get("msg").and_then(Value::as_str),
            v.get("sid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
            v.get("ts").and_then(Value::as_str).and_then(parse_ts),
            v.get("ctx").filter(|c| c.is_object()),
        ) else {
            return;
        };
        let ev = match msg {
            "model changed" => match ctx.get("model").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => Ev::Model(m.to_owned()),
                _ => return,
            },
            "session created" => match ctx.get("cwd").and_then(Value::as_str) {
                Some(c) => Ev::Created(c.to_owned()),
                None => return,
            },
            "shell.turn.inference_done" => {
                if !["prompt_tokens", "cached_prompt_tokens", "completion_tokens"]
                    .iter()
                    .all(|k| ctx.get(*k).is_some())
                {
                    return;
                }
                let Some(loop_index) = ctx.get("loop_index").and_then(Value::as_i64) else {
                    return;
                };
                let prompt = ctx.get("prompt_tokens").map(as_u64).unwrap_or(0);
                let cached = ctx.get("cached_prompt_tokens").map(as_u64).unwrap_or(0);
                Ev::Infer {
                    loop_index,
                    usage: TokenUsage {
                        input: prompt.saturating_sub(cached),
                        output: ctx.get("completion_tokens").map(as_u64).unwrap_or(0),
                        cache_read: cached,
                        cache_write_5m: 0,
                        cache_write_1h: 0,
                    },
                }
            }
            _ => return,
        };
        events.push((at.timestamp(), events.len(), sid.to_owned(), ev));
    });
    events.sort_by_key(|e| (e.0, e.1));
    let default_model = grok_default_model(config).unwrap_or_else(|| "unknown".into());
    let mut model: HashMap<String, (i64, String)> = HashMap::new();
    let mut cwd: HashMap<String, (i64, String)> = HashMap::new();
    let mut out = Vec::new();
    for (at, _, sid, ev) in events {
        match ev {
            Ev::Model(m) => {
                model.insert(sid, (at, m));
            }
            Ev::Created(c) => {
                cwd.insert(sid, (at, c));
            }
            Ev::Infer { loop_index, usage } => {
                let m = model
                    .get(&sid)
                    .filter(|(t, _)| *t < at)
                    .map(|(_, m)| m.as_str())
                    .unwrap_or(&default_model);
                let project = cwd
                    .get(&sid)
                    .filter(|(t, _)| *t < at)
                    .map(|(_, c)| project_name(c))
                    .unwrap_or_else(|| NO_PROJECT.to_owned());
                let _ = loop_index; // loop indexes repeat per turn: never a de-duplication key
                out.push(Request {
                    at,
                    origin: Origin::GrokLog,
                    project,
                    session: sid,
                    model: normalize_grok_model(m),
                    usage,
                    cost_ticks: None,
                    sidechain: false,
                    key: 0,
                });
            }
        }
    }
    out
}

// ---- Codex ----------------------------------------------------------------------------------

/// The token counts of a Codex `*_token_usage` object.
fn codex_counts(v: &Value) -> [u64; 3] {
    let n = |k: &str| v.get(k).map(as_u64).unwrap_or(0);
    [
        n("input_tokens"),
        n("cached_input_tokens"),
        n("output_tokens"),
    ]
}

/// Feeds every line of one session file to the callback it is given.
type LineFeed<'a> = dyn FnMut(&mut dyn FnMut(&[u8])) + 'a;

fn parse_codex_lines(lines: &mut LineFeed<'_>, stem: &str) -> Vec<Request> {
    let mut out = Vec::new();
    let (mut sid, mut cwd, mut model) = (stem.to_owned(), String::new(), String::from("unknown"));
    let mut prev = [0u64; 3];
    lines(&mut |line| {
        let text = text_of(line);
        let meta = text.contains("\"session_meta\"") || text.contains("\"turn_context\"");
        if !meta && !text.contains("\"token_count\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let p = v.get("payload").unwrap_or(&Value::Null);
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(id) = p
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    sid = id.to_owned();
                }
                if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                    cwd = c.to_owned();
                }
                return;
            }
            Some("turn_context") => {
                if let Some(m) = p
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    model = m.to_owned();
                }
                if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                    cwd = c.to_owned();
                }
                return;
            }
            _ => {}
        }
        if p.get("type").and_then(Value::as_str) != Some("token_count") {
            return;
        }
        let Some(total) = p
            .get("info")
            .and_then(|i| i.get("total_token_usage"))
            .filter(|t| t.is_object())
        else {
            return;
        };
        let Some(at) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts)
        else {
            return;
        };
        let cur = codex_counts(total);
        // a repeated line adds nothing
        if cur[0] + cur[2] == prev[0] + prev[2] {
            return;
        }
        let d = if cur[0] + cur[2] < prev[0] + prev[2] {
            // the running total started again (a resumed session): count this turn only
            let last = p
                .get("info")
                .and_then(|i| i.get("last_token_usage"))
                .map(codex_counts)
                .unwrap_or_default();
            if last[0] + last[2] > 0 {
                last
            } else {
                cur
            }
        } else {
            [
                cur[0].saturating_sub(prev[0]),
                cur[1].saturating_sub(prev[1]),
                cur[2].saturating_sub(prev[2]),
            ]
        };
        prev = cur;
        let cached = d[1].min(d[0]);
        out.push(Request {
            at: at.timestamp(),
            origin: Origin::Codex,
            project: project_name(&cwd),
            session: sid.clone(),
            model: model.clone(),
            usage: TokenUsage {
                input: d[0] - cached,
                output: d[2],
                cache_read: cached,
                cache_write_5m: 0,
                cache_write_1h: 0,
            },
            cost_ticks: None,
            sidechain: false,
            // the same running total of the same session (a resumed copy) is one request
            key: fnv(&format!("codex:{sid}:{}:{}", cur[0], cur[2])),
        });
    });
    out
}

/// The requests of one Codex session file (plain `.jsonl` or `.jsonl.zst`).
fn parse_codex_file(path: &Path) -> Vec<Request> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name
        .trim_end_matches(".zst")
        .trim_end_matches(".jsonl")
        .to_owned();
    if name.ends_with(".zst") {
        let Ok(f) = File::open(path) else {
            return Vec::new();
        };
        let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
            BufReader::new(f),
            8 * 1024 * 1024,
        ) else {
            return Vec::new();
        };
        let mut buf = Vec::new();
        if (&mut d).take(MAX_FILE_BYTES).read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        return parse_codex_lines(
            &mut |f| {
                for l in buf.split(|b| *b == b'\n') {
                    if l.len() <= MAX_LINE_BYTES {
                        f(l);
                    }
                }
            },
            &stem,
        );
    }
    parse_codex_lines(&mut |f| for_each_line(path, 0, f), &stem)
}

fn discover_codex(home: &Path, found: &mut Vec<Found>) -> SourceInfo {
    let sessions = home.join("sessions");
    let mut info = SourceInfo {
        label: "Codex sessions".into(),
        path: sessions.clone(),
        exists: sessions.is_dir(),
        files: 0,
        note: String::new(),
    };
    let mut stack: Vec<(PathBuf, usize)> = vec![(sessions, 0), (home.join("archived_sessions"), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                if depth < 4 {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file()
                && name.starts_with("rollout-")
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
                && found.len() < MAX_FILES
            {
                let Ok(meta) = e.metadata() else { continue };
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
                info.files += 1;
                found.push(Found {
                    path,
                    kind: FileKind::Codex,
                    sig: (mtime_nanos(&meta), meta.len()),
                });
            }
        }
    }
    info
}

// ---- discovery ------------------------------------------------------------------------------

/// Where the transcripts are.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `.../projects` of Claude Code.
    pub claude_projects: PathBuf,
    /// The Grok CLI's folder.
    pub grok_home: PathBuf,
    /// Codex's folder (`~/.codex`).
    pub codex_home: PathBuf,
    /// Where the per-file index is persisted (`None` = memory only).
    pub index: Option<PathBuf>,
}

impl Roots {
    pub fn from_env(env: &Env, index: Option<PathBuf>) -> Self {
        let home = env.home.clone().unwrap_or_default();
        Self {
            claude_projects: env
                .claude_config_dir
                .clone()
                .map(|d| d.join("projects"))
                .unwrap_or_else(|| home.join(".claude").join("projects")),
            grok_home: crate::grok::grok_home(env),
            codex_home: crate::codex::codex_home(env),
            index,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FileKind {
    Claude { fallback: String, subagent: bool },
    GrokUpdates { cwd: String, sid: String },
    GrokUnified { config: PathBuf },
    Codex,
}

#[derive(Debug, Clone)]
struct Found {
    path: PathBuf,
    kind: FileKind,
    sig: (u64, u64),
}

fn mtime_nanos(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

/// One folder checked for transcripts, for `--projects` and the empty state.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub label: String,
    pub path: PathBuf,
    pub exists: bool,
    pub files: usize,
    /// e.g. "3 of them subagent transcripts".
    pub note: String,
}

fn discover_claude(root: &Path, found: &mut Vec<Found>) -> SourceInfo {
    let mut info = SourceInfo {
        label: "Claude Code transcripts".into(),
        path: root.to_path_buf(),
        exists: root.is_dir(),
        files: 0,
        note: String::new(),
    };
    let mut sub = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file()
                && path
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("jsonl"))
                && found.len() < MAX_FILES
            {
                let Ok(meta) = e.metadata() else { continue };
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
                let rel = path.strip_prefix(root).unwrap_or(&path);
                let comps: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                let subagent = comps.iter().any(|c| c == "subagents");
                let fallback = comps.first().map_or(String::new(), |c| folder_fallback(c));
                sub += usize::from(subagent);
                info.files += 1;
                found.push(Found {
                    path,
                    kind: FileKind::Claude { fallback, subagent },
                    sig: (mtime_nanos(&meta), meta.len()),
                });
            }
        }
    }
    if sub > 0 {
        info.note = format!("{sub} of them subagent transcripts");
    }
    info
}

fn discover_grok(home: &Path, found: &mut Vec<Found>) -> Vec<SourceInfo> {
    let sessions = home.join("sessions");
    let mut s_info = SourceInfo {
        label: "Grok sessions".into(),
        path: sessions.clone(),
        exists: sessions.is_dir(),
        files: 0,
        note: String::new(),
    };
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        for d1 in rd.flatten() {
            let Ok(rd2) = std::fs::read_dir(d1.path()) else {
                continue;
            };
            for d2 in rd2.flatten() {
                let p = d2.path().join("updates.jsonl");
                let Ok(meta) = std::fs::metadata(&p) else {
                    continue;
                };
                if !meta.is_file() || meta.len() > MAX_FILE_BYTES || found.len() >= MAX_FILES {
                    continue;
                }
                s_info.files += 1;
                found.push(Found {
                    path: p,
                    kind: FileKind::GrokUpdates {
                        cwd: percent_decode(&d1.file_name().to_string_lossy()),
                        sid: d2.file_name().to_string_lossy().into_owned(),
                    },
                    sig: (mtime_nanos(&meta), meta.len()),
                });
            }
        }
    }
    let log = home.join("logs").join("unified.jsonl");
    let config = home.join("config.toml");
    let mut l_info = SourceInfo {
        label: "Grok CLI log".into(),
        path: log.clone(),
        exists: log.is_file(),
        files: 0,
        note: "tokens only, no cost".into(),
    };
    if let Ok(meta) = std::fs::metadata(&log) {
        if meta.is_file() {
            l_info.files = 1;
            // the config's age counts too: it supplies the default model
            let cfg = std::fs::metadata(&config)
                .map(|m| mtime_nanos(&m))
                .unwrap_or(0);
            found.push(Found {
                path: log,
                kind: FileKind::GrokUnified { config },
                sig: (mtime_nanos(&meta).max(cfg), meta.len()),
            });
        }
    }
    vec![s_info, l_info]
}

// ---- scanner --------------------------------------------------------------------------------

struct Entry {
    sig: (u64, u64),
    requests: Vec<Request>,
}

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanSummary {
    pub files: usize,
    pub parsed: usize,
    pub reused: usize,
    pub removed: usize,
    pub requests: usize,
    pub elapsed: Duration,
}

pub struct Scanner {
    roots: Roots,
    files: HashMap<PathBuf, Entry>,
    sources: Vec<SourceInfo>,
    /// Files parsed since the index was last written.
    unsaved: bool,
}

impl Scanner {
    /// A scanner starting from the persisted index when there is a usable one.
    pub fn new(roots: Roots) -> Self {
        let files = roots
            .index
            .as_deref()
            .and_then(load_index)
            .unwrap_or_default();
        Self {
            roots,
            files,
            sources: Vec::new(),
            unsaved: false,
        }
    }

    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    /// Stat everything, re-read what changed, drop what vanished, persist the index if it changed.
    pub fn scan(&mut self) -> ScanSummary {
        let t0 = Instant::now();
        let mut found = Vec::new();
        let mut sources = vec![discover_claude(&self.roots.claude_projects, &mut found)];
        sources.extend(discover_grok(&self.roots.grok_home, &mut found));
        sources.push(discover_codex(&self.roots.codex_home, &mut found));
        self.sources = sources;

        let mut sum = ScanSummary {
            files: found.len(),
            ..Default::default()
        };
        let live: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
        let before = self.files.len();
        self.files.retain(|p, _| live.contains(p));
        sum.removed = before - self.files.len();
        for f in &found {
            if self.files.get(&f.path).is_some_and(|e| e.sig == f.sig) {
                sum.reused += 1;
                continue;
            }
            let requests = match &f.kind {
                FileKind::Claude { fallback, subagent } => {
                    parse_claude_file(&f.path, fallback, *subagent)
                }
                FileKind::GrokUpdates { cwd, sid } => parse_grok_updates(&f.path, cwd, sid),
                FileKind::GrokUnified { config } => parse_grok_unified(&f.path, config),
                FileKind::Codex => parse_codex_file(&f.path),
            };
            self.files.insert(
                f.path.clone(),
                Entry {
                    sig: f.sig,
                    requests,
                },
            );
            sum.parsed += 1;
        }
        sum.requests = self.files.values().map(|e| e.requests.len()).sum();
        if sum.parsed > 0 || sum.removed > 0 || self.unsaved {
            self.unsaved = true;
            if let Some(p) = self.roots.index.clone() {
                if save_index(&p, &self.files).is_ok() {
                    self.unsaved = false;
                }
            }
        }
        sum.elapsed = t0.elapsed();
        sum
    }

    /// Every request found, files in path order (so the cross-file de-duplication never depends
    /// on hash-map order).
    pub fn requests(&self) -> impl Iterator<Item = &Request> {
        let mut paths: Vec<&PathBuf> = self.files.keys().collect();
        paths.sort();
        paths
            .into_iter()
            .flat_map(|p| self.files[p].requests.iter())
    }

    /// The folders looked at by the last [`scan`](Self::scan).
    pub fn sources(&self) -> &[SourceInfo] {
        &self.sources
    }
}

// ---- persisted index ------------------------------------------------------------------------

struct Strings {
    list: Vec<String>,
    index: HashMap<String, usize>,
}

impl Strings {
    fn id(&mut self, s: &str) -> usize {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        self.list.push(s.to_owned());
        self.index.insert(s.to_owned(), self.list.len() - 1);
        self.list.len() - 1
    }
}

fn save_index(path: &Path, files: &HashMap<PathBuf, Entry>) -> std::io::Result<()> {
    let mut st = Strings {
        list: Vec::new(),
        index: HashMap::new(),
    };
    let mut arr = Vec::with_capacity(files.len());
    for (p, e) in files {
        let reqs: Vec<Value> = e
            .requests
            .iter()
            .map(|r| {
                json!([
                    r.at,
                    r.origin.code(),
                    st.id(&r.project),
                    st.id(&r.session),
                    st.id(&r.model),
                    r.usage.input,
                    r.usage.output,
                    r.usage.cache_read,
                    r.usage.cache_write_5m,
                    r.usage.cache_write_1h,
                    r.cost_ticks,
                    u8::from(r.sidechain),
                    r.key
                ])
            })
            .collect();
        arr.push(json!({"p": p.to_string_lossy(), "m": e.sig.0, "s": e.sig.1, "r": reqs}));
    }
    let doc = json!({"version": INDEX_VERSION, "strings": st.list, "files": arr});
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&doc).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

fn load_index(path: &Path) -> Option<HashMap<PathBuf, Entry>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 512 * 1024 * 1024 {
        return None;
    }
    let text = std::fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&text).ok()?;
    if v.get("version").and_then(Value::as_u64) != Some(INDEX_VERSION) {
        return None;
    }
    let strings: Vec<&str> = v
        .get("strings")?
        .as_array()?
        .iter()
        .map(|s| s.as_str().unwrap_or(""))
        .collect();
    let s = |n: &Value| {
        strings
            .get(n.as_u64().unwrap_or(u64::MAX) as usize)
            .copied()
    };
    let mut out = HashMap::new();
    for f in v.get("files")?.as_array()? {
        let mut requests = Vec::new();
        for r in f.get("r")?.as_array()? {
            let a = r.as_array().filter(|a| a.len() == 13)?;
            requests.push(Request {
                at: a[0].as_i64()?,
                origin: Origin::from_code(a[1].as_u64()?)?,
                project: s(&a[2])?.to_owned(),
                session: s(&a[3])?.to_owned(),
                model: s(&a[4])?.to_owned(),
                usage: TokenUsage {
                    input: a[5].as_u64()?,
                    output: a[6].as_u64()?,
                    cache_read: a[7].as_u64()?,
                    cache_write_5m: a[8].as_u64()?,
                    cache_write_1h: a[9].as_u64()?,
                },
                cost_ticks: a[10].as_u64(),
                sidechain: a[11].as_u64()? == 1,
                key: a[12].as_u64()?,
            });
        }
        out.insert(
            PathBuf::from(f.get("p")?.as_str()?),
            Entry {
                sig: (f.get("m")?.as_u64()?, f.get("s")?.as_u64()?),
                requests,
            },
        );
    }
    Some(out)
}

// ---- aggregation ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProjRange {
    Today,
    Days7,
    Days30,
    All,
}

impl ProjRange {
    pub const SHOWN: [ProjRange; 3] = [ProjRange::Days7, ProjRange::Days30, ProjRange::All];

    pub fn label(self) -> &'static str {
        match self {
            ProjRange::Today => "Today",
            ProjRange::Days7 => "7 days",
            ProjRange::Days30 => "30 days",
            ProjRange::All => "All",
        }
    }

    fn index(self) -> usize {
        match self {
            ProjRange::Today => 0,
            ProjRange::Days7 => 1,
            ProjRange::Days30 => 2,
            ProjRange::All => 3,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "today" => Some(ProjRange::Today),
            "7" | "7d" => Some(ProjRange::Days7),
            "30" | "30d" => Some(ProjRange::Days30),
            "all" => Some(ProjRange::All),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, u: &TokenUsage) {
        self.input += u.input;
        self.output += u.output;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_write_5m + u.cache_write_1h;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectUsage {
    pub name: String,
    pub vendor: VendorId,
    pub tokens: Tokens,
    pub total_tokens: u64,
    /// US dollars (Claude: API-price estimate; Grok: recorded cost). Requests without a known
    /// cost add nothing here and are counted in `unpriced_messages`.
    pub cost: f64,
    pub messages: usize,
    pub unpriced_messages: usize,
    pub sessions: usize,
    pub last_active: Option<DateTime<Utc>>,
    /// The part made by subagents / sidechains (for a later per-agent view).
    pub sidechain_messages: usize,
    pub sidechain_tokens: u64,
    pub sidechain_cost: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub tokens: Tokens,
    pub total_tokens: u64,
    pub cost: f64,
    pub messages: usize,
    pub unpriced_messages: usize,
    pub sessions: usize,
    pub projects: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeReport {
    /// Ranked by cost, then tokens.
    pub projects: Vec<ProjectUsage>,
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub generated: DateTime<Utc>,
    pub tz_offset_minutes: i32,
    ranges: [RangeReport; 4],
    /// Human text on where prices came from.
    pub prices: String,
}

impl Report {
    pub fn range(&self, r: ProjRange) -> &RangeReport {
        &self.ranges[r.index()]
    }

    pub fn today(&self) -> &RangeReport {
        self.range(ProjRange::Today)
    }
}

/// Start of the local calendar day containing `now` (`tz_offset_minutes` east of UTC).
pub fn local_day_start(now: DateTime<Utc>, tz_offset_minutes: i32) -> DateTime<Utc> {
    let off = tz_offset_minutes as i64 * 60;
    let local = now.timestamp() + off;
    let day = local.div_euclid(86_400) * 86_400;
    Utc.timestamp_opt(day - off, 0).single().unwrap_or(now)
}

#[derive(Default)]
struct Acc<'a> {
    tokens: Tokens,
    cost: f64,
    messages: usize,
    unpriced: usize,
    sessions: HashSet<&'a str>,
    last: i64,
    side_msgs: usize,
    side_tokens: u64,
    side_cost: f64,
}

/// De-duplicate, price and group `requests`; see the module docs.
pub fn aggregate<'a>(
    requests: impl Iterator<Item = &'a Request>,
    prices: &PriceTable,
    now: DateTime<Utc>,
    tz_offset_minutes: i32,
) -> Report {
    // keep the last chunk of each message; drop Grok log events of sessions with exact updates
    let all: Vec<&Request> = requests.collect();
    let exact: HashSet<&str> = all
        .iter()
        .filter(|r| r.origin == Origin::GrokUpdate)
        .map(|r| r.session.as_str())
        .collect();
    let mut by_key: HashMap<u64, usize> = HashMap::new();
    let mut kept: Vec<&Request> = Vec::with_capacity(all.len());
    for r in all {
        if r.origin == Origin::GrokLog && exact.contains(r.session.as_str()) {
            continue;
        }
        if r.key != 0 {
            match by_key.get(&r.key) {
                Some(i) => {
                    // the later copy wins; on a tie the fuller one (a partial streaming chunk has
                    // fewer tokens), so the result never depends on the order of the input
                    let size = |q: &Request| {
                        let u = &q.usage;
                        u.input + u.output + u.cache_read + u.cache_write_5m + u.cache_write_1h
                    };
                    if (r.at, size(r)) > (kept[*i].at, size(kept[*i])) {
                        kept[*i] = r;
                    }
                    continue;
                }
                None => {
                    by_key.insert(r.key, kept.len());
                }
            }
        }
        kept.push(r);
    }

    let starts: [i64; 4] = [
        local_day_start(now, tz_offset_minutes).timestamp(),
        (now - ChronoDuration::days(7)).timestamp(),
        (now - ChronoDuration::days(30)).timestamp(),
        i64::MIN,
    ];
    let mut maps: [HashMap<(u8, &str), Acc>; 4] = Default::default();
    for r in kept {
        let cost = match r.origin {
            Origin::Claude => Some(prices.cost(&r.model, &r.usage)),
            // a turn whose model the log never named has no known price
            Origin::Codex => match prices.price_for(&r.model) {
                // no table entry for the model: unknown, not Claude's default rates
                (_, crate::pricing::Source::Default) => None,
                (rates, _) => Some(crate::pricing::cost_at(&rates, &r.usage)),
            },
            _ => r.cost_ticks.map(|t| t as f64 * COST_USD_PER_TICK),
        };
        let vendor = r.origin.vendor();
        let vkey: u8 = match vendor {
            VendorId::Grok => 1,
            VendorId::Codex => 2,
            _ => 0,
        };
        for (i, start) in starts.iter().enumerate() {
            if r.at < *start {
                continue;
            }
            let a = maps[i].entry((vkey, r.project.as_str())).or_default();
            a.tokens.add(&r.usage);
            a.messages += 1;
            match cost {
                Some(c) => a.cost += c,
                None => a.unpriced += 1,
            }
            a.sessions.insert(r.session.as_str());
            a.last = a.last.max(r.at);
            if r.sidechain {
                a.side_msgs += 1;
                a.side_tokens += r.usage.total();
                a.side_cost += cost.unwrap_or(0.0);
            }
        }
    }
    let ranges = maps.map(|m| {
        let mut projects: Vec<ProjectUsage> = m
            .into_iter()
            .map(|((v, name), a)| ProjectUsage {
                name: name.to_owned(),
                vendor: match v {
                    1 => VendorId::Grok,
                    2 => VendorId::Codex,
                    _ => VendorId::Claude,
                },
                total_tokens: a.tokens.total(),
                tokens: a.tokens,
                cost: a.cost,
                messages: a.messages,
                unpriced_messages: a.unpriced,
                sessions: a.sessions.len(),
                last_active: Utc.timestamp_opt(a.last, 0).single(),
                sidechain_messages: a.side_msgs,
                sidechain_tokens: a.side_tokens,
                sidechain_cost: a.side_cost,
            })
            .collect();
        projects.sort_by(|a, b| {
            b.cost
                .total_cmp(&a.cost)
                .then(b.total_tokens.cmp(&a.total_tokens))
                .then(a.name.cmp(&b.name))
        });
        let mut totals = Totals {
            projects: projects.len(),
            ..Default::default()
        };
        for p in &projects {
            totals.tokens.input += p.tokens.input;
            totals.tokens.output += p.tokens.output;
            totals.tokens.cache_read += p.tokens.cache_read;
            totals.tokens.cache_write += p.tokens.cache_write;
            totals.cost += p.cost;
            totals.messages += p.messages;
            totals.unpriced_messages += p.unpriced_messages;
            totals.sessions += p.sessions;
        }
        totals.total_tokens = totals.tokens.total();
        RangeReport { projects, totals }
    });
    Report {
        generated: now,
        tz_offset_minutes,
        ranges,
        prices: prices.info().summary(),
    }
}

// ---- text output (`aitrack --projects`) -----------------------------------------------------

/// `1234` -> `1.2k`, `2664257` -> `2.66M`.
pub fn fmt_tokens(n: u64) -> String {
    let f = n as f64;
    if n < 1_000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{:.1}k", f / 1e3)
    } else if n < 1_000_000 {
        format!("{:.0}k", f / 1e3)
    } else if n < 100_000_000 {
        format!("{:.2}M", f / 1e6)
    } else if n < 1_000_000_000 {
        format!("{:.0}M", f / 1e6)
    } else {
        format!("{:.2}B", f / 1e9)
    }
}

/// `$12.34`; `$0.04`; `$123`. `partial` adds a `+` (some requests have no known cost) and a
/// total without any known cost reads `n/a`.
pub fn fmt_cost(cost: f64, messages: usize, unpriced: usize) -> String {
    if messages > 0 && unpriced == messages {
        return "n/a".into();
    }
    let s = if cost >= 100.0 {
        format!("${cost:.0}")
    } else {
        format!("${cost:.2}")
    };
    if unpriced > 0 {
        format!("{s}+")
    } else {
        s
    }
}

/// Name column of `--projects`: Grok and Codex rows end in " (Grok)" / " (ChatGPT)" so they are not mistaken for Claude
/// projects of the same name. A long name is cut before the label, never after it.
pub fn cli_project_label(p: &ProjectUsage) -> String {
    const MAX_NAME: usize = 40;
    let name: String = p.name.chars().take(MAX_NAME).collect();
    match p.vendor {
        VendorId::Grok => format!("{name} (Grok)"),
        VendorId::Codex => format!("{name} (ChatGPT)"),
        _ => name,
    }
}

/// The ranked table, the Today line and the folders looked at, for `aitrack --projects`.
/// Project names are shown; no message content ever is.
pub fn projects_text(report: &Report, range: ProjRange, sources: &[SourceInfo]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let t = &report.today().totals;
    let _ = writeln!(
        s,
        "Today: {} ({} tokens, {} requests)",
        fmt_cost(t.cost, t.messages, t.unpriced_messages),
        fmt_tokens(t.total_tokens),
        t.messages
    );
    let rr = report.range(range);
    let _ = writeln!(s, "\nProjects, {}:", range.label());
    if rr.projects.is_empty() {
        let _ = writeln!(s, "  (no local transcripts in this range)");
    } else {
        let w = rr
            .projects
            .iter()
            .map(|p| cli_project_label(p).chars().count())
            .max()
            .unwrap_or(7)
            .clamp(7, 48);
        let _ = writeln!(
            s,
            "  {:>3}  {:<w$}  {:<6}  {:>9}  {:>9}  {:>8}",
            "#", "project", "vendor", "tokens", "cost", "sessions"
        );
        for (i, p) in rr.projects.iter().enumerate() {
            let name = cli_project_label(p);
            let _ = writeln!(
                s,
                "  {:>3}  {:<w$}  {:<6}  {:>9}  {:>9}  {:>8}",
                i + 1,
                name,
                p.vendor.name(),
                fmt_tokens(p.total_tokens),
                fmt_cost(p.cost, p.messages, p.unpriced_messages),
                p.sessions
            );
        }
        let t = &rr.totals;
        let _ = writeln!(
            s,
            "  total: {} tokens, {}, {} requests, {} sessions",
            fmt_tokens(t.total_tokens),
            fmt_cost(t.cost, t.messages, t.unpriced_messages),
            t.messages,
            t.sessions
        );
    }
    let _ = writeln!(s, "\nPrices: {}", report.prices);
    let _ = writeln!(s, "\nSources:");
    for i in sources {
        let _ = writeln!(
            s,
            "  {}: {} ({}, {} file{}{})",
            i.label,
            i.path.display(),
            if i.exists { "exists" } else { "missing" },
            i.files,
            if i.files == 1 { "" } else { "s" },
            if i.note.is_empty() {
                String::new()
            } else {
                format!("; {}", i.note)
            }
        );
    }
    s
}
