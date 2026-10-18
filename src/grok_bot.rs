// SPDX-License-Identifier: GPL-3.0-or-later
//! The Grok Bot app's second weekly meter, "Weekly Grok Bot Limit" (the first, the SuperGrok pool,
//! comes from the Grok CLI billing log, see [`crate::grok`]).
//!
//! **The file layout below was reported by another agent reading the owner's Mac; aiTrack has
//! verified it only by matching the values on the app's Usage page (59.17 % and the reset time).**
//! Nothing here has been compiled or run on macOS by us. Every step is therefore defensive: a
//! surprise in the layout hides the bar, it never panics (release builds abort on panic).
//!
//! * macOS: `~/Library/Application Support/Grok Bot/sand-client-persistence/`. Windows (also
//!   reported by Grok Bot, same file kind and keys, unverified by us): `%APPDATA%\Grok Bot\sand-client-persistence\`.
//!   (`AITRACK_GROK_BOT_DIR` points at such a folder on any platform; the tests use it.)
//! * Each file there is `<base32 of a key name>.blob` (RFC 4648 alphabet, either case, padding
//!   possibly stripped). The wanted key name ends in `.weekly-usage.cache`; it also holds an
//!   account identifier, which aiTrack never logs, stores or shows.
//! * The file is a small JSON document: `value.reading.usage.percentUsed` (0..100),
//!   `value.reading.usage.nextResetMs` (epoch ms), `value.reading.readAtMs`, `value.expiresAtMs`.
//! * Only the folder `sand-client-persistence` is listed, and only files whose decoded name matches
//!   are opened. The app's neighbouring `auth.json` and `sand-secrets.json` are never touched.
//! * The app refreshes the file only while it is open, so the reading ages while it is closed.
//!
//! Readings are appended (when they change, or every [`SAVE_EVERY_MIN`] minutes) to
//! `usage-grok-bot.jsonl` next to the other history files, as records with the field `bot_weekly`.
//! That file is a separate append-only history (for later charts); the window shows the live file.

use crate::sources::{Env, Os};
use crate::store::Store;
use crate::util::iso;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const SOURCE: &str = "grok-bot-cache";
/// The decoded key name of the wanted file ends with this.
pub const NAME_SUFFIX: &str = ".weekly-usage.cache";
/// A reading older than this is shown dimmed and without projections.
pub const STALE_MIN: f64 = 60.0;
/// Append an unchanged reading to the history at most this often.
pub const SAVE_EVERY_MIN: i64 = 15;
/// The cache file is about 310 bytes; anything much larger is not it.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Look at no more directory entries than this.
const MAX_ENTRIES: usize = 2000;

/// The folder to scan: the override, else the macOS or Windows default; `None` where it is unknown.
pub fn persistence_dir(env: &Env) -> Option<PathBuf> {
    if let Some(d) = &env.grok_bot_dir {
        return Some(d.clone());
    }
    let base = match env.os {
        Os::Mac => env
            .home
            .clone()?
            .join("Library")
            .join("Application Support"),
        // `%APPDATA%` (Roaming), from the environment, never a fixed user name
        Os::Windows => env
            .appdata
            .clone()
            .or_else(|| Some(env.home.clone()?.join("AppData").join("Roaming")))?,
        Os::Linux => return None,
    };
    Some(base.join("Grok Bot").join("sand-client-persistence"))
}

// ---------------------------------------------------------------- names

/// RFC 4648 base32 (either case, `=` padding optional). `None` for any other character.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Does this file name (`<base32>.blob`) decode to a `.weekly-usage.cache` key? Pure; the name is
/// never kept or printed.
pub fn is_weekly_usage_file(file_name: &str) -> bool {
    let Some(stem) = file_name
        .strip_suffix(".blob")
        .or_else(|| file_name.strip_suffix(".BLOB"))
    else {
        return false;
    };
    base32_decode(stem)
        .and_then(|b| String::from_utf8(b).ok())
        .is_some_and(|name| name.ends_with(NAME_SUFFIX))
}

// ---------------------------------------------------------------- the reading

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotReading {
    /// Percent used, 0..=100.
    pub pct: f64,
    /// When the weekly period resets.
    pub resets_at: DateTime<Utc>,
    /// When the app wrote the reading.
    pub read_at: DateTime<Utc>,
    /// When the app itself considers the reading expired, if the file says.
    pub expires_at: Option<DateTime<Utc>>,
}

fn ms(v: &Value) -> Option<DateTime<Utc>> {
    let n = v.as_f64().filter(|n| n.is_finite() && *n > 0.0)?;
    if n > 4.0e12 {
        return None; // not a plausible epoch-millisecond time
    }
    Utc.timestamp_millis_opt(n as i64).single()
}

/// Parse the cache file. `None` for anything that is not a usable reading.
pub fn parse(bytes: &[u8]) -> Option<BotReading> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let reading = v.get("value")?.get("reading")?;
    let usage = reading.get("usage")?;
    let pct = usage
        .get("percentUsed")?
        .as_f64()
        .filter(|p| p.is_finite() && *p >= 0.0 && *p <= 100.5)?
        .min(100.0);
    Some(BotReading {
        pct,
        resets_at: ms(usage.get("nextResetMs")?)?,
        read_at: ms(reading.get("readAtMs")?)?,
        expires_at: v
            .get("value")
            .and_then(|x| x.get("expiresAtMs"))
            .and_then(ms),
    })
}

impl BotReading {
    pub fn age_min(&self, now: DateTime<Utc>) -> f64 {
        (now - self.read_at).num_milliseconds() as f64 / 60000.0
    }

    /// The weekly period this reading belongs to is over (its number is last week's).
    pub fn period_ended(&self, now: DateTime<Utc>) -> bool {
        self.resets_at < now
    }

    /// Older than [`STALE_MIN`], or past the app's own expiry.
    pub fn stale(&self, now: DateTime<Utc>) -> bool {
        self.age_min(now) > STALE_MIN || self.expires_at.is_some_and(|e| e < now)
    }
}

/// What a scan of the folder found (counts only; nothing identifying).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scan {
    pub dir_exists: bool,
    /// Files whose decoded name matched.
    pub matched: usize,
    /// Matching files that could not be read or parsed.
    pub unusable: usize,
    /// The usable reading with the newest `readAtMs`.
    pub reading: Option<BotReading>,
}

/// List `dir` and open only the files whose decoded name matches. Never reads anything else.
pub fn scan_dir(dir: &Path) -> Scan {
    let mut out = Scan::default();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    out.dir_exists = true;
    for entry in rd.filter_map(Result::ok).take(MAX_ENTRIES) {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue; // folders and symlinks are not followed
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_weekly_usage_file(name) {
            continue;
        }
        out.matched += 1;
        let parsed = std::fs::File::open(entry.path()).ok().and_then(|f| {
            let mut buf = Vec::new();
            f.take(MAX_FILE_BYTES + 1).read_to_end(&mut buf).ok()?;
            (buf.len() as u64 <= MAX_FILE_BYTES).then_some(buf)
        });
        match parsed.as_deref().and_then(parse) {
            Some(r) => {
                if out.reading.is_none_or(|b| r.read_at > b.read_at) {
                    out.reading = Some(r);
                }
            }
            None => out.unusable += 1,
        }
    }
    out
}

/// The newest reading for this machine, if there is one.
pub fn read(env: &Env) -> Option<BotReading> {
    scan_dir(&persistence_dir(env)?).reading
}

// ---------------------------------------------------------------- diagnostics

/// Safe-to-paste lines for `--sources`: found / stale / not found, with no paths, names or ids.
pub fn diagnose(env: &Env, now: DateTime<Utc>) -> String {
    let mut o = String::from(
        "Grok Bot app (second weekly meter, \"Weekly Grok Bot Limit\"; reads the app's local cache)\n",
    );
    let state = match persistence_dir(env) {
        None => "not available on this platform (macOS and Windows only)".to_string(),
        Some(dir) => {
            let s = scan_dir(&dir);
            match (s.dir_exists, s.reading) {
                (false, _) => "not found (the Grok Bot app's data folder is missing)".into(),
                (true, None) if s.matched == 0 => {
                    "not found (no weekly usage cache in the folder)".into()
                }
                (true, None) => format!(
                    "found {} file(s) but none readable (layout may have changed)",
                    s.matched
                ),
                (true, Some(r)) => format!(
                    "{} - {}% used, read {} ago, {}",
                    if r.stale(now) {
                        "found, stale"
                    } else {
                        "found"
                    },
                    crate::reading::fmt0(r.pct),
                    crate::sources::age_text(r.age_min(now).round() as i64),
                    if r.period_ended(now) {
                        "its weekly period has ended"
                    } else {
                        "period running"
                    }
                ),
            }
        }
    };
    o.push_str(&format!("   {SOURCE}: {state}\n"));
    o
}

// ---------------------------------------------------------------- history + live reading

/// Reads the cache whenever asked (on the Grok collector thread), keeps the newest reading in
/// memory for the window and appends changes to its own history file.
pub struct BotTracker {
    env: Env,
    store: Box<dyn Store>,
    latest: Mutex<Option<BotReading>>,
    saved: Mutex<Option<Saved>>,
    seeded: Mutex<bool>,
    version: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct Saved {
    at: DateTime<Utc>,
    pct: f64,
    resets_at: DateTime<Utc>,
}

impl BotTracker {
    pub fn new(env: Env, store: Box<dyn Store>) -> Self {
        BotTracker {
            env,
            store,
            latest: Mutex::new(None),
            saved: Mutex::new(None),
            seeded: Mutex::new(false),
            version: AtomicU64::new(0),
        }
    }

    /// Bumps whenever the newest reading changes (lets the window drop its cached state).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// The stored history as readings (the file is read afresh; for the Stats view).
    pub fn readings(&self) -> Vec<crate::reading::Reading> {
        let mut out = Vec::new();
        self.store
            .read_each(&mut |v| out.push(crate::reading::Reading::from_value(&v)));
        out
    }

    pub fn latest(&self) -> Option<BotReading> {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the file now. `now` stamps the history record.
    pub fn refresh(&self, now: DateTime<Utc>) {
        let new = read(&self.env);
        {
            let mut l = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            if *l != new {
                *l = new;
                self.version.fetch_add(1, Ordering::SeqCst);
            }
        }
        if let Some(r) = new {
            self.save(&r, now);
        }
    }

    fn save(&self, r: &BotReading, now: DateTime<Utc>) {
        {
            // once: remember what the history already ends with, so a restart does not repeat it
            let mut seeded = self.seeded.lock().unwrap_or_else(|e| e.into_inner());
            if !*seeded {
                *seeded = true;
                let mut last = None;
                self.store.read_each(&mut |v| {
                    if let Some(s) = saved_of(&v) {
                        last = Some(s);
                    }
                });
                *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = last;
            }
        }
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        let due = match *saved {
            None => true,
            Some(s) => {
                (s.pct - r.pct).abs() > 1e-9
                    || s.resets_at != r.resets_at
                    || now - s.at >= Duration::minutes(SAVE_EVERY_MIN)
            }
        };
        if !due {
            return;
        }
        if self.store.append(&record(r, now)).is_ok() {
            *saved = Some(Saved {
                at: now,
                pct: r.pct,
                resets_at: r.resets_at,
            });
        }
    }
}

/// The stored form of one reading. No account identifier, no file name.
pub fn record(r: &BotReading, now: DateTime<Utc>) -> Value {
    let mut v = json!({
        "v": 1,
        "ts": iso(now),
        "ok": true,
        "vendor": "grok",
        "source": SOURCE,
        "bot_weekly": {
            "pct": r.pct,
            "resets_at": iso(r.resets_at),
            "read_at": iso(r.read_at),
        },
    });
    if let (Some(e), Some(b)) = (r.expires_at, v.get_mut("bot_weekly")) {
        b["expires_at"] = json!(iso(e));
    }
    v
}

fn saved_of(v: &Value) -> Option<Saved> {
    let b = v.get("bot_weekly")?;
    Some(Saved {
        at: crate::util::parse_ts(v.get("ts")?.as_str()?)?,
        pct: b.get("pct")?.as_f64()?,
        resets_at: crate::util::parse_ts(b.get("resets_at")?.as_str()?)?,
    })
}
