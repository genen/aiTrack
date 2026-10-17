// SPDX-License-Identifier: GPL-3.0-or-later
//! scripts/prune-target.sh on a fake cargo target directory: what goes, what stays, the refusals.
#![cfg(unix)]
use crate::common;
use common::TempDir;
use std::path::{Path, PathBuf};
use std::process::Command;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/prune-target.sh")
}

/// A file of `mb` megabytes last modified `minutes_ago` minutes ago.
fn file(dir: &Path, name: &str, mb: usize, minutes_ago: u64) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, vec![0u8; mb * 1024 * 1024 + 10]).unwrap();
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(minutes_ago * 60);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(t)
        .unwrap();
    p
}

fn fake_target(t: &TempDir) -> PathBuf {
    let target = t.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n",
    )
    .unwrap();
    target
}

fn run(target: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("bash")
        .arg(script())
        .arg("--target")
        .arg(target)
        .args(args)
        .output()
        .unwrap();
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
fn superseded_old_builds_go_the_newest_and_recent_and_small_ones_stay() {
    let t = TempDir::new();
    let target = fake_target(&t);
    let deps = target.join("debug/deps");
    let old = file(&deps, "alerts-aaaaaaaaaaaaaaaa", 3, 120);
    let fresh = file(&deps, "alerts-bbbbbbbbbbbbbbbb", 3, 10);
    let building = file(&deps, "alerts-cccccccccccccccc", 3, 2);
    let small_old = file(&deps, "tiny-dddddddddddddddd.rmeta", 0, 500);
    let small_super = file(&deps, "tiny-eeeeeeeeeeeeeeee.rmeta", 0, 600);
    let example = file(
        &target.join("debug/examples"),
        "render-eeeeeeeeeeeeeeee",
        3,
        90,
    );
    let triple = file(
        &target.join("x86_64-pc-windows-gnu/debug/deps"),
        "libaitrack-ffffffffffffffff.rlib",
        3,
        200,
    );
    let src = t.join("src.rs");
    std::fs::write(&src, "fn main() {}").unwrap();
    let (ok, text) = run(&target, &[]);
    assert!(ok, "{text}");
    assert!(!old.exists(), "{text}");
    // the only build of an example / crate is the newest, however old: it is what cargo would reuse
    assert!(example.exists() && triple.exists(), "{text}");
    assert!(
        fresh.exists()
            && building.exists()
            && small_old.exists()
            && small_super.exists()
            && src.exists(),
        "{text}"
    );
    assert!(text.contains("Reclaimed 3 MB in 1 item(s)"), "{text}");
    assert!(
        text.contains("removed debug/deps/alerts-aaaaaaaaaaaaaaaa"),
        "{text}"
    );
    // disk-full mode also takes the old newest builds
    let (ok, text) = run(&target, &["--emergency"]);
    assert!(ok, "{text}");
    assert!(!example.exists() && !triple.exists(), "{text}");
    assert!(fresh.exists() && building.exists(), "{text}");
}

#[test]
fn a_live_dependency_with_no_newer_build_survives_every_mode_but_emergency() {
    let t = TempDir::new();
    let target = fake_target(&t);
    let deps = target.join("release/deps");
    // a dependency (not ours) built long ago and still what cargo links: nothing newer exists
    let rlib = file(&deps, "libserde_json-0123456789abcdef.rlib", 3, 5000);
    let rmeta = file(&deps, "libserde_json-0123456789abcdef.rmeta", 3, 5000);
    let so = file(&deps, "libserde_derive-fedcba9876543210.so", 3, 5000);
    let plain = file(&deps, "build-script-build", 3, 5000); // not hash-suffixed: not judged
    for args in [&[][..], &["--all-but-newest"], &["--minutes", "0"]] {
        let (ok, text) = run(&target, args);
        assert!(ok, "{args:?}: {text}");
        assert!(
            rlib.exists() && rmeta.exists() && so.exists() && plain.exists(),
            "{args:?}: {text}"
        );
        assert!(text.contains("Reclaimed 0 MB in 0 item(s)"), "{text}");
    }
    // a newer build of the same crate supersedes the old one, and only that one goes
    let newer = file(&deps, "libserde_json-1111111111111111.rlib", 3, 100);
    let (ok, text) = run(&target, &[]);
    assert!(
        ok && !rlib.exists() && newer.exists() && rmeta.exists(),
        "{text}"
    );
    // --emergency is the explicit way to drop everything big and old
    let (ok, text) = run(&target, &["--emergency"]);
    assert!(
        ok && !so.exists() && !rmeta.exists() && !plain.exists(),
        "{text}"
    );
}

#[test]
fn dry_run_removes_nothing_and_minutes_is_respected() {
    let t = TempDir::new();
    let target = fake_target(&t);
    let deps = target.join("debug/deps");
    let a = file(&deps, "x-aaaaaaaaaaaaaaaa", 3, 40);
    let newest = file(&deps, "x-1111111111111111", 3, 35);
    let (ok, text) = run(&target, &["--dry-run"]);
    assert!(ok && a.exists(), "{text}");
    assert!(
        text.contains("would remove debug/deps/x-aaaaaaaaaaaaaaaa")
            && text.contains("Would reclaim 3 MB"),
        "{text}"
    );
    // 40 minutes old is not stale at --minutes 60 ...
    let (_, text) = run(&target, &["--minutes", "60"]);
    assert!(a.exists(), "{text}");
    // ... but is at the default 30; the 5 minute safety floor applies even to --minutes 0
    let young_old = file(&deps, "y-bbbbbbbbbbbbbbbb", 3, 3);
    let young_new = file(&deps, "y-cccccccccccccccc", 3, 2);
    let (_, text) = run(&target, &["--minutes", "0"]);
    assert!(!a.exists() && newest.exists(), "{text}");
    assert!(young_old.exists() && young_new.exists(), "{text}");
}

#[test]
fn all_but_newest_keeps_one_artifact_per_name_and_extension() {
    let t = TempDir::new();
    let target = fake_target(&t);
    let deps = target.join("debug/deps");
    // recent enough that --minutes alone would keep them all (but older than the 5 minute floor)
    let v1 = file(&deps, "cache-1111111111111111", 3, 20);
    let v2 = file(&deps, "cache-2222222222222222", 3, 15);
    let v3 = file(&deps, "cache-3333333333333333", 3, 10);
    let lib_old = file(&deps, "libaitrack-1111111111111111.rlib", 3, 20);
    let lib_new = file(&deps, "libaitrack-2222222222222222.rlib", 3, 10);
    let rmeta = file(&deps, "libaitrack-3333333333333333.rmeta", 3, 10);
    let other = file(&deps, "stats-4444444444444444", 3, 12);
    let busy = file(&deps, "cache-5555555555555555", 3, 1); // modified a minute ago: never touched
    let (ok, text) = run(&target, &["--all-but-newest"]);
    assert!(ok, "{text}");
    assert!(!v1.exists() && !v2.exists(), "{text}");
    // v3 is not the newest build any more (busy is), and it is past the 5 minute floor
    assert!(!v3.exists() && busy.exists(), "{text}");
    assert!(
        !lib_old.exists() && lib_new.exists() && rmeta.exists() && other.exists(),
        "{text}"
    );
}

#[test]
fn incremental_directories_older_than_45_minutes_go() {
    let t = TempDir::new();
    let target = fake_target(&t);
    let inc = target.join("debug/incremental");
    let old = inc.join("aitrack-old");
    let new = inc.join("aitrack-new");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&new).unwrap();
    std::fs::write(old.join("data"), b"x").unwrap();
    let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 120);
    std::fs::File::open(&old)
        .unwrap()
        .set_modified(ago)
        .unwrap();
    let (ok, text) = run(&target, &[]);
    assert!(ok && !old.exists() && new.exists(), "{text}");
}

#[test]
fn it_refuses_a_directory_that_is_not_a_cargo_target_and_never_follows_symlinks() {
    let t = TempDir::new();
    let not_target = t.join("stuff");
    let victim = file(&not_target.join("debug/deps"), "x-aaaaaaaaaaaaaaaa", 3, 500);
    let (ok, text) = run(&not_target, &[]);
    assert!(
        !ok && victim.exists() && text.contains("CACHEDIR.TAG"),
        "{text}"
    );
    // a symlink inside a real target must not lead the script out of it
    let target = fake_target(&t);
    let outside = file(&t.join("outside"), "o-bbbbbbbbbbbbbbbb", 3, 500);
    std::os::unix::fs::symlink(t.join("outside"), target.join("debug")).unwrap();
    let (_, text) = run(&target, &[]);
    assert!(outside.exists(), "{text}");
    std::os::unix::fs::symlink(&outside, t.join("link-target-file")).unwrap();
    // bad options are reported
    let (ok, text) = run(&target, &["--minutes", "soon"]);
    assert!(!ok && text.contains("whole numbers"), "{text}");
    let (ok, _) = run(&target, &["--frobnicate"]);
    assert!(!ok);
}
