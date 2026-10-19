// SPDX-License-Identifier: GPL-3.0-or-later
//! User settings, persisted as JSON at `~/.aitrack/settings.json` (or `$AITRACK_SETTINGS`).
//! Keys: `interval_min`, `direction`, `startup`, `on_top`, and more (see `Values`); unknown keys are ignored.

use crate::util::{data_dir, expand_user};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub const INTERVAL_MIN: f64 = 1.0;
pub const INTERVAL_MAX: f64 = 1440.0;
/// How often the cheap local sources (desktop cache/history, ~/.claude.json) are re-read, in seconds.
pub const LOCAL_SEC_MIN: f64 = 15.0;
pub const LOCAL_SEC_MAX: f64 = 600.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Used,
    Remaining,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Used => "used",
            Direction::Remaining => "remaining",
        }
    }
}

/// Which Settings sections are open (`ui.sections` in settings.json). General starts open, the
/// rest collapsed, so the Settings view is short by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sections {
    pub general: bool,
    pub appearance: bool,
    pub display: bool,
    pub notifications: bool,
    pub diagnostics: bool,
    pub about: bool,
}

impl Default for Sections {
    fn default() -> Self {
        Self {
            general: true,
            appearance: false,
            display: false,
            notifications: false,
            diagnostics: false,
            about: false,
        }
    }
}

impl Sections {
    pub const NAMES: [&'static str; 6] = [
        "general",
        "appearance",
        "display",
        "notifications",
        "diagnostics",
        "about",
    ];

    pub fn get(&self, name: &str) -> bool {
        match name {
            "general" => self.general,
            "appearance" => self.appearance,
            "display" => self.display,
            "notifications" => self.notifications,
            "diagnostics" => self.diagnostics,
            "about" => self.about,
            _ => false,
        }
    }

    /// Returns false for an unknown section name.
    pub fn set(&mut self, name: &str, open: bool) -> bool {
        match name {
            "general" => self.general = open,
            "appearance" => self.appearance = open,
            "display" => self.display = open,
            "notifications" => self.notifications = open,
            "diagnostics" => self.diagnostics = open,
            "about" => self.about = open,
            _ => return false,
        }
        true
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            Self::NAMES
                .iter()
                .map(|n| ((*n).to_owned(), Value::Bool(self.get(n))))
                .collect(),
        )
    }
}

/// Whether a vendor's panel is shown: follow the detection of its tool, or always / never.
/// (Stored as `null` / `true` / `false` in `claude_enabled` and `grok_enabled`; the older Grok-only
/// "Track Grok" values keep their meaning: `true` = On, `false` = Off, missing = Auto.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Auto,
    On,
    Off,
}

impl Track {
    pub const ALL: [Track; 3] = [Track::Auto, Track::On, Track::Off];

    pub fn from_opt(o: Option<bool>) -> Self {
        match o {
            None => Track::Auto,
            Some(true) => Track::On,
            Some(false) => Track::Off,
        }
    }

    pub fn to_opt(self) -> Option<bool> {
        match self {
            Track::Auto => None,
            Track::On => Some(true),
            Track::Off => Some(false),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Track::Auto => "Auto",
            Track::On => "On",
            Track::Off => "Off",
        }
    }
}

/// How the vendor badges (the logo squares) look. One logo per vendor; the variant picks the fill
/// and the logo colour (the table is in `theme.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeVariant {
    /// Match the theme: the Dark look on a dark window, the Light look on a light one.
    #[default]
    Auto,
    /// Brand-coloured squircle with a light logo.
    Filled,
    /// Dark squircle, logo in the brand colour.
    Dark,
    /// Light squircle with a thin outline, logo in the brand colour.
    Light,
}

impl BadgeVariant {
    pub const ALL: [BadgeVariant; 4] = [
        BadgeVariant::Auto,
        BadgeVariant::Filled,
        BadgeVariant::Dark,
        BadgeVariant::Light,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BadgeVariant::Auto => "auto",
            BadgeVariant::Filled => "filled",
            BadgeVariant::Dark => "dark",
            BadgeVariant::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BadgeVariant::Auto => "Auto",
            BadgeVariant::Filled => "Filled",
            BadgeVariant::Dark => "Dark",
            BadgeVariant::Light => "Light",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// The concrete look for a window theme: `Auto` becomes Dark on a dark theme and Light on a
    /// light one (light icons on a dark window, dark icons on a light one); the others stay.
    pub fn resolve(self, dark_theme: bool) -> BadgeVariant {
        match self {
            BadgeVariant::Auto if dark_theme => BadgeVariant::Dark,
            BadgeVariant::Auto => BadgeVariant::Light,
            other => other,
        }
    }
}

/// The window's colour theme: dark (the default), light, or follow the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    System,
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Where the window was left: outer position and width in egui points, and the native scale factor
/// they were measured with (see `placement`). The height is never stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowPos {
    pub x: f64,
    pub y: f64,
    pub width: Option<f64>,
    pub scale: Option<f64>,
}

impl WindowPos {
    pub fn to_json(&self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "scale": self.scale})
    }

    /// `None` unless `x` and `y` are finite numbers (a missing width or scale is fine).
    pub fn from_json(v: &Value) -> Option<Self> {
        let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|n| n.is_finite());
        Some(Self {
            x: num("x")?,
            y: num("y")?,
            width: num("width"),
            scale: num("scale"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Values {
    /// Network poll interval (the OAuth endpoint), minutes.
    pub interval_min: f64,
    /// Local-source poll interval, seconds.
    pub local_interval_sec: f64,
    pub direction: Direction,
    pub startup: bool,
    pub on_top: bool,
    /// Toast notifications for usage alerts.
    pub notifications: bool,
    /// Track Grok usage. `None` = automatic: on when the Grok CLI's folder exists.
    pub grok_enabled: Option<bool>,
    /// Show Claude. `None` = automatic: on when the Claude desktop app's data or Claude Code's
    /// config folder exists.
    pub claude_enabled: Option<bool>,
    /// Warning / limit / predictive toasts play a reminder sound.
    pub alert_sounds: bool,
    /// Look of the vendor badges.
    pub badge_style: BadgeVariant,
    /// The window's colour theme.
    pub theme: ThemeChoice,
    /// Open / closed state of the Settings sections.
    pub sections: Sections,
    /// Where the window was left (`None` = default placement).
    pub window: Option<WindowPos>,
    /// Small mode: a borderless always-on-top window with only the vendor meters.
    pub small: bool,
}

impl Default for Values {
    fn default() -> Self {
        Self {
            interval_min: 15.0,
            local_interval_sec: 60.0,
            direction: Direction::Used,
            startup: false,
            on_top: true,
            notifications: true,
            grok_enabled: None,
            claude_enabled: None,
            alert_sounds: true,
            badge_style: BadgeVariant::Auto,
            theme: ThemeChoice::Dark,
            sections: Sections::default(),
            window: None,
            small: false,
        }
    }
}

impl Values {
    /// Is Grok tracking on, given whether the Grok CLI's folder exists?
    pub fn grok_on(&self, detected: bool) -> bool {
        self.grok_enabled.unwrap_or(detected)
    }

    /// Is Claude shown, given whether its tools were detected?
    pub fn claude_on(&self, detected: bool) -> bool {
        self.claude_enabled.unwrap_or(detected)
    }

    pub fn remaining(&self) -> bool {
        self.direction == Direction::Remaining
    }

    pub fn to_json(&self) -> Value {
        json!({
            "interval_min": self.interval_min,
            "local_interval_sec": self.local_interval_sec,
            "direction": self.direction.as_str(),
            "startup": self.startup,
            "on_top": self.on_top,
            "notifications": self.notifications,
            "grok_enabled": self.grok_enabled,
            "claude_enabled": self.claude_enabled,
            "alert_sounds": self.alert_sounds,
            "badge_style": self.badge_style.as_str(),
            "theme": self.theme.as_str(),
            "ui": {"sections": self.sections.to_json()},
            "window": self.window.map(|w| w.to_json()),
            "small": self.small,
        })
    }

    /// Apply the valid fields of `new` (anything malformed is ignored).
    fn apply(&mut self, new: &Value) {
        if let Some(n) = new.get("interval_min").and_then(Value::as_f64) {
            if n.is_finite() {
                self.interval_min = n.clamp(INTERVAL_MIN, INTERVAL_MAX);
            }
        }
        if let Some(n) = new.get("local_interval_sec").and_then(Value::as_f64) {
            if n.is_finite() {
                self.local_interval_sec = n.clamp(LOCAL_SEC_MIN, LOCAL_SEC_MAX);
            }
        }
        match new.get("direction").and_then(Value::as_str) {
            Some("used") => self.direction = Direction::Used,
            Some("remaining") => self.direction = Direction::Remaining,
            _ => {}
        }
        if let Some(b) = new.get("startup").and_then(Value::as_bool) {
            self.startup = b;
        }
        if let Some(b) = new.get("on_top").and_then(Value::as_bool) {
            self.on_top = b;
        }
        if let Some(b) = new.get("notifications").and_then(Value::as_bool) {
            self.notifications = b;
        }
        if let Some(b) = new.get("small").and_then(Value::as_bool) {
            self.small = b;
        }
        if let Some(b) = new.get("alert_sounds").and_then(Value::as_bool) {
            self.alert_sounds = b;
        }
        if let Some(b) = new
            .get("badge_style")
            .and_then(Value::as_str)
            .and_then(BadgeVariant::parse)
        {
            self.badge_style = b;
        }
        match new.get("window") {
            Some(Value::Null) => self.window = None,
            // anything without a usable x and y means "no saved place"
            Some(v) => self.window = WindowPos::from_json(v),
            None => {}
        }
        if let Some(t) = new
            .get("theme")
            .and_then(Value::as_str)
            .and_then(ThemeChoice::parse)
        {
            self.theme = t;
        }
        if let Some(sec) = new.pointer("/ui/sections").and_then(Value::as_object) {
            for (name, open) in sec {
                if let Some(b) = open.as_bool() {
                    self.sections.set(name, b); // unknown names are ignored
                }
            }
        }
        match new.get("claude_enabled") {
            Some(Value::Bool(b)) => self.claude_enabled = Some(*b),
            Some(Value::Null) => self.claude_enabled = None,
            _ => {}
        }
        match new.get("grok_enabled") {
            Some(Value::Bool(b)) => self.grok_enabled = Some(*b),
            Some(Value::Null) => self.grok_enabled = None,
            _ => {}
        }
    }
}

pub fn default_settings_path() -> PathBuf {
    match std::env::var("AITRACK_SETTINGS") {
        Ok(p) if !p.is_empty() => expand_user(&p),
        _ => data_dir().join("settings.json"),
    }
}

/// Shared, thread-safe settings (the GUI edits them, the collector reads the interval).
pub struct Settings {
    pub path: PathBuf,
    values: Mutex<Values>,
    /// The file existed but could not be read as JSON when it was loaded: the first save keeps it
    /// as `settings.json.bak` instead of silently replacing it with defaults.
    unreadable: std::sync::atomic::AtomicBool,
}

impl Settings {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut v = Values::default();
        let text = std::fs::read_to_string(&path).ok();
        let parsed = text
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok());
        let unreadable = text.is_some() && !parsed.as_ref().is_some_and(Value::is_object);
        if let Some(j) = parsed {
            v.apply(&j);
            // A settings file from before the "Auto" badge look has no choice saved: those users
            // have been seeing the Filled look, so keep it. Only new files default to Auto.
            if j.is_object() && j.get("badge_style").is_none() {
                v.badge_style = BadgeVariant::Filled;
            }
        }
        Self {
            path,
            values: Mutex::new(v),
            unreadable: std::sync::atomic::AtomicBool::new(unreadable),
        }
    }

    pub fn load_default() -> Self {
        Self::load(default_settings_path())
    }

    pub fn get(&self) -> Values {
        *self.values.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Merge `new` (a JSON object with any of the keys), clamp, and optionally persist.
    pub fn update(&self, new: &Value, save: bool) -> Values {
        let mut g = self.values.lock().unwrap_or_else(|e| e.into_inner());
        g.apply(new);
        let v = *g;
        if save {
            self.save(&v);
        }
        v
    }

    /// Write the settings: to a temporary file, then renamed over the real one, so a crash cannot
    /// leave a half-written file. A file that could not be read when loaded is kept as `.bak`
    /// first. Best effort (a read-only home must not take the app down), but a failure is logged.
    fn save(&self, v: &Values) {
        let Ok(text) = serde_json::to_string_pretty(&v.to_json()) else {
            crate::alerts::log_line("settings: could not serialise the settings; nothing written");
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if self
            .unreadable
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let mut bak = self.path.clone().into_os_string();
            bak.push(".bak");
            if let Err(e) = std::fs::copy(&self.path, &bak) {
                crate::alerts::log_line(&format!(
                    "settings: could not keep the unreadable file: {e}"
                ));
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            crate::alerts::log_line(&format!("settings: could not save: {e}"));
        }
    }

    pub fn set_interval(&self, minutes: f64) -> Values {
        self.update(&json!({"interval_min": minutes}), true)
    }
    pub fn set_local_interval(&self, seconds: f64) -> Values {
        self.update(&json!({"local_interval_sec": seconds}), true)
    }
    pub fn set_direction(&self, d: Direction) -> Values {
        self.update(&json!({"direction": d.as_str()}), true)
    }
    pub fn set_startup(&self, on: bool) -> Values {
        self.update(&json!({"startup": on}), true)
    }
    pub fn set_claude_track(&self, t: Track) -> Values {
        self.update(&json!({"claude_enabled": t.to_opt()}), true)
    }
    pub fn set_grok_track(&self, t: Track) -> Values {
        self.update(&json!({"grok_enabled": t.to_opt()}), true)
    }
    pub fn set_grok_enabled(&self, on: bool) -> Values {
        self.update(&json!({"grok_enabled": on}), true)
    }
    pub fn set_window(&self, w: Option<WindowPos>) -> Values {
        self.update(&json!({"window": w.map(|w| w.to_json())}), true)
    }
    pub fn set_alert_sounds(&self, on: bool) -> Values {
        self.update(&json!({"alert_sounds": on}), true)
    }
    pub fn alert_options(&self) -> crate::alerts::AlertOptions {
        let v = self.get();
        crate::alerts::AlertOptions {
            sounds: v.alert_sounds,
        }
    }
    pub fn set_section(&self, name: &str, open: bool) -> Values {
        self.update(&json!({"ui": {"sections": {name: open}}}), true)
    }
    pub fn set_theme(&self, t: ThemeChoice) -> Values {
        self.update(&json!({"theme": t.as_str()}), true)
    }
    pub fn set_badge_style(&self, b: BadgeVariant) -> Values {
        self.update(&json!({"badge_style": b.as_str()}), true)
    }
    pub fn set_notifications(&self, on: bool) -> Values {
        self.update(&json!({"notifications": on}), true)
    }
    pub fn set_on_top(&self, on: bool) -> Values {
        self.update(&json!({"on_top": on}), true)
    }
    pub fn set_small(&self, on: bool) -> Values {
        self.update(&json!({"small": on}), true)
    }
}
