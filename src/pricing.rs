// SPDX-License-Identifier: GPL-3.0-or-later
//! API-equivalent cost estimates from token usage (no UI).
//!
//! Plan usage is not billed per token, but "what would this have cost on the API" is a useful
//! yardstick. Rates are USD per million tokens and come from four layers, highest priority first:
//!
//! 1. **User overrides**: `<data dir>/pricing-overrides.json`, the same shape as the built-in table
//!    (`{"claude-sonnet-5-5": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75,
//!    "cache_write_1h": 6}, "default": {...}}`). Missing cache fields are derived from `input`.
//! 2. **Built-in overrides**: [`BUILTIN_OVERRIDES`], a short list of rates that beat the live list
//!    because the live list is known to disagree with Claude Code's own `/usage` figures (see
//!    "Known discrepancy" below). Your own overrides file still wins over these.
//! 3. **Live list**: the community price list maintained by the LiteLLM project
//!    ([`LIVE_URL`]), downloaded at most once per 24 h and kept in `<data dir>/pricing-cache.json`
//!    (only the Claude and OpenAI entries, mapped onto our model ids). With no network the cached copy is used
//!    however old it is; its age is reported in [`TableInfo`].
//! 4. **Built-in fallback**: [`FALLBACK`], dated [`AS_OF`].
//!
//! Within a layer the longest matching key prefix wins (`claude-sonnet-5-5-20260101` matches
//! `claude-sonnet-5-5` before `claude-sonnet`); a prefix only matches at a token boundary, so
//! `claude-opus-4` does not match `claude-opus-45`. A model nothing matches gets the table's
//! default rates ([`Source::Default`]).
//!
//! # Known discrepancy: Sonnet 5.5
//!
//! The community list prices `claude-sonnet-5-5` at $2 / $10 per million input / output tokens with
//! $0.10 cache reads. The author's own Claude Code `/usage` panel showed $0.98 for a Sonnet 5.5
//! session of 48 input, 860 output, 2.4M cache-read and 76.4k cache-write tokens. That fits
//! $3 / $15 / $0.30 / $3.75 (about $1.02) and not $2 / $10 / $0.10 (about $0.44). Because the live
//! list wins over the fallback, [`BUILTIN_OVERRIDES`] pins `claude-sonnet-5-5` to $3 / $15 so our
//! figures keep matching Claude Code's own. Remove that entry if Claude Code's figures ever change.
//! Every other model follows the live list.
//!
//! Only the base per-token prices are used; long-context surcharges ("above 200k tokens"),
//! batch discounts and regional (bedrock / vertex) prices are ignored, so treat results as an
//! estimate.

use crate::util::{data_dir, iso, parse_ts, round_to};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where the built-in table was last checked against published prices.
pub const AS_OF: &str = "2026-10-14";

/// The community price list (JSON object keyed by model name).
pub const LIVE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// The downloaded list is refreshed at most this often.
pub const REFRESH_HOURS: i64 = 24;
/// After a failed download, wait this long before trying again.
pub const RETRY_HOURS: i64 = 1;
/// Largest response body accepted from the network.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Network timeout for the download.
pub const FETCH_TIMEOUT_SECS: u64 = 10;

/// Anthropic's standard multipliers over the input price, used when a source lacks cache prices.
const READ_MULT: f64 = 0.1;
const WRITE_5M_MULT: f64 = 1.25;
const WRITE_1H_MULT: f64 = 2.0;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

impl Rates {
    pub const fn new(
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write_5m: f64,
        cache_write_1h: f64,
    ) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write_5m,
            cache_write_1h,
        }
    }

    /// Input and output prices with the standard cache multipliers.
    pub fn from_base(input: f64, output: f64) -> Self {
        Self::new(
            input,
            output,
            round_to(input * READ_MULT, 6),
            round_to(input * WRITE_5M_MULT, 6),
            round_to(input * WRITE_1H_MULT, 6),
        )
    }

    /// Stored shape (same keys as the built-in table).
    pub fn to_value(&self) -> Value {
        json!({
            "input": self.input, "output": self.output, "cache_read": self.cache_read,
            "cache_write": self.cache_write_5m, "cache_write_1h": self.cache_write_1h,
        })
    }

    /// Lenient parse of `{"input", "output", "cache_read", "cache_write" | "cache_write_5m",
    /// "cache_write_1h"}` in USD per million tokens. `input` and `output` are required; missing
    /// cache prices are derived from `input`. Negative or non-finite numbers reject the entry.
    pub fn from_value(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let get = |k: &str| o.get(k).and_then(Value::as_f64);
        let ok = |x: f64| x.is_finite() && x >= 0.0;
        let (input, output) = (get("input")?, get("output")?);
        let mut r = Self::from_base(input, output);
        for (slot, keys) in [
            (&mut r.cache_read, &["cache_read"][..]),
            (&mut r.cache_write_5m, &["cache_write", "cache_write_5m"]),
            (&mut r.cache_write_1h, &["cache_write_1h"]),
        ] {
            if let Some(x) = keys.iter().find_map(|k| get(k)) {
                *slot = x;
            }
        }
        [
            r.input,
            r.output,
            r.cache_read,
            r.cache_write_5m,
            r.cache_write_1h,
        ]
        .into_iter()
        .all(ok)
        .then_some(r)
    }
}

/// Token counts of one message (or a sum of many).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

impl TokenUsage {
    /// Read a Claude Code transcript `usage` object. Cache creation is split into
    /// `cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` when present;
    /// otherwise the total `cache_creation_input_tokens` is counted at the 5-minute rate.
    pub fn from_usage(usage: &Value) -> Self {
        let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cc = usage.get("cache_creation").unwrap_or(&Value::Null);
        let (w5, w1) = (
            n(cc, "ephemeral_5m_input_tokens"),
            n(cc, "ephemeral_1h_input_tokens"),
        );
        let (w5, w1) = if w5 + w1 > 0 {
            (w5, w1)
        } else {
            (n(usage, "cache_creation_input_tokens"), 0)
        };
        Self {
            input: n(usage, "input_tokens"),
            output: n(usage, "output_tokens"),
            cache_read: n(usage, "cache_read_input_tokens"),
            cache_write_5m: w5,
            cache_write_1h: w1,
        }
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
    }
}

/// Dollar cost of `usage` at `rates`.
pub fn cost_at(rates: &Rates, usage: &TokenUsage) -> f64 {
    (usage.input as f64 * rates.input
        + usage.output as f64 * rates.output
        + usage.cache_read as f64 * rates.cache_read
        + usage.cache_write_5m as f64 * rates.cache_write_5m
        + usage.cache_write_1h as f64 * rates.cache_write_1h)
        / 1e6
}

/// Which layer a model's rates came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The user's `pricing-overrides.json`.
    Override,
    /// A rate pinned in this crate ([`BUILTIN_OVERRIDES`]) that beats the live list.
    BuiltinOverride,
    Live,
    Fallback,
    /// No key matched; the table's default rates.
    Default,
}

/// Where the live list came from on this load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrigin {
    /// Downloaded just now.
    Network,
    /// Read from `pricing-cache.json` (no download needed, allowed or possible).
    Cache,
    /// No live data at all; only the built-in table (and overrides).
    Fallback,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    pub origin: ListOrigin,
    /// When the live list was downloaded (`None` for [`ListOrigin::Fallback`]).
    pub fetched_at: Option<DateTime<Utc>>,
    /// Age of the live list at load time, in seconds.
    pub age_secs: Option<i64>,
    /// The live list is older than [`REFRESH_HOURS`] (a refresh was not possible or not allowed).
    pub stale: bool,
    pub live_models: usize,
    pub override_models: usize,
    /// Entries of [`BUILTIN_OVERRIDES`] in effect.
    pub builtin_override_models: usize,
    /// Why the last download failed, if it did.
    pub fetch_error: Option<String>,
    /// Non-fatal problems (unreadable overrides / cache files).
    pub warnings: Vec<String>,
}

impl TableInfo {
    pub fn age_hours(&self) -> Option<f64> {
        self.age_secs.map(|s| s as f64 / 3600.0)
    }

    /// One line for a tooltip or the diagnostics list.
    pub fn summary(&self) -> String {
        let mut s = match (self.origin, self.age_hours()) {
            (ListOrigin::Network, _) => {
                format!("live prices (downloaded now, {} models)", self.live_models)
            }
            (ListOrigin::Cache, Some(h)) => format!(
                "cached live prices, {}{}",
                fmt_age(h),
                if self.stale { " (stale)" } else { "" }
            ),
            _ => format!("built-in prices as of {AS_OF}"),
        };
        if self.override_models > 0 {
            s.push_str(&format!(", {} override(s)", self.override_models));
        }
        if self.builtin_override_models > 0 {
            s.push_str(&format!(
                ", {} built-in override(s)",
                self.builtin_override_models
            ));
        }
        if let Some(e) = &self.fetch_error {
            s.push_str(&format!("; last download failed: {e}"));
        }
        s
    }
}

fn fmt_age(h: f64) -> String {
    if h < 1.0 {
        format!("{:.0} min old", h * 60.0)
    } else if h < 48.0 {
        format!("{h:.0} h old")
    } else {
        format!("{:.0} d old", h / 24.0)
    }
}

/// The built-in table: `(key prefix, input, output, cache_read, cache_write_5m, cache_write_1h)`.
/// One entry per known model family (list prices in USD per million tokens).
pub const FALLBACK: &[(&str, Rates)] = &[
    ("claude-opus-4-1", Rates::new(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-opus-4-0", Rates::new(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-opus-4", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-5", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-6", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-7", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-8", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-5", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-fable-5", Rates::new(10.0, 50.0, 1.0, 12.5, 20.0)),
    ("claude-sonnet", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-sonnet-4", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-sonnet-4-5", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-sonnet-4-6", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-sonnet-5", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-sonnet-5-5", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-haiku", Rates::new(0.8, 4.0, 0.08, 1.0, 1.6)),
    ("claude-haiku-4", Rates::new(1.0, 5.0, 0.1, 1.25, 2.0)),
    ("claude-haiku-4-5", Rates::new(1.0, 5.0, 0.1, 1.25, 2.0)),
    ("claude-haiku-5", Rates::new(1.0, 5.0, 0.1, 1.25, 2.0)),
    ("claude-3-opus", Rates::new(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-3-5-sonnet", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-3-7-sonnet", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-3-sonnet", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-3-5-haiku", Rates::new(0.8, 4.0, 0.08, 1.0, 1.6)),
    ("claude-3-haiku", Rates::new(0.25, 1.25, 0.03, 0.3, 0.5)),
    // OpenAI (Codex): no cache-write charge, so the write rates equal the input rate (unused)
    ("gpt-5", Rates::new(1.25, 10.0, 0.125, 1.25, 1.25)),
    ("gpt-5-codex", Rates::new(1.25, 10.0, 0.125, 1.25, 1.25)),
    ("gpt-5-mini", Rates::new(0.25, 2.0, 0.025, 0.25, 0.25)),
    ("gpt-5-nano", Rates::new(0.05, 0.4, 0.005, 0.05, 0.05)),
    ("gpt-4.1", Rates::new(2.0, 8.0, 0.5, 2.0, 2.0)),
    ("codex-mini", Rates::new(1.5, 6.0, 0.375, 1.5, 1.5)),
    ("o3", Rates::new(2.0, 8.0, 0.5, 2.0, 2.0)),
    ("o4-mini", Rates::new(1.1, 4.4, 0.275, 1.1, 1.1)),
];

/// Rates that win over the live list (but not over the user's overrides file).
///
/// `claude-sonnet-5-5`: the author's Claude Code `/usage` panel priced a Sonnet 5.5 session of
/// 48 input / 860 output / 2.4M cache-read / 76.4k cache-write tokens at $0.98. These rates give
/// about $1.02 for it; the community list's $2 / $10 / $0.10 would give about $0.44. Remove this
/// entry if Claude Code's own figures ever change.
pub const BUILTIN_OVERRIDES: &[(&str, Rates)] =
    &[("claude-sonnet-5-5", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0))];

/// Rates for a model no key matches.
pub const DEFAULT_RATES: Rates = Rates::new(3.0, 15.0, 0.3, 3.75, 6.0);

// ---- model ids ------------------------------------------------------------------------------

fn strip_date(s: &str) -> &str {
    match s.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => s,
    }
}

/// Canonical lower-case model id: provider prefixes (`anthropic/`, `bedrock/us.anthropic.`,
/// `vertex_ai/`), context suffixes (`[1m]`), bedrock versions (`-v2:0`), vertex dates (`@20241022`),
/// `-latest` and `-YYYYMMDD` dates are removed.
/// `anthropic.claude-3-5-sonnet-20241022-v2:0` -> `claude-3-5-sonnet`.
pub fn normalize_id(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    if let Some(i) = s.find('[') {
        s.truncate(i);
    }
    if let Some(i) = s.rfind('/') {
        s.drain(..=i);
    }
    if let Some(i) = s.find("anthropic.") {
        s.drain(..i + "anthropic.".len());
    }
    if let Some(i) = s.find('@') {
        s.truncate(i);
    }
    if let Some(i) = s.rfind("-v") {
        let rest = &s[i + 2..];
        let mut parts = rest.splitn(2, ':');
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if parts.next().is_some_and(digits) && parts.next().is_none_or(digits) {
            s.truncate(i);
        }
    }
    if let Some(t) = s.strip_suffix("-latest") {
        s.truncate(t.len());
    }
    let t = strip_date(&s).len();
    s.truncate(t);
    s
}

/// `claude-opus-4-8` -> `Opus 4.8`, `claude-3-5-haiku-20241022` -> `Haiku 3.5`,
/// `claude-sonnet-4-20250514` -> `Sonnet 4`. Unknown shapes come back normalised.
pub fn pretty_model(model: &str) -> String {
    let m = normalize_id(model);
    let parts: Vec<&str> = m.split('-').collect();
    let num = |p: &&str| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit());
    let title = |f: &str| {
        let mut c = f.chars();
        c.next()
            .map(|h| h.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
    };
    let alpha = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase());
    match parts.as_slice() {
        ["claude", fam, major] if alpha(fam) && num(major) => format!("{} {major}", title(fam)),
        ["claude", fam, major, minor] if alpha(fam) && num(major) && num(minor) => {
            format!("{} {major}.{minor}", title(fam))
        }
        ["claude", major, fam] if alpha(fam) && num(major) => format!("{} {major}", title(fam)),
        ["claude", major, minor, fam] if alpha(fam) && num(major) && num(minor) => {
            format!("{} {major}.{minor}", title(fam))
        }
        _ => m,
    }
}

/// `key` is a prefix of `model` that ends at a token boundary.
fn prefix_matches(model: &str, key: &str) -> bool {
    model.starts_with(key)
        && (model.len() == key.len()
            || key.ends_with('-')
            || !model.as_bytes()[key.len()].is_ascii_alphanumeric())
}

type Table = BTreeMap<String, Rates>;

fn longest_prefix(table: &Table, model: &str) -> Option<Rates> {
    table
        .iter()
        .filter(|(k, _)| prefix_matches(model, k))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, r)| *r)
}

// ---- live list ------------------------------------------------------------------------------

fn cost_field(o: &Map<String, Value>, key: &str) -> Option<f64> {
    o.get(key)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && *x >= 0.0 && *x < 1.0)
        .map(|x| round_to(x * 1e6, 6))
}

/// The model families the live list is read for: Claude, and OpenAI's (Codex) models.
fn is_priced_family(id: &str) -> bool {
    ["claude-", "gpt-", "codex-", "o3", "o4-"]
        .iter()
        .any(|p| id.starts_with(p))
}

/// Parse the community list into rates keyed by normalised Claude and OpenAI model id. Entries that are not
/// objects, not Claude models or lack numeric `input_cost_per_token` / `output_cost_per_token` are
/// skipped. When several keys map to one id (`claude-x`, `anthropic/claude-x`,
/// `bedrock/...anthropic.claude-x-v1:0`) the least provider-specific key wins.
/// Errors only if the text is not a JSON object.
pub fn parse_live_list(text: &str) -> Result<BTreeMap<String, Rates>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = v.as_object().ok_or("not a JSON object")?;
    let mut best: BTreeMap<String, (u8, Rates)> = BTreeMap::new();
    for (key, entry) in obj {
        let id = normalize_id(key);
        if !is_priced_family(&id) {
            continue;
        }
        let Some(o) = entry.as_object() else { continue };
        let (Some(input), Some(output)) = (
            cost_field(o, "input_cost_per_token"),
            cost_field(o, "output_cost_per_token"),
        ) else {
            continue;
        };
        let mut r = Rates::from_base(input, output);
        if let Some(x) = cost_field(o, "cache_read_input_token_cost") {
            r.cache_read = x;
        }
        if let Some(x) = cost_field(o, "cache_creation_input_token_cost") {
            r.cache_write_5m = x;
        }
        if let Some(x) = cost_field(o, "cache_creation_input_token_cost_above_1hr") {
            r.cache_write_1h = x;
        }
        let rank = if !key.contains('/') && !key.contains("anthropic.") {
            0
        } else if key.starts_with("anthropic/") {
            1
        } else {
            2
        };
        match best.get(&id) {
            Some((r0, _)) if *r0 <= rank => {}
            _ => {
                best.insert(id, (rank, r));
            }
        }
    }
    Ok(best.into_iter().map(|(k, (_, r))| (k, r)).collect())
}

/// Parse a table in the built-in / override shape. Returns the entries (including `default`)
/// and a count of entries that were ignored.
fn parse_table(v: &Value) -> Option<(Table, usize)> {
    let o = v.as_object()?;
    let (mut t, mut skipped) = (Table::new(), 0);
    for (k, e) in o {
        if k.starts_with('_') {
            continue;
        }
        match Rates::from_value(e) {
            Some(r) => {
                let key = if k == "default" {
                    k.clone()
                } else {
                    k.trim().to_ascii_lowercase()
                };
                t.insert(key, r);
            }
            None => skipped += 1,
        }
    }
    Some((t, skipped))
}

// ---- fetching -------------------------------------------------------------------------------

/// Source of the live list's text; injected so tests never touch the network.
pub trait Fetcher {
    fn fetch(&self, url: &str) -> Result<String, String>;
}

impl<F: Fn(&str) -> Result<String, String>> Fetcher for F {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self(url)
    }
}

/// Real HTTP download: short timeout, response size capped at [`MAX_BYTES`], honours proxy
/// environment variables.
pub struct UreqFetcher;

impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            .try_proxy_from_env(true)
            .build();
        let resp = agent
            .get(url)
            .set("User-Agent", "aitrack")
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("HTTP {code}"),
                ureq::Error::Transport(t) => format!("network error: {t}"),
            })?;
        let mut body = Vec::new();
        resp.into_reader()
            .take(MAX_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|e| format!("network error: {e}"))?;
        if body.len() as u64 > MAX_BYTES {
            return Err(format!("response larger than {} MiB", MAX_BYTES >> 20));
        }
        String::from_utf8(body).map_err(|_| "response is not UTF-8".to_owned())
    }
}

// ---- table ----------------------------------------------------------------------------------

/// Where the two files live.
#[derive(Debug, Clone)]
pub struct Paths {
    pub cache: PathBuf,
    pub overrides: PathBuf,
}

impl Paths {
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let d = dir.as_ref();
        Self {
            cache: d.join("pricing-cache.json"),
            overrides: d.join("pricing-overrides.json"),
        }
    }

    /// `~/.aitrack/pricing-cache.json` and `~/.aitrack/pricing-overrides.json`.
    pub fn from_env() -> Self {
        Self::in_dir(data_dir())
    }
}

struct CacheFile {
    fetched_at: Option<DateTime<Utc>>,
    last_attempt: Option<DateTime<Utc>>,
    models: Table,
}

fn read_cache(path: &Path) -> Result<Option<CacheFile>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let bad = |m: &str| format!("{}: {m}", path.display());
    let v: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
    let ts = |k: &str| v.get(k).and_then(Value::as_str).and_then(parse_ts);
    let (models, _) = v
        .get("models")
        .and_then(parse_table)
        .ok_or_else(|| bad("no models"))?;
    Ok(Some(CacheFile {
        fetched_at: ts("fetched_at"),
        last_attempt: ts("last_attempt"),
        models,
    }))
}

fn write_cache(path: &Path, c: &CacheFile) -> std::io::Result<()> {
    let models: Map<String, Value> = c
        .models
        .iter()
        .map(|(k, r)| (k.clone(), r.to_value()))
        .collect();
    let v = json!({
        "version": 1, "source": LIVE_URL,
        "fetched_at": c.fetched_at.map(iso), "last_attempt": c.last_attempt.map(iso),
        "models": models,
    });
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&v).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

#[derive(Debug, Clone)]
pub struct PriceTable {
    overrides: Table,
    builtin: Table,
    live: Table,
    fallback: Table,
    default: Rates,
    info: TableInfo,
}

impl PriceTable {
    /// Built-in table only (no files, no network).
    pub fn fallback() -> Self {
        Self {
            overrides: Table::new(),
            builtin: BUILTIN_OVERRIDES
                .iter()
                .map(|(k, r)| ((*k).to_owned(), *r))
                .collect(),
            live: Table::new(),
            fallback: FALLBACK
                .iter()
                .map(|(k, r)| ((*k).to_owned(), *r))
                .collect(),
            default: DEFAULT_RATES,
            info: TableInfo {
                origin: ListOrigin::Fallback,
                fetched_at: None,
                age_secs: None,
                stale: false,
                live_models: 0,
                override_models: 0,
                builtin_override_models: BUILTIN_OVERRIDES.len(),
                fetch_error: None,
                warnings: Vec::new(),
            },
        }
    }

    /// Load overrides and the live list as described in the module docs, downloading with
    /// [`UreqFetcher`] when `allow_network` is set and a refresh is due. Never fails: every
    /// problem degrades to the next layer and is reported in [`TableInfo`].
    pub fn load(paths: &Paths, now: DateTime<Utc>, allow_network: bool) -> Self {
        Self::load_with(paths, now, allow_network, &UreqFetcher)
    }

    /// [`load`](Self::load) with an injected download function.
    pub fn load_with(
        paths: &Paths,
        now: DateTime<Utc>,
        allow_network: bool,
        fetcher: &dyn Fetcher,
    ) -> Self {
        let mut t = Self::fallback();

        match std::fs::read_to_string(&paths.overrides) {
            Ok(text) => match serde_json::from_str::<Value>(&text)
                .ok()
                .as_ref()
                .and_then(parse_table)
            {
                Some((mut table, skipped)) => {
                    if skipped > 0 {
                        t.info.warnings.push(format!(
                            "{}: ignored {skipped} invalid entr{}",
                            paths.overrides.display(),
                            if skipped == 1 { "y" } else { "ies" }
                        ));
                    }
                    if let Some(d) = table.remove("default") {
                        t.default = d;
                    }
                    t.info.override_models = table.len();
                    t.overrides = table;
                }
                None => t.info.warnings.push(format!(
                    "{}: not a valid price table, ignored",
                    paths.overrides.display()
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => t
                .info
                .warnings
                .push(format!("{}: {e}", paths.overrides.display())),
        }

        let mut cache = match read_cache(&paths.cache) {
            Ok(c) => c,
            Err(w) => {
                t.info.warnings.push(w);
                None
            }
        };
        let age = |c: &CacheFile| c.fetched_at.map(|f| now - f);
        let due = match cache.as_ref().and_then(age) {
            Some(a) => a >= Duration::hours(REFRESH_HOURS) || a < Duration::zero(),
            None => true,
        };
        let may_retry = cache
            .as_ref()
            .and_then(|c| c.last_attempt)
            .is_none_or(|l| now - l >= Duration::hours(RETRY_HOURS) || l > now);

        let mut origin = ListOrigin::Cache;
        if allow_network && due && may_retry {
            let result = fetcher.fetch(LIVE_URL).and_then(|text| {
                let models = parse_live_list(&text)?;
                if models.is_empty() {
                    Err("no Claude prices found in the list".to_owned())
                } else {
                    Ok(models)
                }
            });
            let prev = cache.take();
            match result {
                Ok(models) => {
                    let fresh = CacheFile {
                        fetched_at: Some(now),
                        last_attempt: Some(now),
                        models,
                    };
                    if let Err(e) = write_cache(&paths.cache, &fresh) {
                        t.info
                            .warnings
                            .push(format!("{}: {e}", paths.cache.display()));
                    }
                    cache = Some(fresh);
                    origin = ListOrigin::Network;
                }
                Err(e) => {
                    t.info.fetch_error = Some(e);
                    let mut c = prev.unwrap_or(CacheFile {
                        fetched_at: None,
                        last_attempt: None,
                        models: Table::new(),
                    });
                    c.last_attempt = Some(now);
                    if let Err(e) = write_cache(&paths.cache, &c) {
                        t.info
                            .warnings
                            .push(format!("{}: {e}", paths.cache.display()));
                    }
                    cache = Some(c);
                }
            }
        }

        if let Some(c) = cache.filter(|c| c.fetched_at.is_some() && !c.models.is_empty()) {
            let age = c.fetched_at.map(|f| (now - f).num_seconds().max(0));
            t.info.origin = origin;
            t.info.fetched_at = c.fetched_at;
            t.info.age_secs = age;
            t.info.stale = age.is_some_and(|a| a >= REFRESH_HOURS * 3600);
            t.info.live_models = c.models.len();
            t.live = c.models;
        }
        t
    }

    /// Rates for `model` and the layer they came from (user overrides, built-in overrides, the live
    /// list, the built-in table, then the default).
    pub fn price_for(&self, model: &str) -> (Rates, Source) {
        let m = normalize_id(model);
        longest_prefix(&self.overrides, &m)
            .map(|r| (r, Source::Override))
            .or_else(|| longest_prefix(&self.builtin, &m).map(|r| (r, Source::BuiltinOverride)))
            .or_else(|| longest_prefix(&self.live, &m).map(|r| (r, Source::Live)))
            .or_else(|| longest_prefix(&self.fallback, &m).map(|r| (r, Source::Fallback)))
            .unwrap_or((self.default, Source::Default))
    }

    /// Estimated dollar cost of `usage` on `model`.
    pub fn cost(&self, model: &str, usage: &TokenUsage) -> f64 {
        cost_at(&self.price_for(model).0, usage)
    }

    pub fn info(&self) -> &TableInfo {
        &self.info
    }

    /// Rates used for models nothing matches.
    pub fn default_rates(&self) -> Rates {
        self.default
    }
}
