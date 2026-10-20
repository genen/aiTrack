// SPDX-License-Identifier: GPL-3.0-or-later
//! The Grok Bot app's second weekly meter, "Weekly Grok Bot Limit" (the first, the SuperGrok pool,
//! comes from the Grok CLI billing log, see [`crate::grok`]).
//!
//! **The file layout below was reported by another agent reading the owner's Mac; aiTrack has
//! verified it only by matching the values on the app's Usage page (59.17 % and the reset time).**
//! Nothing here has been compiled or run on macOS by us. Every step is therefore defensive: a
//! surprise in the layout hides the bar, it never panics (release builds abort on panic).
//!
//! * Linux: `${XDG_CONFIG_HOME:-~/.config}/Grok Bot/sand-client-persistence/` (Electron's
//!   `userData` rule, same file kind assumed; **a guess until a Linux listing confirms it**).
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
//! Readings are appended (when the app has written a new reading, stamped with the app's own read time, and never for a week that has ended) to
//! `usage-grok-bot.jsonl` next to the other history files, as records with the field `bot_weekly`.
//! That file is a separate append-only history; the window shows the live file, and the Grok Bot
//! row's detail pane charts this history.

use crate::sources::{Env, Os};
use crate::store::Store;
use crate::util::iso;
use chrono::{DateTime, TimeZone, Utc};
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
        Os::Linux => linux_config_base(env)?,
    };
    Some(base.join(APP_NAME).join("sand-client-persistence"))
}

/// The Electron `userData` folder name (the app's product name).
pub const APP_NAME: &str = "Grok Bot";

/// Linux: `$XDG_CONFIG_HOME`, else `~/.config` (where Electron puts `userData`).
fn linux_config_base(env: &Env) -> Option<PathBuf> {
    env.xdg_config_home
        .clone()
        .or_else(|| Some(env.home.clone()?.join(".config")))
}

/// The Grok Bot app's `userData` folder on Linux (`<config>/Grok Bot`); `None` elsewhere or when
/// there is no home.
pub fn linux_user_data_dir(env: &Env) -> Option<PathBuf> {
    (env.os == Os::Linux)
        .then(|| linux_config_base(env))
        .flatten()
        .map(|b| b.join(APP_NAME))
}

/// Linux only: is the Grok Bot app's data folder there (or the override folder)? This alone is
/// enough to show the Grok block on Linux, where the Grok CLI is often not installed. Windows and
/// macOS keep detecting the CLI's folder only.
pub fn linux_app_present(env: &Env) -> bool {
    env.os == Os::Linux
        && (linux_user_data_dir(env).is_some_and(|d| d.is_dir())
            || env.grok_bot_dir.as_ref().is_some_and(|d| d.is_dir()))
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
        None => "not available (no home folder to look in)".to_string(),
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
    if env.os == Os::Linux {
        // names and counts only: no file names are listed, nothing is opened
        let ud = linux_user_data_dir(env);
        let pd = persistence_dir(env);
        let ud_text = if ud.as_ref().is_some_and(|d| d.is_dir()) {
            "exists"
        } else {
            "missing"
        };
        let pd_text = match pd.as_ref().and_then(|d| std::fs::read_dir(d).ok()) {
            Some(rd) => format!(
                "exists, {} .blob file(s)",
                rd.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".blob"))
                    .count()
            ),
            None => "missing".to_string(),
        };
        let base = if env.xdg_config_home.is_some() {
            "$XDG_CONFIG_HOME"
        } else {
            "~/.config"
        };
        o.push_str(&format!(
            "   checked: {base}/{APP_NAME}: {ud_text}; {base}/{APP_NAME}/sand-client-persistence: {pd_text}\n"
        ));
    }
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
    /// The parsed history, read once and dropped when this tracker appends a record, so the
    /// window thread does not re-read the growing file on every rebuild.
    cached: Mutex<Option<std::sync::Arc<Vec<crate::reading::Reading>>>>,
}

#[derive(Debug, Clone, Copy)]
struct Saved {
    read_at: DateTime<Utc>,
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
            cached: Mutex::new(None),
        }
    }

    /// Bumps whenever the newest reading changes (lets the window drop its cached state).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Prune the history file (see [`Store::prune`]). Runs on the Grok collector thread, the same
    /// thread that appends, so the two never interleave.
    pub fn prune(&self, now: DateTime<Utc>) -> std::io::Result<crate::store::PruneReport> {
        let rep = self.store.prune(now)?;
        if rep.changed() {
            *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.version.fetch_add(1, Ordering::SeqCst);
        }
        Ok(rep)
    }

    /// The history file: where it is and its size.
    pub fn store_info(&self) -> (String, Option<u64>) {
        (self.store.describe(), self.store.size())
    }

    /// The stored history as readings (for the Stats view and charts). The file is read once and
    /// the result kept until this tracker appends a record (it is the only writer).
    pub fn readings(&self) -> std::sync::Arc<Vec<crate::reading::Reading>> {
        let mut c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = c.as_ref() {
            return r.clone();
        }
        let mut out = Vec::new();
        self.store
            .read_each(&mut |v| out.push(crate::reading::Reading::from_value(&v)));
        let r = std::sync::Arc::new(out);
        *c = Some(r.clone());
        r
    }

    /// Linux with the Grok Bot app but no Grok CLI folder: the CLI collector has nothing to read.
    pub fn cli_absent(&self) -> bool {
        !crate::grok::grok_home(&self.env).is_dir() && linux_app_present(&self.env)
    }

    pub fn latest(&self) -> Option<BotReading> {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the file now. `now` stamps the history record.
    pub fn refresh(&self, now: DateTime<Utc>) {
        let new = read(&self.env);
        let changed = {
            let mut l = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            let changed = *l != new;
            *l = new;
            changed
        };
        if let Some(r) = new {
            self.save(&r, now);
        }
        // after the save, so a window that rebuilds on the new version sees the new history
        if changed {
            self.version.fetch_add(1, Ordering::SeqCst);
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
                (s.pct - r.pct).abs() > 1e-9 || s.resets_at != r.resets_at || s.read_at != r.read_at
            }
        };
        // The cache only changes while the app is open: a value that has not been re-read is not
        // appended again, and a value from a finished week is not drawn into the new one.
        if !due || r.period_ended(now) {
            return;
        }
        if self.store.append(&record(r)).is_ok() {
            *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *saved = Some(Saved {
                read_at: r.read_at,
                pct: r.pct,
                resets_at: r.resets_at,
            });
        }
    }
}

/// The stored form of one reading. No account identifier, no file name.
pub fn record(r: &BotReading) -> Value {
    let mut v = json!({
        "v": 1,
        "ts": iso(r.read_at),
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
    let ts = |x: Option<&Value>| crate::util::parse_ts(x?.as_str()?);
    Some(Saved {
        read_at: ts(b.get("read_at")).or_else(|| ts(v.get("ts")))?,
        pct: b.get("pct")?.as_f64()?,
        resets_at: crate::util::parse_ts(b.get("resets_at")?.as_str()?)?,
    })
}
