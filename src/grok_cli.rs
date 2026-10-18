// SPDX-License-Identifier: GPL-3.0-or-later
//! The Grok CLI as a program we may start: finding it, and the "Refresh" that starts it once so it
//! fetches billing (the CLI logs a `billing: fetched credits config` line to `unified.jsonl` and
//! refreshes its login whenever it starts), then ends it again.
//!
//! The refresh never sends a prompt and never spends credits: the CLI is started with no
//! arguments (test builds only: `AITRACK_GROK_REFRESH_ARGS`), no window (`CREATE_NO_WINDOW` on Windows),
//! stdout/stderr on the null device and stdin on the null device (on Unix an open pipe that is never
//! written to, because the CLI quits at once when its input is closed), so it cannot read a user's
//! console or type into it. It
//! is not started at all while another `grok` process is running. We wait until a newer billing
//! line shows up in the log (or the timeout runs out), let it run [`REFRESH_GRACE`] longer and
//! then terminate the process (the whole
//! tree on Windows, where `grok` may be a `.cmd` shim around another program).

use crate::grok::{log_path, read_snapshots, Snapshot};
use crate::sources::{Env, Os};
use chrono::{DateTime, Duration, Utc};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant};

/// `CREATE_NO_WINDOW`: a console program runs without a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Offer the refresh when the newest Grok reading is older than this (minutes), or absent.
pub const REFRESH_AFTER_MIN: f64 = 60.0;
/// After the billing line shows up, let the CLI run this much longer (it refreshes its login after
/// fetching billing) before it is ended.
pub const REFRESH_GRACE: StdDuration = StdDuration::from_secs(5);
/// How long to wait for the CLI to write a billing line.
pub const REFRESH_TIMEOUT: StdDuration = StdDuration::from_secs(25);
const POLL_EVERY: StdDuration = StdDuration::from_millis(200);

/// Override for the program to run (an absolute path or a name looked up on PATH); test builds only
/// (see [`crate::endpoints`]).
pub const ENV_CMD: &str = "AITRACK_GROK_CMD";
/// Override for the arguments of the refresh run (split on whitespace; default none); test builds
/// only.
pub const ENV_REFRESH_ARGS: &str = "AITRACK_GROK_REFRESH_ARGS";

/// Executable names, in order, for `os`.
pub fn exe_names(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &["grok.exe", "grok.cmd", "grok.bat"],
        _ => &["grok"],
    }
}

/// Folders besides PATH where the CLI is usually installed (the detector's folder first): the
/// Grok folder itself and its `bin`, `versions/*` (self-updating installs keep one folder per
/// version), the usual per-user bin folders, `/opt/homebrew/bin` and `/usr/local/bin` on macOS, `%LOCALAPPDATA%\grok` / `Programs\Grok`, npm, scoop
/// and Chocolatey shims.
pub fn extra_dirs(env: &Env) -> Vec<PathBuf> {
    let gh = crate::grok::grok_home(env);
    let mut v = vec![gh.join("bin"), gh.clone()];
    for sub in ["versions", "releases"] {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(gh.join(sub))
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        // newest-looking first
        dirs.sort_by(|a, b| b.cmp(a));
        for d in dirs {
            v.push(d.join("bin"));
            v.push(d);
        }
    }
    if let Some(h) = &env.home {
        v.push(h.join(".local").join("bin"));
        v.push(h.join(".cargo").join("bin"));
        v.push(h.join(".bun").join("bin"));
        v.push(h.join(".grok").join("bin"));
        v.push(h.join("bin"));
        v.push(h.join("scoop").join("shims"));
    }
    if let Some(l) = &env.localappdata {
        for d in ["grok", "Grok"] {
            v.push(l.join(d));
            v.push(l.join(d).join("bin"));
            v.push(l.join("Programs").join(d));
            v.push(l.join("Programs").join(d).join("bin"));
        }
        v.push(l.join("Microsoft").join("WinGet").join("Links"));
    }
    if let Some(a) = &env.appdata {
        v.push(a.join("npm"));
    }
    if env.os == Os::Mac {
        // a GUI-launched app has a minimal PATH: Homebrew (Apple Silicon, Intel) bin folders
        v.push(PathBuf::from("/opt/homebrew/bin"));
        v.push(PathBuf::from("/usr/local/bin"));
    }
    if env.os == Os::Windows {
        if let Some(pd) = std::env::var_os("ProgramData") {
            v.push(PathBuf::from(pd).join("chocolatey").join("bin"));
        }
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            v.push(PathBuf::from(pf).join("Grok"));
        }
    }
    v
}

/// Find the CLI: `$AITRACK_GROK_CMD`, then PATH, then [`extra_dirs`] (Store-app aliases in `WindowsApps` are skipped). `None` when it is not there
/// (the Refresh control is then hidden).
pub fn find_cli(
    env: &Env,
    path_var: Option<&OsStr>,
    cmd_override: Option<&OsStr>,
) -> Option<PathBuf> {
    let names = exe_names(env.os);
    if let Some(c) = cmd_override.filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        if p.components().count() > 1 {
            return p.is_file().then_some(p);
        }
        let found = path_var
            .into_iter()
            .flat_map(std::env::split_paths)
            .map(|d| d.join(c))
            .find(|p| p.is_file());
        return found;
    }
    let dirs = path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .chain(extra_dirs(env));
    for d in dirs.filter(|d| !is_store_alias_dir(d)) {
        for n in names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// `%LOCALAPPDATA%\Microsoft\WindowsApps` holds the app-execution aliases of Store apps: a
/// `grok.exe` there (the Grok Store app) opens the GUI app, it is not the CLI. Never searched.
fn is_store_alias_dir(d: &Path) -> bool {
    d.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("WindowsApps"))
}

/// [`find_cli`] over the real process environment.
pub fn find_cli_here(env: &Env) -> Option<PathBuf> {
    find_cli(
        env,
        std::env::var_os("PATH").as_deref(),
        // the program override is a test hook; real builds always look for `grok` themselves
        crate::endpoints::test_env(ENV_CMD)
            .map(std::ffi::OsString::from)
            .as_deref(),
    )
}

/// The arguments of the refresh run: none, unless `$AITRACK_GROK_REFRESH_ARGS` says otherwise.
pub fn refresh_args(over: Option<&str>) -> Vec<OsString> {
    over.map(|s| s.split_whitespace().map(OsString::from).collect())
        .unwrap_or_default()
}

/// Should the Refresh control be offered? When Grok data is absent or older than
/// [`REFRESH_AFTER_MIN`].
pub fn needs_refresh(newest: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match newest {
        None => true,
        Some(t) => (now - t) > Duration::seconds((REFRESH_AFTER_MIN * 60.0) as i64),
    }
}

// ---------------------------------------------------------------- running it

/// A started process.
pub trait Child {
    /// The operating-system process id, when there is one.
    fn id(&self) -> Option<u32> {
        None
    }
    /// `Some(exit code)` once it ended (`-1` when killed or unknown).
    fn try_wait(&mut self) -> Result<Option<i32>, String>;
    /// End it (and, on Windows, its children).
    fn kill(&mut self);
}

/// What the refresh needs from the machine (fakes in tests).
pub trait Host {
    /// Is a `grok` process running (any user session we can see)?
    fn grok_running(&self) -> bool;
    /// Start `program` with `args`: no window, null stdio.
    fn spawn(&self, program: &Path, args: &[OsString]) -> Result<Box<dyn Child>, String>;
}

/// The failure when the CLI ran but no newer billing line appeared (signed out? offline?).
pub const NO_NEW_DATA: &str = "no new data (Grok did not fetch billing)";

/// The weekly percent of the newest billing line in the log (what a refresh just read).
pub fn newest_pct(env: &Env) -> Option<f64> {
    newest(&log_path(env)).map(|s| s.pct)
}

/// How a refresh ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A newer billing line appeared in the log.
    Refreshed,
}

pub struct RefreshCfg {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub log: PathBuf,
    pub timeout: StdDuration,
    /// How long the CLI keeps running after the billing line appeared (see [`REFRESH_GRACE`]).
    pub grace: StdDuration,
    pub poll_every: StdDuration,
}

impl RefreshCfg {
    pub fn new(env: &Env, program: PathBuf) -> Self {
        RefreshCfg {
            program,
            // extra arguments are a test hook: real builds start the CLI with none
            args: refresh_args(crate::endpoints::test_env(ENV_REFRESH_ARGS).as_deref()),
            log: log_path(env),
            timeout: REFRESH_TIMEOUT,
            grace: REFRESH_GRACE,
            poll_every: POLL_EVERY,
        }
    }
}

fn newest(log: &Path) -> Option<Snapshot> {
    read_snapshots(log).ok().and_then(|v| v.into_iter().next())
}

/// Start the CLI once, wait for a new billing line, end the CLI. `Err` is a one-line reason.
pub fn refresh(cfg: &RefreshCfg, host: &dyn Host) -> Result<Outcome, String> {
    refresh_tracked(cfg, host, &AtomicU32::new(0))
}

/// [`refresh`], publishing the process id of the running CLI in `active` (0 when there is none)
/// so that the app can end it on exit with [`kill_pid`].
pub fn refresh_tracked(
    cfg: &RefreshCfg,
    host: &dyn Host,
    active: &AtomicU32,
) -> Result<Outcome, String> {
    struct Clear<'a>(&'a AtomicU32);
    impl Drop for Clear<'_> {
        fn drop(&mut self) {
            self.0.store(0, Ordering::SeqCst);
        }
    }
    let _clear = Clear(active);
    if host.grok_running() {
        return Err("Grok is already running; it updates the log itself".into());
    }
    let baseline = newest(&cfg.log).map(|s| s.fetched_at);
    let is_new = || newest(&cfg.log).is_some_and(|s| baseline.is_none_or(|b| s.fetched_at > b));
    let mut child = host
        .spawn(&cfg.program, &cfg.args)
        .map_err(|e| format!("Could not start Grok: {e}"))?;
    active.store(child.id().unwrap_or(0), Ordering::SeqCst);
    let start = Instant::now();
    loop {
        if is_new() {
            // give the CLI a moment to finish its login refresh; it may also end by itself
            let until = Instant::now() + cfg.grace;
            while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(cfg.poll_every.min(until - Instant::now()));
            }
            child.kill();
            return Ok(Outcome::Refreshed);
        }
        match child.try_wait() {
            Ok(Some(code)) => {
                // it may have written the line just before it ended
                return if is_new() {
                    Ok(Outcome::Refreshed)
                } else {
                    Err(format!(
                        "no new data (Grok did not fetch billing; it ended with exit code {code})"
                    ))
                };
            }
            Ok(None) => {}
            Err(e) => {
                child.kill();
                return Err(format!("Could not watch Grok: {e}"));
            }
        }
        if start.elapsed() >= cfg.timeout {
            child.kill();
            return Err(NO_NEW_DATA.to_string());
        }
        std::thread::sleep(cfg.poll_every);
    }
}

/// The real host: `std::process`, no window, null stdio; process list through `tasklist` (Windows)
/// or `/proc` / `pgrep`.
pub struct SystemHost;

struct RealChild {
    child: std::process::Child,
    /// The open, silent input pipe (Unix); closed when the refresh ends.
    stdin: Option<std::process::ChildStdin>,
}

impl Child for RealChild {
    fn id(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        self.child
            .try_wait()
            .map(|s| s.map(|s| s.code().unwrap_or(-1)))
            .map_err(|e| e.to_string())
    }

    fn kill(&mut self) {
        self.stdin = None;
        // Unix: the child leads its own process group (see `command`), so the group ends with it.
        // A child that already ended has been reaped and its pid may belong to someone else now:
        // nothing is signalled then.
        #[cfg(unix)]
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        #[cfg(unix)]
        kill_group(self.child.id());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // the whole tree: `grok` may be a .cmd shim that started the real program
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &self.child.id().to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// End process `pid` (and, on Windows, its children) without waiting: used when the window closes
/// during a refresh. The CLI is killed rather than asked to quit: it has no quit request we can
/// send to a hidden TUI, and the refresh already ends it this way once the billing line is in.
pub fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    kill_group(pid);
}

/// Unix: SIGKILL the process group led by `pid` (the refresh child, started with `process_group(0)`),
/// so a `#!/usr/bin/env node` wrapper takes its real program with it; then `pid` itself in case it
/// was not a group leader.
#[cfg(unix)]
fn kill_group(pid: u32) {
    for target in [format!("-{pid}"), pid.to_string()] {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `PATH` for the refresh child: the inherited one plus the usual user locations of a `grok`
/// script's interpreter (a GUI-launched app on macOS gets only `/usr/bin:/bin:/usr/sbin:/sbin`).
/// Existing entries keep their order and come first; nothing is duplicated. Only the child sees it.
pub fn extended_path(existing: Option<&OsStr>, home: Option<&Path>) -> OsString {
    let mut dirs: Vec<PathBuf> = existing
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        extra.push(h.join(".grok").join("bin"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin"));
    extra.push(PathBuf::from("/usr/local/bin"));
    if let Some(h) = home {
        extra.push(h.join(".local").join("bin"));
    }
    for d in extra {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| existing.map(OsString::from).unwrap_or_default())
}

/// The command for a refresh run (also what the tests look at).
pub fn command(program: &Path, args: &[OsString]) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    // Unix: the CLI ends at once when its input is closed, so it gets an open pipe that nothing is
    // ever written to (held by `RealChild` until the refresh is over). Elsewhere: the null device.
    #[cfg(unix)]
    let stdin = std::process::Stdio::piped();
    #[cfg(not(unix))]
    let stdin = std::process::Stdio::null();
    c.args(args)
        .stdin(stdin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // own process group, so ending the refresh can end the whole tree (see `kill_group`)
        c.process_group(0);
        c.env(
            "PATH",
            extended_path(
                std::env::var_os("PATH").as_deref(),
                crate::util::home_dir().as_deref(),
            ),
        );
    }
    c
}

impl Host for SystemHost {
    fn grok_running(&self) -> bool {
        grok_process_running()
    }

    fn spawn(&self, program: &Path, args: &[OsString]) -> Result<Box<dyn Child>, String> {
        let mut child = command(program, args).spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take();
        Ok(Box::new(RealChild { child, stdin }))
    }
}

#[cfg(windows)]
fn grok_process_running() -> bool {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq grok.exe", "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => listing_has_grok(&String::from_utf8_lossy(&o.stdout)),
        // cannot tell: do not start a second copy
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn grok_process_running() -> bool {
    if let Ok(rd) = std::fs::read_dir("/proc") {
        return rd.filter_map(Result::ok).any(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|c| c.trim().eq_ignore_ascii_case("grok"))
        });
    }
    std::process::Command::new("pgrep")
        .args(["-x", "grok"])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Does a `tasklist /FO CSV /NH` listing contain a `grok.exe` row?
pub fn listing_has_grok(text: &str) -> bool {
    text.lines().any(|l| {
        l.trim_start_matches('"')
            .split('"')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case("grok.exe"))
    })
}

// ---------------------------------------------------------------- on a thread

/// Where the Refresh button stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Idle,
    Running,
    /// One-line reason, shown for a while.
    Failed(String),
}
