// SPDX-License-Identifier: GPL-3.0-or-later
//! All integration tests in one binary: every file in `tests/` used to be its own program that
//! linked the whole app (egui and all), which multiplied build time and disk use. Each former
//! `tests/<name>.rs` is now the module `tests/it/<name>.rs`.
//!
//! Tests that change process-wide state (environment variables) and must not see each other's
//! changes live in their own binaries: `tests/alerts_override.rs` and `tests/ui_launch_override.rs`.

#[path = "../common/mod.rs"]
mod common;

mod alerts;
mod analysis;
mod appicon;
mod badges;
mod buildinfo;
mod cache;
mod carry;
mod chart_gaps;
mod chip;
mod cli;
mod collector;
mod detect;
mod grok;
mod grok_app;
mod grok_bot;
mod grok_bot_chart;
mod grok_refresh;
mod instance;
mod launch;
mod linux_sources;
mod live;
mod macos_bundle;
mod pace;
mod placement;
mod pricing;
mod projects_cli;
mod prune_script;
mod release_review;
mod review;
mod settings_layout;
mod sources;
mod stale;
mod stats;
mod statsview;
mod store;
mod theme;
mod toast;
mod transcripts;
mod ui_empty;
mod ui_launch;
mod ui_stats;
mod view;
mod window_fit;
