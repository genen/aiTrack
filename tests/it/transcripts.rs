// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-project usage from local transcripts: Claude Code and Grok fixtures, de-duplication, price
//! stub, today/7d/30d boundaries with a timezone offset, rescan cache and persisted index, caps.
use crate::common;
use aitrack::pricing::{Paths, PriceTable};
use aitrack::transcripts::*;
use aitrack::vendor::VendorId;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Wednesday 2026-10-14 12:00 UTC.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn ago(h: i64) -> DateTime<Utc> {
    now() - Duration::hours(h)
}

/// Prices every model at $1 in / $2 out / $0.5 cache read / $1.5 write / $3 1h write per million.
fn prices(dir: &TempDir) -> PriceTable {
    std::fs::write(
        dir.join("pricing-overrides.json"),
        r#"{"default": {"input": 1, "output": 2, "cache_read": 0.5, "cache_write": 1.5, "cache_write_1h": 3}}"#,
    )
    .unwrap();
    PriceTable::load(&Paths::in_dir(dir.path()), now(), false)
}

struct Tree {
    dir: TempDir,
}

impl Tree {
    fn new() -> Self {
        Tree {
            dir: TempDir::new(),
        }
    }
    fn claude(&self) -> PathBuf {
        self.dir.join("claude/projects")
    }
    fn grok(&self) -> PathBuf {
        self.dir.join("grok")
    }
    fn roots(&self, index: bool) -> Roots {
        Roots {
            claude_projects: self.claude(),
            grok_home: self.grok(),
            index: index.then(|| self.dir.join("index.json")),
        }
    }
    fn write(&self, rel: &str, lines: &[Value]) -> PathBuf {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }
}

#[allow(clippy::too_many_arguments)]
fn asst(
    t: DateTime<Utc>,
    cwd: Option<&str>,
    sid: &str,
    id: &str,
    req: &str,
    model: &str,
    usage: Value,
    side: bool,
) -> Value {
    let mut v = json!({
        "type": "assistant", "timestamp": iso(t), "sessionId": sid, "requestId": req,
        "isSidechain": side,
        "message": {"id": id, "model": model, "usage": usage, "content": [{"type": "text", "text": "SECRET PROMPT TEXT"}]},
    });
    if let Some(c) = cwd {
        v["cwd"] = json!(c);
    }
    v
}

fn usage(i: u64, o: u64, cr: u64, cw: u64) -> Value {
    json!({"input_tokens": i, "output_tokens": o, "cache_read_input_tokens": cr, "cache_creation_input_tokens": cw})
}

fn scan(t: &Tree) -> (Scanner, Report) {
    let mut s = Scanner::new(t.roots(false));
    s.scan();
    let r = aggregate(s.requests(), &prices(&t.dir), now(), 0);
    (s, r)
}

fn project<'a>(r: &'a Report, range: ProjRange, name: &str) -> &'a ProjectUsage {
    r.range(range)
        .projects
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no project {name}: {:?}", r.range(range).projects))
}

// ---- Claude Code transcripts ----------------------------------------------------------------

#[test]
fn streaming_duplicates_keep_the_last_chunk_and_synthetic_models_are_skipped() {
    let t = Tree::new();
    t.write(
        "claude/projects/-home-u-alpha/s1.jsonl",
        &[
            json!({"type": "user", "timestamp": iso(ago(5)), "message": {"content": "SECRET PROMPT TEXT"}}),
            // one message streamed in three chunks: the last has the final counts
            asst(ago(4), Some("/home/u/alpha"), "s1", "m1", "r1", "test-model", usage(10, 5, 0, 0), false),
            asst(ago(4), Some("/home/u/alpha"), "s1", "m1", "r1", "test-model", usage(10, 50, 0, 0), false),
            asst(ago(4), Some("/home/u/alpha"), "s1", "m1", "r1", "test-model", usage(10, 200, 1000, 300), false),
            // same message id, different request: a different message
            asst(ago(3), Some("/home/u/alpha"), "s1", "m1", "r2", "test-model", usage(1, 1, 0, 0), false),
            asst(ago(3), Some("/home/u/alpha"), "s1", "m9", "r9", "<synthetic>", usage(999, 999, 0, 0), false),
            json!("not an object"),
        ],
    );
    let (_, r) = scan(&t);
    let p = project(&r, ProjRange::All, "alpha");
    assert_eq!(p.messages, 2);
    assert_eq!(
        (
            p.tokens.input,
            p.tokens.output,
            p.tokens.cache_read,
            p.tokens.cache_write
        ),
        (11, 201, 1000, 300)
    );
    assert_eq!(p.total_tokens, 11 + 201 + 1000 + 300);
    assert_eq!(p.sessions, 1);
    // $1/$2/$0.5/$1.5 per million
    let want = (11.0 * 1.0 + 201.0 * 2.0 + 1000.0 * 0.5 + 300.0 * 1.5) / 1e6;
    assert!((p.cost - want).abs() < 1e-12, "{} vs {want}", p.cost);
    assert_eq!(r.range(ProjRange::All).projects.len(), 1);
}

#[test]
fn cache_creation_is_split_by_ttl_when_given() {
    let t = Tree::new();
    t.write(
        "claude/projects/-home-u-alpha/s1.jsonl",
        &[asst(
            ago(1),
            Some("/home/u/alpha"),
            "s1",
            "m1",
            "r1",
            "test-model-2",
            json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 3000,
                   "cache_creation": {"ephemeral_5m_input_tokens": 1000, "ephemeral_1h_input_tokens": 2000}}),
            false,
        )],
    );
    let (_, r) = scan(&t);
    let p = project(&r, ProjRange::All, "alpha");
    assert_eq!(p.tokens.cache_write, 3000);
    assert!((p.cost - (1000.0 * 1.5 + 2000.0 * 3.0) / 1e6).abs() < 1e-12);
}

#[test]
fn projects_subagents_folders_and_windows_paths() {
    let t = Tree::new();
    t.write(
        "claude/projects/-home-u-alpha/s1.jsonl",
        &[asst(
            ago(2),
            Some("/home/u/alpha"),
            "s1",
            "m1",
            "r1",
            "test-model",
            usage(100, 0, 0, 0),
            false,
        )],
    );
    // a subagent transcript below the session folder; its lines say isSidechain false, the path says it is
    t.write(
        "claude/projects/-home-u-alpha/s1/subagents/agent-7.jsonl",
        &[asst(
            ago(2),
            Some("/home/u/alpha"),
            "s1",
            "m2",
            "r2",
            "test-model",
            usage(50, 0, 0, 0),
            false,
        )],
    );
    // a Windows cwd
    t.write(
        "claude/projects/C--Users-me-proj/s2.jsonl",
        &[asst(
            ago(2),
            Some("C:\\Users\\me\\proj"),
            "s2",
            "m3",
            "r3",
            "test-model",
            usage(7, 0, 0, 0),
            false,
        )],
    );
    // no cwd: the folder name gives the project
    t.write(
        "claude/projects/-home-u-beta/s3.jsonl",
        &[asst(
            ago(2),
            None,
            "s3",
            "m4",
            "r4",
            "test-model",
            usage(3, 0, 0, 0),
            true,
        )],
    );
    let (s, r) = scan(&t);
    let a = project(&r, ProjRange::All, "alpha");
    assert_eq!((a.messages, a.tokens.input), (2, 150));
    assert_eq!((a.sidechain_messages, a.sidechain_tokens), (1, 50));
    assert_eq!(project(&r, ProjRange::All, "proj").tokens.input, 7);
    let b = project(&r, ProjRange::All, "beta");
    assert_eq!((b.tokens.input, b.sidechain_messages), (3, 1));
    let src = &s.sources()[0];
    assert!(src.exists && src.files == 4);
    assert!(src.note.contains("1 of them subagent"), "{}", src.note);
}

#[test]
fn a_message_copied_into_a_resumed_session_file_counts_once() {
    let t = Tree::new();
    let m = asst(
        ago(2),
        Some("/home/u/alpha"),
        "s1",
        "m1",
        "r1",
        "test-model",
        usage(100, 0, 0, 0),
        false,
    );
    t.write(
        "claude/projects/-home-u-alpha/s1.jsonl",
        std::slice::from_ref(&m),
    );
    t.write("claude/projects/-home-u-alpha/s1-resumed.jsonl", &[m]);
    let (_, r) = scan(&t);
    assert_eq!(project(&r, ProjRange::All, "alpha").messages, 1);
}

#[test]
fn garbage_and_missing_folders_never_fail() {
    let t = Tree::new();
    let p = t.write("claude/projects/-x/s.jsonl", &[]);
    std::fs::write(
        &p,
        b"{not json\n\xff\xfe\x00 binary \"usage\" \"assistant\"\n\n{\"type\":\"assistant\"}\n",
    )
    .unwrap();
    // a file that is really a directory name, and an unreadable-looking odd name
    std::fs::create_dir_all(t.claude().join("-y/dir.jsonl")).unwrap();
    let (s, r) = scan(&t);
    assert!(r.range(ProjRange::All).projects.is_empty());
    assert!(s.sources().iter().all(|i| !i.label.is_empty()));
    // nothing installed at all
    let empty = Tree::new();
    let (s, r) = scan(&empty);
    assert_eq!(r.range(ProjRange::All).totals.messages, 0);
    assert!(!s.sources()[0].exists);
}

#[test]
fn an_oversized_line_is_skipped_and_the_rest_of_the_file_still_counts() {
    let t = Tree::new();
    let p = t.write(
        "claude/projects/-home-u-alpha/s1.jsonl",
        &[asst(
            ago(2),
            Some("/home/u/alpha"),
            "s1",
            "m1",
            "r1",
            "test-model",
            usage(1, 0, 0, 0),
            false,
        )],
    );
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"{\"usage\":\"assistant\",\"pad\":\"").unwrap();
    f.write_all(&vec![b'x'; MAX_LINE_BYTES + 1000]).unwrap();
    f.write_all(b"\"}\n").unwrap();
    writeln!(
        f,
        "{}",
        asst(
            ago(1),
            Some("/home/u/alpha"),
            "s1",
            "m2",
            "r2",
            "test-model",
            usage(2, 0, 0, 0),
            false
        )
    )
    .unwrap();
    drop(f);
    let (_, r) = scan(&t);
    assert_eq!(project(&r, ProjRange::All, "alpha").tokens.input, 3);
}

// ---- ranges ---------------------------------------------------------------------------------

#[test]
fn today_7d_30d_and_all_with_a_timezone_offset() {
    let t = Tree::new();
    // now = 2026-10-14 12:00 UTC. At UTC+2 the local day started 2026-10-13 22:00 UTC.
    let at = |m: i64| now() - Duration::minutes(m);
    let rows = [
        (at(14 * 60 + 1), "today_boundary_before", 1), // 21:59 UTC the day before: yesterday locally
        (at(13 * 60 + 59), "today_boundary_after", 2), // 22:01 UTC: today locally
        (
            now() - Duration::days(7) + Duration::minutes(1),
            "inside_7d",
            4,
        ),
        (
            now() - Duration::days(7) - Duration::minutes(1),
            "outside_7d",
            8,
        ),
        (
            now() - Duration::days(30) + Duration::minutes(1),
            "inside_30d",
            16,
        ),
        (
            now() - Duration::days(30) - Duration::minutes(1),
            "outside_30d",
            32,
        ),
    ];
    for (i, (when, project, tokens)) in rows.iter().enumerate() {
        t.write(
            &format!("claude/projects/-p{i}/s.jsonl"),
            &[asst(
                *when,
                Some(&format!("/w/{project}")),
                "s",
                &format!("m{i}"),
                "r",
                "test-model",
                usage(*tokens, 0, 0, 0),
                false,
            )],
        );
    }
    let mut s = Scanner::new(t.roots(false));
    s.scan();
    let pr = prices(&t.dir);
    let utc = aggregate(s.requests(), &pr, now(), 0);
    let plus2 = aggregate(s.requests(), &pr, now(), 120);
    let names = |r: &Report, range| {
        let mut n: Vec<String> = r
            .range(range)
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        n.sort();
        n
    };
    // local midnight at UTC+2 is 22:00 UTC the day before
    assert_eq!(names(&plus2, ProjRange::Today), ["today_boundary_after"]);
    // at UTC midnight (00:00) nothing of the rows above is "today" (the earliest is 21:59 the day before)
    assert!(names(&utc, ProjRange::Today).is_empty());
    assert_eq!(plus2.today().totals.total_tokens, 2);
    // 7 days is rolling from now
    assert_eq!(
        names(&utc, ProjRange::Days7),
        ["inside_7d", "today_boundary_after", "today_boundary_before"]
    );
    assert_eq!(names(&utc, ProjRange::Days30).len(), 5);
    assert_eq!(names(&utc, ProjRange::All).len(), 6);
    // Today is the same set as 'since local midnight' even for a negative offset (UTC-8: day began 08:00 UTC)
    let minus8 = aggregate(s.requests(), &pr, now(), -480);
    assert!(names(&minus8, ProjRange::Today).is_empty());
    assert_eq!(
        local_day_start(now(), 120),
        Utc.with_ymd_and_hms(2026, 10, 13, 22, 0, 0).unwrap()
    );
    assert_eq!(
        local_day_start(now(), -480),
        Utc.with_ymd_and_hms(2026, 10, 14, 8, 0, 0).unwrap()
    );
}

#[test]
fn projects_rank_by_cost_then_tokens_and_totals_add_up() {
    let t = Tree::new();
    let w = |n: &str, id: &str, u: Value| {
        t.write(
            &format!("claude/projects/-{n}/s.jsonl"),
            &[asst(
                ago(1),
                Some(&format!("/w/{n}")),
                &format!("s-{n}"),
                id,
                "r",
                "test-model",
                u,
                false,
            )],
        );
    };
    w("cheap", "m1", usage(1000, 0, 0, 0));
    w("pricey", "m2", usage(0, 1_000_000, 0, 0)); // $2
    w("mid", "m3", usage(1_000_000, 0, 0, 0)); // $1
    let (_, r) = scan(&t);
    let names: Vec<&str> = r
        .range(ProjRange::All)
        .projects
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["pricey", "mid", "cheap"]);
    let tot = &r.range(ProjRange::All).totals;
    assert_eq!((tot.messages, tot.projects, tot.sessions), (3, 3, 3));
    assert_eq!(tot.total_tokens, 1000 + 1_000_000 + 1_000_000);
    assert!((tot.cost - (2.0 + 1.0 + 0.001)).abs() < 1e-9);
}

// ---- Grok -----------------------------------------------------------------------------------

fn update_line(at: &str, ticks: Value, models: Value) -> Value {
    json!({"method": "session/update", "timestamp": at,
           "params": {"update": {"usage": {"costUsdTicks": ticks, "modelUsage": models}}}})
}

#[test]
fn grok_updates_give_exact_cost_in_ticks_and_tokens_per_model() {
    let t = Tree::new();
    t.write(
        "grok/sessions/%2Fhome%2Fu%2Fgrokproj/sess-1/updates.jsonl",
        &[
            update_line(
                &iso(ago(2)),
                json!(25_000_000_000u64), // $2.50
                json!({"grok-4-build": {"inputTokens": 1000, "cachedReadTokens": 400, "outputTokens": 50, "cacheCreationTokens": 7, "costUsdTicks": 25_000_000_000u64}}),
            ),
            // timestamp as epoch milliseconds; no per-model detail: cost kept, tokens unknown
            json!({"method": "_x.ai/session/update", "timestamp": (ago(1).timestamp() * 1000),
                   "params": {"update": {"usage": {"costUsdTicks": "5000000000"}}}}),
            json!({"method": "other", "params": {"update": {"usage": {"costUsdTicks": 1}}}}),
            json!({"method": "session/update", "timestamp": iso(ago(1)), "params": {"update": {"usage": {"costUsdTicks": -4}}}}),
        ],
    );
    let (s, r) = scan(&t);
    let p = project(&r, ProjRange::All, "grokproj");
    assert_eq!(p.vendor, VendorId::Grok);
    assert_eq!(p.messages, 2);
    assert!((p.cost - 3.0).abs() < 1e-9, "{}", p.cost); // 2.5 + 0.5
    assert_eq!(
        (
            p.tokens.input,
            p.tokens.cache_read,
            p.tokens.output,
            p.tokens.cache_write
        ),
        (600, 400, 50, 7)
    );
    assert_eq!(p.unpriced_messages, 0);
    assert_eq!(s.sources()[1].files, 1);
    assert!((COST_USD_PER_TICK - 1e-10).abs() < 1e-20);
}

fn log_line(at: DateTime<Utc>, sid: &str, msg: &str, ctx: Value) -> Value {
    json!({"ts": iso(at), "sid": sid, "msg": msg, "ctx": ctx})
}

#[test]
fn the_grok_log_gives_tokens_only_with_model_and_folder_from_its_events() {
    let t = Tree::new();
    std::fs::create_dir_all(t.grok()).unwrap();
    std::fs::write(
        t.grok().join("config.toml"),
        "[ui]\ndefault = \"nope\"\n[models]\ndefault = \"grok-default\"\n",
    )
    .unwrap();
    t.write(
        "grok/logs/unified.jsonl",
        &[
            log_line(ago(5), "s1", "session created", json!({"cwd": "/home/u/logproj"})),
            log_line(ago(4), "s1", "shell.turn.inference_done", json!({"prompt_tokens": 1000, "cached_prompt_tokens": 600, "completion_tokens": 40, "loop_index": 0})),
            log_line(ago(3), "s1", "model changed", json!({"model": "grok-code-build"})),
            // loop indexes repeat across turns: both count
            log_line(ago(2), "s1", "shell.turn.inference_done", json!({"prompt_tokens": 10, "cached_prompt_tokens": 0, "completion_tokens": 5, "loop_index": 0})),
            // incomplete events are skipped
            log_line(ago(2), "s1", "shell.turn.inference_done", json!({"prompt_tokens": 10})),
            log_line(ago(2), "s1", "something else", json!({})),
        ],
    );
    let (s, r) = scan(&t);
    let p = project(&r, ProjRange::All, "logproj");
    assert_eq!(p.messages, 2);
    assert_eq!(
        (p.tokens.input, p.tokens.cache_read, p.tokens.output),
        (410, 600, 45)
    );
    // tokens only: the cost is unknown, never a guess
    assert_eq!((p.cost, p.unpriced_messages), (0.0, 2));
    assert_eq!(fmt_cost(p.cost, p.messages, p.unpriced_messages), "n/a");
    assert!(s.sources()[2].note.contains("no cost"));
    // models
    let reqs: Vec<&Request> = s.requests().collect();
    let mut models: Vec<&str> = reqs.iter().map(|r| r.model.as_str()).collect();
    models.sort();
    assert_eq!(models, ["grok-code", "grok-default"]); // "-build" is dropped; the default comes from config.toml
    assert_eq!(
        grok_default_model(&t.grok().join("config.toml")).as_deref(),
        Some("grok-default")
    );
}

#[test]
fn a_grok_session_with_updates_ignores_its_log_events() {
    let t = Tree::new();
    t.write(
        "grok/sessions/%2Fw%2Fboth/s1/updates.jsonl",
        &[update_line(
            &iso(ago(2)),
            json!(10_000_000_000u64),
            json!({"grok-4": {"inputTokens": 100, "outputTokens": 10}}),
        )],
    );
    t.write(
        "grok/logs/unified.jsonl",
        &[
            log_line(ago(3), "s1", "session created", json!({"cwd": "/w/both"})),
            log_line(ago(2), "s1", "shell.turn.inference_done", json!({"prompt_tokens": 100, "cached_prompt_tokens": 0, "completion_tokens": 10, "loop_index": 0})),
            // another session only in the log
            log_line(ago(3), "s2", "session created", json!({"cwd": "/w/logonly"})),
            log_line(ago(2), "s2", "shell.turn.inference_done", json!({"prompt_tokens": 5, "cached_prompt_tokens": 0, "completion_tokens": 1, "loop_index": 0})),
        ],
    );
    let (_, r) = scan(&t);
    let both = project(&r, ProjRange::All, "both");
    assert_eq!((both.messages, both.tokens.input), (1, 100));
    assert!((both.cost - 1.0).abs() < 1e-9);
    assert_eq!(project(&r, ProjRange::All, "logonly").messages, 1);
}

#[test]
fn claude_and_grok_projects_stay_separate_even_with_the_same_name() {
    let t = Tree::new();
    t.write(
        "claude/projects/-w-same/s.jsonl",
        &[asst(
            ago(1),
            Some("/w/same"),
            "s",
            "m1",
            "r",
            "test-model",
            usage(10, 0, 0, 0),
            false,
        )],
    );
    t.write(
        "grok/sessions/%2Fw%2Fsame/g1/updates.jsonl",
        &[update_line(
            &iso(ago(1)),
            json!(1_000_000_000u64),
            json!({"grok-4": {"inputTokens": 20}}),
        )],
    );
    let (_, r) = scan(&t);
    let ps = &r.range(ProjRange::All).projects;
    assert_eq!(ps.len(), 2);
    assert!(
        ps.iter().any(|p| p.vendor == VendorId::Claude)
            && ps.iter().any(|p| p.vendor == VendorId::Grok)
    );
}

// ---- rescan cache and the persisted index -----------------------------------------------------

fn touch_append(p: &Path, line: Value) {
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    writeln!(f, "{line}").unwrap();
}

#[test]
fn a_rescan_with_nothing_changed_reads_no_files_and_is_fast() {
    let t = Tree::new();
    for i in 0..200 {
        t.write(
            &format!("claude/projects/-p{}/s{i}.jsonl", i % 7),
            &[asst(
                ago(1),
                Some("/w/p"),
                &format!("s{i}"),
                &format!("m{i}"),
                "r",
                "test-model",
                usage(1, 1, 0, 0),
                false,
            )],
        );
    }
    let mut s = Scanner::new(t.roots(false));
    let first = s.scan();
    assert_eq!((first.files, first.parsed, first.reused), (200, 200, 0));
    let again = s.scan();
    assert_eq!((again.parsed, again.reused, again.removed), (0, 200, 0));
    assert_eq!(again.requests, 200);
    assert!(
        again.elapsed < std::time::Duration::from_millis(500),
        "{:?}",
        again.elapsed
    );
}

#[test]
fn only_changed_files_are_read_again_and_vanished_files_are_dropped() {
    let t = Tree::new();
    let a = t.write(
        "claude/projects/-a/s.jsonl",
        &[asst(
            ago(2),
            Some("/w/a"),
            "s",
            "m1",
            "r",
            "test-model",
            usage(1, 0, 0, 0),
            false,
        )],
    );
    let b = t.write(
        "claude/projects/-b/s.jsonl",
        &[asst(
            ago(2),
            Some("/w/b"),
            "s",
            "m2",
            "r",
            "test-model",
            usage(2, 0, 0, 0),
            false,
        )],
    );
    let mut s = Scanner::new(t.roots(false));
    s.scan();
    touch_append(
        &a,
        asst(
            ago(1),
            Some("/w/a"),
            "s",
            "m3",
            "r",
            "test-model",
            usage(4, 0, 0, 0),
            false,
        ),
    );
    let sum = s.scan();
    assert_eq!((sum.parsed, sum.reused), (1, 1));
    assert_eq!(s.requests().count(), 3);
    std::fs::remove_file(&b).unwrap();
    let sum = s.scan();
    assert_eq!((sum.removed, sum.files), (1, 1));
    assert_eq!(s.requests().count(), 2);
}

#[test]
fn the_persisted_index_makes_the_first_scan_after_a_restart_read_nothing() {
    let t = Tree::new();
    t.write(
        "claude/projects/-a/s.jsonl",
        &[
            asst(ago(2), Some("/w/a"), "s", "m1", "r", "test-model-2", json!({"input_tokens": 5, "output_tokens": 6, "cache_creation_input_tokens": 7, "cache_creation": {"ephemeral_1h_input_tokens": 7}}), true),
            asst(ago(1), None, "s2", "m2", "r", "test-model", usage(1, 2, 3, 4), false),
        ],
    );
    t.write(
        "grok/sessions/%2Fw%2Fg/s1/updates.jsonl",
        &[update_line(
            &iso(ago(1)),
            json!(123456789u64),
            json!({"grok-4": {"inputTokens": 9, "cachedReadTokens": 2}}),
        )],
    );
    let mut s = Scanner::new(t.roots(true));
    let first = s.scan();
    assert_eq!(first.parsed, 2);
    let before: Vec<Request> = s.requests().cloned().collect();
    assert!(t.dir.join("index.json").is_file());

    let mut again = Scanner::new(t.roots(true));
    let sum = again.scan();
    assert_eq!((sum.parsed, sum.reused), (0, 2), "{sum:?}");
    let mut after: Vec<Request> = again.requests().cloned().collect();
    let mut before = before;
    let key = |r: &Request| (r.at, r.session.clone(), r.model.clone());
    before.sort_by_key(key);
    after.sort_by_key(key);
    assert_eq!(before, after); // every field survives the round trip

    // a corrupt or foreign-version index is ignored, not fatal
    for junk in [
        "{broken",
        r#"{"version": 99, "strings": [], "files": []}"#,
        r#"{"version":1,"strings":[],"files":[{"p":"x","r":[[1]]}]}"#,
    ] {
        std::fs::write(t.dir.join("index.json"), junk).unwrap();
        let mut s = Scanner::new(t.roots(true));
        assert_eq!(s.scan().parsed, 2, "{junk}");
    }
}

// ---- text output ----------------------------------------------------------------------------

#[test]
fn formatting_helpers() {
    assert_eq!(fmt_tokens(950), "950");
    assert_eq!(fmt_tokens(1234), "1.2k");
    assert_eq!(fmt_tokens(812_345), "812k");
    assert_eq!(fmt_tokens(2_664_257), "2.66M");
    assert_eq!(fmt_tokens(3_200_000_000), "3.20B");
    assert_eq!(fmt_cost(12.345, 3, 0), "$12.35");
    assert_eq!(fmt_cost(0.0, 3, 0), "$0.00");
    assert_eq!(fmt_cost(123.4, 3, 0), "$123");
    assert_eq!(fmt_cost(1.5, 3, 1), "$1.50+");
    assert_eq!(fmt_cost(0.0, 3, 3), "n/a");
    assert_eq!(ProjRange::parse("7"), Some(ProjRange::Days7));
    assert_eq!(ProjRange::parse("ALL"), Some(ProjRange::All));
    assert_eq!(ProjRange::parse("x"), None);
}

#[test]
fn the_text_report_shows_projects_today_and_sources_but_no_content() {
    let t = Tree::new();
    t.write(
        "claude/projects/-w-alpha/s.jsonl",
        &[asst(
            ago(1),
            Some("/w/alpha"),
            "s",
            "m1",
            "r",
            "test-model",
            usage(1_000_000, 0, 0, 0),
            false,
        )],
    );
    let (s, r) = scan(&t);
    let text = projects_text(&r, ProjRange::Days7, s.sources());
    assert!(
        text.contains("Today: $1.00 (1.00M tokens, 1 requests)"),
        "{text}"
    );
    assert!(text.contains("alpha") && text.contains("Claude"), "{text}");
    assert!(
        text.contains("Claude Code transcripts") && text.contains("exists, 1 file"),
        "{text}"
    );
    assert!(
        text.contains("Grok CLI log") && text.contains("missing"),
        "{text}"
    );
    assert!(
        !text.contains("SECRET"),
        "message content must never be printed"
    );
    let empty = projects_text(&r, ProjRange::Today, s.sources());
    assert!(empty.contains("Projects, Today"));
}

/// Review #21: a turn with several models where only the turn's total cost is recorded keeps it.
#[test]
fn a_multi_model_grok_turn_with_only_a_parent_cost_is_priced() {
    let t = Tree::new();
    t.write(
        "grok/sessions/%2Fhome%2Fu%2Fmulti/sess-1/updates.jsonl",
        &[update_line(
            &iso(ago(2)),
            json!(30_000_000_000u64), // $3.00 for the whole turn
            json!({
                "grok-4-build": {"inputTokens": 1000, "outputTokens": 50},
                "grok-code-build": {"inputTokens": 200, "outputTokens": 10},
            }),
        )],
    );
    let (_, r) = scan(&t);
    let p = project(&r, ProjRange::All, "multi");
    assert_eq!(p.messages, 2);
    assert_eq!(p.unpriced_messages, 0, "{p:?}");
    assert!((p.cost - 3.0).abs() < 1e-9, "{}", p.cost);
    assert_eq!(p.tokens.input, 1200);
    // children that carry their own cost keep it (the parent total is not added again)
    let t = Tree::new();
    t.write(
        "grok/sessions/%2Fhome%2Fu%2Fmulti/sess-1/updates.jsonl",
        &[update_line(
            &iso(ago(2)),
            json!(30_000_000_000u64),
            json!({
                "grok-4-build": {"inputTokens": 1, "costUsdTicks": 10_000_000_000u64},
                "grok-code-build": {"inputTokens": 1, "costUsdTicks": 20_000_000_000u64},
            }),
        )],
    );
    let (_, r) = scan(&t);
    assert!((project(&r, ProjRange::All, "multi").cost - 3.0).abs() < 1e-9);
}
