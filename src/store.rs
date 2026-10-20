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
    fn prune(&self, _now: DateTime<Utc>) -> io::Result<PruneReport> {
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
}

impl PruneReport {
    /// Did the run change the file?
    pub fn changed(&self) -> bool {
        self.removed > 0 || self.corrupt > 0
    }

    /// One line for the log and the Settings window: `removed 120 records older than 90 days, ...`.
    pub fn describe(&self) -> String {
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
    let at = ["observed_at", "ts"]
        .iter()
        .find_map(|k| record.get(*k).and_then(Value::as_str).and_then(parse_ts));
    let Some(at) = at else {
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
        let cutoff = prune_cutoff(now);
        let data = match std::fs::read(&self.path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PruneReport::default()),
            Err(e) => return Err(e),
        };
        let mut rep = PruneReport {
            bytes_before: data.len() as u64,
            ..PruneReport::default()
        };
        let mut kept: Vec<u8> = Vec::with_capacity(data.len());
        let mut corrupt: Vec<u8> = Vec::new();
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

    fn prune(&self, now: DateTime<Utc>) -> io::Result<PruneReport> {
        self.prune_with(now, &mut |_| true)
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
    fn prune(&self, now: DateTime<Utc>) -> io::Result<PruneReport> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let rep = self.inner.prune(now)?;
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
