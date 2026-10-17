// SPDX-License-Identifier: GPL-3.0-or-later
//! `aitrack --projects` through the real binary, with HOME / CLAUDE_CONFIG_DIR / GROK_HOME pointed
//! at a temp tree.
use crate::common;
use chrono::{Duration, SecondsFormat, Utc};
use common::TempDir;
use serde_json::json;
use std::io::Write;
use std::process::Command;

fn bin(dir: &TempDir) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_aitrack"));
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        c.env(k, dir.path());
    }
    c.env("CLAUDE_CONFIG_DIR", dir.join("cc"))
        .env("GROK_HOME", dir.join("grok"))
        .env("AITRACK_LOCK", dir.join("lock"));
    c
}

fn write(path: std::path::PathBuf, lines: &[serde_json::Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn ts(h: i64) -> String {
    (Utc::now() - Duration::hours(h)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn asst(h: i64, cwd: &str, id: &str, input: u64) -> serde_json::Value {
    json!({"type": "assistant", "timestamp": ts(h), "cwd": cwd, "sessionId": "s1", "requestId": id,
           "message": {"id": id, "model": "claude-sonnet-4", "usage": {"input_tokens": input, "output_tokens": 10},
                       "content": "VERY SECRET PROMPT"}})
}

fn run(dir: &TempDir, args: &[&str]) -> (bool, String) {
    let out = bin(dir).args(args).output().unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn prints_the_ranked_table_today_and_the_source_folders() {
    let dir = TempDir::new();
    write(
        dir.join("cc/projects/-w-alpha/s1.jsonl"),
        &[
            asst(1, "/w/alpha", "a1", 4_000_000),
            asst(2, "/w/alpha", "a2", 10),
        ],
    );
    write(
        dir.join("cc/projects/-w-beta/s2.jsonl"),
        &[asst(1, "/w/beta", "b1", 100)],
    );
    // 30 days ago: only in the 30 days / all ranges
    write(
        dir.join("cc/projects/-w-old/s3.jsonl"),
        &[asst(24 * 20, "/w/old", "o1", 100)],
    );
    write(
        dir.join("grok/sessions/%2Fw%2Fgrokproj/g1/updates.jsonl"),
        &[json!({"method": "session/update", "timestamp": ts(1),
                 "params": {"update": {"usage": {"costUsdTicks": 10_000_000_000u64,
                     "modelUsage": {"grok-4": {"inputTokens": 500, "outputTokens": 5}}}}}})],
    );
    let (ok, text) = run(&dir, &["--projects", "7"]);
    assert!(ok, "{text}");
    assert!(text.contains("Today: $"), "{text}");
    assert!(text.contains("Projects, 7 days:"), "{text}");
    let a = text.find("alpha").expect("alpha");
    let b = text.find("beta").expect("beta");
    let g = text.find("grokproj").expect("grokproj");
    assert!(
        a < b && g < b,
        "alpha ($12+) and grokproj ($1) outrank beta: {text}"
    );
    assert!(!text.contains("old "), "outside 7 days: {text}");
    assert!(text.contains("Grok"), "{text}");
    assert!(
        text.contains("Claude Code transcripts") && text.contains("exists, 3 files"),
        "{text}"
    );
    assert!(
        text.contains("Grok sessions") && text.contains("exists, 1 file"),
        "{text}"
    );
    assert!(
        text.contains("Grok CLI log") && text.contains("missing"),
        "{text}"
    );
    assert!(!text.contains("SECRET"), "no message content: {text}");
    // the index was written next to the data (HOME/.aitrack)
    assert!(dir.join(".aitrack/transcript-index.json").is_file());

    let (ok, text30) = run(&dir, &["--projects", "30"]);
    assert!(ok && text30.contains("old"), "{text30}");
    let (ok, text_all) = run(&dir, &["--projects", "all"]);
    assert!(ok && text_all.contains("Projects, All:"), "{text_all}");
    // the default range is 7 days; the value can be inline
    let (_, d) = run(&dir, &["--projects"]);
    assert!(d.contains("Projects, 7 days:"));
    let (_, i) = run(&dir, &["--projects=30"]);
    assert!(i.contains("Projects, 30 days:"));
}

#[test]
fn works_with_nothing_installed_and_rejects_a_bad_range() {
    let dir = TempDir::new();
    let (ok, text) = run(&dir, &["--projects"]);
    assert!(ok, "{text}");
    assert!(text.contains("no local transcripts"), "{text}");
    assert!(text.contains("missing"), "{text}");
    let (ok, text) = run(&dir, &["--projects=banana"]);
    assert!(!ok && text.contains("expected 7, 30 or all"), "{text}");
}
