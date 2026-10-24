# Mac port hand-off

For the agent that builds the macOS version of aiTrack. The Windows version (0.3.0, the first official release) is
finished and a different agent keeps developing it. This file holds what the source and the README do not say:
decisions, history, owner preferences that matter for a port, what was and was not verified, and traps found on the
Windows side. Read the owner's rules in `genen/claude-setup` (`CLAUDE.general.md`) first, then this file, then the README.

Written 2026-10-15 at version 0.3.0 (commit 3dbbbed). Where a statement is a guess it is marked **unverified**.

## 1. The most important caveat

Everything in aiTrack has been run and checked on **Windows only** (the owner's PC, plus GitHub's `windows-latest`
runner for CI). Nothing has ever run on a Mac. The macOS and Linux paths already in `src/sources.rs` (`Os::Mac`,
`Os::Linux`) were written from general knowledge of Electron and Chromium conventions. **None of them is verified.**
Treat them as a starting point to check on a real Mac, not as facts. The cloud sessions are Linux boxes and cannot run
the app on macOS, so every claim about Mac behaviour needs either a macOS CI job or one command or screenshot from the
owner (see section 7).

## 2. What the app is, in one paragraph

A small always-on-top native window (Rust, eframe/egui 0.31, glow backend) that shows Claude and Grok plan usage as
phosphor LED-segment bars: session and weekly percent, reset times, pace, a history chart, a Stats screen with
per-project token and cost totals, and Windows toasts at 75, 90 and 100 percent. It reads local files written by other
tools; it does not log in to anything itself, and it never prints or stores secrets. The owner wants a small window,
**not a menu-bar item**, and the same look on every platform (charcoal background, amber for Claude, light grey for
Grok, vendor blocks flush together, tooltips for detail, light and dark themes both change the window background).

## 3. Where the numbers come from (and why it matters for a port)

Claude, in order: the Claude desktop app's Chromium HTTP cache, then its `plan-usage-history.json`, then
`~/.claude.json`, then the OAuth usage endpoint. Grok: live billing from `cli-chat-proxy.grok.com` (token from
`~/.grok/auth.json`), falling back to `~/.grok/logs/unified.jsonl`. Details are in README section 5.
Points a port agent will not find in the code:

- **Claude's numbers only update while the Claude desktop app is running.** The cache is written when the app talks to
  Claude. With the app closed the window keeps the last value, shows an amber age chip, dims after 60 minutes, and the
  status bar says the app is not running. This is by design (no pace projections from an old reading). The owner
  confirmed this behaviour on Windows. The README says it near the top.
- Reset times come only from the cache. History gives percentages but no reset time, so a missing reset is carried
  forward from the last stored one while still in the future.
- The usage API fallback needs a Claude Code login (`claude`, then `/login`). A login that exists only in the Claude
  desktop app is not usable. On the owner's PC the fallback has no login, so it has **never been exercised** against
  the live endpoint there.
- Grok: the CLI is at `~/.grok/bin/grok.exe` and is not on the PATH on the owner's PC. Starting the CLI refreshes the
  login and writes a billing line. "Refresh Grok now" starts it hidden, never sends a prompt, and stops it again. On a
  Mac the binary name and location are **unverified**.
- **Mac-specific unknowns to settle first:**
  - The Claude desktop app's data folder on macOS. The code guesses `~/Library/Application Support/Claude/`, with the
    cache at `<dir>/Cache/Cache_Data`. Unverified.
  - Whether the Mac cache uses the same Chromium "simple cache" layout that `src/cache.rs` parses. Unverified.
  - Where Claude Code keeps its OAuth login on macOS. From general knowledge it may be the **Keychain**, not
    `~/.claude/.credentials.json`; the Windows code only reads the file. If it is the Keychain, the API fallback needs a
    Keychain read (which prompts the user) or should simply be left out on Mac. Unverified; ask the owner or check on a
    real Mac before building anything.
  - Where the Grok CLI and its log live on a Mac.
- **Grok Bot's second weekly meter** (`src/grok_bot.rs`, added in 0.3.5-beta). Folder
  `~/Library/Application Support/Grok Bot/sand-client-persistence/` on a Mac and `%APPDATA%\Grok Bot\sand-client-persistence\`
  on Windows; each file is `<base32 key name>.blob`, and the one whose decoded name ends `.weekly-usage.cache` is a
  ~310-byte JSON file (`value.reading.usage.percentUsed`, `nextResetMs`, `value.reading.readAtMs`, `value.expiresAtMs`).
  **Layout as reported by the Grok Bot assistant, verified by us only by matching the values on the owner's Usage page
  (59.17 % and the reset time on the Mac); the Windows file was reported the same way and not seen by us.** The macOS
  code path is not compiled or run by us on a Mac (CI is the first compile). The app writes the file only while open.
  The same folder holds `auth.json` and `sand-secrets.json`: aiTrack must never open them (a test guards this).
- Use `aitrack --sources` (prints where data is or is not found, structure only, safe to paste) as the first command
  to ask the owner to run on a Mac. Never ask for tokens, org ids, or account ids.

## 4. Platform-specific surface (what needs a Mac equivalent)

Modules with `cfg(windows)` code or Windows assumptions, and what the Windows version does:

| Module | Windows behaviour | Mac work |
| --- | --- | --- |
| `src/toast.rs` | Native toasts via WinRT (`Windows.UI.Notifications`), AppUserModelID `aiTrack` registered under HKCU, logging no-op elsewhere. Sound is Windows' own reminder sound. | Notification Center (UNUserNotificationCenter or a small `osascript` fallback), permission prompt, optional sound. Alerts are **plain toasts only**: no persistent banners, no dock bounce or flashing (the owner removed those on purpose). |
| `src/startup.rs` | "Start with Windows" writes the HKCU Run key. | Login item or a LaunchAgent plist (the non-Windows build currently returns an error string). Rename the setting text for Mac. |
| `src/launch.rs` | Clicking a vendor badge launches the app: `claude://`, then the Store app, then classic paths. Grok: Grok Bot, then the Store app, then the CLI in a terminal. Links use `ShellExecuteW`. | `open` / `NSWorkspace` for `claude://` and app bundles. Keep the "fixed arguments, nothing through a shell string, never blocks the window, logs to `launch.log`" rules. |
| `src/instance.rs` | One window per user: an OS file lock on `~/.aitrack/aitrack.lock` (portable), plus bringing the running window to the front by its exact title with `EnumWindows` (Windows only). | Lock works as is. Raising the existing window needs a Mac method (NSRunningApplication or similar). |
| `src/placement.rs` | Remembers window position and width; handles the Windows minimised sentinel (about -32000); restores only if enough of the window is on some monitor. The UI-free rules are unit-tested. | Reuse the rules; supply Mac monitor rectangles (menu bar, notch, Retina scale, multiple displays). |
| `src/grok_cli.rs` | Starts the CLI with `CREATE_NO_WINDOW`, null stdio, kills the process tree. | Plain spawn with null stdio; terminate the child on exit. Find the binary (it may be a script). |
| `src/appicon.rs`, `build.rs`, `aitrack.rc` | Embeds `aitrack.ico`; window and header icons come from PNGs in `assets/icon/`. | `.icns` for the bundle; the PNG parts are portable. |
| `src/main.rs` | Windows-subsystem app (no console). | Normal GUI app; `--collect`, `--once`, `--sources` stay CLI flags. |
| `installer/`, `scripts/*.ps1` | WiX v6 MSI (all users), portable zip, MSI-in-zip. Every Windows zip also holds `README.md` (with `docs/img`), `README.pdf` and `LICENSE`; `scripts/readme-pdf.ps1` makes the PDF in the release build and `scripts/zip-check.ps1` fails the build if a file is missing. | `.app` bundle plus `.dmg` and zip; signing and notarization (section 6). **The Mac zip and dmg must follow the same rule**: README.md, README.pdf and LICENSE next to the app. Reuse `readme-pdf.ps1` if practical (`pwsh` is on macOS runners); it prints with Edge or Chrome and looks for Windows install paths, so the Mac job needs a macOS browser path or its own way to make the PDF. |
| `.github/workflows/release-msi.yml` | One `windows-latest` job: fmt, clippy `-D warnings`, tests, release build, "no test hooks" check, MSI and zips; an ubuntu job publishes. | Add a macOS job (build and test at least; release assets later). |

Everything else (collector, analysis, store, stats, transcripts, pricing, view, ui, theme) is platform-neutral and is
exercised by about 460 integration tests that run on Linux and Windows.

## 5. Traps found on Windows (they will happen again on macOS)

These each cost CI cycles. Build the macOS CI job knowing them.

- **Never assert on pixel-exact text or font metrics.** Windows font metrics gave a collapsed Settings height of
  524 pt where Linux gave less; the test range is now 380-580 pt. macOS will differ again. Tests of layout assert on
  rectangles and bounds derived from the window width, not on fonts.
- **Never compare paths with `/` or case-sensitively.** Windows needs separators and case ignored; **macOS (APFS) is
  case-insensitive by default** too. There is a `norm_path` helper in the tests.
- **Never assert wall-clock time.** A "400 files scanned in under 50 ms" test failed on a loaded runner. Assert work
  done (a per-thread decode counter) and keep any time bound only as a generous pathology guard. Polling waits in tests
  stop as soon as the condition holds and give up after 30 s.
- **Tests take an explicit `now`.** Never mix the real clock with fixed dates.
- **Process-global OS calls from parallel test threads can corrupt the heap.** The test binary crashed intermittently
  on Windows (`STATUS_HEAP_CORRUPTION`, about 1 in 7 parallel runs; 0 in 60 after the fix). The suspected cause, never
  proven with a stack trace: every `App` created a `WindowsNotifier` that set the process AppUserModelID and wrote a
  registry value from many threads at once. It is now behind a `Once`. Any macOS notification or app-delegate setup
  must also be once-per-process and thread-safe.
- **Tests must not start real programs or open real URLs.** The launcher takes an injected spawner and a URL opener;
  tests use fakes. Keep that for the Mac launcher.
- **egui tooltips default to a 600 pt wrap width**, far wider than the 310 pt window. The app now sets the tooltip
  width every frame (`ui::tooltip_width`) so tooltips wrap inside the window and stay on screen.
- **A release build must not contain the test hooks.** `test-hooks` is a feature (env overrides for endpoint URLs and
  the Grok program, plain-http mock servers). `cargo test` turns it on through a self dev-dependency. CI fails if the
  release exe contains the hook marker string; do the same check for the Mac binary.
- Release profile is `panic = "abort"` with `strip`: a panic ends the process silently. Do not add `unwrap()`,
  `expect()` or indexing on any path that parses files, JSON or network data. Return an error or skip the item.
- Run before pushing (cloud sandbox): `cargo fmt`, clippy with `-D warnings` for default features and for
  `--no-default-features`, tests, and `cargo check --target x86_64-pc-windows-gnu`. Add the macOS equivalent in CI.
- Disk: build artifacts fill the cloud sandbox. Use `scripts/test.sh` (it prunes old artifacts after tests). Never run
  `cargo clean`.
- `examples/render.rs` draws the UI headlessly to PNG (used for `docs/img/`); useful to look at layout changes from a
  Linux sandbox without a Mac.

## 6. Process and rules that apply to the Mac work too

- Work on a `claude/...` branch from `main`, push, open a **draft** pull request into `main`. Do not push to `main`.
  Merge your own pull request once the checks pass (the owner's standing rule). `main` is not branch-protected.
- Every merge bumps the last version digit and keeps `-beta`. The first official release was **0.3.0**; builds after it
  are 0.3.1-beta, 0.3.2-beta, and so on, until the owner declares the next stable. Update `Cargo.toml` and
  `Cargo.lock` in the pull request being merged. **How Mac builds are numbered and tagged relative to the Windows ones
  is undecided; ask the owner.** One shared version for all platforms is the simplest guess.
- A release is started only when the owner says "release it": run the release workflow with tag `v<version>`. A plain
  version makes a normal release marked Latest; a `-beta` version makes a pre-release.
- **Every zip and dmg we publish** (Windows zips, the Mac zip and the Mac dmg) contains `README.md`, `README.pdf` and
  `LICENSE`, generated in the release build from the same commit; the release build fails if they are missing. The
  PDF is never committed. See `scripts/readme-pdf.ps1` and section 4.
- The repo is **private** and stays private until the owner says so. Releases are test downloads for the owner until
  then. Builds are **not code-signed** yet (Windows shows "Unknown publisher").
- **Signing and notarization.** SignPath Foundation offers free Windows signing for open-source projects, but only for a
  public repo with an OSI licence. That is for later. A Mac release will need an Apple Developer ID and notarization;
  the Apple Developer Program is believed to be 99 USD a year (**unconfirmed**) and would also cover the planned iPhone
  app. Do not buy or enrol anything; ask the owner.
- Licence: GPL-3.0-or-later. `LICENSE` is in the repo. Source files do **not** have SPDX headers yet: ask before adding
  them. All 371 Rust dependencies at the time were permissive; check any new Mac-only crate (for example for
  notifications) for GPLv3 compatibility.
- Owner style: short answers (they read on a phone and in Windows terminals); say what was and was not verified;
  never claim something ran on their machine unless it did; ask before irreversible or outward-facing steps; give the
  exact test command. For a Windows test they use `D:\git\aiTrack` and a fixed pull-and-run line. **A
  Mac equivalent does not exist yet; ask what Mac the owner has, or whether they have one at all.**
- Subagent rules: ask for a progress note every 5 minutes while one runs; every summary names the model each subagent
  ran on and each is recorded in `docs/subagent-log.md`. The code review by an Opus-model subagent happens **only when
  the owner asks**.
- Do not add vendors early. The order is Windows (done), Mac, Linux, then Gemini and ChatGPT. After Mac and Linux, an
  iPhone viewer is planned (a native app that shows numbers the desktop app sends through a small relay; the plan is in
  an owner-private page and the open decisions are alerts versus widgets only, hosting the relay, and Android).
  The Mac port was meant to include the desktop-side "send a summary to the relay" option; not started, not decided.
- The `genen/claude-setup` repo holds the owner's general rules that apply to every project. (The Claude and Grok Bot "agent notes" thread lived in the old private repo and was not carried over; the Claude sessions now message each other directly.)

## 7. Open questions for the owner (ask once, early)

1. Which Mac (Apple silicon, Intel, macOS version), and can they run a test command and send back its output? Does the
   owner have a Mac at all, or should CI be the only test bed at first?
2. Version numbers and tags for Mac builds: shared with Windows, or separate?
3. Distribution format: `.dmg`, zip, or both; universal binary or arm64 only; minimum macOS version.
4. App bundle identifier (undecided; do not invent one) and whether a Mac release waits for Apple signing.
5. Whether the Mac build should include the relay "send summary" option now or leave it for the iPhone work.
6. Whether to read the Claude Code login from the Keychain, or skip the API fallback on Mac.

## 8. Where things are

- `genen/claude-setup` (`CLAUDE.general.md`): the owner's rules for every project. **Authoritative.**
- `README.md`: user guide; section 5 covers data sources, the last sections cover internals and the roadmap.
- `CHANGELOG.md`: what each version changed.
- `docs/subagent-log.md`: which model each subagent ran on.
- `src/sources.rs`: path discovery per OS (`Os::{Windows, Mac, Linux}`), the source abstraction, and the cache source.
- `src/cache.rs`: the Chromium cache reader. `src/collector.rs`: polling loop. `src/state.rs`, `src/store.rs`: storage
  (append-only JSONL under `~/.aitrack/`, behind a trait).
- `tests/it/`: one integration binary; `tests/alerts_override.rs`: the one test that needs its own process.
- `.github/workflows/release-msi.yml`: the build and release pipeline for Windows.
