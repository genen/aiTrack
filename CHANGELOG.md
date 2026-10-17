# Changelog

All notable changes to aiTrack. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are in `Cargo.toml`; a release tag is `v<version>`.

## [0.3.9-beta] - 2026-10-17

### Fixed
- Release workflows: the Windows publish step now behaves like the macOS one. A create race between the two jobs no
  longer stops the Windows upload, and the Windows section ("Windows SHA256:") is appended to an existing release text
  once instead of replacing it, so the macOS checksums and signing note survive. Unverified until a real release run.
- Polls take their stored timestamps and the token-expiry check from the poll's own `now`, not the real clock.
- The Windows spawner honours `AITRACK_NO_SPAWN` like the macOS one.
- Usage records no longer store the whole server response under `raw`: only known fields, with account, organisation
  and user id keys removed. Older files still load.
- Tests that set environment variables moved to their own test binaries.

## [0.3.8-beta] - 2026-10-17

### Fixed
- macOS alerts: an alert is no longer recorded as sent when macOS did not show it. The first alert waits (a few
  seconds) for the notification permission answer; if it is still undecided the alert is offered again at the next
  poll. If you turned aiTrack's notifications off in System Settings, aiTrack respects it (no fallback through
  "Script Editor"; logged once). Other native errors still fall back to `osascript`. **Unverified on a real Mac.**
- macOS Keychain read: waits at most 10 seconds for `security` (an unanswered access dialog no longer stalls Claude
  polling), then carries on with the other sources.
- macOS Grok refresh: the CLI starts in its own process group and the whole group is ended; `PATH` for that child is
  extended with `~/.grok/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` so script wrappers
  (`#!/usr/bin/env node`) start from a Finder-launched app.

## [0.3.7-beta] - 2026-10-17

### Changed
- Every Rust source file now starts with `// SPDX-License-Identifier: GPL-3.0-or-later` (the owner approved the
  headers). No code changes.

## [0.3.6-beta] - 2026-10-17

### Changed
- Grok Bot bar: the age label in the Grok header now appears after 60 minutes (it was 15), the same as the dimming.
  The Grok Bot app rewrites its cache only every 30 to 40 minutes while it is open (checked on a Mac and a PC), so a
  15 minute label was almost always showing. The tooltips now say so.

## [0.3.5-beta] - 2026-10-17

### Added
- Grok block: a second bar, **Grok Bot**, for the Grok Bot app's "Weekly Grok Bot Limit" (macOS and Windows). It is read
  from the app's local cache file, only while the app has written it (dimmed after 60 minutes, dashes when the weekly
  period has ended). Hidden when the file is not found. The file layout was reported by Grok Bot and matches the Usage
  page by value; **it is unverified by us** (Windows location included). Only the one weekly-usage file is opened, never
  the app's `auth.json` or `sand-secrets.json`; no account id or file name is logged or stored.
- `usage-grok-bot.jsonl`: its own append-only history (records with a `bot_weekly` field); old `usage-grok.jsonl` files
  are untouched. `aitrack --sources` lists the source as found, stale or not found.
- Env override `AITRACK_GROK_BOT_DIR` for the folder.
- Not yet: alerts for this bar.

## [0.3.4-beta] - 2026-10-17

### Added
- macOS releases can be signed with a Developer ID certificate and notarized by Apple. The release workflow does this
  only when the repository has the Apple secrets; without them (pull requests, forks, local builds) the app stays
  ad-hoc signed, and the release notes say which it is. See `docs/MAC-SIGNING.md`.
- The macOS zip and dmg now carry `README.md` (with `docs/img`), `README.pdf` and `LICENSE` next to the app (not inside
  it, so the signature holds), made in the release build from the same commit; the build fails if any is missing.

### Changed
- Updated the TLS library `rustls` to 0.23.46, which fixes four low and medium severity advisories.

### Docs
- README: a note about Windows Defender flagging the unsigned downloads as `Trojan:Win32/Cloxer` (reported to
  Microsoft as a suspected false positive), and the Mac checks that are now confirmed (window position, Dock name).

## [0.3.3-beta] - 2026-10-17

### Added
- Every Windows zip (portable and MSI-in-zip) now holds `README.md` with its pictures (`docs\img`), `README.pdf` and
  `LICENSE`. The PDF is made in the release build from the same commit (`scripts/readme-pdf.ps1`, Edge headless),
  never committed, and the build fails if a zip lacks any of these files; each zip's file list is printed in the log.
  The bare `.msi` is unchanged.

### Docs
- Rule for later builds: every zip and dmg we publish (the Mac zip and dmg too) carries `README.md`, `README.pdf` and
  `LICENSE` (CLAUDE.md, `docs/MAC-HANDOFF.md`). The Mac scripts do not do this yet.

## [0.3.2-beta] - 2026-10-16

### Added
- macOS support (beta, Apple silicon). Claude numbers come from the Claude app's data folder in
  `~/Library/Application Support/Claude`; Grok from the CLI (`~/.local/bin` or `~/.grok/bin`).
- Clicking a logo opens the app with `open -a`; a second launch raises the running window.
- Start at login through a LaunchAgent (`com.sirgene.aitrack`).
- Native macOS notifications with the aiTrack icon when run from the app, with a Script Editor banner fallback
  outside the bundle.
- `scripts/make-app.sh` builds `dist/aiTrack.app` (ad-hoc signed); `scripts/make-dmg.sh` and the
  `release-macos.yml` workflow build the dmg and zip.
- The README covers macOS.

### Changed
- Settings says "Found on this Mac" on a Mac.

### Fixed
- The window height fits its content on macOS.
- Tests no longer start real programs.
- `scripts/prune-target.sh` works with macOS bash and BSD tools.

## [0.3.1-beta] - 2026-10-15

### Docs
- `docs/MAC-HANDOFF.md`: what the next agent needs for the macOS port (what is verified and what is not, the
  platform-specific code, traps found on Windows, process rules, open questions for the owner).

## [0.3.0] - 2026-10-15

The first official release. 0.2.0 was an earlier test release; the 0.2.x entries below stay as history.

### Since 0.2.0
- Settings > About: a link to the project, "Developed by Sir Gene" with the contact address, and the licence name as a
  link to the LICENSE file.
- Tooltips wrap inside the window and stay on screen.
- Fixed a rare crash of the test process on Windows (the toast app id is now set once per process) and made the flaky
  cache tests deterministic.
- The launcher threads initialise COM before opening links, and tests never start real programs.
- The README says the Claude desktop app has to be running for Claude's numbers to update, and that Grok's update
  when the Grok CLI runs.
- `docs/subagent-log.md` records which model each subagent ran on.

## [0.2.7-beta] - 2026-10-15

### Changed
- Settings > About: the License row's View button is gone; the licence name `GPL-3.0-or-later` is an underlined
  link (like the project link) that opens the LICENSE file, with the URL as hover text.

## [0.2.6-beta] - 2026-10-15

### Fixed
- Tooltips (the age chip, bars, Settings) no longer run off the edge of the 310 pt window: they wrap at the window
  width minus 12 pt on each side (egui's default is 600 pt) and stay on screen.

### Docs
- The README says near the top and in the Claude data-source section that the Claude desktop app has to be running
  for Claude's numbers to update, and that Grok's update when the Grok CLI runs.

## [0.2.5-beta] - 2026-10-15

### Changed
- Settings > About now reads: "aiTrack" with a link to github.com/genen/aiTrack (the version and build text are gone;
  the build line is at the bottom of Settings), "Developed by Sir Gene" with aitrack@sirgene.com under it as plain
  text, and License with View. The Contact row and its Email and Copy buttons are gone.

### Docs
- `docs/subagent-log.md` records which model each subagent ran on and where that was verified; CLAUDE.md asks for it.

## [0.2.4-beta] - 2026-10-15

### Changed
- Settings > About: the "Source code" row and the Copy button on the build line are gone (the build line at the
  bottom of Settings still copies on click). License (View) and Contact (Email, Copy) stay.

## [0.2.3-beta] - 2026-10-15

### Fixed
- Two tests that failed now and then on Windows CI. `live_local_polling...` counted the stored rows as soon as the
  cache reading appeared, while the same poll was still appending its backfilled rows; it now waits for the poll to
  finish. `scanning_a_400_file_cache_stays_cheap...` asserted a 50 ms wall time; it now checks the work done (repeated
  scans of unchanged files decode no body again) and keeps only a 2 s guard against pathologies. The shared decode
  memo holds 1024 bodies instead of 64, so parallel tests do not empty it under each other.

## [0.2.2-beta] - 2026-10-15

### Fixed
- Rare crash of the test process on Windows CI (heap corruption, exit 0xc0000374): every App created a toast
  notifier, and each one set the process-wide toast app id (`SetCurrentProcessExplicitAppUserModelID` plus a registry
  write) from parallel test threads. It is now done once per process. The same crash showed in older commits at about
  1 in 7 parallel runs (not with `--test-threads=1`); after the change 60 parallel runs on windows-latest had none.
  The shell call is the suspect found by elimination (no stack was captured), so the cause is likely, not proven.

## [0.2.1-beta] - 2026-10-15

### Fixed
- Opening a link or program from a launcher thread (Claude desktop, Help, About) now runs in an initialised COM
  apartment, as `ShellExecute` requires. A Windows CI run once crashed the test process with a heap corruption
  (0xc0000374) while a test really launched Claude on the runner; that test now uses a fake spawner, so tests never
  start programs on the machine they run on. The crash was intermittent and has not been reproduced here, so the
  cause is not proven.

## [0.2.0] - 2026-10-15

The first release (the owner dropped the -beta suffix); the 0.1.x builds were test builds on the way here.

### Added
- Settings > About (last section, collapsed at first): app name and build line with Copy, developer credit, the
  GPL-3.0-or-later licence with a link to it, a link to the repository, and the contact address
  aitrack@sirgene.com with Email (`mailto:`) and Copy buttons.

- The release also carries the installer in a zip (`aiTrack-Setup-<version>-win-x64-msi.zip`, same MSI bytes) for
  browsers that block a bare `.msi`; the workflow checks the zip holds the identical file and lists its SHA256.

### Fixed
- The GitHub release is titled "aiTrack <version>" and its notes start "aiTrack <version> (pre-release)"; the
  channel word was appended twice ("0.1.6-beta beta").

## [0.1.6-beta] - 2026-10-15

### Changed
- Settings > Notifications: hovering the **Alert sounds** row says that Windows must also allow notification sounds
  (Settings > System > Notifications > Allow notifications to play sounds). The README says the same under
  Notifications and Troubleshooting.

### Fixed
- Tests no longer depend on path case or separators, or on Linux font metrics (found by the first Windows CI run).

## [0.1.5-beta] - 2026-10-15

### Fixed
- A carried reset time that has passed now ends the window (dashes and "waiting for a new reading"), like a reported
  one.
- By-model limits come only from the current weekly window; past model resets are no longer listed.
- Readings dated in the future (clock skew) are ignored instead of becoming "now".
- A bad `Retry-After` header (huge, negative, not a number) can no longer panic the window; the wait is capped.
- The Grok CLI is looked for off the UI thread (a dead network share on PATH no longer freezes the window), a running
  Grok refresh is ended when the window closes, and a second launch raises only the window of the running aiTrack.
- The Grok chip age follows the newest reading, so it no longer says "about 50 minutes ago" beside fresh polls; a
  failing live source now leaves a row in Diagnostics.
- A window that gains its reset time later no longer repeats alerts it already sent.
- The Store app's `grok.exe` alias in `WindowsApps` is never taken for the Grok CLI.
- The render example never raises real toasts; a build with test hooks says so in its build line.
- The prune scripts keep the newest build of every crate (live dependencies were deleted and rebuilt on every run).

### Changed
- Stats > Projects: each row starts with the Claude or Grok logo and the tooltip names the vendor; `--projects`
  marks Grok rows with ` (Grok)`.

## [0.1.4-beta] - 2026-10-15

### Added
- Windows installer (MSI, WiX Toolset v6): all users, Program Files, Start menu and desktop shortcuts, a "Run after
  install?" box, upgrades in place and refuses downgrades. Uninstall keeps `~/.aitrack` and removes the "Start with
  Windows" value of the user who uninstalls.
- Portable zip (`aiTrack-<version>-win-x64.zip`) with `aitrack.exe`, `LICENSE` and `README.md`.
- GitHub Actions workflow that checks, tests and builds both on every push and pull request and publishes them on a
  `v<version>` tag, with SHA256 checksums in the release notes.
- `scripts/installer.ps1`, `scripts/portable-zip.ps1`, `scripts/build-release.ps1`.
- GPL-3.0-or-later licence (`LICENSE`, shown on the installer's licence page).

## [0.1.3-beta] - 2026-10-14

### Fixed
- Window grouping with tens of thousands of readings is fast (it was quadratic) and a fall across a window boundary no
  longer splits a window in two (duplicate alerts, doubled window counts).
- A reset time a reading itself reported that has passed ends the window: the row shows dashes and "waiting for a new
  reading" instead of "resets in 1m" with the old percentage.
- Live readings carry the note why local sources were skipped; the status line follows the vendors shown; the
  "By model" block survives a newer history sample.
- Tests no longer depend on the date they run on; the cache creation-time check takes `now`.

### Changed
- Minimum Rust version declared (1.89).

## [0.1.2-beta] - 2026-10-14

### Added
- A user-guide README with screenshots, a section on the Grok CLI and its installer, and troubleshooting.

## [0.1.1-beta] - 2026-10-14

### Added
- History chart: one connected line per window, thinner glow, zoomed y axis for low data, and a default range that
  fits the stored data.

### Fixed
- Grok panel kept while its history lock is busy; stable cross-file de-duplication of transcripts; backfill flag,
  expired-login and multi-model Grok cost handling; windows with and without reset times are one window.
- settings.json is written atomically and an unreadable file is kept as `.bak`.

### Security
- Credentials are sent only to `api.anthropic.com` and `cli-chat-proxy.grok.com` over HTTPS, redirects are not
  followed, bodies and login files are capped at 1 MiB, header values are checked. The `AITRACK_*` URL and Grok program
  overrides work only in test builds.

## [0.1.0] - 2026-10-14

First native version (Rust, egui), ported from the earlier Python tool, which was then removed.

### Added
- Always-on-top window with LED-bar rows per limit: Claude session and weekly, Grok weekly; hover tooltips with reset
  time, pace and source; click a row for details and a history chart.
- Claude sources in order: Claude desktop app cache, desktop app history, `~/.claude.json`, the usage service.
  Reset times are carried forward from earlier readings when a source has none.
- Grok: live billing with the Grok CLI's login, falling back to the CLI log; last known value kept, dimmed, with a
  data-age label; "Refresh Grok now" starts the CLI briefly with no window and no prompt.
- Windows notifications at 75 %, 90 % and 100 %, when a limit would run out within 30 minutes, and after a reset.
- Stats view: today's cost, per-project tokens and cost (Claude Code and Grok), plan statistics.
- Settings: update frequency, theme, logo look, remaining/used, always on top, Show Claude/Grok (Auto/On/Off),
  start with Windows, alert sounds, diagnostics log. The window remembers its position.
- Clicking a logo opens that tool. Single-instance lock. `aitrack --sources`, `--projects`, `--collect`, `--once`.
- App icon, build number in `--version` and Settings.
