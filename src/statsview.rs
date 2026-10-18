// SPDX-License-Identifier: GPL-3.0-or-later
//! The Stats view's data and view-model (no UI code).
//!
//! * [`StatsService`] owns a worker thread (std thread + channel) that scans the local transcripts
//!   ([`crate::transcripts`]), prices them and computes the plan statistics ([`crate::stats`]);
//!   the window only reads the latest [`Snapshot`] and shows "updating…" while `busy`.
//!   It rescans every few minutes and on "Refresh now"; the price list may only be downloaded in
//!   those background scans (`allow_network`), never when the view is merely opened.
//! * [`build_page`] turns a snapshot plus the selected ranges into formatted rows ([`StatsPage`]) so
//!   what is shown can be tested without egui.

use crate::pricing::{Paths, PriceTable};
use crate::reading::{Kind, Reading};
use crate::stats::{compute, Stats, StatsRange};
use crate::store::History;
use crate::transcripts::{
    aggregate, fmt_cost, fmt_tokens, ProjRange, Report, Roots, ScanSummary, Scanner, SourceInfo,
};
use crate::vendor::VendorId;
use chrono::{DateTime, Utc};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the worker rescans on its own.
pub const RESCAN_EVERY: Duration = Duration::from_secs(5 * 60);
/// Project rows shown (the rest are summed in a "+N more" line).
pub const MAX_ROWS: usize = 12;

/// The plan statistics for each range, per vendor.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanData {
    pub claude: Vec<(StatsRange, Stats)>,
    /// `None` when Grok is not tracked.
    pub grok: Option<Vec<(StatsRange, Stats)>>,
    /// `None` when the Grok Bot app is not tracked (weekly only).
    pub grok_bot: Option<Vec<(StatsRange, Stats)>>,
}

impl PlanData {
    pub fn claude_at(&self, r: StatsRange) -> Option<&Stats> {
        self.claude.iter().find(|(k, _)| *k == r).map(|(_, s)| s)
    }

    pub fn grok_at(&self, r: StatsRange) -> Option<&Stats> {
        self.grok
            .as_ref()?
            .iter()
            .find(|(k, _)| *k == r)
            .map(|(_, s)| s)
    }

    pub fn grok_bot_at(&self, r: StatsRange) -> Option<&Stats> {
        self.grok_bot
            .as_ref()?
            .iter()
            .find(|(k, _)| *k == r)
            .map(|(_, s)| s)
    }
}

/// What the worker has produced so far.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub projects: Option<Arc<Report>>,
    pub sources: Vec<SourceInfo>,
    pub scan: Option<ScanSummary>,
    pub plan: Option<Arc<PlanData>>,
    /// A scan or computation is running.
    pub busy: bool,
}

enum Cmd {
    Scan { network: bool },
    Plan,
    Stop,
}

pub struct ServiceConfig {
    pub roots: Roots,
    pub prices: Paths,
    /// Local UTC offset in minutes (east positive), asked at every computation.
    pub tz: Arc<dyn Fn() -> i32 + Send + Sync>,
    pub claude: Option<Arc<History>>,
    pub grok: Option<Arc<History>>,
    pub grok_bot: Option<Arc<crate::grok_bot::BotTracker>>,
    pub rescan_every: Duration,
}

type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

pub struct StatsService {
    shared: Arc<Mutex<Snapshot>>,
    tx: Mutex<Option<Sender<Cmd>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    waker: Waker,
    config: Mutex<Option<ServiceConfig>>,
}

impl StatsService {
    /// A service that has not started its thread yet (see [`start`](Self::start)).
    pub fn new(config: ServiceConfig) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Snapshot::default())),
            tx: Mutex::new(None),
            handle: Mutex::new(None),
            waker: Arc::new(Mutex::new(None)),
            config: Mutex::new(Some(config)),
        }
    }

    /// A service with fixed content and no thread (screenshots, tests).
    pub fn fixed(snapshot: Snapshot) -> Self {
        Self {
            shared: Arc::new(Mutex::new(snapshot)),
            tx: Mutex::new(None),
            handle: Mutex::new(None),
            waker: Arc::new(Mutex::new(None)),
            config: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Called from the worker when something changed (the window repaints).
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    /// Start the worker thread (once). The first scan uses the cached price list only.
    pub fn start(&self) {
        let Some(config) = self.config.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let (tx, rx) = channel();
        let _ = tx.send(Cmd::Scan { network: false });
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        let (shared, waker) = (self.shared.clone(), self.waker.clone());
        let h = std::thread::Builder::new()
            .name("aitrack-stats".into())
            .spawn(move || worker(config, rx, shared, waker))
            .ok();
        *self.handle.lock().unwrap_or_else(|e| e.into_inner()) = h;
    }

    fn send(&self, c: Cmd) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(c);
        }
    }

    /// Rescan the transcripts now; `network` lets this scan refresh the price list.
    pub fn request_scan(&self, network: bool) {
        self.send(Cmd::Scan { network });
    }

    /// Recompute the plan statistics (cheap; used when the view opens or the readings changed).
    pub fn request_plan(&self) {
        self.send(Cmd::Plan);
    }

    pub fn stop(&self) {
        self.send(Cmd::Stop);
    }

    pub fn join(&self) {
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

fn worker(
    cfg: ServiceConfig,
    rx: std::sync::mpsc::Receiver<Cmd>,
    shared: Arc<Mutex<Snapshot>>,
    waker: Waker,
) {
    let mut scanner = Scanner::new(cfg.roots.clone());
    let wake = || {
        let w = waker.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(w) = w {
            w();
        }
    };
    let busy = |b: bool| {
        shared.lock().unwrap_or_else(|e| e.into_inner()).busy = b;
        wake();
    };
    let do_plan = || {
        busy(true);
        let tz = (cfg.tz)();
        let now = Utc::now();
        let compute_all = |r: &[Reading]| -> Vec<(StatsRange, Stats)> {
            StatsRange::ALL
                .into_iter()
                .map(|k| (k, compute(r, k, now, tz)))
                .collect()
        };
        // copy the readings out and compute without holding the history lock
        let ranges = |h: &Arc<History>| compute_all(&h.snapshot());
        let plan = PlanData {
            claude: cfg.claude.as_ref().map(ranges).unwrap_or_default(),
            grok: cfg.grok.as_ref().map(ranges),
            grok_bot: cfg.grok_bot.as_ref().map(|b| compute_all(&b.readings())),
        };
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.plan = Some(Arc::new(plan));
        s.busy = false;
        drop(s);
        wake();
    };
    let mut do_scan = |network: bool| {
        busy(true);
        let summary = scanner.scan();
        let now = Utc::now();
        let prices = PriceTable::load(&cfg.prices, now, network);
        let report = aggregate(scanner.requests(), &prices, now, (cfg.tz)());
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.projects = Some(Arc::new(report));
        s.sources = scanner.sources().to_vec();
        s.scan = Some(summary);
        s.busy = false;
        drop(s);
        wake();
    };
    loop {
        match rx.recv_timeout(cfg.rescan_every) {
            Ok(Cmd::Scan { network }) => {
                do_scan(network);
                do_plan();
            }
            Ok(Cmd::Plan) => do_plan(),
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            // the periodic background scan may refresh the price list
            Err(RecvTimeoutError::Timeout) => {
                do_scan(true);
                do_plan();
            }
        }
    }
}

// ---- view-model -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TodayLine {
    pub cost: String,
    pub tokens: String,
    /// "Today: $3.20 (1.2M tokens)".
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    pub rank: usize,
    pub name: String,
    pub vendor: VendorId,
    pub tokens: String,
    pub cost: String,
    /// 0..=1, relative to the biggest project (cost, or tokens when no project has a cost).
    pub frac: f32,
    /// Hover text: the vendor in words, tokens, cost, sessions, requests, last activity.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmptyState {
    pub message: String,
    /// "Claude Code transcripts: C:\\...\\projects (missing)".
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectsBlock {
    pub range: ProjRange,
    pub rows: Vec<ProjectRow>,
    /// Projects beyond [`MAX_ROWS`] and what they add up to, e.g. "+3 more: $0.40".
    pub more: Option<String>,
    /// "Total: $12.30 (4.2M tokens)".
    pub total: Option<String>,
    pub empty: Option<EmptyState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanBlock {
    pub vendor: VendorId,
    /// "Claude session (5 h)".
    pub title: String,
    pub rows: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsPage {
    pub updating: bool,
    pub today: Option<TodayLine>,
    pub projects: ProjectsBlock,
    pub plan: Vec<PlanBlock>,
    /// Text for the plan section when there is nothing to show yet.
    pub plan_note: Option<String>,
}

fn pct(x: f64) -> String {
    format!("{}%", crate::util::js_round(x) as i64)
}

fn hours(h: f64) -> String {
    crate::view::left_long(h)
}

fn plan_rows(st: &Stats, kind: Kind, now: DateTime<Utc>) -> Vec<(String, String)> {
    let k = st.kind(kind);
    let mut rows = Vec::new();
    let seen = if k.ended_early > 0 {
        format!("{} ({} cut short by a reset)", k.windows, k.ended_early)
    } else {
        k.windows.to_string()
    };
    rows.push(("Windows seen".into(), seen));
    match (k.avg_peak, k.max_peak) {
        (Some(a), Some(m)) => {
            rows.push(("Peak avg / max".into(), format!("{} / {}", pct(a), pct(m))))
        }
        _ => rows.push((
            "Peak avg / max".into(),
            "not enough complete windows".into(),
        )),
    }
    if let Some(h) = k.hit_rate {
        rows.push((
            "Hit the limit".into(),
            format!("{} ({} of {})", pct(h), k.hits, k.counted()),
        ));
    }
    match &k.current {
        Some(c) => {
            rows.push((
                "Current window".into(),
                format!("{} used ({} in)", pct(c.pct), hours(c.elapsed_h)),
            ));
            if let Some(r) = c.resets_at {
                rows.push((
                    "Resets in".into(),
                    hours(((r - now).num_minutes() as f64 / 60.0).max(0.0)),
                ));
            }
        }
        None => rows.push(("Current window".into(), "none in progress".into())),
    }
    let resets = st.resets.iter().filter(|e| e.kind == kind).count();
    rows.push(("Early resets".into(), resets.to_string()));
    if kind == Kind::Session {
        if let Some(r) = st.session_to_weekly {
            rows.push((
                "A full session".into(),
                format!(
                    "~{} of the week ({})",
                    pct(r.points_per_session),
                    r.confidence.label()
                ),
            ));
        }
    }
    rows
}

/// Format everything the Stats view shows.
pub fn build_page(
    snap: &Snapshot,
    proj_range: ProjRange,
    plan_range: StatsRange,
    now: DateTime<Utc>,
) -> StatsPage {
    let mut page = StatsPage {
        updating: snap.busy,
        today: None,
        projects: ProjectsBlock {
            range: proj_range,
            rows: Vec::new(),
            more: None,
            total: None,
            empty: None,
        },
        plan: Vec::new(),
        plan_note: None,
    };
    if let Some(report) = &snap.projects {
        let t = &report.today().totals;
        let (cost, tokens) = (
            fmt_cost(t.cost, t.messages, t.unpriced_messages),
            fmt_tokens(t.total_tokens),
        );
        page.today = Some(TodayLine {
            text: format!("Today: {cost} ({tokens} tokens)"),
            cost,
            tokens,
        });
        let rr = report.range(proj_range);
        let max_cost = rr.projects.iter().map(|p| p.cost).fold(0.0, f64::max);
        let max_tok = rr
            .projects
            .iter()
            .map(|p| p.total_tokens)
            .max()
            .unwrap_or(0);
        for (i, p) in rr.projects.iter().take(MAX_ROWS).enumerate() {
            let frac = if max_cost > 0.0 {
                p.cost / max_cost
            } else if max_tok > 0 {
                p.total_tokens as f64 / max_tok as f64
            } else {
                0.0
            };
            page.projects.rows.push(ProjectRow {
                rank: i + 1,
                name: p.name.clone(),
                vendor: p.vendor,
                tokens: fmt_tokens(p.total_tokens),
                cost: fmt_cost(p.cost, p.messages, p.unpriced_messages),
                frac: frac.clamp(0.0, 1.0) as f32,
                detail: format!(
                    "{}: {} tokens, {}, {} session{}, {} requests{}",
                    p.vendor.name(),
                    fmt_tokens(p.total_tokens),
                    fmt_cost(p.cost, p.messages, p.unpriced_messages),
                    p.sessions,
                    if p.sessions == 1 { "" } else { "s" },
                    p.messages,
                    p.last_active
                        .map(|t| format!(", last {}", t.format("%Y-%m-%d %H:%M UTC")))
                        .unwrap_or_default()
                ),
            });
        }
        if rr.projects.len() > MAX_ROWS {
            let rest = &rr.projects[MAX_ROWS..];
            let cost: f64 = rest.iter().map(|p| p.cost).sum();
            let tokens: u64 = rest.iter().map(|p| p.total_tokens).sum();
            page.projects.more = Some(format!(
                "+{} more: {} ({} tokens)",
                rest.len(),
                fmt_cost(cost, 1, 0),
                fmt_tokens(tokens)
            ));
        }
        if rr.projects.is_empty() {
            page.projects.empty = Some(EmptyState {
                message: "No local transcripts found".into(),
                folders: snap
                    .sources
                    .iter()
                    .map(|s| {
                        format!(
                            "{}: {} ({})",
                            s.label,
                            s.path.display(),
                            if s.exists {
                                "no usage in this range"
                            } else {
                                "missing"
                            }
                        )
                    })
                    .collect(),
            });
        } else {
            let t = &rr.totals;
            page.projects.total = Some(format!(
                "Total: {} ({} tokens)",
                fmt_cost(t.cost, t.messages, t.unpriced_messages),
                fmt_tokens(t.total_tokens)
            ));
        }
    }
    match &snap.plan {
        None => page.plan_note = Some("Computing…".into()),
        Some(plan) => {
            if let Some(st) = plan
                .claude_at(plan_range)
                .filter(|s| s.coverage.readings > 0)
            {
                page.plan.push(PlanBlock {
                    vendor: VendorId::Claude,
                    title: "Claude session (5 h)".into(),
                    rows: plan_rows(st, Kind::Session, now),
                });
                page.plan.push(PlanBlock {
                    vendor: VendorId::Claude,
                    title: "Claude weekly (7 d)".into(),
                    rows: plan_rows(st, Kind::Weekly, now),
                });
            }
            // Grok has a weekly window only
            if let Some(st) = plan.grok_at(plan_range).filter(|s| s.coverage.readings > 0) {
                page.plan.push(PlanBlock {
                    vendor: VendorId::Grok,
                    title: "Grok weekly".into(),
                    rows: plan_rows(st, Kind::Weekly, now),
                });
            }
            // the Grok Bot app's own weekly limit (weekly only), when it has a history
            if let Some(st) = plan
                .grok_bot_at(plan_range)
                .filter(|s| s.coverage.readings > 0)
            {
                page.plan.push(PlanBlock {
                    vendor: VendorId::Grok,
                    title: "Grok Bot weekly".into(),
                    rows: plan_rows(st, Kind::Weekly, now),
                });
            }
            if page.plan.is_empty() {
                page.plan_note = Some("No plan readings in this range yet".into());
            }
        }
    }
    page
}
