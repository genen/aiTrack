// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the real binary headless against the mock endpoint.
use crate::common;
use common::*;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aitrack"))
}

/// Point every location the sources look at into `dir`, so the real machine's files never leak in.
fn isolate(cmd: &mut Command, dir: &std::path::Path) {
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(k, dir);
    }
}

#[test]
fn once_collects_one_reading_into_the_data_file() {
    let dir = TempDir::new();
    write_creds(dir.path(), "tok-cli", now_ms() + 3_600_000.0);
    let mock = Mock::start();
    let data = dir.join("out/usage.jsonl");
    let mut cmd = bin();
    cmd.args(["--once", "--interval", "5", "--data"])
        .arg(&data)
        .env("AITRACK_USAGE_URL", &mock.url)
        .env("CLAUDE_CONFIG_DIR", dir.path());
    isolate(&mut cmd, dir.path());
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("every 5 min"), "{stdout}");
    assert!(stdout.contains("session 42%, weekly 22%"), "{stdout}");
    let text = std::fs::read_to_string(&data).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains("tok-cli"));
    assert_eq!(
        mock.seen.lock().unwrap()[0][0].as_deref(),
        Some("Bearer tok-cli")
    );
}

#[test]
fn once_without_login_exits_1_and_records_the_error() {
    let dir = TempDir::new();
    let empty = TempDir::new();
    let data = dir.join("usage.jsonl");
    let mut cmd = bin();
    cmd.args(["--once", "--data"])
        .arg(&data)
        .env("CLAUDE_CONFIG_DIR", empty.path())
        .env("AITRACK_USAGE_URL", "http://127.0.0.1:1/");
    isolate(&mut cmd, empty.path());
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("no Claude login found"));
    assert!(std::fs::read_to_string(&data)
        .unwrap()
        .contains("\"ok\":false"));
}

#[test]
fn bad_flags_are_rejected() {
    let out = bin().arg("--nope").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option"));
    assert_eq!(
        bin()
            .args(["--interval", "0"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

// ------------------------------------------------------------ single instance (real binary)

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A command with every location isolated in `dir`: own lock, data file, credentials and settings.
fn collector_cmd(dir: &TempDir, url: &str, args: &[&str]) -> Command {
    let mut cmd = bin();
    cmd.args(args)
        .env("AITRACK_LOCK", dir.join("t.lock"))
        .env("AITRACK_DATA", dir.join("usage.jsonl"))
        .env("AITRACK_SETTINGS", dir.join("settings.json"))
        .env("AITRACK_SOURCES", "oauth")
        .env("AITRACK_USAGE_URL", url)
        .env("CLAUDE_CONFIG_DIR", dir.path());
    isolate(&mut cmd, dir.path());
    cmd
}

/// Start a real `--collect` and wait until it has polled once (so it surely holds the lock).
fn start_collector(dir: &TempDir, url: &str) -> Child {
    let child = collector_cmd(dir, url, &["--collect", "--interval", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("first collector to poll", || {
        std::fs::read_to_string(dir.join("usage.jsonl"))
            .map(|t| !t.is_empty())
            .unwrap_or(false)
    });
    child
}

#[test]
fn a_second_collector_is_refused_with_exit_code_3() {
    let dir = TempDir::new();
    write_creds(dir.path(), "tok-1", now_ms() + 3_600_000.0);
    let mock = Mock::start();
    let mut first = start_collector(&dir, &mock.url);

    let out = collector_cmd(&dir, &mock.url, &["--collect"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        format!(
            "aiTrack is already running (pid {}); not starting a second collector",
            first.id()
        )
    );
    // the refused copy did not poll: still exactly one record
    assert_eq!(
        std::fs::read_to_string(dir.join("usage.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(mock.seen.lock().unwrap().len(), 1);

    // the window mode is refused too, without ever opening a window (no display needed), exit 0
    #[cfg(feature = "gui")]
    {
        let out = collector_cmd(&dir, &mock.url, &[]).output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
    }

    // after the first one dies (killed: no clean shutdown) the OS has released the lock
    first.kill().unwrap();
    first.wait().unwrap();
    let mut again = collector_cmd(&dir, &mock.url, &["--collect", "--interval", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("replacement collector to poll", || {
        std::fs::read_to_string(dir.join("usage.jsonl"))
            .map(|t| t.lines().count() >= 2)
            .unwrap_or(false)
    });
    assert!(
        again.try_wait().unwrap().is_none(),
        "the replacement keeps running"
    );
    again.kill().unwrap();
    again.wait().unwrap();
}

#[test]
fn once_and_sources_and_help_ignore_the_lock() {
    let dir = TempDir::new();
    write_creds(dir.path(), "tok-2", now_ms() + 3_600_000.0);
    let mock = Mock::start();
    let mut holder = start_collector(&dir, &mock.url);

    let out = collector_cmd(&dir, &mock.url, &["--once"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("usage.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );

    let out = collector_cmd(&dir, &mock.url, &["--sources"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("aiTrack sources"));

    let out = collector_cmd(&dir, &mock.url, &["--help"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    holder.kill().unwrap();
    holder.wait().unwrap();
}

#[test]
fn a_leftover_lock_file_does_not_block_a_new_collector() {
    let dir = TempDir::new();
    write_creds(dir.path(), "tok-3", now_ms() + 3_600_000.0);
    std::fs::write(dir.join("t.lock"), "stale").unwrap();
    std::fs::write(dir.join("t.pid"), "99999999").unwrap();
    let mock = Mock::start();
    let out = collector_cmd(&dir, &mock.url, &["--collect", "--once"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let mut c = start_collector(&dir, &mock.url);
    assert!(c.try_wait().unwrap().is_none());
    c.kill().unwrap();
    c.wait().unwrap();
}
