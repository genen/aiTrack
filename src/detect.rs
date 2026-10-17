// SPDX-License-Identifier: GPL-3.0-or-later
//! Which tools are installed? (stat only, so it is cheap enough to ask every few seconds)
//!
//! * **Claude** is present when the Claude desktop app's data folder exists (`%APPDATA%\Claude`,
//!   `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude` for the Store app,
//!   `~/Library/Application Support/Claude` on macOS, `~/.config/Claude` on Linux; see
//!   [`crate::sources::desktop_data_dirs`]) **or** a Claude Code config
//!   folder exists (`$CLAUDE_CONFIG_DIR`, else `~/.claude`).
//! * **Grok** is present when the Grok CLI's folder exists (`$GROK_HOME`, else `~/.grok`).
//!
//! Detection is about the tool, not about data: a present tool with no reading yet shows its
//! panel with its status; an absent one shows nothing (no panel, no polling noise, no alerts).
//! The settings can override it per vendor (Auto / On / Off).

use crate::sources::Env;
use std::path::PathBuf;

/// Claude Code's config folder: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_code_dir(env: &Env) -> PathBuf {
    env.claude_config_dir
        .clone()
        .unwrap_or_else(|| env.home.clone().unwrap_or_default().join(".claude"))
}

/// Is the Claude desktop app or Claude Code installed for this user?
pub fn claude_present(env: &Env) -> bool {
    claude_code_dir(env).is_dir()
        || crate::sources::desktop_data_dirs(env)
            .iter()
            .any(|d| d.is_dir())
}

/// Is the Grok CLI installed for this user?
pub fn grok_present(env: &Env) -> bool {
    crate::grok::detected(env)
}

/// Both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub claude: bool,
    pub grok: bool,
}

pub fn detect(env: &Env) -> Presence {
    Presence {
        claude: claude_present(env),
        grok: grok_present(env),
    }
}
