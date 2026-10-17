// SPDX-License-Identifier: GPL-3.0-or-later
//! Which build is this? `build.rs` captures the git short hash, a dirty flag, the commit count (the
//! human "build number") and the UTC build date; [`line`] turns them into one line:
//!
//! `aiTrack v0.1.0 · build 214 (8acc7ac) · 2026-10-14 10:55`
//!
//! (`8acc7ac+dirty` when the working tree had uncommitted changes). Without git, every part that
//! is unknown is left out (`aiTrack v0.1.0 · build unknown`).

/// The pieces of a build line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    /// Commit count; `"unknown"` when git was not available.
    pub number: String,
    /// Short commit hash; `"unknown"` when git was not available.
    pub hash: String,
    pub dirty: bool,
    /// UTC `YYYY-MM-DD HH:MM`; `"unknown"` when not captured.
    pub date: String,
}

/// This binary's build.
pub fn current() -> Build {
    Build {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        number: env!("AITRACK_BUILD_NUMBER").to_owned(),
        hash: env!("AITRACK_BUILD_HASH").to_owned(),
        dirty: env!("AITRACK_BUILD_DIRTY") == "1",
        date: env!("AITRACK_BUILD_DATE").to_owned(),
    }
}

fn known(s: &str) -> bool {
    !s.is_empty() && s != "unknown"
}

/// The display line for `b`.
pub fn format_line(b: &Build) -> String {
    let mut s = format!("aiTrack v{}", b.version);
    if !known(&b.number) && !known(&b.hash) {
        s.push_str(" \u{b7} build unknown");
    } else {
        s.push_str(" \u{b7} build ");
        s.push_str(if known(&b.number) { &b.number } else { "?" });
        if known(&b.hash) {
            s.push_str(&format!(
                " ({}{})",
                b.hash,
                if b.dirty { "+dirty" } else { "" }
            ));
        } else if b.dirty {
            s.push_str(" (+dirty)");
        }
    }
    if known(&b.date) {
        s.push_str(" \u{b7} ");
        s.push_str(&b.date);
    }
    s
}

/// The line for the running binary. A binary compiled with the `test-hooks` feature (only
/// `cargo test` / `--all-targets` builds; never the release build) says so.
pub fn line() -> String {
    let mut s = format_line(&current());
    if crate::endpoints::TEST_HOOKS {
        s.push_str(" \u{b7} test hooks");
    }
    s
}
