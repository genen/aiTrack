// SPDX-License-Identifier: GPL-3.0-or-later
//! The Stats view's view-model and its background service (real thread, temp folders).
use crate::common;
use aitrack::pricing::{Paths, PriceTable};
use aitrack::reading::Reading;
use aitrack::stats::{compute, StatsRange};
use aitrack::statsview::*;
use aitrack::store::{History, JsonlStore};
use aitrack::transcripts::*;
use aitrack::vendor::VendorId;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::json;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn req(project: &str, origin: Origin, hours_ago: i64, input: u64, ticks: Option<u64>) -> Request {
    Request {
        at: (now() - Duration::hours(hours_ago)).timestamp(),
        origin,
        project: project.into(),
        session: format!("s-{project}"),
        model: "claude-sonnet-4-5".into(),
        usage: aitrack::pricing::TokenUsage {
            input,
            ..Default::default()
        },
        cost_ticks: ticks,
        sidechain: false,
        key: 0,
    }
}

fn snapshot(reqs: &[Request], sources: Vec<SourceInfo>, plan: Option<PlanData>) -> Snapshot {
    let report = aggregate(reqs.iter(), &PriceTable::fallback(), now(), 0);
    Snapshot {
        projects: Some(Arc::new(report)),
        sources,
        scan: None,
        plan: plan.map(Arc::new),
        busy: false,
    }
}

fn reading(minutes_ago: i64, s: f64, w: f64) -> Reading {
    Reading::from_value(&json!({
        "ok": true, "ts": (now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        "session": {"pct": s, "resets_at": (now() + Duration::hours(2)).to_rfc3339()},
        "weekly": {"pct": w, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
    }))
}

fn plan_of(readings: &[Reading]) -> PlanData {
    PlanData {
        claude: StatsRange::ALL
            .into_iter()
            .map(|k| (k, compute(readings, k, now(), 0)))
            .collect(),
        grok: None,
        grok_bot: None,
    }
}

#[test]
fn the_page_ranks_projects_with_bars_totals_and_a_today_line() {
    let reqs = [
        req("big", Origin::Claude, 2, 2_000_000, None), // $6
        req("small", Origin::Claude, 2, 100_000, None), // $0.30
        req("older", Origin::Claude, 24 * 10, 1_000_000, None), // outside 7 days
    ];
    let snap = snapshot(&reqs, vec![], None);
    let page = build_page(&snap, ProjRange::Days7, StatsRange::Last30d, now());
    let today = page.today.as_ref().unwrap();
    assert_eq!(today.text, "Today: $6.30 (2.10M tokens)");
    let names: Vec<&str> = page.projects.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["big", "small"]);
    assert_eq!(page.projects.rows[0].rank, 1);
    assert_eq!(page.projects.rows[0].frac, 1.0);
    assert!((page.projects.rows[1].frac - 0.05).abs() < 1e-3);
    assert_eq!(page.projects.rows[0].cost, "$6.00");
    assert_eq!(page.projects.rows[0].tokens, "2.00M");
    assert_eq!(
        page.projects.total.as_deref(),
        Some("Total: $6.30 (2.10M tokens)")
    );
    assert!(page.projects.rows[0].detail.contains("1 session"));
    // the range toggle
    let all = build_page(&snap, ProjRange::All, StatsRange::Last30d, now());
    assert_eq!(all.projects.rows.len(), 3);
    assert!(page.projects.empty.is_none() && !page.updating);
}

#[test]
fn many_projects_are_cut_with_a_more_line_and_grok_without_cost_reads_na() {
    let mut reqs: Vec<Request> = (0..15)
        .map(|i| {
            req(
                &format!("p{i:02}"),
                Origin::Claude,
                1,
                1_000_000 - i * 10_000,
                None,
            )
        })
        .collect();
    reqs.push(req("grokproj", Origin::GrokLog, 1, 500, None));
    let page = build_page(
        &snapshot(&reqs, vec![], None),
        ProjRange::Days7,
        StatsRange::Last7d,
        now(),
    );
    assert_eq!(page.projects.rows.len(), MAX_ROWS);
    assert!(
        page.projects.more.as_ref().unwrap().starts_with("+4 more"),
        "{:?}",
        page.projects.more
    );
    // tokens-only Grok projects have no cost: n/a, and a bar by tokens when nothing has a cost
    let only = build_page(
        &snapshot(&[req("g", Origin::GrokLog, 1, 500, None)], vec![], None),
        ProjRange::Days7,
        StatsRange::Last7d,
        now(),
    );
    assert_eq!(only.projects.rows[0].cost, "n/a");
    assert_eq!(only.projects.rows[0].frac, 1.0);
    assert_eq!(only.projects.rows[0].vendor, VendorId::Grok);
}

#[test]
fn the_empty_state_names_the_folders_that_were_checked() {
    let src = |l: &str, exists: bool| SourceInfo {
        label: l.into(),
        path: format!("/home/u/{l}").into(),
        exists,
        files: 0,
        note: String::new(),
    };
    let snap = snapshot(
        &[],
        vec![
            src("Claude Code transcripts", false),
            src("Grok sessions", true),
        ],
        None,
    );
    let page = build_page(&snap, ProjRange::Days7, StatsRange::Last30d, now());
    let e = page.projects.empty.expect("empty state");
    assert_eq!(e.message, "No local transcripts found");
    assert_eq!(e.folders.len(), 2);
    assert!(e.folders[0].contains("(missing)") && e.folders[1].contains("no usage in this range"));
    assert!(page.projects.rows.is_empty() && page.projects.total.is_none());
    assert_eq!(page.today.unwrap().text, "Today: $0.00 (0 tokens)");
}

#[test]
fn before_the_first_scan_the_page_says_so_and_updating_is_carried() {
    let page = build_page(
        &Snapshot {
            busy: true,
            ..Default::default()
        },
        ProjRange::Days7,
        StatsRange::Last30d,
        now(),
    );
    assert!(page.updating && page.today.is_none());
    assert!(page.projects.empty.is_none() && page.projects.rows.is_empty());
    assert_eq!(page.plan_note.as_deref(), Some("Computing…"));
}

#[test]
fn the_plan_section_shows_complete_windows_current_window_resets_and_the_ratio_only_when_it_exists()
{
    // three finished session windows (25 / 40 / 55 %) and one in progress
    let mut rs = Vec::new();
    for (i, peak) in [25.0, 40.0, 55.0].into_iter().enumerate() {
        let start = now() - Duration::hours(20 - 6 * i as i64);
        for (k, p) in [1.0, peak / 2.0, peak].into_iter().enumerate() {
            rs.push(Reading::from_value(&json!({
                "ok": true, "ts": (start + Duration::minutes(15 * k as i64)).to_rfc3339(),
                "session": {"pct": p, "resets_at": (start + Duration::hours(5)).to_rfc3339()},
                "weekly": {"pct": 10.0 + i as f64, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
            })));
        }
    }
    rs.push(reading(30, 5.0, 13.0));
    rs.push(reading(5, 8.0, 13.0));
    let snap = snapshot(&[], vec![], Some(plan_of(&rs)));
    let page = build_page(&snap, ProjRange::Days7, StatsRange::Last7d, now());
    let sess = page
        .plan
        .iter()
        .find(|b| b.title.starts_with("Claude session"))
        .unwrap();
    let get = |k: &str| sess.rows.iter().find(|r| r.0 == k).map(|r| r.1.as_str());
    assert_eq!(get("Windows seen"), Some("3"));
    assert_eq!(
        get("Peak avg / max"),
        Some("40% / 55%"),
        "the in-progress window is not in the averages"
    );
    assert_eq!(get("Hit the limit"), Some("0% (0 of 3)"));
    assert!(get("Current window").unwrap().starts_with("8% used"));
    assert_eq!(get("Resets in"), Some("2h"));
    assert_eq!(get("Early resets"), Some("0"));
    assert_eq!(
        get("A full session"),
        None,
        "the ratio is not shown until it exists"
    );
    // weekly block exists; Grok has none without data
    assert!(page
        .plan
        .iter()
        .any(|b| b.title.starts_with("Claude weekly")));
    assert!(page.plan.iter().all(|b| b.vendor != VendorId::Grok));
    // no complete windows at all: the existing wording
    let few = build_page(
        &snapshot(&[], vec![], Some(plan_of(&[reading(5, 8.0, 13.0)]))),
        ProjRange::Days7,
        StatsRange::Last7d,
        now(),
    );
    let s2 = few
        .plan
        .iter()
        .find(|b| b.title.starts_with("Claude session"))
        .unwrap();
    assert!(s2.rows.iter().any(|r| r.1 == "not enough complete windows"));
}

#[test]
fn grok_gets_a_weekly_block_only_and_an_empty_plan_has_a_note() {
    let rs = vec![reading(60, 5.0, 20.0), reading(5, 8.0, 21.0)];
    let mut plan = plan_of(&[]);
    plan.grok = Some(
        StatsRange::ALL
            .into_iter()
            .map(|k| (k, compute(&rs, k, now(), 0)))
            .collect(),
    );
    let page = build_page(
        &snapshot(&[], vec![], Some(plan)),
        ProjRange::Days7,
        StatsRange::Last7d,
        now(),
    );
    let grok: Vec<_> = page
        .plan
        .iter()
        .filter(|b| b.vendor == VendorId::Grok)
        .collect();
    assert_eq!(grok.len(), 1);
    assert_eq!(grok[0].title, "Grok weekly");
    assert!(
        page.plan.iter().all(|b| !b.title.starts_with("Claude")),
        "no Claude readings, no Claude block"
    );
    let none = build_page(
        &snapshot(&[], vec![], Some(plan_of(&[]))),
        ProjRange::Days7,
        StatsRange::Last7d,
        now(),
    );
    assert_eq!(
        none.plan_note.as_deref(),
        Some("No plan readings in this range yet")
    );
}

// ---- the service ----------------------------------------------------------------------------

fn transcript(dir: &std::path::Path, name: &str, id: &str, input: u64) {
    let p = dir.join(format!("claude/projects/-w-svc/{name}.jsonl"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap();
    writeln!(
        f,
        "{}",
        json!({"type": "assistant", "timestamp": Utc::now().to_rfc3339(), "cwd": "/w/svc", "sessionId": "s",
               "requestId": id, "message": {"id": id, "model": "claude-sonnet-4-5", "usage": {"input_tokens": input}}})
    )
    .unwrap();
}

fn wait_for(svc: &StatsService, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let t0 = Instant::now();
    loop {
        let s = svc.snapshot();
        if ok(&s) {
            return s;
        }
        assert!(
            t0.elapsed() < StdDuration::from_secs(10),
            "timed out waiting for the stats service"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    }
}

#[test]
fn the_service_scans_off_thread_wakes_the_window_and_rescans_on_request() {
    let dir = TempDir::new();
    transcript(dir.path(), "a", "m1", 1000);
    let history = Arc::new(History::new(Box::new(JsonlStore::new(dir.join("u.jsonl")))));
    history.load();
    let cfg = ServiceConfig {
        roots: Roots {
            claude_projects: dir.join("claude/projects"),
            grok_home: dir.join("grok"),
            index: Some(dir.join("idx.json")),
        },
        prices: Paths::in_dir(dir.path()),
        tz: Arc::new(|| 0),
        claude: Some(history),
        grok: None,
        grok_bot: None,
        rescan_every: StdDuration::from_secs(3600),
    };
    let svc = StatsService::new(cfg);
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    svc.set_waker(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    assert!(svc.snapshot().projects.is_none(), "nothing before start");
    svc.start();
    let first = wait_for(&svc, |s| {
        s.projects.is_some() && s.plan.is_some() && !s.busy
    });
    let rep = first.projects.unwrap();
    assert_eq!(rep.range(ProjRange::All).projects[0].name, "svc");
    assert_eq!(rep.range(ProjRange::All).projects[0].tokens.input, 1000);
    assert!(
        wakes.load(Ordering::SeqCst) > 0,
        "the window is woken when results arrive"
    );
    assert!(first
        .sources
        .iter()
        .any(|s| s.label == "Claude Code transcripts" && s.exists));
    // a new request appears after a rescan
    transcript(dir.path(), "a", "m2", 500);
    svc.request_scan(false);
    wait_for(&svc, |s| {
        s.projects
            .as_ref()
            .is_some_and(|r| r.range(ProjRange::All).totals.messages == 2)
            && !s.busy
    });
    svc.request_plan();
    svc.stop();
    svc.join();
}

#[test]
fn a_fixed_service_serves_its_snapshot_and_ignores_requests() {
    let svc = StatsService::fixed(snapshot(
        &[req("x", Origin::Claude, 1, 10, None)],
        vec![],
        None,
    ));
    svc.start();
    svc.request_scan(true);
    svc.request_plan();
    assert_eq!(
        svc.snapshot()
            .projects
            .unwrap()
            .range(ProjRange::All)
            .projects
            .len(),
        1
    );
    svc.stop();
    svc.join();
}

#[test]
fn a_project_row_tooltip_names_the_vendor_and_the_numbers() {
    let reqs = [
        req("same", Origin::Claude, 2, 2_000_000, None),
        req("same", Origin::GrokLog, 2, 1_000, None),
    ];
    let page = build_page(
        &snapshot(&reqs, vec![], None),
        ProjRange::Days7,
        StatsRange::Last30d,
        now(),
    );
    // same-named rows stay separate, one per vendor, with the vendor on the row
    let rows = &page.projects.rows;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let claude = rows.iter().find(|r| r.vendor == VendorId::Claude).unwrap();
    let grok = rows.iter().find(|r| r.vendor == VendorId::Grok).unwrap();
    assert!(
        claude
            .detail
            .starts_with("Claude: 2.00M tokens, $6.00, 1 session, 1 requests"),
        "{}",
        claude.detail
    );
    assert!(grok.detail.starts_with("Grok: "), "{}", grok.detail);
    assert!(grok.detail.contains("1 session"), "{}", grok.detail);
    // the name itself carries no label: the logo does that in the window
    assert_eq!((claude.name.as_str(), grok.name.as_str()), ("same", "same"));
}

#[test]
fn the_projects_cli_labels_grok_rows_and_cuts_a_long_name_before_the_label() {
    let long = "a-very-long-project-name-that-goes-on-and-on-and-on-forever";
    let reqs = [
        req("aiTrack", Origin::Claude, 2, 2_000_000, None),
        req("aiTrack", Origin::GrokLog, 2, 1_000, None),
        req(long, Origin::GrokLog, 2, 3_000_000, None),
    ];
    let report = aggregate(reqs.iter(), &PriceTable::fallback(), now(), 0);
    let text = projects_text(&report, ProjRange::Days7, &[]);
    assert!(text.contains("aiTrack (Grok)"), "{text}");
    let plain = text
        .lines()
        .filter(|l| l.contains("aiTrack") && !l.contains("(Grok)"))
        .count();
    assert_eq!(plain, 1, "the Claude row stays plain\n{text}");
    let cut = text.lines().find(|l| l.contains("a-very-long")).unwrap();
    assert!(
        cut.contains(&format!("{} (Grok)", &long[..40])),
        "cut to 40 characters, then the label\n{cut}"
    );
    assert!(!cut.contains(&long[..41]), "{cut}");
}

/// A stored Grok Bot record (the `bot_weekly` form that `grok_bot::record` writes).
fn bot_record(minutes_ago: i64, pct: f64) -> serde_json::Value {
    json!({
        "v": 1, "ok": true, "vendor": "grok", "source": "grok-bot",
        "ts": (now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        "bot_weekly": {
            "pct": pct,
            "resets_at": (now() + Duration::days(3)).to_rfc3339(),
            "read_at": (now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        },
    })
}

#[test]
fn grok_bot_weekly_follows_grok_weekly_and_is_absent_without_history() {
    let bot: Vec<Reading> = [bot_record(60, 5.0), bot_record(5, 9.0)]
        .iter()
        .map(Reading::from_value)
        .collect();
    assert_eq!(bot[1].weekly.as_ref().map(|b| b.pct), Some(9.0));
    let all = |rs: &[Reading]| {
        StatsRange::ALL
            .into_iter()
            .map(|k| (k, compute(rs, k, now(), 0)))
            .collect::<Vec<_>>()
    };
    let rs = vec![reading(60, 5.0, 20.0), reading(5, 8.0, 21.0)];
    let mut plan = plan_of(&[]);
    plan.grok = Some(all(&rs));
    plan.grok_bot = Some(all(&bot));
    let titles = |plan: PlanData| -> Vec<String> {
        build_page(
            &snapshot(&[], vec![], Some(plan)),
            ProjRange::Days7,
            StatsRange::Last7d,
            now(),
        )
        .plan
        .into_iter()
        .map(|b| b.title)
        .collect()
    };
    assert_eq!(titles(plan.clone()), ["Grok weekly", "Grok Bot weekly"]);
    // tracked but nothing stored, or not tracked: no section
    plan.grok_bot = Some(all(&[]));
    assert_eq!(titles(plan.clone()), ["Grok weekly"]);
    plan.grok_bot = None;
    assert_eq!(titles(plan), ["Grok weekly"]);
}

#[test]
fn the_service_reads_the_grok_bot_history_file() {
    let dir = TempDir::new();
    let hist = dir.join("usage-grok-bot.jsonl");
    let store = JsonlStore::new(&hist);
    for r in [bot_record(60, 5.0), bot_record(5, 9.0)] {
        aitrack::store::Store::append(&store, &r).unwrap();
    }
    let tracker = Arc::new(aitrack::grok_bot::BotTracker::new(
        aitrack::sources::Env::new(aitrack::sources::Os::Windows),
        Box::new(JsonlStore::new(&hist)),
    ));
    let cfg = ServiceConfig {
        roots: Roots {
            claude_projects: dir.join("claude/projects"),
            grok_home: dir.join("grok"),
            index: Some(dir.join("idx.json")),
        },
        prices: Paths::in_dir(dir.path()),
        tz: Arc::new(|| 0),
        claude: None,
        grok: None,
        grok_bot: Some(tracker),
        rescan_every: StdDuration::from_secs(3600),
    };
    let svc = StatsService::new(cfg);
    svc.start();
    let snap = wait_for(&svc, |s| s.plan.is_some() && !s.busy);
    let plan = snap.plan.unwrap();
    assert!(plan.grok.is_none());
    assert!(plan.claude.is_empty());
    let st = plan.grok_bot_at(StatsRange::All).unwrap();
    assert_eq!(st.coverage.readings, 2);
    svc.stop();
    svc.join();
}
