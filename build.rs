// SPDX-License-Identifier: GPL-3.0-or-later
//! Captures the build identity (git short hash, dirty flag, commit count, UTC date) for
//! `src/buildinfo.rs`. Every lookup is optional: without git (a source archive) the values are
//! "unknown" and the build still succeeds.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// UTC "YYYY-MM-DD HH:MM" from the system clock (days-to-civil, no date crate needed).
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Windows only: the .exe's icon and version information (`aitrack.rc`). A failure here (no
/// resource compiler installed) is only a warning; the program works without the embedded icon.
fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=aitrack.rc");
    println!("cargo:rerun-if-changed=assets/icon/aitrack.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut nums: Vec<String> = version
        .split(['.', '-', '+'])
        .map(|p| {
            p.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .map(|p| if p.is_empty() { "0".to_owned() } else { p })
        .take(4)
        .collect();
    while nums.len() < 4 {
        nums.push("0".into());
    }
    // write the .rc with the version baked in (macro quoting differs between windres and rc.exe)
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let template = std::fs::read_to_string("aitrack.rc").unwrap_or_default();
    let icon = format!("{manifest_dir}/assets/icon/aitrack.ico").replace('\\', "/");
    let rc = template
        .replace("assets/icon/aitrack.ico", &icon)
        .replace("VERSION_NUM", &nums.join(","))
        .replace("VERSION_STR", &format!("\"{version}\""));
    let rc_path = format!("{out_dir}/aitrack.rc");
    if std::fs::write(&rc_path, rc).is_err() {
        println!("cargo:warning=could not write the Windows resource file");
        return;
    }
    let result = embed_resource::compile(&rc_path, embed_resource::NONE);
    if let Err(e) = result.manifest_optional() {
        println!("cargo:warning=could not embed the Windows icon: {e}");
    }
}

fn main() {
    embed_windows_resources();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    // where git keeps HEAD and the refs (a worktree's `.git` is a file: ask git)
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
    }
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        println!("cargo:rerun-if-changed={common}/packed-refs");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={common}/{r}");
        }
    }
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=AITRACK_BUILD_HASH={hash}");
    println!("cargo:rustc-env=AITRACK_BUILD_NUMBER={count}");
    println!("cargo:rustc-env=AITRACK_BUILD_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=AITRACK_BUILD_DATE={}", utc_now());
}
