// SPDX-License-Identifier: GPL-3.0-or-later
//! The timed collector: polls immediately on start, then on its own thread:
//!
//! * every `interval_min` a full poll (all sources; the network source only when no local source
//!   has a current reading),
//! * and, when a local poll function is given, every `local_interval_sec` a cheap local-only poll
//!   (the desktop cache / history / `~/.claude.json` files), so the numbers follow the Claude app live.
//!
//! * `refresh()` polls now; `reschedule()` just re-evaluates the timer (used when the interval
//!   changes) and does not poll by itself.
//! * After a failure the next poll waits at least as long as the server's `Retry-After`.
//! * After loading the history, and then once a day, the history files are pruned (see
//!   [`Store::prune`]); `prune_now()` asks for a run at once. Pruning never runs on the GUI thread.

use crate::live::{CollectError, Poller};
use crate::settings::Settings;
use crate::store::{PruneReport, Store};
use crate::util::parse_ts;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type PollFn = Box<dyn Fn(&dyn Store) -> Result<Value, CollectError> + Send + Sync>;
type Waker = Box<dyn Fn() + Send + Sync>;
/// Prunes one more history file (beyond the collector's own store), e.g. the Grok Bot's.
pub type Pruner = Box<dyn Fn(DateTime<Utc>) -> std::io::Result<PruneReport> + Send + Sync>;
/// What one prune run did, by file: the report, or why it failed.
pub type PruneResult = (String, Result<PruneReport, String>);
/// Told what each prune run did, and whether it was asked for.
type PruneDone = Box<dyn Fn(&[PruneResult], bool) + Send + Sync>;

/// Automatic prunes run at most this often.
pub const PRUNE_EVERY_HOURS: i64 = 24;

/// Is an automatic prune due? Daily; a clock that went backwards counts as due.
pub fn prune_due(last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last.is_none_or(|t| now < t || now - t >= chrono::Duration::hours(PRUNE_EVERY_HOURS))
}

/// What the last successful poll saw: which source, how old its sample is, and whether it was new.
/// A poll that finds nothing newer than what is stored (the Claude app is closed) still succeeds;
/// this is how the status line can say "Last sample 47 min ago (app not running?)".
#[derive(Debug, Clone, PartialEq)]
pub struct SampleInfo {
    pub source: String,
    pub observed_at: Option<DateTime<Utc>>,
    /// `false` when the sample was already stored (nothing new this time).
    pub stored: bool,
}

impl SampleInfo {
    /// Read it off the record a poll returns (`source`, `observed_at`, optional `stored: false`).
    pub fn from_record(v: &Value) -> Option<SampleInfo> {
        Some(SampleInfo {
            source: v.get("source")?.as_str()?.to_owned(),
            observed_at: v
                .get("observed_at")
                .and_then(Value::as_str)
                .and_then(parse_ts),
            stored: v.get("stored").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// A snapshot of what the collector is doing, for the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub error: Option<String>,
    pub polling: bool,
    /// Seconds until the next scheduled poll (`None` before the first poll has finished).
    pub next_in: Option<f64>,
    /// Number of finished polls.
    pub polls: u64,
    /// The reading the last successful poll saw.
    pub sample: Option<SampleInfo>,
    /// Local sources are re-read on their own short timer (the numbers follow the Claude app live).
    pub live: bool,
}

#[derive(Default)]
struct State {
    poll_now: bool,
    wake: bool,
    stopped: bool,
    /// Last full poll.
    last: Option<Instant>,
    /// Last poll of any kind (full or local).
    last_tick: Option<Instant>,
    backoff: f64,
    /// Last prune run, by the wall clock (a monotonic clock stops while the PC sleeps, and the
    /// window may stay open for months). `None` until the first, which follows loading.
    last_prune: Option<DateTime<Utc>>,
    prune_now: bool,
    last_error: Option<String>,
    polling: bool,
    polls: u64,
    sample: Option<SampleInfo>,
}

struct Shared {
    store: Arc<dyn Store>,
    settings: Arc<Settings>,
    poll_fn: PollFn,
    local_fn: Option<PollFn>,
    after_poll: Mutex<Option<Waker>>,
    pruners: Mutex<Vec<(String, Pruner)>>,
    /// Daily automatic prunes (the app turns them on; tests that run a collector over fixture
    /// files with fixed dates leave them off, so the real clock never deletes their data).
    auto_prune: std::sync::atomic::AtomicBool,
    on_prune: Mutex<Option<PruneDone>>,
    local_override: Mutex<Option<Duration>>,
    state: Mutex<State>,
    cv: Condvar,
    waker: Mutex<Option<Waker>>,
}

pub struct Collector {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Collector {
    pub fn new(store: Arc<dyn Store>, settings: Arc<Settings>, poll_fn: PollFn) -> Self {
        Self::with_local(store, settings, poll_fn, None)
    }

    /// `poll_fn` is the full poll; `local_fn` (if any) the cheap local-only poll run between them.
    pub fn with_local(
        store: Arc<dyn Store>,
        settings: Arc<Settings>,
        poll_fn: PollFn,
        local_fn: Option<PollFn>,
    ) -> Self {
        let state = State {
            poll_now: true,
            ..State::default()
        };
        Self {
            shared: Arc::new(Shared {
                store,
                settings,
                poll_fn,
                local_fn,
                after_poll: Mutex::new(None),
                pruners: Mutex::new(Vec::new()),
                auto_prune: std::sync::atomic::AtomicBool::new(false),
                on_prune: Mutex::new(None),
                local_override: Mutex::new(None),
                state: Mutex::new(state),
                cv: Condvar::new(),
                waker: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Collector that polls the real endpoint through `poller`.
    pub fn with_poller(store: Arc<dyn Store>, settings: Arc<Settings>, poller: Poller) -> Self {
        let poller = Arc::new(poller);
        let p2 = poller.clone();
        Self::with_local(
            store,
            settings,
            Box::new(move |s| poller.poll_once(s)),
            Some(Box::new(move |s| p2.poll_local(s))),
        )
    }

    /// Use this local interval instead of the setting (tests only: the setting is clamped to >= 15 s).
    pub fn set_local_interval_override(&self, every: Option<Duration>) {
        *lock(&self.shared.local_override) = every;
        self.reschedule();
    }

    /// Called on the collector thread after every successful poll (for alerts and the like).
    pub fn set_after_poll(&self, f: impl Fn() + Send + Sync + 'static) {
        *lock(&self.shared.after_poll) = Some(Box::new(f));
    }

    /// Prune another history file together with this collector's own store (same thread, same
    /// schedule). `name` labels it in the report.
    pub fn add_pruner(
        &self,
        name: impl Into<String>,
        f: impl Fn(DateTime<Utc>) -> std::io::Result<PruneReport> + Send + Sync + 'static,
    ) {
        lock(&self.shared.pruners).push((name.into(), Box::new(f)));
    }

    /// Called on the collector thread after each prune run with what it did, and whether the run
    /// was asked for with [`prune_now`](Self::prune_now).
    pub fn set_on_prune(&self, f: impl Fn(&[PruneResult], bool) + Send + Sync + 'static) {
        *lock(&self.shared.on_prune) = Some(Box::new(f));
    }

    /// Turn the daily automatic prune on or off (off by default; the app turns it on).
    pub fn set_auto_prune(&self, on: bool) {
        self.shared
            .auto_prune
            .store(on, std::sync::atomic::Ordering::Relaxed);
        self.reschedule();
    }

    /// Prune the history now (on the collector thread, as soon as it is free).
    pub fn prune_now(&self) {
        let mut st = lock(&self.shared.state);
        st.prune_now = true;
        st.wake = true;
        self.shared.cv.notify_all();
    }

    /// Run one prune on the calling thread with an explicit `now` (tests, `--prune`-style callers).
    pub fn prune_blocking(&self, now: DateTime<Utc>) -> Vec<PruneResult> {
        self.shared.prune(now)
    }

    /// Called (from the collector thread) whenever the status changes, e.g. to repaint the GUI.
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *lock(&self.shared.waker) = Some(Box::new(f));
    }

    pub fn start(&self) {
        let mut t = lock(&self.thread);
        if t.is_some() {
            return;
        }
        let shared = self.shared.clone();
        *t = Some(
            std::thread::Builder::new()
                .name("aitrack-collector".into())
                .spawn(move || shared.run())
                .expect("spawn collector thread"),
        );
    }

    /// Poll as soon as possible.
    pub fn refresh(&self) {
        let mut st = lock(&self.shared.state);
        st.poll_now = true;
        st.wake = true;
        self.shared.cv.notify_all();
    }

    /// Re-evaluate the timer (the interval setting changed). Does not force a poll.
    pub fn reschedule(&self) {
        lock(&self.shared.state).wake = true;
        self.shared.cv.notify_all();
    }

    pub fn stop(&self) {
        let mut st = lock(&self.shared.state);
        st.stopped = true;
        st.wake = true;
        self.shared.cv.notify_all();
    }

    pub fn join(&self) {
        if let Some(h) = lock(&self.thread).take() {
            let _ = h.join();
        }
    }

    /// Run one poll on the calling thread (used by tests and `--once`-style callers).
    pub fn poll_blocking(&self) {
        self.shared.poll();
    }

    pub fn seconds_until_next(&self) -> f64 {
        self.shared.seconds_until_next(&lock(&self.shared.state))
    }

    pub fn status(&self) -> Status {
        let st = lock(&self.shared.state);
        Status {
            error: st.last_error.clone(),
            polling: st.polling,
            next_in: st.last.map(|_| self.shared.seconds_until_next(&st)),
            polls: st.polls,
            sample: st.sample.clone(),
            live: self.shared.local_fn.is_some(),
        }
    }
}

impl Shared {
    fn next_at(&self, st: &State) -> Option<Instant> {
        let last = st.last?;
        let secs = (self.settings.get().interval_min * 60.0).max(st.backoff);
        // (an impossible value would panic in from_secs_f64, and a panic aborts the window)
        Some(last + Duration::try_from_secs_f64(secs).unwrap_or(Duration::from_secs(86_400)))
    }

    /// When the next local-only poll is due (`None` without a local poll function).
    fn next_local_at(&self, st: &State) -> Option<Instant> {
        self.local_fn.as_ref()?;
        let last = st.last_tick?;
        let every = (*lock(&self.local_override)).unwrap_or_else(|| {
            Duration::try_from_secs_f64(self.settings.get().local_interval_sec)
                .unwrap_or(Duration::from_secs(60))
        });
        Some(last + every)
    }

    fn seconds_until_next(&self, st: &State) -> f64 {
        match self.next_at(st) {
            Some(at) => at.saturating_duration_since(Instant::now()).as_secs_f64(),
            None => 0.0,
        }
    }

    fn wake_ui(&self) {
        if let Some(w) = lock(&self.waker).as_ref() {
            w();
        }
    }

    fn after_poll(&self) {
        if let Some(f) = lock(&self.after_poll).as_ref() {
            f();
        }
    }

    fn poll(&self) {
        lock(&self.state).polling = true;
        self.wake_ui();
        let result = (self.poll_fn)(self.store.as_ref());
        let ok = result.is_ok();
        {
            let mut st = lock(&self.state);
            match result {
                Ok(rec) => {
                    st.sample = SampleInfo::from_record(&rec);
                    st.last_error = None;
                    st.backoff = 0.0;
                }
                Err(e) => {
                    // whatever a CollectError carries, the wait stays finite and at most a day
                    st.backoff = e.retry_after.filter(|r| r.is_finite()).map_or(0.0, |r| {
                        r.clamp(0.0, crate::live::MAX_RETRY_AFTER_SECS as f64)
                    });
                    st.last_error = Some(e.message);
                }
            }
            let now = Instant::now();
            st.last = Some(now);
            st.last_tick = Some(now);
            st.polling = false;
            st.polls += 1;
        }
        if ok {
            self.after_poll();
        }
        self.wake_ui();
    }

    /// Cheap local-only poll. It is silent: no spinner, and a failure changes nothing (the last
    /// full poll already said why there is no data, or a recent reading is still current).
    fn poll_local(&self) {
        let Some(f) = &self.local_fn else { return };
        let result = f(self.store.as_ref());
        let ok = {
            let mut st = lock(&self.state);
            st.last_tick = Some(Instant::now());
            match result {
                Ok(rec) => {
                    st.sample = SampleInfo::from_record(&rec);
                    st.last_error = None;
                    st.polls += 1;
                    true
                }
                Err(_) => false,
            }
        };
        if ok {
            self.after_poll();
            self.wake_ui();
        }
    }

    fn prune(&self, now: DateTime<Utc>) -> Vec<PruneResult> {
        let mut out = vec![(
            self.store.describe(),
            self.store.prune(now).map_err(|e| e.to_string()),
        )];
        for (name, f) in lock(&self.pruners).iter() {
            out.push((name.clone(), f(now).map_err(|e| e.to_string())));
        }
        out
    }

    /// Prune when due (after loading, then daily) or asked for.
    fn prune_if_due(&self) {
        let now = Utc::now();
        let manual = {
            let mut st = lock(&self.state);
            let auto = self.auto_prune.load(std::sync::atomic::Ordering::Relaxed)
                && prune_due(st.last_prune, now);
            if !auto && !st.prune_now {
                return;
            }
            let manual = st.prune_now;
            st.prune_now = false;
            st.last_prune = Some(now);
            manual
        };
        let reports = self.prune(now);
        if let Some(f) = lock(&self.on_prune).as_ref() {
            f(&reports, manual);
        }
        if manual
            || reports
                .iter()
                .any(|(_, r)| r.as_ref().map_or(true, PruneReport::changed))
        {
            self.wake_ui();
        }
    }

    fn run(&self) {
        self.store.prepare();
        self.wake_ui();
        loop {
            // 0 = nothing due, 1 = full poll, 2 = local poll
            let action = {
                let mut st = lock(&self.state);
                if st.stopped {
                    return;
                }
                let now = Instant::now();
                let full = st.poll_now || self.next_at(&st).is_none_or(|at| now >= at);
                let local = self.next_local_at(&st).is_some_and(|at| now >= at);
                st.poll_now = false;
                if full {
                    1
                } else if local {
                    2
                } else {
                    0
                }
            };
            match action {
                1 => self.poll(),
                2 => self.poll_local(),
                _ => {}
            }
            // after the first poll, so start-up shows numbers first
            self.prune_if_due();
            let st = lock(&self.state);
            if st.stopped {
                return;
            }
            let now = Instant::now();
            let until =
                |at: Option<Instant>| at.map(|a| a.saturating_duration_since(now).as_secs_f64());
            let wait = [until(self.next_at(&st)), until(self.next_local_at(&st))]
                .into_iter()
                .flatten()
                .fold(30.0, f64::min)
                .clamp(0.2, 30.0);
            let (mut st, _) = self
                .cv
                .wait_timeout_while(st, Duration::from_secs_f64(wait), |s| {
                    !s.wake && !s.poll_now && !s.stopped && !s.prune_now
                })
                .unwrap_or_else(|e| e.into_inner());
            st.wake = false;
        }
    }
}
