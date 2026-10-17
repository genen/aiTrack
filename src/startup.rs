// SPDX-License-Identifier: GPL-3.0-or-later
//! "Start at login": a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` on Windows,
//! a LaunchAgent plist in `~/Library/LaunchAgents` on macOS.

use std::path::{Path, PathBuf};

/// Label of the macOS LaunchAgent (also its file name without `.plist`).
pub const LAUNCH_AGENT_LABEL: &str = "com.sirgene.aitrack";

/// `<home>/Library/LaunchAgents/com.sirgene.aitrack.plist`.
pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The LaunchAgent plist that starts `exe` at login (`RunAtLoad`, no arguments, no keep-alive).
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n",
            "<dict>\n",
            "    <key>Label</key>\n",
            "    <string>{label}</string>\n",
            "    <key>ProgramArguments</key>\n",
            "    <array>\n",
            "        <string>{exe}</string>\n",
            "    </array>\n",
            "    <key>RunAtLoad</key>\n",
            "    <true/>\n",
            "</dict>\n",
            "</plist>\n"
        ),
        label = LAUNCH_AGENT_LABEL,
        exe = xml_escape(&exe.to_string_lossy())
    )
}

/// Is the LaunchAgent plist present under `home`?
pub fn agent_is_enabled(home: &Path) -> bool {
    launch_agent_path(home).is_file()
}

/// Write the plist for `exe` under `home`. It applies at the next login: `launchctl` is not called.
pub fn agent_enable(home: &Path, exe: &Path) -> Result<(), String> {
    let path = launch_agent_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, launch_agent_plist(exe)).map_err(|e| e.to_string())
}

/// Remove the plist under `home` (a missing file is fine).
pub fn agent_disable(home: &Path) -> Result<(), String> {
    match std::fs::remove_file(launch_agent_path(home)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Register or remove the app. Returns an error message on failure.
#[cfg(windows)]
pub fn set_startup(enabled: bool) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey_with_flags(RUN, KEY_SET_VALUE)
        .map_err(|e| e.to_string())?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value("aiTrack", &format!("\"{}\"", exe.display()))
            .map_err(|e| e.to_string())
    } else {
        match key.delete_value("aiTrack") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// macOS: write or remove the LaunchAgent plist. It takes effect at the next login (no `launchctl`).
#[cfg(target_os = "macos")]
pub fn set_startup(enabled: bool) -> Result<(), String> {
    let home = crate::util::home_dir().ok_or("home folder not found")?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        agent_enable(&home, &exe)
    } else {
        agent_disable(&home)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn set_startup(_enabled: bool) -> Result<(), String> {
    Err("Start at login is only available on Windows and macOS".into())
}
