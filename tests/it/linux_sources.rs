// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux data-source discovery with temp-dir trees (HOME and XDG_CONFIG_HOME injected, never the
//! real home). Everything here is synthetic: it proves the path logic and the "skip what we do not
//! understand" behaviour, not that a real Linux Claude desktop cache looks like these files.
use crate::common::TempDir;
use aitrack::cache;
use aitrack::detect::{claude_present, detect};
use aitrack::sources::{self, DesktopCache, Env, Os, UsageSource};
use chrono::{DateTime, TimeZone, Utc};
use std::path::{Path, PathBuf};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 18, 12, 0, 0).unwrap()
}

fn env(root: &Path, xdg: bool) -> Env {
    let mut e = Env::new(Os::Linux);
    e.home = Some(root.join("home"));
    e.xdg_config_home = xdg.then(|| root.join("xdg"));
    e
}

fn mkdir(p: impl AsRef<Path>) -> PathBuf {
    std::fs::create_dir_all(p.as_ref()).unwrap();
    p.as_ref().to_path_buf()
}

#[test]
fn default_folders_follow_xdg_then_dot_config() {
    let d = TempDir::new();
    let with_xdg = env(d.path(), true);
    assert_eq!(
        sources::desktop_data_dirs(&with_xdg),
        vec![d.join("xdg/Claude")]
    );
    assert_eq!(
        sources::desktop_history_candidates(&with_xdg),
        vec![d.join("xdg/Claude/plan-usage-history.json")]
    );
    assert_eq!(
        sources::cache_dir_of(&d.join("xdg/Claude")),
        d.join("xdg/Claude/Cache/Cache_Data")
    );
    let no_xdg = env(d.path(), false);
    assert_eq!(
        sources::desktop_data_dirs(&no_xdg),
        vec![d.join("home/.config/Claude")]
    );
}

#[test]
fn desktop_folder_with_cache_data_is_present_and_scanned() {
    let d = TempDir::new();
    let e = env(d.path(), true);
    let cache_dir = mkdir(d.join("xdg/Claude/Cache/Cache_Data"));
    for n in ["0090689ba677d2c3_0", "index", "f_000001"] {
        std::fs::write(cache_dir.join(n), b"dummy").unwrap();
    }
    assert!(
        claude_present(&e),
        "the desktop app folder alone shows Claude"
    );
    assert!(detect(&e).claude && !detect(&e).grok);
    let scan = cache::scan_usage_entries_at(&cache_dir, now());
    assert!(scan.dir_exists);
    assert_eq!(scan.files, 3);
    assert!(scan.entries.is_empty());
    let err = DesktopCache { env: e }.read(now()).unwrap_err().to_string();
    assert!(err.contains("no usage entry"), "{err}");
}

#[test]
fn desktop_folder_without_cache_data_falls_back() {
    let d = TempDir::new();
    let e = env(d.path(), true);
    mkdir(d.join("xdg/Claude"));
    assert!(claude_present(&e));
    let err = DesktopCache { env: e.clone() }
        .read(now())
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"), "{err}");
    // the chain still ends in the file based sources, which report plain "not found" states
    let chain = sources::build_chain(&e, "http://127.0.0.1:1/none", None);
    assert_eq!(chain.len(), 4);
}

#[test]
fn nothing_installed_hides_the_vendor() {
    let d = TempDir::new();
    mkdir(d.join("home"));
    let e = env(d.path(), true);
    assert!(!claude_present(&e));
    assert!(!detect(&e).claude && !detect(&e).grok);
    assert!(sources::linux_sandbox_data_dirs(&d.join("home")).is_empty());
}

#[test]
fn missing_credentials_file_is_a_normal_state() {
    let d = TempDir::new();
    let e = env(d.path(), true);
    mkdir(d.join("home/.claude")); // Claude Code folder, no .credentials.json (login is in the keyring)
    assert!(claude_present(&e));
    let text = sources::diagnose(&e, now());
    assert!(text.contains("keyring"), "{text}");
    assert!(text.contains("no usable token"));
}

#[test]
fn flatpak_and_snap_candidates_are_guesses_that_only_appear_when_present() {
    let d = TempDir::new();
    let home = d.join("home");
    mkdir(home.join(".var/app/com.example.Claude/config/Claude"));
    mkdir(home.join(".var/app/org.other.Editor/config/Claude")); // name has no "claude"
    mkdir(home.join("snap/claude-desktop/current/.config/Claude"));
    mkdir(home.join("snap/claude-empty")); // no data folder inside
    let e = env(d.path(), true);
    let dirs = sources::desktop_data_dirs(&e);
    assert_eq!(
        dirs,
        vec![
            d.join("xdg/Claude"),
            home.join(".var/app/com.example.Claude/config/Claude"),
            home.join("snap/claude-desktop/current/.config/Claude"),
        ]
    );
    assert!(claude_present(&e), "a sandboxed install alone shows Claude");
    let text = sources::diagnose(&e, now());
    assert!(text.contains("Flatpak/Snap"));
    assert!(text.contains(": 2"), "{text}");
}

#[test]
fn windows_and_mac_candidates_are_unchanged_by_the_linux_guesses() {
    let d = TempDir::new();
    mkdir(d.join("home/.var/app/com.example.Claude/config/Claude"));
    let mut e = env(d.path(), true);
    e.os = Os::Mac;
    assert_eq!(sources::desktop_data_dirs(&e).len(), 1);
    e.os = Os::Windows;
    e.appdata = Some(d.join("Roaming"));
    e.localappdata = Some(d.join("Local"));
    assert_eq!(sources::desktop_data_dirs(&e).len(), 1);
}

#[test]
fn sources_output_is_structure_only() {
    let d = TempDir::new();
    let e = env(d.path(), true);
    let cache_dir = mkdir(d.join("xdg/Claude/Cache/Cache_Data"));
    std::fs::write(cache_dir.join("0090689ba677d2c3_0"), b"SECRET-CONTENT-xyz").unwrap();
    std::fs::write(d.join("home/.claude.json"), "").ok();
    let text = sources::diagnose(&e, now());
    assert!(text.contains("linux folders checked"));
    assert!(text.contains("[exists ]") && text.contains("(1 entries)"));
    assert!(!text.contains("SECRET-CONTENT"));
    assert!(
        !text.contains("0090689ba677d2c3"),
        "file names are not printed"
    );
}

#[test]
fn unfamiliar_simple_cache_entries_are_skipped_quietly() {
    let d = TempDir::new();
    let dir = mkdir(d.join("xdg/Claude/Cache/Cache_Data"));
    // simple-cache style name, content we do not understand, in several shapes
    std::fs::write(dir.join("0090689ba677d2c3_0"), b"not a cache entry at all").unwrap();
    std::fs::write(dir.join("1190689ba677d2c3_0"), b"").unwrap();
    std::fs::write(dir.join("2290689ba677d2c3_0"), vec![0xABu8; 5000]).unwrap();
    // right magic, absurd key length
    let mut f = 0xfcfb6d1ba7725c30u64.to_le_bytes().to_vec();
    f.extend(5u32.to_le_bytes());
    f.extend(u32::MAX.to_le_bytes());
    f.extend([0u8; 8]);
    std::fs::write(dir.join("3390689ba677d2c3_0"), f).unwrap();
    // names of a different layout are ignored too
    std::fs::write(dir.join("0090689ba677d2c3_s"), b"x").unwrap();
    let scan = cache::scan_usage_entries_at(&dir, now());
    assert_eq!(scan.files, 5);
    assert!(scan.entries.is_empty());
    assert!(scan.notes.is_empty(), "no log spam: {:?}", scan.notes);
}
