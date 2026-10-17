// SPDX-License-Identifier: GPL-3.0-or-later
//! Single-instance guard: one aiTrack window / collector per machine and user.
//!
//! An exclusive OS lock (`File::try_lock`) on `~/.aitrack/aitrack.lock` is held for the life of the
//! process. The OS drops it when the process ends, however it ends, so a leftover file never blocks
//! a later start. Acquiring never waits: contention is reported immediately.
//!
//! The holder's pid goes into a separate `aitrack.pid` file (on Windows an exclusive lock also stops
//! other processes from reading the locked file itself), best effort, only for the message.

use crate::util::data_dir;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Title of the main window. The window is created with exactly this title and a second launch
/// finds the running one by it, so both use this constant.
pub const WINDOW_TITLE: &str = "aiTrack";

/// `$AITRACK_LOCK` or `~/.aitrack/aitrack.lock`.
pub fn lock_path() -> PathBuf {
    match std::env::var("AITRACK_LOCK") {
        Ok(p) if !p.is_empty() => crate::util::expand_user(&p),
        _ => data_dir().join("aitrack.lock"),
    }
}

fn pid_path(lock: &Path) -> PathBuf {
    lock.with_extension("pid")
}

/// Held for the process lifetime; dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    // kept open: closing the handle releases the OS lock
    _file: File,
    pid_file: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

#[derive(Debug)]
pub enum Acquire {
    Acquired(InstanceLock),
    /// Another process holds the lock (its pid, if it could be read).
    AlreadyRunning {
        pid: Option<u32>,
    },
    /// The lock could not be used at all (read-only folder, ...). Callers carry on without it.
    Unavailable(String),
}

/// Try to take the lock without waiting.
pub fn acquire(path: &Path) -> Acquire {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Acquire::Unavailable(format!("cannot create {}: {e}", dir.display()));
        }
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => return Acquire::Unavailable(format!("cannot open {}: {e}", path.display())),
    };
    match file.try_lock() {
        Ok(()) => {
            let pid_file = pid_path(path);
            let _ = std::fs::write(&pid_file, std::process::id().to_string());
            Acquire::Acquired(InstanceLock {
                _file: file,
                pid_file,
            })
        }
        Err(TryLockError::WouldBlock) => Acquire::AlreadyRunning {
            pid: std::fs::read_to_string(pid_path(path))
                .ok()
                .and_then(|t| t.trim().parse().ok()),
        },
        Err(TryLockError::Error(e)) => {
            Acquire::Unavailable(format!("cannot lock {}: {e}", path.display()))
        }
    }
}

/// `aiTrack is already running (pid 1234)` / `... running`.
pub fn running_text(pid: Option<u32>) -> String {
    match pid {
        Some(p) => format!("aiTrack is already running (pid {p})"),
        None => "aiTrack is already running".into(),
    }
}

/// Is a top-level window the aiTrack window of the running instance? Its title must be
/// [`WINDOW_TITLE`] and, when the pid of the instance is known (from `aitrack.pid`), it must belong
/// to that process: another program's window that happens to be called "aiTrack" (an Explorer folder
/// window, say) is not ours.
pub fn is_our_window(title: &str, window_pid: u32, instance_pid: Option<u32>) -> bool {
    title == WINDOW_TITLE && instance_pid.is_none_or(|p| p == window_pid)
}

/// Bring the already running window to the front (restore it if minimised). `pid` is the running
/// instance's process id when known. Returns whether its window was found.
#[cfg(windows)]
pub fn raise_existing_window(pid: Option<u32>) -> bool {
    use std::ffi::c_void;
    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(f: extern "system" fn(*mut c_void, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowTextW(hwnd: *mut c_void, text: *mut u16, max: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        fn IsIconic(hwnd: *mut c_void) -> i32;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    struct Search {
        pid: Option<u32>,
        found: *mut c_void,
    }
    extern "system" fn visit(hwnd: *mut c_void, lparam: isize) -> i32 {
        // SAFETY: `lparam` is the address of the `Search` that `raise_existing_window` keeps alive
        // for the whole EnumWindows call; the buffers below outlive the calls that fill them.
        unsafe {
            let search = &mut *(lparam as *mut Search);
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut buf = [0u16; 128];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            let mut window_pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut window_pid);
            if is_our_window(&title, window_pid, search.pid) {
                search.found = hwnd;
                return 0; // stop
            }
        }
        1
    }
    let mut search = Search {
        pid,
        found: std::ptr::null_mut(),
    };
    // SAFETY: plain Win32 calls; `search` outlives EnumWindows and is only touched by `visit`.
    unsafe {
        EnumWindows(visit, &mut search as *mut Search as isize);
        if search.found.is_null() {
            return false;
        }
        if IsIconic(search.found) != 0 {
            ShowWindow(search.found, SW_RESTORE);
        }
        SetForegroundWindow(search.found);
        true
    }
}

/// `osascript` arguments that bring the process `pid` to the front (System Events). `None` when
/// the pid is unknown: without it any app could be raised by mistake.
pub fn raise_osascript_args_mac(pid: Option<u32>) -> Option<Vec<String>> {
    let pid = pid?;
    Some(vec![
        "-e".into(),
        format!(
            "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
        ),
    ])
}

/// macOS: best effort through `osascript`. True only when it exited successfully. UNVERIFIED on a
/// real Mac; System Events may ask for Automation permission the first time.
#[cfg(target_os = "macos")]
pub fn raise_existing_window(pid: Option<u32>) -> bool {
    let Some(args) = raise_osascript_args_mac(pid) else {
        return false;
    };
    if crate::launch::spawning_disabled() {
        return false;
    }
    std::process::Command::new("/usr/bin/osascript")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn raise_existing_window(_pid: Option<u32>) -> bool {
    false
}
