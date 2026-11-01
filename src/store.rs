// SPDX-License-Identifier: GPL-3.0-or-later
//! Append-only storage for plan-usage snapshots.
//!
//! The [`Store`] trait is deliberately tiny (append + read + prune) so the flat JSONL file can be
//! swapped for SQLite or another database later without touching the collector or the analysis code.
//!
//! Retention: records older than [`KEEP_DAYS`] are deleted by [`Store::prune`], which the collector
//! runs after loading and then once a day (see `collector`).

use crate::reading::Reading;
use crate::util::{data_dir, expand_user, parse_ts};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// `$AITRACK_DATA` or `~/.aitrack/usage.jsonl`.
pub fn default_data_path() -> PathBuf {
    match std::env::var("AITRACK_DATA") {
        Ok(p) if !p.is_empty() => expand_user(&p),
        _ => data_dir().join("usage.jsonl"),
    }
}

pub trait Store: Send + Sync {
    fn append(&self, record: &Value) -> io::Result<()>;
    /// Visit every stored record in order. Unreadable records are skipped, not fatal.
    fn read_each(&self, f: &mut dyn FnMut(Value));
    fn read(&self) -> Vec<Value> {
        let mut out = Vec::new();
        self.read_each(&mut |v| out.push(v));
        out
    }
    /// Human-readable location, for logs.
    fn describe(&self) -> String;
    /// Called once on the collector thread before the first poll (e.g. to warm a cache).
    fn prepare(&self) {}
    /// Delete the records that fall out of retention (see [`prune_cutoff`]). Stores that keep
    /// nothing on disk do nothing.
    fn prune(&self, now: DateTime<Utc>) -> io::Result<PruneReport> {
        self.prune_limited(now, PruneRule::Free)
    }
    /// [`prune`](Self::prune) under a [`PruneRule`] of the clock guard ([`PruneGuard`]): a held
    /// run changes nothing and says so (see [`PruneReport::held`]).
    fn prune_limited(&self, _now: DateTime<Utc>, _rule: PruneRule) -> io::Result<PruneReport> {
        Ok(PruneReport::default())
    }
    /// Size of the backing file in bytes, when there is one.
    fn size(&self) -> Option<u64> {
        None
    }
}

/// Usage history is kept for this many days; older records are deleted.
pub const KEEP_DAYS: i64 = 90;

/// Records observed before this time are pruned, unless their billing window is still recent.
pub fn prune_cutoff(now: DateTime<Utc>) -> DateTime<Utc> {
    now - Duration::days(KEEP_DAYS)
}

/// How the clock guard lets a prune run (see [`PruneGuard`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneRule {
    /// Prune whatever is due at `now`.
    Free,
    /// Hold the run (change nothing) when it would delete more than [`GUARD_DAYS`] days at once:
    /// when the cutoff moved more than that past `last_cutoff` (the cutoff of this file's last
    /// prune), or, with none saved, when the records to delete span more than that.
    Guarded { last_cutoff: Option<DateTime<Utc>> },
    /// The run the user was warned about: prune, but never past this cutoff (the one the held run
    /// would have used), so a clock that went further wrong cannot delete more.
    Capped(DateTime<Utc>),
}

/// Would deleting the records observed at `times` (before `cutoff`) delete more than
/// [`GUARD_DAYS`] days at once? Returns how many days, when it would.
///
/// With the cutoff of the last prune saved this is calendar time: how far the cutoff moved, which
/// also catches files with readings only on the days a tool was used. Without one (the first
/// prune of a file) it is the span from the oldest to the newest record to delete, leaving out the
/// single oldest one (a lone stray time cannot set it off), or the number of distinct days.
pub fn held_days(
    cutoff: DateTime<Utc>,
    last_cutoff: Option<DateTime<Utc>>,
    times: &[DateTime<Utc>],
) -> Option<i64> {
    if times.is_empty() {
        return None; // nothing to delete, nothing to protect
    }
    let limit = GUARD_DAYS as i64;
    let days = match last_cutoff {
        Some(last) => (cutoff - last).num_days(),
        None => {
            let mut sorted = times.to_vec();
            sorted.sort();
            let span = (sorted[sorted.len() - 1] - sorted[1.min(sorted.len() - 1)]).num_days();
            let distinct = sorted
                .iter()
                .map(|t| t.date_naive())
                .collect::<std::collections::BTreeSet<_>>()
                .len() as i64;
            span.max(distinct)
        }
    };
    (days > limit).then_some(days)
}

/// What one prune run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Records left in the file.
    pub kept: usize,
    /// Records deleted (older than the cutoff).
    pub removed: usize,
    /// Unreadable lines moved to the `.corrupt` file next to the history.
    pub corrupt: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Non-zero when the run was held by the clock guard: it would have deleted this many days of
    /// history at once (see [`held_days`]), so nothing was changed (not even the `.bak`).
    pub held_days: i64,
    /// Records the held run would have deleted.
    pub held_records: usize,
    /// The cutoff the run used, or the held run would have used (`None`: no file to prune).
    pub cutoff: Option<DateTime<Utc>>,
}

impl PruneReport {
    /// Did the run change the file?
    pub fn changed(&self) -> bool {
        self.removed > 0 || self.corrupt > 0
    }

    /// Was the run held by the clock guard (nothing changed)?
    pub fn held(&self) -> bool {
        self.held_days > 0
    }

    /// One line for the log and the Settings window: `removed 120 records older than 90 days, ...`.
    pub fn describe(&self) -> String {
        if self.held() {
            return format!(
                "held: would delete {} record{} covering {} days of history at once; {CLOCK_WARNING}",
                self.held_records,
                if self.held_records == 1 { "" } else { "s" },
                self.held_days
            );
        }
        if !self.changed() {
            return format!("nothing older than {KEEP_DAYS} days ({} kept)", self.kept);
        }
        let mut s = format!(
            "removed {} record{} older than {KEEP_DAYS} days, {} kept",
            self.removed,
            if self.removed == 1 { "" } else { "s" },
            self.kept
        );
        if self.corrupt > 0 {
            s.push_str(&format!(
                ", {} unreadable line(s) moved aside",
                self.corrupt
            ));
        }
        s.push_str(&format!(
            " ({} -> {})",
            human_size(self.bytes_before),
            human_size(self.bytes_after)
        ));
        s
    }
}

/// `512 B`, `12.3 KB`, `4.5 MB`.
pub fn human_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Does `record` fall out of retention? Only when it is datable, was observed before `cutoff`, and
/// every billing window it reports ended before `cutoff` too, so a window is never cut in half and
/// the current and previous windows (whatever their length) keep all their readings.
pub fn expired(record: &Value, cutoff: DateTime<Utc>) -> bool {
    let Some(at) = record_time(record) else {
        return false; // never delete what cannot be dated
    };
    if at >= cutoff {
        return false;
    }
    !["session", "weekly", "bot_weekly"].iter().any(|k| {
        record
            .get(*k)
            .and_then(|b| b.get("resets_at"))
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .is_some_and(|r| r >= cutoff)
    })
}

/// When a record was observed (`observed_at`, else `ts`).
fn record_time(record: &Value) -> Option<DateTime<Utc>> {
    ["observed_at", "ts"]
        .iter()
        .find_map(|k| record.get(*k).and_then(Value::as_str).and_then(parse_ts))
}

/// A step of [`JsonlStore::prune_with`], for tests that simulate a crash part-way through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneStep {
    /// Unreadable lines were appended to the `.corrupt` file.
    CorruptSaved,
    /// The new contents are in the `.tmp` file (synced); the original is untouched.
    TempWritten,
    /// The original was copied to `.bak`.
    BackupWritten,
    /// The `.tmp` file replaced the original.
    Replaced,
}

/// One JSON object per line. Appended to, and rewritten only by [`Store::prune`].
pub struct JsonlStore {
    pub path: PathBuf,
}

impl JsonlStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn default_path() -> Self {
        Self::new(default_data_path())
    }

    /// `usage.jsonl` -> `usage.jsonl.<ext>` in the same directory.
    pub fn sibling(&self, ext: &str) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".");
        name.push(ext);
        self.path.with_file_name(name)
    }

    /// [`Store::prune`] with a hook called after each step; when the hook returns `false` the run
    /// stops there, as if the process had died (tests use this to check every crash point leaves a
    /// valid file).
    ///
    /// The rewrite is crash-safe: unreadable lines are saved to `.corrupt` first, the kept lines go
    /// to `.tmp` (flushed and synced), the original is copied to `.bak` (replacing the previous run's
    /// backup only now), and only then is `.tmp` renamed over the original in one step. Records
    /// appended by anyone while the rewrite ran are carried over before the rename.
    pub fn prune_with(
        &self,
        now: DateTime<Utc>,
        hook: &mut dyn FnMut(PruneStep) -> bool,
    ) -> io::Result<PruneReport> {
        self.prune_limited_with(now, PruneRule::Free, hook)
    }

    /// [`prune_with`](Self::prune_with) under the clock guard's limit (see
    /// [`Store::prune_limited`]). A held run returns before anything is written.
    pub fn prune_limited_with(
        &self,
        now: DateTime<Utc>,
        rule: PruneRule,
        hook: &mut dyn FnMut(PruneStep) -> bool,
    ) -> io::Result<PruneReport> {
        let cutoff = match rule {
            PruneRule::Capped(cap) => prune_cutoff(now).min(cap),
            _ => prune_cutoff(now),
        };
        let data = match std::fs::read(&self.path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PruneReport::default()),
            Err(e) => return Err(e),
        };
        let mut rep = PruneReport {
            bytes_before: data.len() as u64,
            cutoff: Some(cutoff),
            ..PruneReport::default()
        };
        let mut kept: Vec<u8> = Vec::with_capacity(data.len());
        let mut corrupt: Vec<u8> = Vec::new();
        // when the expired records were observed
        let mut times = Vec::new();
        for line in data.split_inclusive(|b| *b == b'\n') {
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            if body.iter().all(u8::is_ascii_whitespace) {
                continue; // blank lines carry nothing
            }
            match serde_json::from_slice::<Value>(body) {
                Ok(v @ Value::Object(_)) => {
                    if expired(&v, cutoff) {
                        rep.removed += 1;
                        if let Some(at) = record_time(&v) {
                            times.push(at);
                        }
                    } else {
                        rep.kept += 1;
                        // kept lines are copied byte for byte, never re-serialised
                        kept.extend_from_slice(body);
                        kept.push(b'\n');
                    }
                }
                _ => {
                    rep.corrupt += 1;
                    corrupt.extend_from_slice(body);
                    corrupt.push(b'\n');
                }
            }
        }
        let held = match rule {
            PruneRule::Guarded { last_cutoff } => held_days(cutoff, last_cutoff, &times),
            _ => None,
        };
        if let Some(days) = held {
            return Ok(PruneReport {
                kept: rep.kept + rep.removed,
                bytes_after: rep.bytes_before,
                held_days: days,
                held_records: rep.removed,
                removed: 0,
                corrupt: 0,
                ..rep
            });
        }
        if !rep.changed() {
            rep.bytes_after = rep.bytes_before;
            return Ok(rep);
        }
        if !corrupt.is_empty() {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.sibling("corrupt"))?;
            f.write_all(&corrupt)?;
            f.sync_all()?;
        }
        if !hook(PruneStep::CorruptSaved) {
            return Ok(rep);
        }
        let tmp = self.sibling("tmp");
        let result = self.replace_with(&tmp, &kept, data.len() as u64, &mut rep, hook);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|()| rep)
    }

    fn replace_with(
        &self,
        tmp: &Path,
        kept: &[u8],
        mut read_len: u64,
        rep: &mut PruneReport,
        hook: &mut dyn FnMut(PruneStep) -> bool,
    ) -> io::Result<()> {
        let mut out = std::fs::File::create(tmp)?;
        out.write_all(kept)?;
        let mut written = kept.len() as u64;
        // Records appended while this ran (another process, or a caller not holding the lock)
        // are copied over, so the rename cannot lose them.
        written += copy_tail(&self.path, &mut read_len, &mut out)?;
        out.sync_all()?;
        if !hook(PruneStep::TempWritten) {
            return Ok(());
        }
        let bak = self.sibling("bak");
        let bak_tmp = self.sibling("bak.tmp");
        std::fs::copy(&self.path, &bak_tmp)?;
        rename_retrying(&bak_tmp, &bak)?;
        if !hook(PruneStep::BackupWritten) {
            return Ok(());
        }
        // last look for appends; the window between this and the rename is a few microseconds,
        // and the collector holds the history lock anyway
        let extra = copy_tail(&self.path, &mut read_len, &mut out)?;
        if extra > 0 {
            written += extra;
            out.sync_all()?;
        }
        drop(out); // no handle on the temp file during the rename (Windows)
        rename_retrying(tmp, &self.path)?;
        sync_dir(&self.path);
        rep.bytes_after = written;
        hook(PruneStep::Replaced);
        Ok(())
    }
}

/// Append to `out` whatever was added to `path` after its first `read_len` bytes; returns the
/// number of bytes copied and moves `read_len` past them. A partial last line is copied too (the
/// next prune moves it aside if it stays torn).
fn copy_tail(path: &Path, read_len: &mut u64, out: &mut std::fs::File) -> io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len <= *read_len {
        return Ok(0);
    }
    f.seek(SeekFrom::Start(*read_len))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    // the kept part always ends with a newline; a tail written after a torn line may not start
    // with one, which is fine: the torn line was already moved aside or kept whole
    out.write_all(&tail)?;
    *read_len += tail.len() as u64;
    Ok(tail.len() as u64)
}

/// `rename` that replaces `to`, retried briefly: on Windows a virus scanner or the search indexer
/// can hold the target open for a moment ("Access is denied"). The source is untouched on failure.
fn rename_retrying(from: &Path, to: &Path) -> io::Result<()> {
    let mut wait = std::time::Duration::from_millis(50);
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 6 && e.kind() != io::ErrorKind::NotFound => {
                tries += 1;
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Make the rename durable (Unix; on Windows the rename itself is enough).
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl Store for JsonlStore {
    fn append(&self, record: &Value) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut f = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)?;
        let mut line = String::new();
        // A torn last line (crash mid-write) has no newline; start a fresh line so the new
        // record does not get glued onto the damaged one.
        if ends_without_newline(&mut f) {
            line.push('\n');
        }
        line.push_str(&serde_json::to_string(record).map_err(io::Error::other)?);
        line.push('\n');
        f.write_all(line.as_bytes())
    }

    fn read_each(&self, f: &mut dyn FnMut(Value)) {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Ok(v @ Value::Object(_)) = serde_json::from_slice::<Value>(&buf) {
                f(v);
            }
        }
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn prune_limited(&self, now: DateTime<Utc>, rule: PruneRule) -> io::Result<PruneReport> {
        self.prune_limited_with(now, rule, &mut |_| true)
    }

    fn size(&self) -> Option<u64> {
        std::fs::metadata(&self.path).ok().map(|m| m.len())
    }
}

fn ends_without_newline(f: &mut std::fs::File) -> bool {
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return false;
    };
    if len == 0 || f.seek(SeekFrom::Start(len - 1)).is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    f.read_exact(&mut b).is_ok() && b[0] != b'\n'
}

/// In-memory, compact copy of the history in front of another [`Store`].
///
/// Records are reduced to [`Reading`]s (the full `raw` payload stays only on disk), so a year of
/// 15-minute polls costs a few MB. Loading happens on whichever thread calls [`History::load`]
/// (the collector thread), and the GUI uses the non-blocking `try_*` accessors.
pub struct History {
    inner: Box<dyn Store>,
    state: Mutex<HistoryState>,
    version: AtomicU64,
}

#[derive(Default)]
struct HistoryState {
    loaded: bool,
    readings: Vec<Reading>,
}

impl History {
    pub fn new(inner: Box<dyn Store>) -> Self {
        Self {
            inner,
            state: Mutex::new(HistoryState::default()),
            version: AtomicU64::new(0),
        }
    }

    /// Read the backing store into memory (once). Appends made before this are picked up from disk.
    pub fn load(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.loaded {
            return;
        }
        let mut readings = Vec::new();
        self.inner
            .read_each(&mut |v| readings.push(Reading::from_value(&v)));
        st.readings = readings;
        st.loaded = true;
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn is_loaded(&self) -> bool {
        self.state.try_lock().map(|s| s.loaded).unwrap_or(false)
    }

    /// Bumps whenever the history changes; lets callers skip recomputing.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Run `f` over the loaded readings. Returns `None` while loading is in progress or not done.
    pub fn try_with_readings<R>(&self, f: impl FnOnce(&[Reading]) -> R) -> Option<R> {
        let st = self.state.try_lock().ok()?;
        st.loaded.then(|| f(&st.readings))
    }

    /// A copy of the loaded readings, taken under a `try_lock` (so the caller can analyse them
    /// without holding the lock). `None` while loading is in progress or the lock is busy.
    pub fn try_snapshot(&self) -> Option<Vec<Reading>> {
        self.try_with_readings(|r| r.to_vec())
    }

    /// A copy of the readings (loading them first if needed), taken under the lock; the caller
    /// analyses the copy without holding it.
    pub fn snapshot(&self) -> Vec<Reading> {
        self.with_readings(|r| r.to_vec())
    }

    /// Like [`try_with_readings`](Self::try_with_readings) but loads and waits if necessary.
    pub fn with_readings<R>(&self, f: impl FnOnce(&[Reading]) -> R) -> R {
        self.load();
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&st.readings)
    }
}

impl Store for History {
    fn append(&self, record: &Value) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.append(record)?;
        if st.loaded {
            st.readings.push(Reading::from_value(record));
            self.version.fetch_add(1, Ordering::Release);
        }
        Ok(())
    }

    fn read_each(&self, f: &mut dyn FnMut(Value)) {
        self.inner.read_each(f);
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn prepare(&self) {
        self.load();
    }

    /// Prunes under the lock, so no append can interleave, then reloads the readings if anything
    /// was removed.
    fn prune_limited(&self, now: DateTime<Utc>, rule: PruneRule) -> io::Result<PruneReport> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let rep = self.inner.prune_limited(now, rule)?;
        if rep.changed() && st.loaded {
            let mut readings = Vec::new();
            self.inner
                .read_each(&mut |v| readings.push(Reading::from_value(&v)));
            st.readings = readings;
            self.version.fetch_add(1, Ordering::Release);
        }
        Ok(rep)
    }

    fn size(&self) -> Option<u64> {
        self.inner.size()
    }
}

/// A prune that would delete more than this many days of history at once is held by
/// [`PruneGuard`] (a normal daily prune deletes one).
pub const GUARD_DAYS: usize = 30;

/// What the window and the toast say when the guard held a prune.
pub const CLOCK_WARNING: &str = "Check Clock about to delete 30+ days of history on next launch.";

/// The clock guard of the 90-day prune.
///
/// The cutoff is worked out from the system clock, so a clock that jumped months ahead would make
/// one prune delete most of the history (and the next prune would overwrite the only `.bak`). So
/// every file's prune runs under [`PruneRule::Guarded`]: when it would delete more than
/// [`GUARD_DAYS`] days at once (see [`held_days`]) it is held for the rest of this run, the user is
/// warned, and its cutoff is saved. On the next launch (the restart is the user's confirmation)
/// that file's first prune runs under [`PruneRule::Capped`]: worked out again with the clock as it
/// is then, but never past the saved cutoff. A fixed clock deletes only what is really older than
/// 90 days, a clock still ahead deletes what the user was warned about and no more. Then the guard
/// holds that file again.
///
/// Saved per file, next to settings.json: the cutoff of its last prune (how far the next one may
/// move is measured from it) and, while a prune waits for the next launch, the held cutoff.
pub struct PruneGuard {
    path: PathBuf,
    state: Mutex<GuardState>,
    /// Taken around each write, so the newest state is the one that lands on disk; the state lock
    /// is never held during the file I/O (the window reads the state every frame).
    saving: Mutex<()>,
}

#[derive(Default, Clone, PartialEq)]
struct GuardFile {
    last_cutoff: Option<DateTime<Utc>>,
    held_cutoff: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct GuardState {
    files: std::collections::BTreeMap<String, GuardFile>,
    /// Files held on the last launch: their first prune this run is [`PruneRule::Capped`].
    allowed: std::collections::BTreeSet<String>,
    /// Files held in this run (they prune on the next launch).
    held: std::collections::BTreeSet<String>,
    /// The first hold of this run has not been told to the window yet.
    tell: bool,
    /// The last save failed: why (shown in Diagnostics).
    save_error: Option<String>,
}

fn ts_field(v: &Value, k: &str) -> Option<DateTime<Utc>> {
    v.get(k).and_then(Value::as_str).and_then(parse_ts)
}

impl PruneGuard {
    /// Read what the last run saved at `path` (missing or unreadable = nothing saved).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut st = GuardState::default();
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|d| serde_json::from_slice::<Value>(&d).ok());
        if let Some(files) = saved
            .as_ref()
            .and_then(|v| v.get("files"))
            .and_then(Value::as_object)
        {
            for (name, f) in files {
                let entry = GuardFile {
                    last_cutoff: ts_field(f, "last_cutoff"),
                    held_cutoff: ts_field(f, "held_cutoff"),
                };
                if entry.held_cutoff.is_some() {
                    st.allowed.insert(name.clone());
                }
                st.files.insert(name.clone(), entry);
            }
        }
        Self {
            path,
            state: Mutex::new(st),
            saving: Mutex::new(()),
        }
    }

    /// Where the state is kept.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How the next prune of `name` may run.
    pub fn rule_for(&self, name: &str) -> PruneRule {
        let st = self.lock();
        let entry = st.files.get(name).cloned().unwrap_or_default();
        match entry.held_cutoff {
            Some(cap) if st.allowed.contains(name) => PruneRule::Capped(cap),
            _ => PruneRule::Guarded {
                last_cutoff: entry.last_cutoff,
            },
        }
    }

    /// Note what a prune of `name` did and save it: a held run waits for the next launch with its
    /// cutoff; a run that went through clears that and remembers its cutoff (and the guard holds
    /// the file again). A failed run changes nothing (the file is as it was).
    pub fn record(&self, name: &str, result: &io::Result<PruneReport>) {
        let Ok(rep) = result else { return };
        {
            let mut st = self.lock();
            let before = st.files.get(name).cloned();
            if rep.held() {
                if st.held.is_empty() {
                    st.tell = true;
                }
                st.held.insert(name.to_owned());
                let e = st.files.entry(name.to_owned()).or_default();
                // held again in the same run: keep the earlier (smaller) cutoff
                e.held_cutoff = match (e.held_cutoff, rep.cutoff) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            } else {
                st.allowed.remove(name);
                st.held.remove(name);
                let e = st.files.entry(name.to_owned()).or_default();
                e.held_cutoff = None;
                if rep.cutoff.is_some() {
                    e.last_cutoff = rep.cutoff;
                }
            }
            if st.files.get(name) == before.as_ref() {
                return;
            }
        }
        self.save();
    }

    /// Files whose prune waits for the next launch, as saved.
    pub fn pending(&self) -> Vec<String> {
        let st = self.lock();
        st.files
            .iter()
            .filter(|(_, f)| f.held_cutoff.is_some())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Did the guard hold a prune in this run?
    pub fn held_this_run(&self) -> bool {
        !self.lock().held.is_empty()
    }

    /// The Diagnostics text: the clock warning when a prune is held in this run, and why the
    /// guard's state could not be saved, when it could not.
    pub fn warning(&self) -> Option<String> {
        let st = self.lock();
        let lines: Vec<&str> = [
            (!st.held.is_empty()).then_some(CLOCK_WARNING),
            st.save_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// `true` once, after the first hold of this run (the toast and the switch to Diagnostics).
    pub fn take_first_hold(&self) -> bool {
        std::mem::take(&mut self.lock().tell)
    }

    /// Write the state (temporary file, then renamed over the real one).
    fn save(&self) {
        let _saving = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let text = {
            let st = self.lock();
            let iso = |t: Option<DateTime<Utc>>| t.map(|t| t.to_rfc3339());
            let files: serde_json::Map<String, Value> = st
                .files
                .iter()
                .map(|(n, f)| {
                    let mut o = serde_json::Map::new();
                    if let Some(t) = iso(f.last_cutoff) {
                        o.insert("last_cutoff".into(), Value::String(t));
                    }
                    if let Some(t) = iso(f.held_cutoff) {
                        o.insert("held_cutoff".into(), Value::String(t));
                    }
                    (n.clone(), Value::Object(o))
                })
                .collect();
            serde_json::json!({ "files": files }).to_string()
        };
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        let written = std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| rename_retrying(&tmp, &self.path));
        let error = written.err().map(|e| {
            let _ = std::fs::remove_file(&tmp);
            let msg = format!("Could not save {}: {e}", self.path.display());
            crate::alerts::log_line(&msg);
            msg
        });
        self.lock().save_error = error;
    }
}
