// SPDX-License-Identifier: GPL-3.0-or-later
//! The refresh gives the Grok CLI an open, silent input on Unix. This starts a real process, so it
//! lives in its own test binary: a child between fork and exec briefly shares every open file of
//! its parent, which would race with the file-lock tests of the shared integration binary.
#![cfg(unix)]
use aitrack::grok_cli::{self, Host};
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration as StdDuration, Instant};

/// A program that quits when its input is closed stays alive while the refresh holds the pipe open.
#[test]
fn the_cli_gets_an_open_silent_input_on_unix() {
    let host = grok_cli::SystemHost;
    let args = vec![OsString::from("-c"), OsString::from("read line")];
    let mut child = host.spawn(Path::new("/bin/sh"), &args).unwrap();
    std::thread::sleep(StdDuration::from_millis(400));
    assert_eq!(
        child.try_wait(),
        Ok(None),
        "it must still be waiting for input"
    );
    child.kill();
    let t0 = Instant::now();
    while child.try_wait() == Ok(None) {
        assert!(
            t0.elapsed() < StdDuration::from_secs(5),
            "kill did not end it"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    }
}
