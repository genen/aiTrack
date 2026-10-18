// SPDX-License-Identifier: GPL-3.0-or-later
//! Launch (or focus) the Claude desktop app from the Claude badge in the window.
//!
//! Default install locations only, tried in this order, stopping at the first that works:
//!
//! 1. the `claude://` protocol (`ShellExecuteW "open"`),
//! 2. the Microsoft Store app: `explorer.exe shell:AppsFolder\<PackageFamilyName>!Claude`, where the
//!    package family name is the name of the `Claude_*` folder under `%LOCALAPPDATA%\Packages`,
//! 3. a classic install: `%LOCALAPPDATA%\AnthropicClaude\claude.exe`, then
//!    `%LOCALAPPDATA%\Programs\Claude\Claude.exe`, if present.
//!
//! On macOS the same idea applies with `/usr/bin/open` (see [`claude_open_argvs_mac`]): the app by
//! name first, then by bundle id. The bundle id is UNVERIFIED (a guess from the Windows naming);
//! `osascript -e 'id of app "Claude"'` on the Mac settles it.
//!
//! Nothing is run through a shell string: the programs get fixed arguments. Launching never blocks
//! the window (it happens on a short-lived thread) and never panics; what was tried and how it went
//! is appended to `launch.log` in the data folder.

use std::path::{Path, PathBuf};

pub const PROTOCOL_URI: &str = "claude://";
/// AppId of the Store package's single application entry (the default; the real manifest is not readable).
pub const STORE_APP_ID: &str = "Claude";

/// Does the real thing (or a fake in tests).
pub trait Spawner: Send + Sync {
    /// Open a URI with its registered handler (`ShellExecuteW "open"`).
    fn open_uri(&self, uri: &str) -> Result<(), String>;
    /// Start a program with fixed arguments, without waiting for it.
    fn run(&self, program: &Path, args: &[String]) -> Result<(), String>;
    /// Run a short program and return its standard output (no window). Default: unsupported.
    fn capture(&self, _program: &Path, _args: &[String]) -> Result<String, String> {
        Err("not supported".into())
    }
}

/// One try.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub method: String,
    pub ok: bool,
    pub detail: Option<String>,
}

/// `Claude_*` folder names under `<localappdata>\Packages` (these are the package family names),
/// sorted, matched case-insensitively.
pub fn store_families(localappdata: &Path) -> Vec<String> {
    families_with_prefix(localappdata, "claude_")
}

/// Folder names under `<localappdata>\Packages` that start with `prefix` (case-insensitive), sorted.
pub fn families_with_prefix(localappdata: &Path, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(localappdata.join("Packages"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.to_ascii_lowercase().starts_with(prefix))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The argument `explorer.exe` activates a Store app with.
pub fn apps_folder_target(family: &str) -> String {
    format!("shell:AppsFolder\\{family}!{STORE_APP_ID}")
}

/// The default classic-install executables, in order.
pub fn classic_candidates(localappdata: &Path) -> Vec<PathBuf> {
    vec![
        localappdata.join("AnthropicClaude").join("claude.exe"),
        localappdata
            .join("Programs")
            .join("Claude")
            .join("Claude.exe"),
    ]
}

/// Try the methods in order and stop at the first that works. The returned list is everything that
/// was tried (the last entry is the successful one, if any).
pub fn launch_claude(localappdata: Option<&Path>, spawner: &dyn Spawner) -> Vec<Attempt> {
    let mut tried = Vec::new();
    let attempt = |method: String, r: Result<(), String>, tried: &mut Vec<Attempt>| -> bool {
        let ok = r.is_ok();
        tried.push(Attempt {
            method,
            ok,
            detail: r.err(),
        });
        ok
    };
    if attempt(
        format!("protocol {PROTOCOL_URI}"),
        spawner.open_uri(PROTOCOL_URI),
        &mut tried,
    ) {
        return tried;
    }
    let Some(local) = localappdata else {
        tried.push(Attempt {
            method: "store app / classic install".into(),
            ok: false,
            detail: Some("%LOCALAPPDATA% is not set".into()),
        });
        return tried;
    };
    for fam in store_families(local) {
        let target = apps_folder_target(&fam);
        let r = spawner.run(Path::new("explorer.exe"), std::slice::from_ref(&target));
        if attempt(format!("store app {target}"), r, &mut tried) {
            return tried;
        }
    }
    for exe in classic_candidates(local) {
        if !exe.is_file() {
            continue;
        }
        let r = spawner.run(&exe, &[]);
        if attempt(format!("classic install {}", exe.display()), r, &mut tried) {
            return tried;
        }
    }
    tried
}

/// Did the last attempt succeed?
pub fn succeeded(tried: &[Attempt]) -> bool {
    tried.last().is_some_and(|a| a.ok)
}

/// One text line for `launch.log`.
pub fn log_text(tried: &[Attempt]) -> String {
    let parts: Vec<String> = tried
        .iter()
        .map(|a| match (&a.detail, a.ok) {
            (_, true) => format!("{}: ok", a.method),
            (Some(d), false) => format!("{}: failed ({d})", a.method),
            (None, false) => format!("{}: failed", a.method),
        })
        .collect();
    format!(
        "{}  claude desktop launch: {}",
        crate::util::now_iso(),
        if parts.is_empty() {
            "nothing to try".into()
        } else {
            parts.join("; ")
        }
    )
}

/// Append to `<data dir>/launch.log` (best effort).
pub fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Default `launch.log` location.
pub fn log_path() -> PathBuf {
    match std::env::var_os("AITRACK_LAUNCH_LOG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::util::data_dir().join("launch.log"),
    }
}

/// Launch on a short-lived thread (never blocks the caller); `done(ok)` runs on that thread.
pub fn launch_claude_async(log: PathBuf, done: impl FnOnce(bool) + Send + 'static) {
    launch_claude_async_with(log, std::sync::Arc::new(SystemSpawner), done);
}

/// [`launch_claude_async`] through `spawner` (tests pass a fake: the real one opens programs).
pub fn launch_claude_async_with(
    log: PathBuf,
    spawner: std::sync::Arc<dyn Spawner>,
    done: impl FnOnce(bool) + Send + 'static,
) {
    let spawn = std::thread::Builder::new()
        .name("aitrack-launch".into())
        .spawn(move || {
            let local = std::env::var_os("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from);
            let tried = if cfg!(target_os = "macos") {
                launch_claude_mac(spawner.as_ref())
            } else if cfg!(target_os = "linux") {
                let home = crate::sources::Env::from_process().home;
                let dirs = xdg_data_dirs(
                    home.as_deref(),
                    std::env::var_os("XDG_DATA_HOME").as_deref(),
                    std::env::var_os("XDG_DATA_DIRS").as_deref(),
                );
                launch_claude_linux(&dirs, spawner.as_ref())
            } else {
                launch_claude(local.as_deref(), spawner.as_ref())
            };
            append_log(&log, &log_text(&tried));
            done(succeeded(&tried));
        });
    if spawn.is_err() {
        // could not even start the thread: report failure, never panic
        append_log(
            &log_path(),
            &format!(
                "{}  claude desktop launch: could not start a thread",
                crate::util::now_iso()
            ),
        );
    }
}

/// Set (to anything) by the test runs (CI, `scripts/test.sh`): the real spawner, the window raiser
/// and the system notifier then do nothing, so a test can never start Safari, Claude or a banner.
pub const NO_SPAWN_ENV: &str = "AITRACK_NO_SPAWN";
#[allow(dead_code)]
const SPAWN_DISABLED: &str = "starting programs is disabled (AITRACK_NO_SPAWN)";

/// True when [`NO_SPAWN_ENV`] is set.
pub fn spawning_disabled() -> bool {
    spawning_disabled_for(std::env::var_os(NO_SPAWN_ENV).as_deref())
}

/// The decision on its own: disabled when the variable is set, to anything (even empty).
pub fn spawning_disabled_for(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some()
}

/// The real spawner: Win32 `ShellExecuteW` and `std::process` on Windows, `/usr/bin/open` and
/// `std::process` on macOS, `xdg-open` and `std::process` on Linux. On every platform each method refuses when
/// [`NO_SPAWN_ENV`] is set.
pub struct SystemSpawner;

#[cfg(windows)]
impl Spawner for SystemSpawner {
    fn open_uri(&self, uri: &str) -> Result<(), String> {
        use std::ffi::c_void;
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }

        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                hwnd: *mut c_void,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> isize;
        }
        #[link(name = "ole32")]
        extern "system" {
            fn CoInitializeEx(reserved: *const c_void, flags: u32) -> i32;
            fn CoUninitialize();
        }
        // ShellExecute must run on a thread whose COM apartment is initialised (the shell loads
        // handlers into it); the launcher threads are plain std threads, and without this a handler
        // can touch an uninitialised apartment. 0x2 = apartment threaded, 0x4 = no OLE1 DDE.
        // SAFETY: plain COM calls; CoUninitialize only when CoInitializeEx succeeded (S_OK or
        // S_FALSE = already initialised, both need the matching call; a failure such as
        // RPC_E_CHANGED_MODE does not).
        let com = unsafe { CoInitializeEx(std::ptr::null(), 0x2 | 0x4) } >= 0;
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let (op, file) = (wide("open"), wide(uri));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call; null is allowed for
        // the window, parameters and directory. Returns a value > 32 on success.
        let code = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if com {
            // SAFETY: balances the successful CoInitializeEx above on this thread.
            unsafe { CoUninitialize() };
        }
        if code > 32 {
            Ok(())
        } else {
            Err(format!(
                "ShellExecute returned {code} (no handler registered?)"
            ))
        }
    }

    fn run(&self, program: &Path, args: &[String]) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }
        std::process::Command::new(program)
            .args(args)
            .creation_flags(crate::grok_cli::CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn capture(&self, program: &Path, args: &[String]) -> Result<String, String> {
        use std::os::windows::process::CommandExt;
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }
        let out = std::process::Command::new(program)
            .args(args)
            .creation_flags(crate::grok_cli::CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(target_os = "macos")]
impl Spawner for SystemSpawner {
    fn open_uri(&self, uri: &str) -> Result<(), String> {
        self.run(Path::new(OPEN_PROGRAM), &[uri.to_owned()])
    }

    /// Waits for the program: `open` and `osascript` return quickly and a non-zero exit is the
    /// only sign that the app was not found.
    fn run(&self, program: &Path, args: &[String]) -> Result<(), String> {
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }
        let st = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("exited with {st}"))
        }
    }

    fn capture(&self, program: &Path, args: &[String]) -> Result<String, String> {
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
impl Spawner for SystemSpawner {
    /// `xdg-open <uri>`; a non-zero exit within the grace period means no handler.
    fn open_uri(&self, uri: &str) -> Result<(), String> {
        self.run(Path::new(XDG_OPEN_PROGRAM), &[uri.to_owned()])
    }

    /// Starts the program detached (own process group, no stdio, no shell) and returns without
    /// waiting for it, except for a short grace period: a program that exits non-zero inside it
    /// (`xdg-open` without a handler, `gio launch` of a broken entry) counts as a failure. A
    /// reaper thread collects the exit status so no zombie is left.
    fn run(&self, program: &Path, args: &[String]) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        if spawning_disabled() {
            return Err(SPAWN_DISABLED.into());
        }
        let mut child = std::process::Command::new(program)
            .args(args)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + LINUX_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(st)) if st.success() => return Ok(()),
                Ok(Some(st)) => return Err(format!("exited with {st}")),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Ok(None) => {
                    // still running: it started; reap it in the background
                    let _ = std::thread::Builder::new()
                        .name("aitrack-reap".into())
                        .spawn(move || {
                            let _ = child.wait();
                        });
                    return Ok(());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------- Linux

/// `xdg-open`, looked up on PATH.
pub const XDG_OPEN_PROGRAM: &str = "xdg-open";
/// The program the Claude desktop `.deb` installs (code.claude.com/docs/en/desktop-linux).
pub const CLAUDE_LINUX_COMMAND: &str = "claude-desktop";
/// How long a spawned program may run before it counts as started (Linux).
#[allow(dead_code)]
const LINUX_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Splits an XDG path list (`$XDG_DATA_DIRS`) on `:` as the spec says, whatever the host's own
/// path-list separator is. Empty entries are dropped.
fn split_xdg_list(list: &std::ffi::OsStr) -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        list.as_bytes()
            .split(|b| *b == b':')
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(std::ffi::OsStr::from_bytes(s)))
            .collect()
    }
    #[cfg(not(unix))]
    {
        list.to_string_lossy()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect()
    }
}

/// Folders that hold `applications/*.desktop` entries: `$XDG_DATA_HOME` (default
/// `~/.local/share`), then each of `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`), then
/// the Flatpak and Snap export folders.
pub fn xdg_data_dirs(
    home: Option<&Path>,
    data_home: Option<&std::ffi::OsStr>,
    data_dirs: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    match data_home.filter(|d| !d.is_empty()) {
        Some(d) => v.push(PathBuf::from(d)),
        None => v.extend(home.map(|h| h.join(".local").join("share"))),
    }
    match data_dirs.filter(|d| !d.is_empty()) {
        Some(d) => v.extend(split_xdg_list(d)),
        None => {
            v.push(PathBuf::from("/usr/local/share"));
            v.push(PathBuf::from("/usr/share"));
        }
    }
    v.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    v.extend(home.map(|h| h.join(".local/share/flatpak/exports/share")));
    v.push(PathBuf::from("/var/lib/snapd/desktop"));
    let mut seen = Vec::new();
    v.retain(|d| {
        let new = !seen.contains(d);
        seen.push(d.clone());
        new
    });
    v
}

/// `claude*.desktop` files in `<dir>/applications` for each of `data_dirs`, in folder order, each
/// folder sorted by name.
pub fn find_claude_desktop_files(data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in data_dirs {
        let mut names: Vec<String> = std::fs::read_dir(d.join("applications"))
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        let l = n.to_ascii_lowercase();
                        l.starts_with("claude") && l.ends_with(".desktop")
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        out.extend(names.into_iter().map(|n| d.join("applications").join(n)));
    }
    out
}

/// Linux: 1. `xdg-open claude://`, 2. `claude-desktop`, 3. each `claude*.desktop` entry found in
/// `data_dirs` with `gio launch <file>`, then `gtk-launch <id>`. Stops at the first that works.
pub fn launch_claude_linux(data_dirs: &[PathBuf], spawner: &dyn Spawner) -> Vec<Attempt> {
    let mut tried = Vec::new();
    let mut push = |method: String, r: Result<(), String>| -> bool {
        let ok = r.is_ok();
        tried.push(Attempt {
            method,
            ok,
            detail: r.err(),
        });
        ok
    };
    if push(
        format!("xdg-open {PROTOCOL_URI}"),
        spawner.open_uri(PROTOCOL_URI),
    ) {
        return tried;
    }
    if push(
        CLAUDE_LINUX_COMMAND.into(),
        spawner.run(Path::new(CLAUDE_LINUX_COMMAND), &[]),
    ) {
        return tried;
    }
    for f in find_claude_desktop_files(data_dirs) {
        let file = f.to_string_lossy().into_owned();
        if push(
            format!("gio launch {file}"),
            spawner.run(Path::new("gio"), &["launch".into(), file]),
        ) {
            return tried;
        }
        if let Some(id) = f.file_stem().map(|s| s.to_string_lossy().into_owned()) {
            if push(
                format!("gtk-launch {id}"),
                spawner.run(Path::new("gtk-launch"), std::slice::from_ref(&id)),
            ) {
                return tried;
            }
        }
    }
    if tried.iter().all(|a| !a.ok) {
        tried.push(Attempt {
            method: "Claude desktop".into(),
            ok: false,
            detail: Some(
                "not installed? no claude:// handler, no claude-desktop, no claude*.desktop entry"
                    .into(),
            ),
        });
    }
    tried
}

/// Terminal emulators tried after `$TERMINAL`, with the argument that precedes the command.
pub const LINUX_TERMINALS: &[(&str, &str)] = &[
    ("x-terminal-emulator", "-e"),
    ("gnome-terminal", "--"),
    ("konsole", "-e"),
    ("xterm", "-e"),
];

/// Terminal emulators found on `path_var`, in discovery order (`$TERMINAL` first, with `-e`), as
/// full path plus the argument that precedes the command. Duplicates are dropped.
pub fn linux_terminals(
    terminal_env: Option<&std::ffi::OsStr>,
    path_var: Option<&std::ffi::OsStr>,
) -> Vec<(PathBuf, &'static str)> {
    let find = |name: &std::ffi::OsStr| -> Option<PathBuf> {
        let p = PathBuf::from(name);
        if p.components().count() > 1 {
            return p.is_file().then_some(p);
        }
        path_var
            .into_iter()
            .flat_map(std::env::split_paths)
            .map(|d| d.join(name))
            .find(|c| c.is_file())
    };
    let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut add = |p: Option<PathBuf>, flag: &'static str| {
        if let Some(p) = p {
            if !out.iter().any(|(q, _)| *q == p) {
                out.push((p, flag));
            }
        }
    };
    if let Some(t) = terminal_env.filter(|t| !t.is_empty()) {
        // the exec flag of an arbitrary $TERMINAL is a guess (-e works for most)
        let flag = match PathBuf::from(t).file_name().and_then(|n| n.to_str()) {
            Some("gnome-terminal") => "--",
            _ => "-e",
        };
        add(find(t), flag);
    }
    for (name, flag) in LINUX_TERMINALS {
        add(find(std::ffi::OsStr::new(name)), flag);
    }
    out
}

/// Linux: the Grok CLI in a terminal window. Tries each terminal of `terminals` in order and stops
/// at the first that starts. Nothing is typed into the CLI.
pub fn launch_grok_linux(
    cli: Option<&Path>,
    terminals: &[(PathBuf, &'static str)],
    spawner: &dyn Spawner,
) -> Vec<Attempt> {
    let fail = |detail: &str| {
        vec![Attempt {
            method: "grok CLI in a terminal".into(),
            ok: false,
            detail: Some(detail.into()),
        }]
    };
    let Some(cli) = cli else {
        return fail("the Grok CLI was not found (~/.grok/bin, ~/.local/bin, PATH)");
    };
    if terminals.is_empty() {
        return fail("no terminal emulator found ($TERMINAL, x-terminal-emulator, gnome-terminal, konsole, xterm)");
    }
    let mut tried = Vec::new();
    for (term, flag) in terminals {
        let r = spawner.run(
            term,
            &[(*flag).to_owned(), cli.to_string_lossy().into_owned()],
        );
        let ok = r.is_ok();
        tried.push(Attempt {
            method: format!("grok CLI in {} {flag}", term.display()),
            ok,
            detail: r.err(),
        });
        if ok {
            break;
        }
    }
    tried
}

/// Where the Grok Bot `.deb` installs its Electron binary (the path has a space).
pub const GROK_BOT_LINUX_BINARY: &str = "/opt/Grok Bot/grok-bot";

/// `grok-bot*.desktop` files in `<dir>/applications` for each of `data_dirs`, in folder order,
/// each folder sorted by name.
pub fn find_grok_bot_desktop_files(data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in data_dirs {
        let mut names: Vec<String> = std::fs::read_dir(d.join("applications"))
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        let l = n.to_ascii_lowercase();
                        l.starts_with("grok-bot") && l.ends_with(".desktop")
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        out.extend(names.into_iter().map(|n| d.join("applications").join(n)));
    }
    out
}

/// Linux: the Grok Bot app. 1. each `grok-bot*.desktop` entry in `data_dirs` (`gio launch <file>`,
/// then `gtk-launch <id>`), 2. `binary` started directly (argv, no shell). Stops at the first that
/// works; an empty result means the app is not installed.
pub fn launch_grok_bot_linux(
    data_dirs: &[PathBuf],
    binary: Option<&Path>,
    spawner: &dyn Spawner,
) -> Vec<Attempt> {
    let mut tried = Vec::new();
    let mut push = |method: String, r: Result<(), String>| -> bool {
        let ok = r.is_ok();
        tried.push(Attempt {
            method,
            ok,
            detail: r.err(),
        });
        ok
    };
    for f in find_grok_bot_desktop_files(data_dirs) {
        let file = f.to_string_lossy().into_owned();
        if push(
            format!("Grok Bot: gio launch {file}"),
            spawner.run(Path::new("gio"), &["launch".into(), file]),
        ) {
            return tried;
        }
        if let Some(id) = f.file_stem().map(|s| s.to_string_lossy().into_owned()) {
            if push(
                format!("Grok Bot: gtk-launch {id}"),
                spawner.run(Path::new("gtk-launch"), std::slice::from_ref(&id)),
            ) {
                return tried;
            }
        }
    }
    if let Some(b) = binary {
        push(format!("Grok Bot: {}", b.display()), spawner.run(b, &[]));
    }
    tried
}

/// Linux, full order: the Grok Bot app, then the CLI in a terminal.
pub fn launch_grok_linux_all(
    data_dirs: &[PathBuf],
    bot_binary: Option<&Path>,
    cli: Option<&Path>,
    terminals: &[(PathBuf, &'static str)],
    spawner: &dyn Spawner,
) -> Vec<Attempt> {
    let mut tried = launch_grok_bot_linux(data_dirs, bot_binary, spawner);
    if succeeded(&tried) {
        return tried;
    }
    tried.extend(launch_grok_linux(cli, terminals, spawner));
    tried
}

// ---------------------------------------------------------------- macOS

/// The macOS `open` tool.
pub const OPEN_PROGRAM: &str = "/usr/bin/open";
/// The macOS AppleScript runner.
pub const OSASCRIPT_PROGRAM: &str = "/usr/bin/osascript";
/// Name the Claude app is opened by (`open -a Claude`).
pub const CLAUDE_MAC_APP_NAME: &str = "Claude";
/// Bundle id of the Claude desktop app. UNVERIFIED guess; the name is tried first.
pub const CLAUDE_MAC_BUNDLE_ID: &str = "com.anthropic.claudefordesktop";

/// Arguments for `open`, in the order tried: by name, then by bundle id.
pub fn claude_open_argvs_mac() -> Vec<Vec<String>> {
    vec![
        vec!["-a".into(), CLAUDE_MAC_APP_NAME.into()],
        vec!["-b".into(), CLAUDE_MAC_BUNDLE_ID.into()],
    ]
}

/// macOS: try [`claude_open_argvs_mac`] in order and stop at the first that works.
pub fn launch_claude_mac(spawner: &dyn Spawner) -> Vec<Attempt> {
    let mut tried = Vec::new();
    for args in claude_open_argvs_mac() {
        let r = spawner.run(Path::new(OPEN_PROGRAM), &args);
        let ok = r.is_ok();
        tried.push(Attempt {
            method: format!("open {}", args.join(" ")),
            ok,
            detail: r.err(),
        });
        if ok {
            break;
        }
    }
    tried
}

/// `.app` bundles named `Grok*` among `names` (file names of an Applications folder), best first:
/// "Grok Bot.app", then "Grok.app", then the rest, each group sorted.
pub fn grok_mac_app_names(names: &[String]) -> Vec<String> {
    let mut v: Vec<String> = names
        .iter()
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.starts_with("grok") && l.ends_with(".app")
        })
        .cloned()
        .collect();
    let rank = |n: &String| match n.to_ascii_lowercase().as_str() {
        "grok bot.app" => 0,
        "grok.app" => 1,
        _ => 2,
    };
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    v
}

/// `Grok*.app` bundles in `dirs` (usually `/Applications` and `~/Applications`), as full paths.
pub fn find_grok_apps_mac(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let names: Vec<String> = std::fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.extend(grok_mac_app_names(&names).into_iter().map(|n| d.join(n)));
    }
    out
}

/// `'text'` for a POSIX shell (single quotes inside become `'\''`).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `text` as the inside of an AppleScript string literal.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `osascript` arguments that open a new Terminal window in `dir` running the CLI with no
/// arguments (nothing is typed into it afterwards).
pub fn terminal_osascript_args(cli: &Path, dir: &Path) -> Vec<String> {
    let cmd = format!(
        "cd {} && {}",
        sh_quote(&dir.to_string_lossy()),
        sh_quote(&cli.to_string_lossy())
    );
    vec![
        "-e".into(),
        format!(
            "tell application \"Terminal\" to do script \"{}\"",
            applescript_escape(&cmd)
        ),
        "-e".into(),
        "tell application \"Terminal\" to activate".into(),
    ]
}

/// macOS: 1. a `Grok*.app` in `app_dirs` (`open -a <path>`), 2. the CLI in a new Terminal window.
/// Stops at the first that starts. Nothing is typed into the CLI.
pub fn launch_grok_mac(
    app_dirs: &[PathBuf],
    home: Option<&Path>,
    cli: Option<&Path>,
    spawner: &dyn Spawner,
) -> Vec<Attempt> {
    let mut tried = Vec::new();
    let mut push = |method: String, r: Result<(), String>| -> bool {
        let ok = r.is_ok();
        tried.push(Attempt {
            method,
            ok,
            detail: r.err(),
        });
        ok
    };
    for app in find_grok_apps_mac(app_dirs) {
        let r = spawner.run(
            Path::new(OPEN_PROGRAM),
            &["-a".into(), app.to_string_lossy().into_owned()],
        );
        if push(format!("Grok app {}", app.display()), r) {
            return tried;
        }
    }
    match cli {
        Some(cli) => {
            let dir = home.unwrap_or_else(|| Path::new("."));
            let r = spawner.run(
                Path::new(OSASCRIPT_PROGRAM),
                &terminal_osascript_args(cli, dir),
            );
            push("grok CLI in Terminal (osascript)".into(), r);
        }
        None => {
            push(
                "grok CLI in Terminal".into(),
                Err("no Grok app or grok CLI found".into()),
            );
        }
    }
    tried
}

// ---------------------------------------------------------------- Grok

/// Package-family prefix of the Grok Store app (the suffix hash differs per machine).
pub const GROK_STORE_PREFIX: &str = "grok.com-";
/// AppId of that package's application entry (`<family>!App`).
pub const GROK_STORE_APP_ID: &str = "App";
/// Start-menu names of the Grok apps. Exact matches only (not anything that contains "grok").
pub const GROK_BOT_NAME: &str = "Grok Bot";
pub const GROK_NAME: &str = "Grok";

/// One row of the Start menu (`Get-StartApps`).
#[derive(Debug, Clone, PartialEq)]
pub struct StartApp {
    pub name: String,
    pub app_id: String,
}

/// PowerShell command that prints `name<TAB>AppID` per Start-menu entry.
pub const START_APPS_SCRIPT: &str =
    "Get-StartApps | ForEach-Object { $_.Name + \"`t\" + $_.AppID }";

/// Parse the tab-separated output of [`START_APPS_SCRIPT`].
pub fn parse_start_apps(text: &str) -> Vec<StartApp> {
    text.lines()
        .filter_map(|l| {
            let (name, id) = l.split_once('\t')?;
            let (name, id) = (name.trim(), id.trim());
            (!name.is_empty() && !id.is_empty()).then(|| StartApp {
                name: name.to_owned(),
                app_id: id.to_owned(),
            })
        })
        .collect()
}

/// The `shell:AppsFolder` targets of "Grok Bot" entries: `electron.app.*` AppIDs first.
pub fn grok_bot_targets(apps: &[StartApp]) -> Vec<String> {
    let mut v: Vec<&StartApp> = apps
        .iter()
        .filter(|a| a.name.eq_ignore_ascii_case(GROK_BOT_NAME))
        .collect();
    v.sort_by_key(|a| !a.app_id.to_ascii_lowercase().starts_with("electron.app."));
    v.into_iter()
        .map(|a| format!("shell:AppsFolder\\{}", a.app_id))
        .collect()
}

/// The targets of the Store app "Grok": the `grok.com-*` package folders (AUMID `<family>!App`),
/// and a Start entry named exactly "Grok" (its own AppID wins, `grok.com-` ones first).
pub fn grok_store_targets(localappdata: Option<&Path>, apps: &[StartApp]) -> Vec<String> {
    let mut ids: Vec<String> = apps
        .iter()
        .filter(|a| a.name.eq_ignore_ascii_case(GROK_NAME))
        .map(|a| a.app_id.clone())
        .collect();
    ids.sort_by_key(|id| !id.to_ascii_lowercase().starts_with(GROK_STORE_PREFIX));
    if let Some(l) = localappdata {
        for fam in families_with_prefix(l, GROK_STORE_PREFIX) {
            let id = format!("{fam}!{GROK_STORE_APP_ID}");
            if !ids.iter().any(|i| i.eq_ignore_ascii_case(&id)) {
                ids.push(id);
            }
        }
    }
    ids.into_iter()
        .map(|id| format!("shell:AppsFolder\\{id}"))
        .collect()
}

/// How to open the CLI in a new terminal window in `dir`, running `grok` with no arguments:
/// Windows Terminal when `wt` is given, else a new console through `cmd /c start`.
pub fn terminal_command(cli: &Path, dir: &Path, wt: Option<&Path>) -> (PathBuf, Vec<String>) {
    let (cli, dir) = (
        cli.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    );
    match wt {
        Some(wt) => (wt.to_path_buf(), vec!["-d".into(), dir, cli]),
        None => (
            PathBuf::from("cmd.exe"),
            vec![
                "/c".into(),
                "start".into(),
                "Grok".into(),
                "/D".into(),
                dir,
                cli,
            ],
        ),
    }
}

/// What [`launch_grok`] needs to know about the machine.
pub struct GrokLaunchEnv {
    pub localappdata: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// The Grok CLI, when found (see [`crate::grok_cli::find_cli`]).
    pub cli: Option<PathBuf>,
    /// `wt.exe`, when found.
    pub wt: Option<PathBuf>,
}

/// Open Grok: 1. the "Grok Bot" app, 2. the Store app "Grok", 3. the CLI in a new terminal window.
/// Stops at the first that starts. Nothing is typed into the CLI.
pub fn launch_grok(env: &GrokLaunchEnv, spawner: &dyn Spawner) -> Vec<Attempt> {
    let mut tried = Vec::new();
    let push = |method: String, r: Result<(), String>, tried: &mut Vec<Attempt>| -> bool {
        let ok = r.is_ok();
        tried.push(Attempt {
            method,
            ok,
            detail: r.err(),
        });
        ok
    };
    // the Start menu list (a PowerShell call) is asked once
    let apps = match spawner.capture(
        Path::new("powershell.exe"),
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            START_APPS_SCRIPT.into(),
        ],
    ) {
        Ok(t) => parse_start_apps(&t),
        Err(_) => Vec::new(),
    };
    for (label, targets) in [
        ("Grok Bot app", grok_bot_targets(&apps)),
        (
            "Grok app",
            grok_store_targets(env.localappdata.as_deref(), &apps),
        ),
    ] {
        for t in targets {
            let r = spawner.run(Path::new("explorer.exe"), std::slice::from_ref(&t));
            if push(format!("{label} {t}"), r, &mut tried) {
                return tried;
            }
        }
    }
    match (&env.cli, &env.home) {
        (Some(cli), home) => {
            let dir = home.clone().unwrap_or_else(|| PathBuf::from("."));
            let (prog, args) = terminal_command(cli, &dir, env.wt.as_deref());
            let r = spawner.run(&prog, &args);
            push(
                format!("grok CLI in a terminal ({})", prog.display()),
                r,
                &mut tried,
            );
        }
        (None, _) => tried.push(Attempt {
            method: "grok CLI in a terminal".into(),
            ok: false,
            detail: Some("no Grok app or grok CLI found".into()),
        }),
    }
    tried
}

/// Launch Grok on a short-lived thread; `done(ok)` runs on that thread.
pub fn launch_grok_async(log: PathBuf, done: impl FnOnce(bool) + Send + 'static) {
    let spawn = std::thread::Builder::new()
        .name("aitrack-launch-grok".into())
        .spawn(move || {
            let env = crate::sources::Env::from_process();
            if cfg!(target_os = "macos") {
                let mut dirs = vec![PathBuf::from("/Applications")];
                dirs.extend(env.home.as_ref().map(|h| h.join("Applications")));
                let cli = crate::grok_cli::find_cli_here(&env);
                let tried =
                    launch_grok_mac(&dirs, env.home.as_deref(), cli.as_deref(), &SystemSpawner);
                let line = log_text(&tried).replace("claude desktop launch", "grok launch");
                append_log(&log, &line);
                done(succeeded(&tried));
                return;
            }
            if cfg!(target_os = "linux") {
                let terms = linux_terminals(
                    std::env::var_os("TERMINAL").as_deref(),
                    std::env::var_os("PATH").as_deref(),
                );
                let cli = crate::grok_cli::find_cli_here(&env);
                let dirs = xdg_data_dirs(
                    env.home.as_deref(),
                    std::env::var_os("XDG_DATA_HOME").as_deref(),
                    std::env::var_os("XDG_DATA_DIRS").as_deref(),
                );
                let bin = Path::new(GROK_BOT_LINUX_BINARY);
                let tried = launch_grok_linux_all(
                    &dirs,
                    bin.is_file().then_some(bin),
                    cli.as_deref(),
                    &terms,
                    &SystemSpawner,
                );
                let line = log_text(&tried).replace("claude desktop launch", "grok launch");
                append_log(&log, &line);
                done(succeeded(&tried));
                return;
            }
            let wt = env
                .localappdata
                .as_ref()
                .map(|l| l.join("Microsoft").join("WindowsApps").join("wt.exe"))
                .filter(|p| p.is_file())
                .or_else(|| {
                    std::env::var_os("PATH").and_then(|p| {
                        std::env::split_paths(&p)
                            .map(|d| d.join("wt.exe"))
                            .find(|c| c.is_file())
                    })
                });
            let genv = GrokLaunchEnv {
                localappdata: env.localappdata.clone(),
                home: env.home.clone(),
                cli: crate::grok_cli::find_cli_here(&env),
                wt,
            };
            let tried = launch_grok(&genv, &SystemSpawner);
            let line = log_text(&tried).replace("claude desktop launch", "grok launch");
            append_log(&log, &line);
            done(succeeded(&tried));
        });
    if spawn.is_err() {
        append_log(
            &log_path(),
            &format!(
                "{}  grok launch: could not start a thread",
                crate::util::now_iso()
            ),
        );
    }
}

/// Where "Open help" goes. One place to change when a real help page exists.
pub const HELP_URL: &str = "https://github.com/genen/aiTrack#readme";

/// The project page (Settings > About).
pub const REPO_URL: &str = "https://github.com/genen/aiTrack";
/// The licence text in the repository.
pub const LICENSE_URL: &str = "https://github.com/genen/aiTrack/blob/main/LICENSE";
/// Where to write to the developer.
pub const CONTACT_EMAIL: &str = "aitrack@sirgene.com";

/// Open [`HELP_URL`] in the default browser through `spawner` (`ShellExecuteW "open"` on Windows),
/// logging the attempt next to the launcher's log. Returns whether it worked.
pub fn open_help(spawner: &dyn Spawner, log: Option<&Path>) -> Result<(), String> {
    open_url(spawner, "help", HELP_URL, log)
}

/// Open `url` (https or mailto) with its registered handler, logging `what` and the outcome.
/// The log line names the url, never anything personal.
pub fn open_url(
    spawner: &dyn Spawner,
    what: &str,
    url: &str,
    log: Option<&Path>,
) -> Result<(), String> {
    let r = spawner.open_uri(url);
    if let Some(p) = log {
        let line = match &r {
            Ok(()) => format!("{what} {url}: opened"),
            Err(e) => format!("{what} {url}: failed ({e})"),
        };
        append_log(p, &format!("{}  {line}", crate::util::now_iso()));
    }
    r
}
