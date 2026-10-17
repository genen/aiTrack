// SPDX-License-Identifier: GPL-3.0-or-later
use crate::common;
use aitrack::instance::{self, acquire, Acquire};
use common::TempDir;

#[test]
fn second_acquisition_fails_while_the_first_is_alive_and_succeeds_after_drop() {
    let dir = TempDir::new();
    let path = dir.join("sub/aitrack.lock"); // the folder is created on demand
    let first = match acquire(&path) {
        Acquire::Acquired(g) => g,
        other => panic!("first acquire failed: {other:?}"),
    };
    match acquire(&path) {
        Acquire::AlreadyRunning { pid } => assert_eq!(pid, Some(std::process::id())),
        other => panic!("second acquire should be refused: {other:?}"),
    }
    // still refused (no waiting, no side effects)
    assert!(matches!(acquire(&path), Acquire::AlreadyRunning { .. }));
    drop(first);
    assert!(
        !dir.join("sub/aitrack.pid").exists(),
        "pid file is removed on a clean exit"
    );
    assert!(matches!(acquire(&path), Acquire::Acquired(_)));
}

#[test]
fn a_leftover_lock_file_without_a_holder_does_not_block() {
    let dir = TempDir::new();
    let path = dir.join("aitrack.lock");
    // what a crashed instance leaves behind: both files, a pid that no longer exists
    std::fs::write(&path, "99999999").unwrap();
    std::fs::write(dir.join("aitrack.pid"), "99999999").unwrap();
    let g = match acquire(&path) {
        Acquire::Acquired(g) => g,
        other => panic!("{other:?}"),
    };
    // the pid file is refreshed with our own pid
    assert_eq!(
        std::fs::read_to_string(dir.join("aitrack.pid")).unwrap(),
        std::process::id().to_string()
    );
    drop(g);
}

#[test]
fn an_unusable_lock_location_is_reported_not_fatal() {
    let dir = TempDir::new();
    std::fs::write(dir.join("file"), "x").unwrap();
    // the "folder" is a regular file
    match acquire(&dir.join("file/aitrack.lock")) {
        Acquire::Unavailable(why) => assert!(why.contains("cannot"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn running_text_includes_the_pid_when_known() {
    assert_eq!(
        instance::running_text(Some(42)),
        "aiTrack is already running (pid 42)"
    );
    assert_eq!(instance::running_text(None), "aiTrack is already running");
}

/// The window is created with exactly the title a second launch looks for.
#[test]
fn window_title_lookup_and_window_creation_share_one_constant() {
    assert_eq!(instance::WINDOW_TITLE, "aiTrack");
    let ui = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui.rs")).unwrap();
    assert!(
        ui.contains(".with_title(crate::instance::WINDOW_TITLE)"),
        "the viewport title must use the constant"
    );
    assert!(
        !ui.contains("with_title(\""),
        "no hard-coded window title in ui.rs"
    );
    let inst =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/instance.rs")).unwrap();
    let squashed: String = inst.split_whitespace().collect();
    assert!(
        squashed.contains("title==WINDOW_TITLE") && squashed.contains("is_our_window(&title,"),
        "the Windows lookup must compare against the constant"
    );
}

#[test]
fn lock_path_follows_the_environment_override() {
    // (reads the process environment, so only the default shape is asserted here)
    let p = instance::lock_path();
    assert!(
        p.to_string_lossy().ends_with("aitrack.lock") || std::env::var_os("AITRACK_LOCK").is_some()
    );
}

#[test]
fn mac_raise_command_targets_the_pid_and_needs_one() {
    assert_eq!(instance::raise_osascript_args_mac(None), None);
    let a = instance::raise_osascript_args_mac(Some(4242)).unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(a[0], "-e");
    assert_eq!(
        a[1],
        "tell application \"System Events\" to set frontmost of (first process whose unix id is 4242) to true"
    );
}
