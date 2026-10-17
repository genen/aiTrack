// SPDX-License-Identifier: GPL-3.0-or-later
//! Append-only storage for plan-usage snapshots.
//!
//! The [`Store`] trait is deliberately tiny (append + read) so the flat JSONL file can be swapped
//! for SQLite or another database later without touching the collector or the analysis code.

use crate::reading::Reading;
use crate::util::{data_dir, expand_user};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
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
}

/// One JSON object per line. Never rewritten, only appended to.
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
}
