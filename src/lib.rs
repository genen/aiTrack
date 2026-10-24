// SPDX-License-Identifier: GPL-3.0-or-later
//! aiTrack core: everything except the window. No GUI dependencies, so it is unit-testable headless.
//!
//! * [`live`]: credential discovery, the usage endpoint call, normalisation into stored records
//! * [`store`]: append-only JSONL store behind the [`store::Store`] trait (+ in-memory [`store::History`])
//! * [`analysis`]: window grouping, pace, projection, per-window statistics
//! * [`settings`], [`collector`]: persisted settings and the timed poller thread
//! * [`state`], [`view`], [`app`]: view-model for the window

pub mod alerts;
pub mod analysis;
pub mod app;
pub mod appicon;
pub mod buildinfo;
pub mod cache;
pub mod codex;
pub mod collector;
pub mod detect;
pub mod endpoints;
pub mod grok;
pub mod grok_bot;
pub mod grok_cli;
pub mod instance;
pub mod launch;
pub mod live;
pub mod placement;
pub mod pricing;
pub mod reading;
pub mod settings;
pub mod sources;
pub mod startup;
pub mod state;
pub mod stats;
pub mod statsview;
pub mod store;
pub mod toast;
pub mod transcripts;
pub mod util;
pub mod vendor;
pub mod view;

#[cfg(feature = "gui")]
pub mod theme;
#[cfg(feature = "gui")]
pub mod ui;
