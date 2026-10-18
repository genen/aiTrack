// SPDX-License-Identifier: GPL-3.0-or-later
//! "Start at login": a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` on Windows,
//! a LaunchAgent plist in `~/Library/LaunchAgents` on macOS, an XDG autostart `.desktop` file in
//! `$XDG_CONFIG_HOME/autostart` on Linux.

use std::ffi::OsStr;
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

/// The XDG autostart folder: `$XDG_CONFIG_HOME/autostart` when that is an absolute path (the spec
/// ignores relative values), else `<home>/.config/autostart`.
pub fn autostart_dir(xdg_config_home: Option<&OsStr>, home: &Path) -> PathBuf {
    match xdg_config_home.map(Path::new) {
        Some(p) if p.is_absolute() => p.join("autostart"),
        _ => home.join(".config").join("autostart"),
    }
}

/// `<autostart folder>/aitrack.desktop`.
pub fn autostart_file(xdg_config_home: Option<&OsStr>, home: &Path) -> PathBuf {
    autostart_dir(xdg_config_home, home).join("aitrack.desktop")
}

/// One `Exec=` program path for the Desktop Entry spec. Arguments containing a space or one of the
/// reserved characters are wrapped in double quotes, with `"`, `` ` ``, `$` and `\` backslash-escaped
/// there; `%` is doubled (field codes); then the whole is escaped once more as a .desktop string
/// (backslash doubled, line breaks as `\n`) so the file stays one line per key.
pub fn desktop_exec_quote(arg: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    let mut q = String::with_capacity(arg.len() + 2);
    let quote = arg.is_empty() || arg.chars().any(|c| RESERVED.contains(c));
    if quote {
        q.push('"');
    }
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' if quote => {
                q.push('\\');
                q.push(c);
            }
            '%' => q.push_str("%%"),
            c => q.push(c),
        }
    }
    if quote {
        q.push('"');
    }
    let mut out = String::with_capacity(q.len());
    for c in q.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// The autostart `.desktop` file that starts `exe` at login.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        concat!(
            "[Desktop Entry]\n",
            "Type=Application\n",
            "Name=aiTrack\n",
            "Comment=Claude and Grok plan usage meter\n",
            "Exec={exec}\n",
            "Icon=aitrack\n",
            "Terminal=false\n",
            "X-GNOME-Autostart-enabled=true\n"
        ),
        exec = desktop_exec_quote(&exe.to_string_lossy())
    )
}

/// Is the autostart file present?
pub fn autostart_is_enabled(xdg_config_home: Option<&OsStr>, home: &Path) -> bool {
    autostart_file(xdg_config_home, home).is_file()
}

/// Write the autostart file for `exe`. It applies at the next login.
pub fn autostart_enable(
    xdg_config_home: Option<&OsStr>,
    home: &Path,
    exe: &Path,
) -> Result<(), String> {
    let path = autostart_file(xdg_config_home, home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, desktop_entry(exe)).map_err(|e| e.to_string())
}

/// Remove the autostart file (a missing file is fine).
pub fn autostart_disable(xdg_config_home: Option<&OsStr>, home: &Path) -> Result<(), String> {
    match std::fs::remove_file(autostart_file(xdg_config_home, home)) {
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

/// Linux and other Unix: write or remove the XDG autostart file. It takes effect at the next login.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn set_startup(enabled: bool) -> Result<(), String> {
    let home = crate::util::home_dir().ok_or("home folder not found")?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        autostart_enable(xdg.as_deref(), &home, &exe)
    } else {
        autostart_disable(xdg.as_deref(), &home)
    }
}
