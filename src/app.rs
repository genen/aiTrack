// SPDX-License-Identifier: GPL-3.0-or-later
//! Everything the window needs behind one handle: history, settings, collector and a cached
//! [`State`]. The GUI thread only calls cheap methods here; all I/O happens on the collector thread.

use crate::alerts::{AlertKind, Alerts, Notification, Notifier, Raising};
use crate::collector::{Collector, PollFn};
use crate::grok_cli::Progress;
use crate::live::Poller;
use crate::settings::{Direction, Settings};
use crate::startup;
use crate::state::{
    build_codex_state, build_grok_state, build_state, CodexState, GrokState, State,
};
use crate::store::{History, Store};
use chrono::Utc;
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Recompute the analysis at most this often when nothing changed (countdowns drift slowly).
const REFRESH_EVERY: Duration = Duration::from_secs(5);

pub struct App {
    pub history: Arc<History>,
    pub settings: Arc<Settings>,
    pub collector: Collector,
    /// Grok: its own history file and collector (see [`App::with_grok`]).
    pub grok: Option<GrokPart>,
    /// Codex: its own history file and collector (see [`App::with_codex`]).
    pub codex: Option<CodexPart>,
    notifier: Arc<Mutex<Arc<dyn Notifier>>>,
    /// A usage alert fired: the window comes to the front (see [`App::take_raise`]).
    raise: Arc<AtomicBool>,
    cache: Mutex<Option<Cached>>,
    /// Is Claude's tool installed? (stat only; `true` until the window installs the real check)
    claude_detect: Arc<Mutex<Detect>>,
    claude_seen: Remembered,
    /// The Stats view's background service (transcripts + plan statistics), when set up.
    stats: std::sync::OnceLock<Arc<crate::statsview::StatsService>>,
    /// What the last prune asked for in Settings did (shown under the "Prune now" button).
    prune_result: Arc<Mutex<Option<String>>>,
}

/// Log what a prune run did, and keep the summary of an asked-for run for Settings.
fn prune_reporter(
    slot: Arc<Mutex<Option<String>>>,
) -> impl Fn(&[crate::collector::PruneResult], bool) + Send + Sync + 'static {
    move |reports, manual| {
        let mut lines = Vec::new();
        for (name, r) in reports {
            let text = match r {
                Ok(rep) if !rep.changed() && !manual => continue, // daily run, nothing to say
                Ok(rep) => rep.describe(),
                Err(e) => format!("prune failed, file left as it was: {e}"),
            };
            crate::alerts::log_line(&format!("prune {name}: {text}"));
            let file = std::path::Path::new(name)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            lines.push(format!("{file}: {text}"));
        }
        if manual {
            let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
            // the Claude and Grok collectors each report; keep both
            let joined = lines.join("\n");
            *s = Some(match s.take() {
                Some(prev) if !prev.is_empty() && !joined.is_empty() => format!("{prev}\n{joined}"),
                Some(prev) if joined.is_empty() => prev,
                _ => joined,
            });
        }
    }
}

/// Grok's history (a separate `usage-grok.jsonl`) and its collector.
pub struct GrokPart {
    pub history: Arc<History>,
    pub collector: Collector,
    detect: Detect,
    seen: Remembered,
    refresh: GrokRefresh,
    /// The Grok Bot app's second weekly meter (set by [`App::with_grok_bot`]); read on the Grok
    /// collector thread after every poll, never on the window thread.
    bot: Arc<std::sync::OnceLock<Arc<crate::grok_bot::BotTracker>>>,
}

impl GrokPart {
    /// Is the Grok CLI's folder there? (reused for a few seconds)
    fn detected(&self) -> bool {
        self.seen.get(&*self.detect)
    }
}

/// Codex's history (a separate `usage-codex.jsonl`) and its collector.
pub struct CodexPart {
    pub history: Arc<History>,
    pub collector: Collector,
    detect: Detect,
    seen: Remembered,
}

impl CodexPart {
    /// Is Codex's folder there? (reused for a few seconds)
    fn detected(&self) -> bool {
        self.seen.get(&*self.detect)
    }
}

/// How long a refresh failure stays on screen.
const REFRESH_FAIL_SHOWN: Duration = Duration::from_secs(20);
/// How long a "CLI found?" answer is reused.
const CLI_PROBE_EVERY: Duration = Duration::from_secs(30);

/// How long a "is the tool installed?" answer is reused (the check stats files and lists the
/// Store packages folder, which the window must not do every frame).
pub const PRESENCE_TTL: Duration = Duration::from_secs(3);

/// A yes/no answer reused for a few seconds.
struct Remembered {
    ttl: Mutex<Duration>,
    slot: Mutex<Option<(Instant, bool)>>,
}

impl Remembered {
    fn new() -> Self {
        Remembered {
            ttl: Mutex::new(PRESENCE_TTL),
            slot: Mutex::new(None),
        }
    }

    fn get(&self, ask: &dyn Fn() -> bool) -> bool {
        let ttl = *self.ttl.lock().unwrap_or_else(|e| e.into_inner());
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match *slot {
            Some((at, v)) if at.elapsed() < ttl => v,
            _ => {
                let v = ask();
                *slot = Some((Instant::now(), v));
                v
            }
        }
    }

    fn forget(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn set_ttl(&self, ttl: Duration) {
        *self.ttl.lock().unwrap_or_else(|e| e.into_inner()) = ttl;
        self.forget();
    }
}

/// Runs one Grok refresh (start the CLI, wait for billing, end it): `Err` is a one-line reason.
/// `Ok` carries the weekly percent the CLI just wrote, when it could be read.
pub type Refresher = Arc<dyn Fn() -> Result<Option<f64>, String> + Send + Sync>;

/// The outcome of the latest refresh, kept until the next one starts (or the app restarts); the
/// Settings row shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshStatus {
    Never,
    Running,
    Done {
        at: chrono::DateTime<Utc>,
        pct: Option<f64>,
    },
    Failed {
        at: chrono::DateTime<Utc>,
        reason: String,
    },
}

impl RefreshStatus {
    /// The one line for the Settings row, times in the zone `offset_secs` east of UTC.
    pub fn text(&self, offset_secs: i32) -> Option<String> {
        let tz = chrono::FixedOffset::east_opt(offset_secs)?;
        match self {
            RefreshStatus::Never => None,
            RefreshStatus::Running => Some("Refreshing\u{2026}".into()),
            RefreshStatus::Done { at, pct } => {
                let t = at.with_timezone(&tz).format("%H:%M");
                Some(match pct {
                    Some(p) => format!("Refreshed {t}: weekly {}%", crate::reading::fmt0(*p)),
                    None => format!("Refreshed {t}"),
                })
            }
            RefreshStatus::Failed { reason, .. } => Some(reason.clone()),
        }
    }
}

/// "Is the Grok CLI installed?", answered in the background: looking for it stats files in PATH
/// and about twenty usual folders, which can stall on a network share, so the UI thread only ever
/// reads the last answer.
struct CliProbe {
    detect: Mutex<Detect>,
    /// The last answer and when it was taken.
    value: Mutex<Option<(Instant, bool)>>,
    running: std::sync::atomic::AtomicBool,
}

impl CliProbe {
    fn new(detect: Detect) -> Arc<Self> {
        Arc::new(CliProbe {
            detect: Mutex::new(detect),
            value: Mutex::new(None),
            running: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The last answer (`false` before the first probe finished); starts a new probe in the
    /// background when the answer is older than [`CLI_PROBE_EVERY`].
    fn get(self: &Arc<Self>) -> bool {
        use std::sync::atomic::Ordering;
        let cached = *self.value.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = cached.is_some_and(|(t, _)| t.elapsed() < CLI_PROBE_EVERY);
        if !fresh && !self.running.swap(true, Ordering::SeqCst) {
            let probe = self.clone();
            let detect = self
                .detect
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let spawned = std::thread::Builder::new()
                .name("aitrack-grok-probe".into())
                .spawn(move || {
                    let found = detect();
                    *probe.value.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((Instant::now(), found));
                    probe.running.store(false, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.running.store(false, Ordering::SeqCst);
            }
        }
        cached.is_some_and(|(_, v)| v)
    }
}

/// State of the "Refresh Grok" control.
struct GrokRefresh {
    progress: Arc<Mutex<(Progress, Instant)>>,
    probe: Arc<CliProbe>,
    /// Process id of the hidden Grok CLI while a refresh runs (0 = none); `stop` ends it.
    active: Arc<std::sync::atomic::AtomicU32>,
    run: Mutex<Refresher>,
    /// The latest outcome (kept until the next refresh starts).
    status: Mutex<RefreshStatus>,
    /// One Diagnostics row per attempt, newest last (this run of the app only).
    rows: Mutex<Vec<crate::state::Diag>>,
}

impl GrokRefresh {
    fn real() -> Self {
        let env = crate::sources::Env::from_process();
        let e2 = env.clone();
        let active = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let a2 = active.clone();
        let refresher: Refresher = Arc::new(move || {
            let env = crate::sources::Env::from_process();
            let program = crate::grok_cli::find_cli_here(&env)
                .ok_or_else(|| "The Grok CLI was not found".to_string())?;
            let cfg = crate::grok_cli::RefreshCfg::new(&env, program);
            crate::grok_cli::refresh_tracked(&cfg, &crate::grok_cli::SystemHost, &a2)?;
            Ok(crate::grok_cli::newest_pct(&env))
        });
        GrokRefresh {
            progress: Arc::new(Mutex::new((Progress::Idle, Instant::now()))),
            active,
            probe: CliProbe::new(Arc::new(move || {
                crate::grok_cli::find_cli_here(&e2).is_some()
            })),
            run: Mutex::new(refresher),
            status: Mutex::new(RefreshStatus::Never),
            rows: Mutex::new(Vec::new()),
        }
    }
}

/// What the Grok block shows for the refresh control.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshMode {
    /// Nothing: data is recent, the CLI is not installed, or Grok is off.
    Hidden,
    /// "Refresh" (data is missing or old).
    Offer,
    /// "Refreshing\u{2026}"
    Running,
    /// One-line reason.
    Failed(String),
}

/// Does the Grok CLI's folder exist? (Decides "automatic" tracking.)
pub type Detect = Arc<dyn Fn() -> bool + Send + Sync>;

struct Cached {
    version: u64,
    grok_version: u64,
    codex_version: u64,
    at: Instant,
    state: State,
}

impl App {
    pub fn new(store: Box<dyn Store>, settings: Arc<Settings>, poller: Poller) -> Self {
        let poller = Arc::new(poller);
        let p2 = poller.clone();
        Self::with_poll_fns(
            store,
            settings,
            Box::new(move |s| poller.poll_once(s)),
            Some(Box::new(move |s| p2.poll_local(s))),
        )
    }

    pub fn with_poll_fn(store: Box<dyn Store>, settings: Arc<Settings>, poll_fn: PollFn) -> Self {
        Self::with_poll_fns(store, settings, poll_fn, None)
    }

    /// `poll_fn` is the full poll, `local_fn` the cheap local-only one run on the short timer.
    pub fn with_poll_fns(
        store: Box<dyn Store>,
        settings: Arc<Settings>,
        poll_fn: PollFn,
        local_fn: Option<PollFn>,
    ) -> Self {
        let claude_detect: Arc<Mutex<Detect>> = Arc::new(Mutex::new(Arc::new(|| true)));
        // Claude not installed (and not forced on): nothing is polled and nothing is reported
        let gate = |f: PollFn| -> PollFn {
            let (st, det) = (settings.clone(), claude_detect.clone());
            Box::new(move |s| {
                let d = det.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if st.get().claude_on(d()) {
                    f(s)
                } else {
                    Ok(Value::Null)
                }
            })
        };
        let (poll_fn, local_fn) = (gate(poll_fn), local_fn.map(gate));
        let history = Arc::new(History::new(store));
        let collector = Collector::with_local(history.clone(), settings.clone(), poll_fn, local_fn);
        let notifier: Arc<Mutex<Arc<dyn Notifier>>> =
            Arc::new(Mutex::new(crate::toast::default_notifier()));
        let alerts = Arc::new(Alerts::load(settings.path.with_file_name("alerts.json")));
        let raise = Arc::new(AtomicBool::new(false));
        {
            // after every successful poll (on the collector thread): tell the user about limits
            let (h, st, n) = (history.clone(), settings.clone(), notifier.clone());
            let up = raise.clone();
            let det = claude_detect.clone();
            collector.set_after_poll(move || {
                let d = det.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if !st.get().notifications || !st.get().claude_on(d()) {
                    return;
                }
                alerts.set_options(st.alert_options());
                let now = Utc::now();
                let Some(plan) = crate::analysis::analyze(&h.snapshot(), now) else {
                    return;
                };
                // only what is current: an old backfilled sample must not trigger a toast
                if !crate::state::reading_is_current(&plan, &st.get(), now) {
                    return;
                }
                let notifier = n.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let raising = Raising {
                    inner: notifier.as_ref(),
                    raise: &up,
                };
                alerts.evaluate("Claude", &plan, now, &raising);
            });
        }
        Self {
            history,
            settings,
            collector,
            grok: None,
            codex: None,
            notifier,
            raise,
            cache: Mutex::new(None),
            stats: std::sync::OnceLock::new(),
            claude_detect,
            claude_seen: Remembered::new(),
            prune_result: Arc::new(Mutex::new(None)),
        }
        .with_prune_reporting()
    }

    fn with_prune_reporting(self) -> Self {
        self.collector
            .set_on_prune(prune_reporter(self.prune_result.clone()));
        self
    }

    /// Add Grok tracking: its own store and collector, polled on the same timer structure (live
    /// source on the full interval, the CLI log on the local tick). Nothing is polled, and no error
    /// is recorded, unless Grok tracking is on (setting, or automatic because `detect()` is true).
    pub fn with_grok(
        mut self,
        store: Box<dyn Store>,
        full: PollFn,
        local: Option<PollFn>,
        detect: Detect,
    ) -> Self {
        let bot: Arc<std::sync::OnceLock<Arc<crate::grok_bot::BotTracker>>> =
            Arc::new(std::sync::OnceLock::new());
        let gate = |f: PollFn| -> PollFn {
            let (st, det, bot) = (self.settings.clone(), detect.clone(), bot.clone());
            Box::new(move |s| {
                if st.get().grok_on(det()) {
                    // Only the Grok Bot app is installed (Linux): there is no CLI login or log to
                    // read, so no failing poll is made and no "login not found" error is shown.
                    if let Some(b) = bot.get().filter(|b| b.cli_absent()) {
                        b.refresh(Utc::now());
                        return Ok(Value::Null);
                    }
                    let r = f(s);
                    // the second meter is independent of the CLI log: read it whatever `f` said
                    if let Some(b) = bot.get() {
                        b.refresh(Utc::now());
                    }
                    r
                } else {
                    Ok(Value::Null) // Grok not in use: nothing to do, nothing to report
                }
            })
        };
        let (full, local) = (gate(full), local.map(gate));
        let history = Arc::new(History::new(store));
        let collector = Collector::with_local(history.clone(), self.settings.clone(), full, local);
        let alerts = Arc::new(Alerts::load(
            self.settings.path.with_file_name("alerts-grok.json"),
        ));
        {
            let (h, st, n, det) = (
                history.clone(),
                self.settings.clone(),
                self.notifier.clone(),
                detect.clone(),
            );
            let up = self.raise.clone();
            collector.set_after_poll(move || {
                let v = st.get();
                if !v.notifications || !v.grok_on(det()) {
                    return;
                }
                alerts.set_options(st.alert_options());
                let now = Utc::now();
                let Some(plan) = crate::analysis::analyze(&h.snapshot(), now) else {
                    return;
                };
                if !crate::state::reading_is_current(&plan, &v, now) {
                    return;
                }
                let notifier = n.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let raising = Raising {
                    inner: notifier.as_ref(),
                    raise: &up,
                };
                alerts.evaluate("Grok", &plan, now, &raising);
            });
        }
        collector.set_on_prune(prune_reporter(self.prune_result.clone()));
        self.grok = Some(GrokPart {
            history,
            collector,
            detect,
            seen: Remembered::new(),
            refresh: GrokRefresh::real(),
            bot,
        });
        self
    }

    /// Track the Grok Bot app's second weekly meter (after [`with_grok`](Self::with_grok); ignored
    /// without it). `store` is its own append-only history; `env` says where the app keeps its cache.
    pub fn with_grok_bot(self, store: Box<dyn Store>, env: crate::sources::Env) -> Self {
        if let Some(g) = &self.grok {
            let bot = Arc::new(crate::grok_bot::BotTracker::new(env, store));
            if g.bot.set(bot.clone()).is_ok() {
                // same thread and schedule as Grok's own history
                g.collector
                    .add_pruner(bot.store_info().0, move |now| bot.prune(now));
            }
        }
        self
    }

    /// Add Codex tracking: its own store and collector, polled like Grok's (live usage on the full
    /// interval, the session logs on the local tick). Nothing is polled, and no error is recorded,
    /// unless Codex tracking is on (setting, or automatic because `detect()` is true).
    pub fn with_codex(
        mut self,
        store: Box<dyn Store>,
        full: PollFn,
        local: Option<PollFn>,
        detect: Detect,
    ) -> Self {
        let gate = |f: PollFn| -> PollFn {
            let (st, det) = (self.settings.clone(), detect.clone());
            Box::new(move |s| {
                if st.get().codex_on(det()) {
                    f(s)
                } else {
                    Ok(Value::Null) // Codex not in use: nothing to do, nothing to report
                }
            })
        };
        let (full, local) = (gate(full), local.map(gate));
        let history = Arc::new(History::new(store));
        let collector = Collector::with_local(history.clone(), self.settings.clone(), full, local);
        let alerts = Arc::new(Alerts::load(
            self.settings.path.with_file_name("alerts-codex.json"),
        ));
        {
            let (h, st, n, det) = (
                history.clone(),
                self.settings.clone(),
                self.notifier.clone(),
                detect.clone(),
            );
            let up = self.raise.clone();
            collector.set_after_poll(move || {
                let v = st.get();
                if !v.notifications || !v.codex_on(det()) {
                    return;
                }
                alerts.set_options(st.alert_options());
                let now = Utc::now();
                let Some(plan) = crate::analysis::analyze(&h.snapshot(), now) else {
                    return;
                };
                if !crate::state::reading_is_current(&plan, &v, now) {
                    return;
                }
                let notifier = n.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let raising = Raising {
                    inner: notifier.as_ref(),
                    raise: &up,
                };
                alerts.evaluate("ChatGPT", &plan, now, &raising);
            });
        }
        collector.set_on_prune(prune_reporter(self.prune_result.clone()));
        self.codex = Some(CodexPart {
            history,
            collector,
            detect,
            seen: Remembered::new(),
        });
        self
    }

    /// [`with_codex`](Self::with_codex) over a real [`Poller`] (live usage, then the session logs).
    pub fn with_codex_poller(self, store: Box<dyn Store>, poller: Poller, detect: Detect) -> Self {
        let poller = Arc::new(poller);
        let p2 = poller.clone();
        self.with_codex(
            store,
            Box::new(move |s| poller.poll_once(s)),
            Some(Box::new(move |s| p2.poll_local(s))),
            detect,
        )
    }

    /// The collectors of the vendors besides Claude (Grok, Codex) that are set up.
    fn other_collectors(&self) -> impl Iterator<Item = &Collector> {
        self.grok
            .as_ref()
            .map(|g| &g.collector)
            .into_iter()
            .chain(self.codex.as_ref().map(|c| &c.collector))
    }

    /// Turn on the daily pruning of every history file (the real app; tests leave it off).
    pub fn enable_auto_prune(&self) {
        self.collector.set_auto_prune(true);
        for c in self.other_collectors() {
            c.set_auto_prune(true);
        }
    }

    /// Prune every history file now (on the collector threads; the result shows in
    /// [`prune_result`](Self::prune_result)).
    pub fn prune_now(&self) {
        *self.prune_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(String::new());
        self.collector.prune_now();
        for c in self.other_collectors() {
            c.prune_now();
        }
    }

    /// What the last "Prune now" did, one line per file (empty while it runs).
    pub fn prune_result(&self) -> Option<String> {
        self.prune_result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The history files with their sizes, for Settings > Data: (label, size in bytes).
    pub fn data_files(&self) -> Vec<(String, Option<u64>)> {
        let name = |d: String| {
            std::path::Path::new(&d)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or(d)
        };
        let mut out = vec![(name(self.history.describe()), self.history.size())];
        if let Some(g) = &self.grok {
            out.push((name(g.history.describe()), g.history.size()));
            if let Some(b) = g.bot.get() {
                let (d, size) = b.store_info();
                out.push((name(d), size));
            }
        }
        if let Some(c) = &self.codex {
            out.push((name(c.history.describe()), c.history.size()));
        }
        out
    }

    /// Replace how a refresh runs and how "is the CLI installed" is answered (tests use fakes).
    pub fn set_grok_refresh(&self, run: Refresher, cli_present: Detect) {
        if let Some(g) = &self.grok {
            *g.refresh.run.lock().unwrap_or_else(|e| e.into_inner()) = run;
            // an injected probe is trivial: answer it now so the next call already knows
            let found = cli_present();
            *g.refresh
                .probe
                .detect
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = cli_present;
            *g.refresh
                .probe
                .value
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), found));
        }
    }

    /// Where the process id of a running refresh is kept (0 = none); tests that inject their own
    /// refresher publish the child's id here so that [`stop`](Self::stop) can end it.
    pub fn grok_refresh_pid_slot(&self) -> Arc<std::sync::atomic::AtomicU32> {
        self.grok
            .as_ref()
            .map(|g| g.refresh.active.clone())
            .unwrap_or_default()
    }

    /// Replace only the "is the CLI installed?" check; it is asked on a background thread and the
    /// answer is not known until that finishes (tests of the threading use this).
    pub fn set_grok_cli_probe(&self, cli_present: Detect) {
        if let Some(g) = &self.grok {
            *g.refresh
                .probe
                .detect
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = cli_present;
            *g.refresh
                .probe
                .value
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// What the Grok block's refresh control shows now: offered when Grok is tracked, the CLI is
    /// installed and the newest Grok reading is missing or older than
    /// [`crate::grok_cli::REFRESH_AFTER_MIN`]; always visible while running and for a few seconds
    /// after a failure.
    pub fn grok_refresh_mode(&self, st: &State, now: chrono::DateTime<Utc>) -> RefreshMode {
        let Some(g) = &self.grok else {
            return RefreshMode::Hidden;
        };
        if !st.grok.as_ref().is_some_and(|x| x.enabled) {
            return RefreshMode::Hidden;
        }
        let (prog, at) = g
            .refresh
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match prog {
            Progress::Running => return RefreshMode::Running,
            Progress::Failed(m) if at.elapsed() < REFRESH_FAIL_SHOWN => {
                return RefreshMode::Failed(m)
            }
            _ => {}
        }
        let newest = st
            .grok
            .as_ref()
            .and_then(|x| x.plan.as_ref())
            .and_then(|p| crate::util::parse_ts(&p.last));
        if !crate::grok_cli::needs_refresh(newest, now) {
            return RefreshMode::Hidden;
        }
        let present = self.grok_cli_present();
        if present {
            RefreshMode::Offer
        } else {
            RefreshMode::Hidden
        }
    }

    /// Is the Grok CLI installed? The answer is the last one a background probe found (refreshed
    /// every [`CLI_PROBE_EVERY`]); never blocks. Without the CLI the refresh is hidden.
    pub fn grok_cli_present(&self) -> bool {
        self.grok.as_ref().is_some_and(|g| g.refresh.probe.get())
    }

    /// How the latest refresh ended (`Never` before the first one of this run).
    pub fn grok_refresh_status(&self) -> RefreshStatus {
        self.grok.as_ref().map_or(RefreshStatus::Never, |g| {
            g.refresh
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }

    /// Is a refresh running right now?
    pub fn grok_refreshing(&self) -> bool {
        self.grok.as_ref().is_some_and(|g| {
            g.refresh
                .progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                == Progress::Running
        })
    }

    /// Run a Grok refresh on a background thread (ignored while one is running), then poll Grok
    /// again. `wake` repaints the window when it ends.
    pub fn refresh_grok(self: &Arc<Self>, wake: impl Fn() + Send + 'static) {
        let Some(g) = &self.grok else { return };
        {
            let mut p = g.refresh.progress.lock().unwrap_or_else(|e| e.into_inner());
            if p.0 == Progress::Running {
                return;
            }
            *p = (Progress::Running, Instant::now());
        }
        *g.refresh.status.lock().unwrap_or_else(|e| e.into_inner()) = RefreshStatus::Running;
        let run = g
            .refresh
            .run
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let app = self.clone();
        let spawned = std::thread::Builder::new()
            .name("aitrack-grok-refresh".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run()))
                    .unwrap_or_else(|_| Err("The refresh crashed".to_string()));
                if let Some(g) = &app.grok {
                    let at = Utc::now();
                    let status = match &r {
                        Ok(pct) => RefreshStatus::Done { at, pct: *pct },
                        Err(m) => RefreshStatus::Failed {
                            at,
                            reason: m.clone(),
                        },
                    };
                    // the Diagnostics row (the main way to check a refresh): no tokens or ids
                    let row = crate::state::Diag {
                        ts: crate::util::iso(at),
                        ok: r.is_ok(),
                        text: match &r {
                            Ok(Some(p)) => format!(
                                "Grok refresh: ok, weekly {}% \u{b7} {}",
                                crate::reading::fmt0(*p),
                                crate::grok::SOURCE_LOG
                            ),
                            Ok(None) => "Grok refresh: ok".to_string(),
                            Err(m) => format!("Grok refresh: failed, {m}"),
                        },
                    };
                    {
                        let mut rows = g.refresh.rows.lock().unwrap_or_else(|e| e.into_inner());
                        rows.push(row);
                        let n = rows.len();
                        if n > 20 {
                            rows.drain(..n - 20);
                        }
                    }
                    *g.refresh.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
                    *g.refresh.progress.lock().unwrap_or_else(|e| e.into_inner()) = (
                        match r {
                            Ok(_) => Progress::Idle,
                            Err(m) => Progress::Failed(m),
                        },
                        Instant::now(),
                    );
                    // the CLI refreshed its login and log: look again right away
                    g.collector.refresh();
                }
                wake();
            });
        if spawned.is_err() {
            if let Some(g) = &self.grok {
                *g.refresh.status.lock().unwrap_or_else(|e| e.into_inner()) =
                    RefreshStatus::Failed {
                        at: Utc::now(),
                        reason: "Could not start the refresh".into(),
                    };
                *g.refresh.progress.lock().unwrap_or_else(|e| e.into_inner()) = (
                    Progress::Failed("Could not start the refresh".into()),
                    Instant::now(),
                );
            }
        }
    }

    /// [`with_grok`](Self::with_grok) over a real [`Poller`] (live billing, then the CLI log).
    pub fn with_grok_poller(self, store: Box<dyn Store>, poller: Poller, detect: Detect) -> Self {
        let poller = Arc::new(poller);
        let p2 = poller.clone();
        self.with_grok(
            store,
            Box::new(move |s| poller.poll_once(s)),
            Some(Box::new(move |s| p2.poll_local(s))),
            detect,
        )
    }

    /// Set up the Stats view's service over this app's histories: transcripts from `roots`, prices
    /// from `prices` (downloaded only by the service's background scans).
    pub fn with_stats(
        self,
        roots: crate::transcripts::Roots,
        prices: crate::pricing::Paths,
    ) -> Self {
        let cfg = crate::statsview::ServiceConfig {
            roots,
            prices,
            tz: Arc::new(|| chrono::Local::now().offset().local_minus_utc() / 60),
            claude: Some(self.history.clone()),
            grok: self.grok.as_ref().map(|g| g.history.clone()),
            codex: self.codex.as_ref().map(|c| c.history.clone()),
            grok_bot: self.grok.as_ref().and_then(|g| g.bot.get().cloned()),
            rescan_every: crate::statsview::RESCAN_EVERY,
        };
        self.set_stats(crate::statsview::StatsService::new(cfg));
        self
    }

    /// Use `service` for the Stats view (once; later calls are ignored).
    pub fn set_stats(&self, service: crate::statsview::StatsService) {
        let _ = self.stats.set(Arc::new(service));
    }

    pub fn stats(&self) -> Option<&Arc<crate::statsview::StatsService>> {
        self.stats.get()
    }

    /// Replace the notifier (tests use a recording fake).
    pub fn set_notifier(&self, n: Arc<dyn Notifier>) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = n;
    }

    /// "Send test notification" buttons: delivers one toast in the style of `kind` (Info, Warning or
    /// Limit) with the current alert settings, and reports whether delivery worked.
    pub fn send_test_notification(&self, kind: AlertKind) -> Result<(), String> {
        let n = self
            .notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (title, body) = match kind {
            AlertKind::Info => ("Claude session: 75% used", "Test: an informational alert"),
            AlertKind::Limit => ("Claude session: limit reached", "Test: a limit alert"),
            AlertKind::Predictive => ("Claude session: empty in ~20m", "Test: a predictive alert"),
            AlertKind::Recovery => ("Claude session: reset", "Test: a recovery notice"),
            AlertKind::Warning => ("Claude session: 10% left", "Test: a warning alert"),
        };
        let mut note = Notification::new(
            title,
            format!("{body}. If you can read this, alerts show up like this."),
            kind,
        );
        // no vendor: the test toast carries the app icon
        note.options = self.settings.alert_options();
        let raising = Raising {
            inner: n.as_ref(),
            raise: &self.raise,
        };
        raising.notify(&note)
    }

    /// Did a usage alert fire since the last call? (The window then comes to the front.)
    pub fn take_raise(&self) -> bool {
        self.raise.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_notifications(&self, on: bool) {
        self.settings.set_notifications(on);
    }

    /// Start the collector thread (polls immediately).
    pub fn start(&self) {
        self.collector.start();
        for c in self.other_collectors() {
            c.start();
        }
        if let Some(s) = self.stats.get() {
            s.start();
        }
    }

    pub fn stop(&self) {
        self.collector.stop();
        if let Some(c) = &self.codex {
            c.collector.stop();
        }
        if let Some(g) = &self.grok {
            g.collector.stop();
            // closing the window during a refresh must not leave the hidden CLI running (a later
            // refresh would refuse with "already running")
            crate::grok_cli::kill_pid(g.refresh.active.load(std::sync::atomic::Ordering::SeqCst));
        }
        if let Some(s) = self.stats.get() {
            s.stop();
        }
    }

    /// Wait for the collector threads to end (after [`stop`](Self::stop)).
    pub fn join(&self) {
        self.collector.join();
        for c in self.other_collectors() {
            c.join();
        }
        if let Some(s) = self.stats.get() {
            s.join();
        }
    }

    /// Poll everything now.
    pub fn refresh(&self) {
        self.collector.refresh();
        for c in self.other_collectors() {
            c.refresh();
        }
        // "Refresh now" also rescans the transcripts, and may refresh the price list
        if let Some(s) = self.stats.get() {
            s.request_scan(true);
        }
    }

    /// Called from the collector threads when something changed (the window repaints).
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + Clone + 'static) {
        self.collector.set_waker(f.clone());
        if let Some(s) = self.stats.get() {
            let w = f.clone();
            s.set_waker(w);
        }
        if let Some(c) = &self.codex {
            c.collector.set_waker(f.clone());
        }
        if let Some(g) = &self.grok {
            g.collector.set_waker(f);
        }
    }

    /// Is Grok tracking on (setting, or automatic because the Grok CLI's folder exists)?
    pub fn grok_on(&self) -> bool {
        self.grok
            .as_ref()
            .is_some_and(|g| self.settings.get().grok_on(g.detected()))
    }

    /// Install the check for Claude's tools (the window uses [`crate::detect::claude_present`]).
    pub fn set_claude_detect(&self, d: Detect) {
        *self.claude_detect.lock().unwrap_or_else(|e| e.into_inner()) = d;
        self.claude_seen.forget();
    }

    /// How long "is the tool installed?" answers are reused ([`PRESENCE_TTL`] by default; tests
    /// use zero to see a change at once).
    pub fn set_presence_ttl(&self, ttl: Duration) {
        self.claude_seen.set_ttl(ttl);
        if let Some(g) = &self.grok {
            g.seen.set_ttl(ttl);
        }
        if let Some(c) = &self.codex {
            c.seen.set_ttl(ttl);
        }
    }

    /// Were Claude's tools found on this PC (whatever the setting says)?
    pub fn claude_detected(&self) -> bool {
        self.claude_seen.get(&|| {
            let d = self
                .claude_detect
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            d()
        })
    }

    /// Was the Grok CLI found on this PC (whatever the setting says)? `false` without a Grok part.
    pub fn grok_detected(&self) -> bool {
        self.grok.as_ref().is_some_and(|g| g.detected())
    }

    /// Was Codex's folder found on this PC (whatever the setting says)? `false` without a Codex part.
    pub fn codex_detected(&self) -> bool {
        self.codex.as_ref().is_some_and(|c| c.detected())
    }

    /// Is Codex tracking on (setting, or automatic because Codex's folder exists)?
    pub fn codex_on(&self) -> bool {
        self.codex
            .as_ref()
            .is_some_and(|c| self.settings.get().codex_on(c.detected()))
    }

    pub fn set_codex_track(&self, t: crate::settings::Track) {
        self.settings.set_codex_track(t);
        if let Some(c) = &self.codex {
            c.collector.refresh();
        }
    }

    /// Is Claude's panel shown (setting, or automatic because a Claude tool was detected)?
    pub fn claude_on(&self) -> bool {
        self.settings.get().claude_on(self.claude_detected())
    }

    pub fn set_claude_track(&self, t: crate::settings::Track) {
        self.settings.set_claude_track(t);
        self.collector.refresh();
    }

    pub fn set_grok_track(&self, t: crate::settings::Track) {
        self.settings.set_grok_track(t);
        if let Some(g) = &self.grok {
            g.collector.refresh();
        }
    }

    pub fn set_grok_enabled(&self, on: bool) {
        self.settings.set_grok_enabled(on);
        if let Some(g) = &self.grok {
            g.collector.refresh();
        }
    }

    /// The history of a vendor's readings (for its chart).
    pub fn history_for(&self, vendor: crate::vendor::VendorId) -> &Arc<History> {
        match (vendor, &self.grok, &self.codex) {
            (crate::vendor::VendorId::Grok, Some(g), _) => &g.history,
            (crate::vendor::VendorId::Codex, _, Some(c)) => &c.history,
            _ => &self.history,
        }
    }

    /// Version of the history behind a chart (`bot`: the Grok Bot's own weekly history).
    pub fn chart_version(&self, vendor: crate::vendor::VendorId, bot: bool) -> u64 {
        if bot {
            return self.bot_tracker().map_or(0, |b| b.version());
        }
        self.history_for(vendor).version()
    }

    /// Run `f` over the readings a chart draws: the vendor's history, or with `bot` the Grok Bot's
    /// own weekly history (`usage-grok-bot.jsonl`). `None` while that history is still loading.
    pub fn chart_readings<R>(
        &self,
        vendor: crate::vendor::VendorId,
        bot: bool,
        f: impl FnOnce(&[crate::reading::Reading]) -> R,
    ) -> Option<R> {
        if bot {
            let rs = self.bot_tracker()?.readings();
            return Some(f(&rs));
        }
        self.history_for(vendor).try_with_readings(f)
    }

    fn bot_tracker(&self) -> Option<Arc<crate::grok_bot::BotTracker>> {
        self.grok.as_ref().and_then(|g| g.bot.get().cloned())
    }

    /// Current view state. Never blocks on I/O: while the history is loading it returns a
    /// "loading" state; otherwise the analysis is reused unless the data changed or is a few
    /// seconds old. Settings and collector status are always live.
    pub fn state(&self) -> State {
        let settings = self.settings.get();
        let claude_on = self.claude_on();
        let status = self.collector.status();
        let version = self.history.version();
        let gver = self.grok.as_ref().map_or(0, |g| {
            g.history
                .version()
                .wrapping_add(g.bot.get().map_or(0, |b| b.version()))
        });
        let cver = self.codex.as_ref().map_or(0, |c| c.history.version());
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cache.as_mut() {
            if c.version == version
                && c.grok_version == gver
                && c.codex_version == cver
                && c.at.elapsed() < REFRESH_EVERY
            {
                c.state.settings = settings;
                c.state.collector = status;
                c.state.claude_on = claude_on;
                if let (Some(gs), Some(g)) = (c.state.grok.as_mut(), &self.grok) {
                    gs.collector = g.collector.status();
                    gs.enabled = settings.grok_on(g.detected());
                }
                if let (Some(cs), Some(p)) = (c.state.codex.as_mut(), &self.codex) {
                    cs.collector = p.collector.status();
                    cs.enabled = settings.codex_on(p.detected());
                }
                return c.state.clone();
            }
        }
        let now = Utc::now();
        let built = self
            .history
            .try_with_readings(|r| build_state(r, settings, status.clone(), now));
        match built {
            Some(mut state) => {
                state.claude_on = claude_on;
                let prev = cache.as_ref().and_then(|c| c.state.grok.clone());
                let (grok, grok_real) = match self.build_grok(settings, now, prev.as_ref()) {
                    Some((g, real)) => (Some(g), real),
                    None => (None, true),
                };
                state.grok = grok;
                let prev_codex = cache.as_ref().and_then(|c| c.state.codex.clone());
                let (codex, codex_real) = match self.build_codex(settings, now, prev_codex.as_ref())
                {
                    Some((c, real)) => (Some(c), real),
                    None => (None, true),
                };
                state.codex = codex;
                // Grok's and Codex's polls (ok or failed) are rows of the same Diagnostics list
                let mut grok_rows = Vec::new();
                if let Some(g) = state.grok.as_ref().filter(|g| g.enabled) {
                    grok_rows = g.diagnostics.clone();
                    if let Some(p) = &self.grok {
                        let rows = p.refresh.rows.lock().unwrap_or_else(|e| e.into_inner());
                        grok_rows.extend(rows.iter().rev().cloned());
                    }
                }
                let codex_rows = state
                    .codex
                    .as_ref()
                    .filter(|c| c.enabled)
                    .map(|c| c.diagnostics.clone())
                    .unwrap_or_default();
                if !grok_rows.is_empty() || !codex_rows.is_empty() {
                    state.diagnostics = crate::state::merged_diagnostics_of(&[
                        &state.diagnostics,
                        &grok_rows,
                        &codex_rows,
                    ]);
                }
                *cache = Some(Cached {
                    version,
                    // a stand-in Grok state is not remembered as current: the next call retries
                    grok_version: if grok_real {
                        gver
                    } else {
                        gver.wrapping_add(1)
                    },
                    codex_version: if codex_real {
                        cver
                    } else {
                        cver.wrapping_add(1)
                    },
                    at: Instant::now(),
                    state: state.clone(),
                });
                state
            }
            None => cache
                .as_ref()
                .map(|c| State {
                    settings,
                    collector: status.clone(),
                    claude_on,
                    ..c.state.clone()
                })
                .unwrap_or_else(|| {
                    let mut s = State::loading(settings, status);
                    s.claude_on = claude_on;
                    s
                }),
        }
    }

    /// Grok's part of the state, and whether it was built from the real readings. While the
    /// history lock is busy (or still loading) the previous state is kept (with the live collector
    /// status), never an empty placeholder; the readings are copied under the lock and analysed
    /// outside it.
    fn build_grok(
        &self,
        settings: crate::settings::Values,
        now: chrono::DateTime<Utc>,
        prev: Option<&GrokState>,
    ) -> Option<(GrokState, bool)> {
        let g = self.grok.as_ref()?;
        let (status, enabled) = (g.collector.status(), settings.grok_on(g.detected()));
        let bot = g.bot.get().and_then(|b| b.latest());
        // the Grok Bot's saved readings get the same analysis as Grok's: window stats and a pace
        let bot_plan = g
            .bot
            .get()
            .and_then(|b| crate::analysis::analyze(&b.readings(), now));
        let bot_stats = bot_plan.as_ref().map(|p| p.weekly_stats.clone());
        let bot_gauge = bot_plan.and_then(|p| p.weekly);
        match g.history.try_snapshot() {
            Some(r) => {
                let mut s = build_grok_state(&r, status, enabled, now);
                s.bot = bot;
                s.bot_stats = bot_stats.clone();
                s.bot_gauge = bot_gauge.clone();
                Some((s, true))
            }
            None => Some(match prev {
                Some(p) => {
                    let mut p = p.clone();
                    p.collector = status;
                    p.enabled = enabled;
                    p.bot = bot;
                    p.bot_stats = bot_stats.clone();
                    p.bot_gauge = bot_gauge.clone();
                    (p, false)
                }
                None => {
                    let mut s = build_grok_state(&[], status, enabled, now);
                    s.bot = bot;
                    s.bot_stats = bot_stats.clone();
                    s.bot_gauge = bot_gauge.clone();
                    (s, false)
                }
            }),
        }
    }

    /// Codex's part of the state, and whether it was built from the real readings (see
    /// [`build_grok`](Self::build_grok) for the busy-lock rule).
    fn build_codex(
        &self,
        settings: crate::settings::Values,
        now: chrono::DateTime<Utc>,
        prev: Option<&CodexState>,
    ) -> Option<(CodexState, bool)> {
        let c = self.codex.as_ref()?;
        let (status, enabled) = (c.collector.status(), settings.codex_on(c.detected()));
        Some(match c.history.try_snapshot() {
            Some(r) => (build_codex_state(&r, status, enabled, now), true),
            None => match prev {
                Some(p) => {
                    let mut p = p.clone();
                    p.collector = status;
                    p.enabled = enabled;
                    (p, false)
                }
                None => (build_codex_state(&[], status, enabled, now), false),
            },
        })
    }

    pub fn set_interval(&self, minutes: f64) {
        self.settings.set_interval(minutes);
        self.reschedule();
    }

    pub fn set_local_interval(&self, seconds: f64) {
        self.settings.set_local_interval(seconds);
        self.reschedule();
    }

    fn reschedule(&self) {
        self.collector.reschedule();
        for c in self.other_collectors() {
            c.reschedule();
        }
    }

    pub fn set_direction(&self, d: Direction) {
        self.settings.set_direction(d);
    }

    pub fn set_badge_style(&self, b: crate::settings::BadgeVariant) {
        self.settings.set_badge_style(b);
    }

    pub fn set_on_top(&self, on: bool) {
        self.settings.set_on_top(on);
    }

    /// Toggle "Start with Windows" ("Start at login" on macOS). On failure the setting is left as it was and the reason returned.
    pub fn set_startup(&self, on: bool) -> Result<(), String> {
        startup::set_startup(on)?;
        self.settings.set_startup(on);
        Ok(())
    }

    /// Apply a JSON settings patch (any of the settings keys).
    pub fn update_settings(&self, new: &Value) {
        self.settings.update(new, true);
        self.reschedule();
    }
}
