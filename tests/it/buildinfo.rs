// SPDX-License-Identifier: GPL-3.0-or-later
//! The build line: formatting with/without dirty, the unknown fallbacks, the binary's `--version`.
use aitrack::buildinfo::*;
use std::process::Command;

fn b(number: &str, hash: &str, dirty: bool, date: &str) -> Build {
    Build {
        version: "0.1.0".into(),
        number: number.into(),
        hash: hash.into(),
        dirty,
        date: date.into(),
    }
}

#[test]
fn the_fixed_fixture_formats_like_the_documented_example() {
    assert_eq!(
        format_line(&b("214", "8acc7ac", false, "2026-10-14 10:55")),
        "aiTrack v0.1.0 \u{b7} build 214 (8acc7ac) \u{b7} 2026-10-14 10:55"
    );
}

#[test]
fn a_dirty_tree_is_marked_after_the_hash() {
    assert_eq!(
        format_line(&b("214", "8acc7ac", true, "2026-10-14 10:55")),
        "aiTrack v0.1.0 \u{b7} build 214 (8acc7ac+dirty) \u{b7} 2026-10-14 10:55"
    );
}

#[test]
fn missing_git_data_degrades_without_failing() {
    assert_eq!(
        format_line(&b("unknown", "unknown", false, "unknown")),
        "aiTrack v0.1.0 \u{b7} build unknown"
    );
    assert_eq!(
        format_line(&b("unknown", "unknown", false, "2026-10-14 10:55")),
        "aiTrack v0.1.0 \u{b7} build unknown \u{b7} 2026-10-14 10:55"
    );
    assert_eq!(
        format_line(&b("214", "unknown", false, "unknown")),
        "aiTrack v0.1.0 \u{b7} build 214"
    );
    assert_eq!(
        format_line(&b("unknown", "8acc7ac", true, "unknown")),
        "aiTrack v0.1.0 \u{b7} build ? (8acc7ac+dirty)"
    );
    assert_eq!(
        format_line(&b("", "", false, "")),
        "aiTrack v0.1.0 \u{b7} build unknown"
    );
}

#[test]
fn this_build_has_a_plausible_line() {
    let c = current();
    assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
    let l = line();
    assert!(l.starts_with("aiTrack v"), "{l}");
    assert!(l.contains("build"), "{l}");
    // the date, when captured, has the documented shape
    if c.date != "unknown" {
        assert_eq!(c.date.len(), 16, "{}", c.date);
        assert_eq!(&c.date[10..11], " ");
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aitrack"))
}

#[test]
fn the_binary_prints_the_build_for_version_and_sources_and_the_copy_log_starts_with_it() {
    for flag in ["--version", "-V"] {
        let out = bin().arg(flag).output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.starts_with("aiTrack v"), "{text}");
        assert_eq!(text.trim_end(), line());
    }
    let dir = std::env::temp_dir().join(format!("aitrack-bi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = bin();
    cmd.arg("--sources");
    for k in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
    ] {
        cmd.env(k, &dir);
    }
    cmd.env("CLAUDE_CONFIG_DIR", &dir).env("GROK_HOME", &dir);
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().next(), Some(line().as_str()), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(aitrack::view::copy_log(&[]).starts_with(&line()));
}

#[test]
fn a_build_with_test_hooks_says_so_in_its_line() {
    // `cargo test --release` / `--all-targets` can compile the binary with the hooks on; the line
    // (Settings, --version) must then show it, and an ordinary build must not mention it.
    let line = line();
    assert_eq!(
        line.contains("test hooks"),
        aitrack::endpoints::TEST_HOOKS,
        "{line}"
    );
    assert!(!format_line(&b("1", "abc", false, "2026-10-14 10:55")).contains("test hooks"));
}
