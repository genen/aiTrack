// SPDX-License-Identifier: GPL-3.0-or-later
//! Where credentials may be sent, and the test hooks that are compiled out of real builds.
//!
//! A bearer token goes only to `https://api.anthropic.com` (Claude),
//! `https://cli-chat-proxy.grok.com` (Grok) or `https://chatgpt.com` (Codex): no `http:`, no user
//! info, no other host, no redirect. The environment variables that used to replace those URLs
//! (`AITRACK_USAGE_URL`, `AITRACK_GROK_URL`, `AITRACK_CODEX_URL`) and the ones that choose the
//! Grok program and its arguments (`AITRACK_GROK_CMD`, `AITRACK_GROK_REFRESH_ARGS`) exist for the
//! tests only: they are read, and a non-HTTPS URL is accepted, **only** when the crate is built with the `test-hooks` feature. The
//! feature is switched on for `cargo test` through the self dev-dependency in `Cargo.toml`
//! (the integration tests start the real binary against a mock server); `cargo build`,
//! `cargo run` and release builds never have it.

/// The host of the Claude usage endpoint.
pub const CLAUDE_HOST: &str = "api.anthropic.com";
/// The host of the Grok billing endpoint.
pub const GROK_HOST: &str = "cli-chat-proxy.grok.com";
/// The host of the Codex usage endpoint.
pub const CODEX_HOST: &str = "chatgpt.com";

/// Are the test hooks compiled in?
pub const TEST_HOOKS: bool = cfg!(feature = "test-hooks");

/// The value of environment variable `name`, but only in test builds (otherwise always `None`).
pub fn test_env(name: &str) -> Option<String> {
    if !TEST_HOOKS {
        return None;
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// May credentials be sent to `url`? In real builds only `https://<host>[:443]/...` without user
/// info; with the `test-hooks` feature any URL (a mock server on localhost).
pub fn check(url: &str, host: &str) -> Result<(), String> {
    if TEST_HOOKS {
        return Ok(());
    }
    check_strict(url, host)
}

/// The real rule, whatever the build: `https://<host>[:443]` and a path, no user info.
pub fn check_strict(url: &str, host: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("endpoint is not an https:// URL")?;
    if url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("endpoint URL contains control characters".into());
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err("endpoint URL contains user info".into());
    }
    let hostname = authority.strip_suffix(":443").unwrap_or(authority);
    if !hostname.eq_ignore_ascii_case(host) {
        return Err(format!("endpoint host is not {host}"));
    }
    Ok(())
}

/// A header value must not carry control characters (no CR/LF smuggling of extra headers).
pub fn header_value_ok(v: &str) -> bool {
    !v.is_empty() && !v.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// Upper bound for a usage / billing / credentials body (bytes).
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Read at most [`MAX_BODY_BYTES`] from `r`; `Err` when there is more.
pub fn read_capped(r: &mut dyn std::io::Read) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    r.take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("network error: {e}"))?;
    if buf.len() as u64 > MAX_BODY_BYTES {
        return Err("response is larger than 1 MiB".into());
    }
    Ok(buf)
}
