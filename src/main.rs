// SPDX-License-Identifier: GPL-3.0-or-later
//! aiTrack: a small native window that tracks Claude plan usage.
//!
//! * no flags: open the window (the collector runs on a background thread)
//! * `--collect`: headless collector loop, one log line per poll
//! * `--once`: take a single reading and exit (implies `--collect`)
//! * `--interval MIN`, `--data PATH`
//! * only the window and `--collect` take the single-instance lock; `--once`, `--sources` and
//!   `--help` never do
//! * `--sources`: print where usage data is (or is not) found, structure only, and exit

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(feature = "gui")]
use aitrack::app::App;
use aitrack::instance::{self, Acquire};
use aitrack::live::{self, Poller};
#[cfg(feature = "gui")]
use aitrack::settings::Settings;
use aitrack::store::{default_data_path, JsonlStore};
use std::path::PathBuf;
#[cfg(feature = "gui")]
use std::sync::Arc;

const USAGE: &str = "aiTrack - Claude, Grok and ChatGPT plan usage tracker

USAGE:
    aitrack [OPTIONS]

With no options, opens the usage window.

OPTIONS:
    --collect          run the headless collector (polls on a timer, prints one line per poll)
    --once             take a single reading and exit (implies --collect)
    --interval MIN     collector poll interval in minutes (default 15; headless mode only)
    --data PATH        usage snapshot file (default ~/.aitrack/usage.jsonl or $AITRACK_DATA)
    --projects [R]     per-project token / cost usage from local transcripts (R = 7, 30 or all; default 7);
                       prints project names, tokens, cost, sessions and the folders checked, no message content
    --sources          diagnose the usage sources (paths, file structure, ages); prints no ids or tokens
    -V, --version      print the build (version, build number, commit, date) and exit
    -h, --help         show this help

ENVIRONMENT:
    AITRACK_WAYLAND=native   Linux: use native Wayland instead of XWayland (no always-on-top,
                             window position is not remembered)
    CODEX_HOME               folder of Codex (default ~/.codex)
";

#[derive(Default)]
struct Cli {
    collect: bool,
    once: bool,
    interval: Option<f64>,
    data: Option<PathBuf>,
    sources: bool,
    projects: Option<aitrack::transcripts::ProjRange>,
    help: bool,
    version: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "--collect" => cli.collect = true,
            "--once" => {
                cli.once = true;
                cli.collect = true;
            }
            "--app" => {}
            "--sources" => cli.sources = true,
            "--projects" => {
                use aitrack::transcripts::ProjRange;
                let given = match &inline {
                    Some(v) => Some(v.clone()),
                    None => it
                        .next_if(|n| ProjRange::parse(n).is_some())
                        .map(|n| n.to_owned()),
                };
                cli.projects = Some(match given {
                    Some(v) => ProjRange::parse(&v)
                        .ok_or_else(|| format!("--projects: expected 7, 30 or all, got {v}"))?,
                    None => ProjRange::Days7,
                });
            }
            "--interval" => {
                let v = value("--interval")?;
                let n: f64 = v
                    .parse()
                    .map_err(|_| format!("--interval: not a number: {v}"))?;
                if !(n > 0.0 && n.is_finite()) {
                    return Err("--interval must be greater than 0".into());
                }
                cli.interval = Some(n);
            }
            "--data" => cli.data = Some(PathBuf::from(value("--data")?)),
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(cli)
}

/// `--projects`: scan the local transcripts (cached index next to the data file) and print the table.
fn projects_report(range: aitrack::transcripts::ProjRange) -> String {
    use aitrack::pricing::{Paths, PriceTable};
    use aitrack::transcripts::{aggregate, projects_text, Roots, Scanner};
    let env = aitrack::sources::Env::from_process();
    let data_dir = aitrack::util::data_dir();
    let mut scanner = Scanner::new(Roots::from_env(
        &env,
        Some(data_dir.join("transcript-index.json")),
    ));
    scanner.scan();
    let now = chrono::Utc::now();
    // prices from the cache only: the network is left to the window's background refresh
    let prices = PriceTable::load(&Paths::in_dir(&data_dir), now, false);
    let tz = chrono::Local::now().offset().local_minus_utc() / 60;
    let report = aggregate(scanner.requests(), &prices, now, tz);
    projects_text(&report, range, scanner.sources())
}

/// A release Windows build has no console; attach to the parent's so `--collect` output shows.
#[cfg(windows)]
fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: plain Win32 call with a constant argument (ATTACH_PARENT_PROCESS = u32::MAX).
    unsafe {
        AttachConsole(u32::MAX);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            attach_console();
            eprintln!("aitrack: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if cli.help || cli.version || cli.collect || cli.sources || cli.projects.is_some() {
        attach_console();
    }
    if cli.help {
        print!("{USAGE}");
        return;
    }
    if cli.version {
        println!("{}", aitrack::buildinfo::line());
        return;
    }
    if cli.sources {
        println!("{}", aitrack::buildinfo::line());
        #[cfg(target_os = "linux")]
        {
            let d = aitrack::placement::current_decision();
            println!(
                "{} [{}]",
                aitrack::placement::backend_line(d.choice, None),
                d.why
            );
        }
        print!(
            "{}",
            aitrack::sources::diagnose(&aitrack::sources::Env::from_process(), chrono::Utc::now())
        );
        return;
    }
    if let Some(range) = cli.projects {
        print!("{}", projects_report(range));
        return;
    }
    let data = cli.data.clone().unwrap_or_else(default_data_path);

    if cli.collect {
        // the endless collector must not run next to another collector (or the window's);
        // a one-shot --once is harmless and takes no lock
        let lock = if cli.once {
            None
        } else {
            match instance::acquire(&instance::lock_path()) {
                Acquire::Acquired(g) => Some(g),
                Acquire::AlreadyRunning { pid } => {
                    println!(
                        "{}; not starting a second collector",
                        instance::running_text(pid)
                    );
                    std::process::exit(3);
                }
                Acquire::Unavailable(why) => {
                    eprintln!(
                        "aitrack: single-instance lock unavailable ({why}); continuing without it"
                    );
                    None
                }
            }
        };
        let store = JsonlStore::new(data);
        let interval = cli.interval.unwrap_or(live::DEFAULT_INTERVAL_MIN);
        let code = live::run(&Poller::from_env(), &store, interval, cli.once, &mut |l| {
            println!("{l}")
        });
        drop(lock);
        std::process::exit(code);
    }

    open_window(data);
}

#[cfg(feature = "gui")]
fn open_window(data: PathBuf) {
    let lock = match instance::acquire(&instance::lock_path()) {
        Acquire::Acquired(g) => Some(g),
        Acquire::AlreadyRunning { pid } => {
            // a second window is never opened: show the running one instead
            if !instance::raise_existing_window(pid) {
                let since = instance::started_at(&instance::lock_path()).map(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                });
                let text = instance::running_text_since(pid, since.as_deref());
                eprintln!("{text}");
                aitrack::alerts::log_line(&format!("instance: {text}"));
            }
            return;
        }
        Acquire::Unavailable(why) => {
            eprintln!("aitrack: single-instance lock unavailable ({why}); continuing without it");
            None
        }
    };
    // Linux: pick X11 (XWayland) or native Wayland now, before any thread starts
    #[cfg(target_os = "linux")]
    let chosen = aitrack::ui::prepare_display();
    let settings = Arc::new(Settings::load_default());
    // Grok readings go to their own file next to the Claude one
    let grok_file = data.with_file_name("usage-grok.jsonl");
    let grok_bot_file = data.with_file_name("usage-grok-bot.jsonl");
    let codex_file = data.with_file_name("usage-codex.jsonl");
    let app = Arc::new(
        App::new(
            Box::new(JsonlStore::new(data)),
            settings,
            Poller::from_env(),
        )
        .with_grok_poller(
            Box::new(JsonlStore::new(grok_file)),
            Poller::grok_from_env(),
            Arc::new(|| aitrack::grok::detected(&aitrack::sources::Env::from_process())),
        )
        .with_grok_bot(
            Box::new(JsonlStore::new(grok_bot_file)),
            aitrack::sources::Env::from_process(),
        )
        .with_codex_poller(
            Box::new(JsonlStore::new(codex_file)),
            Poller::codex_from_env(),
            Arc::new(|| aitrack::codex::detected(&aitrack::sources::Env::from_process())),
        )
        .with_stats(
            aitrack::transcripts::Roots::from_env(
                &aitrack::sources::Env::from_process(),
                Some(aitrack::util::data_dir().join("transcript-index.json")),
            ),
            aitrack::pricing::Paths::from_env(),
        ),
    );
    // Claude's panel (and its polling, and its alerts) only when its tools are installed
    app.set_claude_detect(Arc::new(|| {
        aitrack::detect::claude_present(&aitrack::sources::Env::from_process())
    }));
    // usage history is kept for 90 days (checked after loading, then daily)
    app.enable_auto_prune();
    app.start();
    if let Err(e) = aitrack::ui::run(app) {
        attach_console();
        eprintln!("aitrack: window error: {e}");
        #[cfg(target_os = "linux")]
        eprintln!("aitrack: {}", aitrack::placement::failure_hint(chosen));
        drop(lock);
        std::process::exit(1);
    }
    drop(lock);
}

#[cfg(not(feature = "gui"))]
fn open_window(_data: PathBuf) {
    attach_console();
    eprintln!("aitrack: built without the `gui` feature; use --collect, --once or --sources");
    std::process::exit(2);
}
