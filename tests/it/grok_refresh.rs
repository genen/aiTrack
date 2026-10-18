// SPDX-License-Identifier: GPL-3.0-or-later
//! Grok staleness and the Refresh control: an old log sample is still shown (dimmed, with the
//! chip), the refresh run (command, gating, kill path), finding the CLI, and launching Grok.
use crate::common;
use aitrack::app::{App, RefreshMode};
use aitrack::collector::Status;
use aitrack::grok::{self, GrokLog};
use aitrack::grok_cli::{self, Child, Host, RefreshCfg};
use aitrack::launch::{
    self, grok_bot_targets, grok_store_targets, launch_grok, parse_start_apps, terminal_command,
    Attempt, GrokLaunchEnv, Spawner,
};
use aitrack::live::Poller;
use aitrack::reading::{Kind, Reading};
use aitrack::settings::{Settings, Values};
use aitrack::sources::{Env, Os};
use aitrack::state::{build_grok_state, build_state};
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::{self, VendorId};
use aitrack::view;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn ts(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn billing_line(fetched: DateTime<Utc>, pct: f64, end: DateTime<Utc>) -> String {
    json!({"ts": ts(fetched), "level": "info", "msg": "billing: fetched credits config",
        "ctx": {"config": {"creditUsagePercent": pct, "currentPeriod": {"end": ts(end)}}, "subscriptionTier": "SuperGrok"}})
    .to_string()
}

struct Home {
    dir: TempDir,
    env: Env,
}

fn home() -> Home {
    let dir = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.grok_home = Some(dir.join("grokhome"));
    std::fs::create_dir_all(dir.join("grokhome/logs")).unwrap();
    Home { dir, env }
}

fn write_log(h: &Home, lines: &[String]) {
    std::fs::write(grok::log_path(&h.env), lines.join("\n") + "\n").unwrap();
}

fn status() -> Status {
    Status {
        error: None,
        polling: false,
        next_in: None,
        polls: 1,
        live: false,
        sample: None,
    }
}

/// Poll the log chain at `at` and return the stored Grok readings.
fn stored_readings(h: &Home, at: DateTime<Utc>) -> (Vec<Reading>, Option<String>) {
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let poller = Poller::with_chain_for(vec![Box::new(GrokLog { env: h.env.clone() })], "grok");
    let err = poller.poll_at(&store, at).err().map(|e| e.message);
    (store.read().iter().map(Reading::from_value).collect(), err)
}

fn state_with(readings: &[Reading], at: DateTime<Utc>) -> aitrack::state::State {
    let mut st = build_state(&[], Values::default(), status(), at);
    st.grok = Some(build_grok_state(readings, status(), true, at));
    st
}

// ------------------------------------------------------------ the stale log

#[test]
fn a_nine_hour_old_log_sample_is_shown_dimmed_with_the_chip_and_the_stale_tooltip() {
    let h = home();
    write_log(
        &h,
        &[billing_line(
            now() - Duration::hours(9),
            8.0,
            now() + Duration::days(3),
        )],
    );
    let (readings, err) = stored_readings(&h, now());
    // the poll itself is "no current data" (the log is stale) ...
    assert!(err.unwrap().contains("Grok CLI log is stale"));
    // ... but the sample was kept, and the window shows it
    let st = state_with(&readings, now());
    let vs = vendor::from_state_at(&st, now());
    let g = vs.iter().find(|v| v.id == VendorId::Grok).unwrap();
    assert!(g.current, "the last known value must stay visible");
    let home = view::build_home_for(&st, &vs, now());
    let gv = home
        .vendors
        .iter()
        .find(|v| v.id == VendorId::Grok)
        .unwrap();
    let row = &gv.rows[0];
    assert!(!row.stale, "{row:?}");
    assert_eq!(row.value, "8%");
    assert!(row.dim, "an old reading is drawn dimmed");
    let chip = gv.age_chip.as_ref().expect("data-age chip");
    assert_eq!(chip.compact, "9 h");
    let last = &row.tip.last().unwrap().text;
    assert!(
        last.starts_with("Data is about 9 h old")
            && last.contains("only updates when you use the CLI"),
        "{last}"
    );
}

#[test]
fn a_fresh_reading_is_not_dimmed_and_an_ended_period_is_never_shown() {
    let h = home();
    write_log(
        &h,
        &[billing_line(
            now() - Duration::minutes(5),
            8.0,
            now() + Duration::days(3),
        )],
    );
    let (readings, err) = stored_readings(&h, now());
    assert!(err.is_none());
    let st = state_with(&readings, now());
    let vs = vendor::from_state_at(&st, now());
    let home = view::build_home_for(&st, &vs, now());
    assert!(!home.vendors[0].rows[0].dim);

    // nine hours old AND its billing period ended since: last week's number, dashes
    let h2 = self::home();
    write_log(
        &h2,
        &[billing_line(
            now() - Duration::hours(9),
            8.0,
            now() - Duration::hours(1),
        )],
    );
    let (readings, _) = stored_readings(&h2, now());
    let st = state_with(&readings, now());
    let vs = vendor::from_state_at(&st, now());
    let g = vs.iter().find(|v| v.id == VendorId::Grok).unwrap();
    assert!(!g.current);
    let home = view::build_home_for(&st, &vs, now());
    assert!(home.vendors[0].rows[0].stale);
    assert_eq!(home.vendors[0].rows[0].value, "--");
}

#[test]
fn grok_diagnostics_say_which_source_supplied_the_reading_and_why_billing_failed() {
    let h = home();
    // an expired login and an old log: both reasons end up in the failed row
    std::fs::write(
        h.dir.join("grokhome/auth.json"),
        json!({"https://auth.x.ai::c": {"key": "xai-SECRET-TOKEN", "user_id": "uid-SECRET",
            "expires_at": ts(Utc::now() - Duration::hours(2))}})
        .to_string(),
    )
    .unwrap();
    let lines = [billing_line(
        Utc::now() - Duration::hours(9),
        8.0,
        Utc::now() + Duration::days(3),
    )];
    write_log(&h, &lines);
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let poller = Poller::with_chain_for(
        vec![
            Box::new(grok::GrokBilling::new(h.env.clone())),
            Box::new(GrokLog { env: h.env.clone() }),
        ],
        "grok",
    );
    let e = poller.poll_once(&store).unwrap_err().message;
    assert!(e.contains("Grok login expired"), "{e}");
    assert!(e.contains("log is stale"), "{e}");
    let readings: Vec<Reading> = store.read().iter().map(Reading::from_value).collect();
    let gs = build_grok_state(&readings, status(), true, Utc::now());
    let texts: Vec<&str> = gs.diagnostics.iter().map(|d| d.text.as_str()).collect();
    assert!(
        texts.iter().any(|t| t.contains("Grok login expired")),
        "{texts:?}"
    );
    assert!(texts.iter().all(|t| !t.contains("SECRET")), "{texts:?}");

    // and a good reading names its source
    write_log(
        &h,
        &[billing_line(
            Utc::now() - Duration::minutes(2),
            9.0,
            Utc::now() + Duration::days(3),
        )],
    );
    let log_only = Poller::with_chain_for(vec![Box::new(GrokLog { env: h.env.clone() })], "grok");
    log_only.poll_once(&store).unwrap();
    let readings: Vec<Reading> = store.read().iter().map(Reading::from_value).collect();
    let gs = build_grok_state(&readings, status(), true, Utc::now());
    assert_eq!(gs.diagnostics[0].text, "Grok: weekly 9% \u{b7} grok-log");
}

// ------------------------------------------------------------ finding the CLI

fn touch(p: &Path) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "").unwrap();
}

#[test]
fn the_cli_is_found_on_path_then_in_the_usual_folders_and_not_otherwise() {
    let t = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(t.join("home"));
    env.grok_home = Some(t.join("gh"));
    assert_eq!(grok_cli::find_cli(&env, None, None), None);

    // a usual folder
    touch(&t.join("home/.local/bin/grok"));
    assert_eq!(
        grok_cli::find_cli(&env, None, None),
        Some(t.join("home/.local/bin/grok"))
    );
    // PATH wins over the usual folders
    touch(&t.join("bin/grok"));
    let path = std::env::join_paths([t.join("empty"), t.join("bin")]).unwrap();
    assert_eq!(
        grok_cli::find_cli(&env, Some(&path), None),
        Some(t.join("bin/grok"))
    );
    // an override: a path must exist, a bare name is looked up on PATH
    assert_eq!(
        grok_cli::find_cli(&env, Some(&path), Some(OsStr::new("/nope/grok"))),
        None
    );
    touch(&t.join("bin/grok-dev"));
    assert_eq!(
        grok_cli::find_cli(&env, Some(&path), Some(OsStr::new("grok-dev"))),
        Some(t.join("bin/grok-dev"))
    );
    // Windows names
    let mut w = Env::new(Os::Windows);
    w.home = Some(t.join("whome"));
    w.appdata = Some(t.join("appdata"));
    w.localappdata = Some(t.join("local"));
    touch(&t.join("appdata/npm/grok.cmd"));
    assert_eq!(
        grok_cli::find_cli(&w, None, None),
        Some(t.join("appdata/npm/grok.cmd"))
    );
    touch(&t.join("local/Programs/grok/grok.exe"));
    assert_eq!(
        grok_cli::find_cli(&w, None, None),
        Some(t.join("local/Programs/grok/grok.exe"))
    );
}

#[test]
fn the_refresh_is_offered_only_when_grok_data_is_missing_or_older_than_an_hour() {
    assert!(grok_cli::needs_refresh(None, now()));
    assert!(!grok_cli::needs_refresh(
        Some(now() - Duration::minutes(59)),
        now()
    ));
    assert!(!grok_cli::needs_refresh(
        Some(now() - Duration::minutes(60)),
        now()
    ));
    assert!(grok_cli::needs_refresh(
        Some(now() - Duration::minutes(61)),
        now()
    ));
}

#[test]
fn the_refresh_command_has_no_arguments_by_default_and_null_stdio() {
    assert!(grok_cli::refresh_args(None).is_empty());
    assert_eq!(
        grok_cli::refresh_args(Some("--a  b")),
        vec![OsString::from("--a"), OsString::from("b")]
    );
    let c = grok_cli::command(Path::new("/x/grok"), &[]);
    assert_eq!(c.get_program(), "/x/grok");
    assert_eq!(c.get_args().count(), 0);
    assert_eq!(grok_cli::CREATE_NO_WINDOW, 0x0800_0000);
    assert!(grok_cli::listing_has_grok(
        "\"grok.exe\",\"123\",\"Console\",\"1\",\"5 K\"\r\n"
    ));
    assert!(!grok_cli::listing_has_grok(
        "INFO: No tasks are running which match the specified criteria.\r\n"
    ));
    assert!(!grok_cli::listing_has_grok("\"grokbot.exe\",\"9\"\r\n"));
}

// ------------------------------------------------------------ the refresh run

struct FakeChild {
    killed: Arc<AtomicUsize>,
    exit_after: Option<usize>,
    polls: usize,
    write_line_after: Option<(usize, PathBuf)>,
}

impl Child for FakeChild {
    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        self.polls += 1;
        if let Some((n, log)) = &self.write_line_after {
            if self.polls == *n {
                let line = billing_line(Utc::now(), 8.0, Utc::now() + Duration::days(3));
                std::fs::write(log, line + "\n").unwrap();
            }
        }
        Ok(self.exit_after.filter(|n| self.polls >= *n).map(|_| 3))
    }
    fn kill(&mut self) {
        self.killed.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeHost {
    running: bool,
    spawned: AtomicUsize,
    killed: Arc<AtomicUsize>,
    exit_after: Option<usize>,
    write_after: Option<usize>,
    log: PathBuf,
    spawn_error: Option<String>,
    args_seen: Mutex<Vec<(PathBuf, Vec<OsString>)>>,
}

impl FakeHost {
    fn new(log: PathBuf) -> Self {
        FakeHost {
            running: false,
            spawned: AtomicUsize::new(0),
            killed: Arc::new(AtomicUsize::new(0)),
            exit_after: None,
            write_after: None,
            log,
            spawn_error: None,
            args_seen: Mutex::new(vec![]),
        }
    }
}

impl Host for FakeHost {
    fn grok_running(&self) -> bool {
        self.running
    }
    fn spawn(&self, program: &Path, args: &[OsString]) -> Result<Box<dyn Child>, String> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        self.args_seen
            .lock()
            .unwrap()
            .push((program.to_path_buf(), args.to_vec()));
        if let Some(e) = &self.spawn_error {
            return Err(e.clone());
        }
        Ok(Box::new(FakeChild {
            killed: self.killed.clone(),
            exit_after: self.exit_after,
            polls: 0,
            write_line_after: self.write_after.map(|n| (n, self.log.clone())),
        }))
    }
}

fn cfg(h: &Home, timeout_ms: u64) -> RefreshCfg {
    RefreshCfg {
        program: PathBuf::from("/fake/grok"),
        args: vec![],
        log: grok::log_path(&h.env),
        timeout: StdDuration::from_millis(timeout_ms),
        grace: StdDuration::ZERO,
        poll_every: StdDuration::from_millis(5),
    }
}

#[test]
fn a_new_billing_line_ends_the_wait_and_the_process_is_killed() {
    let h = home();
    // an older line already there: only a NEWER one counts
    write_log(
        &h,
        &[billing_line(
            Utc::now() - Duration::hours(9),
            7.0,
            Utc::now() + Duration::days(3),
        )],
    );
    let mut host = FakeHost::new(grok::log_path(&h.env));
    host.write_after = Some(3);
    let r = grok_cli::refresh(&cfg(&h, 5000), &host);
    assert_eq!(r, Ok(grok_cli::Outcome::Refreshed));
    assert_eq!(host.killed.load(Ordering::SeqCst), 1);
    let seen = host.args_seen.lock().unwrap();
    assert_eq!(seen[0].0, PathBuf::from("/fake/grok"));
    assert!(seen[0].1.is_empty(), "no prompt, no arguments");
}

#[test]
fn nothing_is_started_while_grok_is_already_running() {
    let h = home();
    let mut host = FakeHost::new(grok::log_path(&h.env));
    host.running = true;
    let e = grok_cli::refresh(&cfg(&h, 100), &host).unwrap_err();
    assert!(e.contains("already running"), "{e}");
    assert_eq!(host.spawned.load(Ordering::SeqCst), 0);
}

#[test]
fn a_timeout_kills_the_process_and_says_why() {
    let h = home();
    let host = FakeHost::new(grok::log_path(&h.env));
    let t0 = Instant::now();
    let e = grok_cli::refresh(&cfg(&h, 60), &host).unwrap_err();
    assert_eq!(e, grok_cli::NO_NEW_DATA);
    assert!(t0.elapsed() < StdDuration::from_secs(5));
    assert_eq!(host.killed.load(Ordering::SeqCst), 1);
}

#[test]
fn an_early_exit_without_a_line_and_a_spawn_error_are_reported() {
    let h = home();
    let mut host = FakeHost::new(grok::log_path(&h.env));
    host.exit_after = Some(2);
    let e = grok_cli::refresh(&cfg(&h, 5000), &host).unwrap_err();
    assert!(
        e.contains("exit code 3") && e.contains("no new data (Grok did not fetch billing"),
        "{e}"
    );

    let mut host = FakeHost::new(grok::log_path(&h.env));
    host.spawn_error = Some("boom".into());
    let e = grok_cli::refresh(&cfg(&h, 5000), &host).unwrap_err();
    assert_eq!(e, "Could not start Grok: boom");
}

/// The real path with a fake executable: it writes a billing line, then would run for a minute.
#[cfg(unix)]
#[test]
fn the_real_host_starts_a_fake_cli_and_terminates_it_after_the_line_appears() {
    use std::os::unix::fs::PermissionsExt;
    let h = home();
    let pidfile = h.dir.join("pid");
    let script = h.dir.join("fake-grok");
    let log = grok::log_path(&h.env);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nprintf '%s\\n' '{}' >> '{}'\nexec sleep 60\n",
            pidfile.display(),
            billing_line(Utc::now(), 8.0, Utc::now() + Duration::days(3)),
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let c = RefreshCfg {
        program: script,
        args: vec![],
        log,
        timeout: StdDuration::from_secs(10),
        grace: StdDuration::ZERO,
        poll_every: StdDuration::from_millis(20),
    };
    let t0 = Instant::now();
    let r = grok_cli::refresh(&c, &grok_cli::SystemHost);
    assert_eq!(r, Ok(grok_cli::Outcome::Refreshed));
    assert!(
        t0.elapsed() < StdDuration::from_secs(9),
        "{:?}",
        t0.elapsed()
    );
    let pid = std::fs::read_to_string(pidfile).unwrap().trim().to_string();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the fake CLI must have been terminated"
    );
}

#[cfg(unix)]
#[test]
fn the_real_host_kills_a_fake_cli_that_never_writes_anything() {
    use std::os::unix::fs::PermissionsExt;
    let h = home();
    let script = h.dir.join("fake-grok");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let c = RefreshCfg {
        program: script,
        args: vec![],
        log: grok::log_path(&h.env),
        timeout: StdDuration::from_millis(300),
        grace: StdDuration::ZERO,
        poll_every: StdDuration::from_millis(20),
    };
    let t0 = Instant::now();
    let e = grok_cli::refresh(&c, &grok_cli::SystemHost).unwrap_err();
    assert_eq!(e, grok_cli::NO_NEW_DATA);
    assert!(t0.elapsed() < StdDuration::from_secs(5));
}

// ------------------------------------------------------------ the app: gating and progress

fn app_with_grok(t: &TempDir) -> Arc<App> {
    let settings = Arc::new(Settings::load(t.join("s.json")));
    let app = App::with_poll_fn(
        Box::new(JsonlStore::new(t.join("u.jsonl"))),
        settings,
        Box::new(|_| Ok(Value::Null)),
    )
    .with_grok(
        Box::new(JsonlStore::new(t.join("g.jsonl"))),
        Box::new(|_| Ok(Value::Null)),
        None,
        Arc::new(|| true),
    );
    Arc::new(app)
}

fn grok_reading(age: Duration) -> Reading {
    Reading::from_value(&json!({
        "ok": true, "source": "grok-log", "vendor": "grok", "ts": (now() - age).to_rfc3339(),
        "weekly": {"pct": 8.0, "resets_at": (now() + Duration::days(3)).to_rfc3339()},
    }))
}

#[test]
fn the_refresh_control_is_gated_on_data_age_and_on_the_cli_being_installed() {
    let t = TempDir::new();
    let app = app_with_grok(&t);
    let present = Arc::new(Mutex::new(true));
    let p2 = present.clone();
    app.set_grok_refresh(Arc::new(|| Ok(None)), Arc::new(move || *p2.lock().unwrap()));
    // absent data: offered
    assert_eq!(
        app.grok_refresh_mode(&state_with(&[], now()), now()),
        RefreshMode::Offer
    );
    // 9 h old: offered; 10 min old: hidden
    let old = state_with(&[grok_reading(Duration::hours(9))], now());
    assert_eq!(app.grok_refresh_mode(&old, now()), RefreshMode::Offer);
    let fresh = state_with(&[grok_reading(Duration::minutes(10))], now());
    assert_eq!(app.grok_refresh_mode(&fresh, now()), RefreshMode::Hidden);
    // CLI not installed: hidden even when the data is old (the answer is cached, so reset it)
    *present.lock().unwrap() = false;
    app.set_grok_refresh(
        Arc::new(|| Ok(None)),
        Arc::new({
            let p = present.clone();
            move || *p.lock().unwrap()
        }),
    );
    assert_eq!(app.grok_refresh_mode(&old, now()), RefreshMode::Hidden);
    assert!(!app.grok_cli_present());
    // Grok switched off: hidden
    let mut off = state_with(&[], now());
    off.grok.as_mut().unwrap().enabled = false;
    *present.lock().unwrap() = true;
    app.set_grok_refresh(Arc::new(|| Ok(None)), Arc::new(|| true));
    assert_eq!(app.grok_refresh_mode(&off, now()), RefreshMode::Hidden);
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < StdDuration::from_secs(30), "timed out");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}

#[test]
fn a_refresh_shows_progress_runs_once_at_a_time_and_reports_a_failure() {
    let t = TempDir::new();
    let app = app_with_grok(&t);
    let runs = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicUsize::new(0));
    let (r2, rel2) = (runs.clone(), release.clone());
    app.set_grok_refresh(
        Arc::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
            while rel2.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(StdDuration::from_millis(5));
            }
            if rel2.load(Ordering::SeqCst) == 2 {
                Err(grok_cli::NO_NEW_DATA.into())
            } else {
                Ok(None)
            }
        }),
        Arc::new(|| true),
    );
    let st = state_with(&[], now());
    let woke = Arc::new(AtomicUsize::new(0));
    let w = woke.clone();
    app.refresh_grok(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    wait_for(|| runs.load(Ordering::SeqCst) == 1);
    assert_eq!(app.grok_refresh_mode(&st, now()), RefreshMode::Running);
    assert!(app.grok_refreshing());
    // a second click while it runs does nothing
    app.refresh_grok(|| {});
    std::thread::sleep(StdDuration::from_millis(50));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    release.store(2, Ordering::SeqCst);
    wait_for(|| woke.load(Ordering::SeqCst) == 1);
    match app.grok_refresh_mode(&st, now()) {
        RefreshMode::Failed(m) => assert_eq!(m, grok_cli::NO_NEW_DATA),
        m => panic!("{m:?}"),
    }
    assert!(!app.grok_refreshing());
}

// ------------------------------------------------------------ launching Grok

#[derive(Default)]
struct FakeSpawner {
    start_apps: Option<String>,
    fail_runs: bool,
    runs: Mutex<Vec<(String, Vec<String>)>>,
}

impl Spawner for FakeSpawner {
    fn open_uri(&self, _uri: &str) -> Result<(), String> {
        Err("unused".into())
    }
    fn run(&self, program: &Path, args: &[String]) -> Result<(), String> {
        self.runs
            .lock()
            .unwrap()
            .push((program.to_string_lossy().into_owned(), args.to_vec()));
        if self.fail_runs {
            Err("nope".into())
        } else {
            Ok(())
        }
    }
    fn capture(&self, _p: &Path, _a: &[String]) -> Result<String, String> {
        self.start_apps
            .clone()
            .ok_or_else(|| "no powershell".into())
    }
}

const START_APPS: &str = "Grok\tgrok.com-653F74E8_0cm83s0t91vjm!App\nGrok Bot\tcom.anysphere.sand\nGrok Bot\telectron.app.Grok Bot\nGrok Notes\tsome.other!App\nGroking\tx.y\nNotepad\tMicrosoft.Notepad\n";

fn launch_env(cli: bool) -> GrokLaunchEnv {
    GrokLaunchEnv {
        localappdata: None,
        home: Some(PathBuf::from("C:/Users/me")),
        cli: cli.then(|| PathBuf::from("C:/tools/grok.exe")),
        wt: None,
    }
}

fn targets(tried: &[Attempt]) -> Vec<String> {
    tried.iter().map(|a| a.method.clone()).collect()
}

#[test]
fn start_menu_lines_are_parsed_and_only_exact_names_match() {
    let apps = parse_start_apps(START_APPS);
    assert_eq!(apps.len(), 6);
    assert_eq!(
        grok_bot_targets(&apps),
        vec![
            "shell:AppsFolder\\electron.app.Grok Bot".to_string(),
            "shell:AppsFolder\\com.anysphere.sand".to_string()
        ]
    );
    assert_eq!(
        grok_store_targets(None, &apps),
        vec!["shell:AppsFolder\\grok.com-653F74E8_0cm83s0t91vjm!App".to_string()]
    );
    // unrelated names never match
    let other = parse_start_apps("Grok Notes\ta!b\nGroking\tc!d\nMy Grok Bot\te!f\n");
    assert!(grok_bot_targets(&other).is_empty());
    assert!(grok_store_targets(None, &other).is_empty());
}

#[test]
fn the_store_package_folder_gives_the_aumid_without_the_start_list() {
    let t = TempDir::new();
    std::fs::create_dir_all(t.join("Packages/grok.com-ABC123_0xyz")).unwrap();
    std::fs::create_dir_all(t.join("Packages/Other_1")).unwrap();
    assert_eq!(
        grok_store_targets(Some(t.path()), &[]),
        vec!["shell:AppsFolder\\grok.com-ABC123_0xyz!App".to_string()]
    );
}

#[test]
fn grok_bot_is_tried_first_then_the_store_app_then_the_cli() {
    // both apps: Grok Bot (electron.app entry first) wins
    let sp = FakeSpawner {
        start_apps: Some(START_APPS.into()),
        ..Default::default()
    };
    let tried = launch_grok(&launch_env(true), &sp);
    assert!(launch::succeeded(&tried));
    assert_eq!(tried.len(), 1);
    assert!(tried[0].method.contains("Grok Bot app") && tried[0].method.contains("electron.app"));
    let runs = sp.runs.lock().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].0, "explorer.exe");

    // only the Store app: it is used
    let sp = FakeSpawner {
        start_apps: Some("Grok\tgrok.com-X_0y!App\n".into()),
        ..Default::default()
    };
    let tried = launch_grok(&launch_env(true), &sp);
    assert!(launch::succeeded(&tried) && tried[0].method.starts_with("Grok app"));

    // Grok Bot fails to start: the Store app is the next try
    let sp = FakeSpawner {
        start_apps: Some(START_APPS.into()),
        fail_runs: true,
        ..Default::default()
    };
    let tried = launch_grok(&launch_env(false), &sp);
    let m = targets(&tried);
    assert!(m[0].starts_with("Grok Bot app") && m[1].starts_with("Grok Bot app"));
    assert!(m[2].starts_with("Grok app"));
    assert!(!launch::succeeded(&tried));

    // neither app: the CLI in a terminal
    let sp = FakeSpawner {
        start_apps: Some("Notepad\tn!App\n".into()),
        ..Default::default()
    };
    let tried = launch_grok(&launch_env(true), &sp);
    assert!(launch::succeeded(&tried));
    assert!(tried[0].method.starts_with("grok CLI in a terminal"));
    let runs = sp.runs.lock().unwrap();
    assert_eq!(runs[0].0, "cmd.exe");

    // nothing at all: one failed attempt that says so (the window shows "Could not launch Grok")
    let sp = FakeSpawner::default();
    let tried = launch_grok(&launch_env(false), &sp);
    assert!(!launch::succeeded(&tried));
    assert!(tried[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("no Grok app or grok CLI"));
}

#[test]
fn the_terminal_command_runs_grok_alone_in_the_home_folder() {
    let cli = Path::new("C:/tools/grok.exe");
    let dir = Path::new("C:/Users/me");
    let (prog, args) = terminal_command(cli, dir, Some(Path::new("C:/wt.exe")));
    assert_eq!(prog, PathBuf::from("C:/wt.exe"));
    assert_eq!(args, vec!["-d", "C:/Users/me", "C:/tools/grok.exe"]);
    let (prog, args) = terminal_command(cli, dir, None);
    assert_eq!(prog, PathBuf::from("cmd.exe"));
    assert_eq!(
        args,
        vec![
            "/c",
            "start",
            "Grok",
            "/D",
            "C:/Users/me",
            "C:/tools/grok.exe"
        ]
    );
    // never a prompt: nothing after the program
    assert_eq!(args.last().unwrap(), "C:/tools/grok.exe");
}

#[test]
fn kinds_are_unchanged_by_the_grok_state() {
    // the weekly-only block has exactly one row
    let t = state_with(&[grok_reading(Duration::hours(9))], now());
    let vs = vendor::from_state_at(&t, now());
    assert_eq!(vs[0].kinds(), &[Kind::Weekly]);
}

// ------------------------------------------------------------ no projection from an old reading

#[test]
fn an_old_reading_keeps_its_value_but_shows_no_empty_in_warning_or_pace() {
    let demo = |age_min: i64| {
        let mut v = vendor::demo_vendors(now())
            .into_iter()
            .find(|v| v.id == VendorId::Grok)
            .unwrap();
        v.current = true;
        v.source = Some("grok-log".into());
        v.observed_at = Some(now() - Duration::minutes(age_min));
        v.weekly.as_mut().unwrap().rate_method = Some(aitrack::analysis::PaceMethod::Recent);
        v
    };
    let build = |v: &vendor::Vendor| {
        let st = state_with(&[], now());
        view::build_home_for(&st, std::slice::from_ref(v), now())
    };
    // 30 minutes old: the projection is still shown
    let fresh = demo(30);
    let home = build(&fresh);
    let gv = &home.vendors[0];
    assert!(gv
        .warn
        .as_ref()
        .is_some_and(|w| w.full.contains("Empty in")));
    assert!(gv.rows[0].tip.iter().any(|l| l.text.starts_with("Pace:")));
    let d = view::build_detail_at(&fresh, Kind::Weekly, false, now());
    assert!(d.rows.iter().any(|r| r.0 == "Pace"));

    // 9 hours old: plain value, no "Empty in", no pace, no projected bar cells, no detail pace
    let old = demo(540);
    let home = build(&old);
    let gv = &home.vendors[0];
    assert_eq!(gv.warn, None);
    let row = &gv.rows[0];
    assert_eq!(row.value, "94%");
    assert!(row.dim);
    assert!(row
        .tip
        .iter()
        .all(|l| !l.text.starts_with("Pace:") && !l.text.contains("Empty in")));
    let lit = |r: &view::Row| r.cells.iter().filter(|c| **c != view::Cell::Off).count();
    let mut plain = fresh.clone();
    plain.weekly.as_mut().unwrap().projected_pct = None;
    assert_eq!(lit(row), lit(&build(&plain).vendors[0].rows[0]));
    let d = view::build_detail_at(&old, Kind::Weekly, false, now());
    assert_eq!(d.value, "94");
    assert!(d.note.is_none());
    assert!(d.rows.iter().all(|r| r.0 != "Pace" && r.0 != "At reset"));
}

// ------------------------------------------------------------ Grok rows in the Diagnostics list

#[test]
fn grok_polls_show_up_in_the_diagnostics_list_next_to_claudes() {
    use aitrack::state::merged_diagnostics;
    let h = home();
    // two live-ish readings and one failure, as the Grok store would hold them
    let store = JsonlStore::new(h.dir.join("usage-grok.jsonl"));
    let rec = |m: i64, pct: f64| {
        json!({"ok": true, "vendor": "grok", "source": "grok-billing", "ts": (now() - Duration::minutes(m)).to_rfc3339(),
            "weekly": {"pct": pct, "resets_at": (now() + Duration::days(3)).to_rfc3339()}})
    };
    store.append(&rec(20, 8.0)).unwrap();
    store
        .append(&json!({"ok": false, "vendor": "grok", "ts": (now() - Duration::minutes(10)).to_rfc3339(), "error": "Grok billing HTTP 500"}))
        .unwrap();
    store.append(&rec(2, 9.0)).unwrap();
    let readings: Vec<Reading> = store.read().iter().map(Reading::from_value).collect();
    let gs = build_grok_state(&readings, status(), true, now());
    let texts: Vec<&str> = gs.diagnostics.iter().map(|d| d.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "Grok: weekly 9% \u{b7} grok-billing",
            "Grok: Grok billing HTTP 500",
            "Grok: weekly 8% \u{b7} grok-billing"
        ]
    );
    // merged with Claude's rows by time, newest first
    let claude = |m: i64| aitrack::state::Diag {
        ts: (now() - Duration::minutes(m)).to_rfc3339(),
        ok: true,
        text: format!("claude {m}"),
    };
    let merged = merged_diagnostics(&[claude(5), claude(30)], &gs.diagnostics);
    let order: Vec<&str> = merged.iter().map(|d| d.text.as_str()).collect();
    assert_eq!(
        order,
        vec![
            "Grok: weekly 9% \u{b7} grok-billing",
            "claude 5",
            "Grok: Grok billing HTTP 500",
            "Grok: weekly 8% \u{b7} grok-billing",
            "claude 30"
        ]
    );
}

#[test]
fn the_cli_lookup_also_covers_versioned_and_per_user_install_folders() {
    let t = TempDir::new();
    let mut w = Env::new(Os::Windows);
    w.home = Some(t.join("u"));
    w.localappdata = Some(t.join("local"));
    for rel in [
        "u/.grok/bin/grok.exe",
        "u/.grok/versions/1.2.3/grok.exe",
        "local/grok/grok.exe",
        "local/Programs/Grok/grok.exe",
        "u/scoop/shims/grok.exe",
    ] {
        // each location alone is found
        let t2 = TempDir::new();
        let mut e = w.clone();
        e.home = Some(t2.join("u"));
        e.localappdata = Some(t2.join("local"));
        e.grok_home = None;
        touch(&t2.join(rel));
        // (on Windows the file system ignores case and mixes `/` and `\\`: `Programs\\grok` and
        // `Programs/Grok` are the same file, and the lookup tries both spellings; Unix stays strict)
        let found = grok_cli::find_cli(&e, None, None).map(|p| norm_path(&p));
        assert_eq!(found, Some(norm_path(&t2.join(rel))), "{rel}");
    }
}

/// A path as text for comparing two spellings of the same file: exact on Linux, case-insensitive
/// on Windows (with one kind of separator) and on macOS (APFS ignores case by default).
fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('\\', "/").to_lowercase()
    } else if cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s
    }
}

// ------------------------------------------------------------ the outcome of a refresh

#[test]
fn the_refresh_status_text_uses_the_given_zone_and_the_value_read() {
    use aitrack::app::RefreshStatus as S;
    let at = Utc.with_ymd_and_hms(2026, 10, 14, 17, 42, 0).unwrap();
    // 2 h east of UTC
    assert_eq!(
        S::Done { at, pct: Some(8.0) }.text(7200).unwrap(),
        "Refreshed 19:42: weekly 8%"
    );
    assert_eq!(
        S::Done { at, pct: None }.text(0).unwrap(),
        "Refreshed 17:42"
    );
    assert_eq!(S::Running.text(0).unwrap(), "Refreshing\u{2026}");
    assert_eq!(
        S::Failed {
            at,
            reason: grok_cli::NO_NEW_DATA.into()
        }
        .text(0)
        .unwrap(),
        "no new data (Grok did not fetch billing)"
    );
    assert_eq!(S::Never.text(0), None);
}

#[test]
fn every_refresh_attempt_is_a_diagnostics_row_and_the_outcome_stays() {
    use aitrack::app::RefreshStatus;
    let t = TempDir::new();
    let app = app_with_grok(&t);
    let results = Arc::new(Mutex::new(vec![
        Err(grok_cli::NO_NEW_DATA.to_string()),
        Ok(Some(8.0)),
    ]));
    let r2 = results.clone();
    app.set_grok_refresh(
        Arc::new(move || r2.lock().unwrap().pop().unwrap()),
        Arc::new(|| true),
    );
    app.start();
    wait_for(|| !app.state().loading);
    assert_eq!(app.grok_refresh_status(), RefreshStatus::Never);
    let run = |app: &Arc<App>| {
        let woke = Arc::new(AtomicUsize::new(0));
        let w = woke.clone();
        app.refresh_grok(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        wait_for(|| woke.load(Ordering::SeqCst) == 1);
    };
    // success: the value read is in the status and in the log
    run(&app);
    assert!(matches!(
        app.grok_refresh_status(),
        RefreshStatus::Done { pct: Some(p), .. } if p == 8.0
    ));
    // failure: the distinct "no new data" reason, kept (not time limited) in the status
    run(&app);
    match app.grok_refresh_status() {
        RefreshStatus::Failed { reason, .. } => assert_eq!(reason, grok_cli::NO_NEW_DATA),
        s => panic!("{s:?}"),
    }
    wait_for(|| {
        app.state()
            .diagnostics
            .iter()
            .filter(|d| d.text.starts_with("Grok refresh:"))
            .count()
            == 2
    });
    let st = app.state();
    let rows: Vec<(&str, bool)> = st
        .diagnostics
        .iter()
        .filter(|d| d.text.starts_with("Grok refresh:"))
        .map(|d| (d.text.as_str(), d.ok))
        .collect();
    // newest first
    assert_eq!(
        rows,
        vec![
            (
                "Grok refresh: failed, no new data (Grok did not fetch billing)",
                false
            ),
            ("Grok refresh: ok, weekly 8% \u{b7} grok-log", true),
        ]
    );
    // and in Copy log
    let log = view::copy_log(&st.diagnostics);
    assert!(
        log.contains("ok Grok refresh: ok, weekly 8% \u{b7} grok-log"),
        "{log}"
    );
    assert!(
        log.contains("FAIL Grok refresh: failed, no new data"),
        "{log}"
    );
    app.stop();
    app.collector.join();
}

#[test]
fn refresh_rows_merge_into_the_diagnostics_list_by_time() {
    use aitrack::state::{merged_diagnostics, Diag};
    let d = |m: i64, text: &str| Diag {
        ts: (now() - Duration::minutes(m)).to_rfc3339(),
        ok: true,
        text: text.into(),
    };
    let merged = merged_diagnostics(
        &[d(1, "claude"), d(9, "claude old")],
        &[d(5, "Grok refresh: ok, weekly 8% \u{b7} grok-log")],
    );
    let texts: Vec<&str> = merged.iter().map(|x| x.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "claude",
            "Grok refresh: ok, weekly 8% \u{b7} grok-log",
            "claude old"
        ]
    );
}

#[test]
fn a_store_app_execution_alias_in_windowsapps_is_never_taken_for_the_cli() {
    // The Grok Store app can register an app-execution alias named grok.exe in
    // %LOCALAPPDATA%\Microsoft\WindowsApps (also on PATH): running it opens the GUI app, which
    // is not the CLI and would stay open as a hidden "refresh".
    let t = TempDir::new();
    let mut w = Env::new(Os::Windows);
    w.home = Some(t.join("whome"));
    w.localappdata = Some(t.join("local"));
    let alias = t.join("local/Microsoft/WindowsApps/grok.exe");
    touch(&alias);
    assert_eq!(grok_cli::find_cli(&w, None, None), None, "usual folders");
    let path = std::env::join_paths([t.join("local/Microsoft/WindowsApps")]).unwrap();
    assert_eq!(grok_cli::find_cli(&w, Some(&path), None), None, "PATH");
    // case does not matter on Windows, and a real CLI later on PATH still wins
    let path =
        std::env::join_paths([t.join("local/microsoft/windowsapps"), t.join("tools")]).unwrap();
    touch(&t.join("tools/grok.exe"));
    assert_eq!(
        grok_cli::find_cli(&w, Some(&path), None),
        Some(t.join("tools/grok.exe"))
    );
    // an explicit override is the user's decision
    assert_eq!(
        grok_cli::find_cli(&w, None, Some(alias.as_os_str())),
        Some(alias)
    );
}

#[test]
fn path_for_the_refresh_child_gains_the_user_locations_once() {
    // built with the platform's own separator and path joining, so it holds on Windows too
    let home = PathBuf::from("home").join("u");
    let existing =
        std::env::join_paths([PathBuf::from("usr/bin"), PathBuf::from("/usr/local/bin")]).unwrap();
    let p = grok_cli::extended_path(Some(existing.as_os_str()), Some(&home));
    let dirs: Vec<PathBuf> = std::env::split_paths(&p).collect();
    assert_eq!(dirs[0], PathBuf::from("usr/bin"));
    for want in [
        home.join(".grok").join("bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local").join("bin"),
    ] {
        assert_eq!(
            dirs.iter().filter(|d| **d == want).count(),
            1,
            "{}",
            want.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn ending_a_refresh_ends_the_wrappers_children_too() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempDir::new();
    let pidfile = t.join("child.pid");
    let script = t.join("grok");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\nwait\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = grok_cli::SystemHost.spawn(&script, &[]).unwrap();
    let start = Instant::now();
    while !pidfile.exists() && start.elapsed() < StdDuration::from_secs(5) {
        std::thread::sleep(StdDuration::from_millis(50));
    }
    std::thread::sleep(StdDuration::from_millis(100));
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    child.kill();
    std::thread::sleep(StdDuration::from_millis(300));
    // /proc/<pid> may linger as a zombie of init; its state must not be a live one
    let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.contains(") Z"))
        .unwrap_or(false);
    assert!(!alive, "the wrapper's child survived the kill");
}

#[cfg(unix)]
#[test]
fn a_child_that_already_ended_is_not_signalled() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempDir::new();
    let pidfile = t.join("bystander.pid");
    let script = t.join("grok");
    // the leader ends at once and leaves a process of its group behind
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = grok_cli::SystemHost.spawn(&script, &[]).unwrap();
    let start = Instant::now();
    while !matches!(child.try_wait(), Ok(Some(_))) && start.elapsed() < StdDuration::from_secs(5) {
        std::thread::sleep(StdDuration::from_millis(20));
    }
    assert!(matches!(child.try_wait(), Ok(Some(_))), "the script ended");
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    child.kill();
    std::thread::sleep(StdDuration::from_millis(200));
    let survived = alive(&pid);
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid])
        .status();
    assert!(survived, "no signal was sent for a pid that had ended");
}

#[test]
fn the_process_runs_for_the_grace_period_after_the_line_appears() {
    let h = home();
    let mut host = FakeHost::new(grok::log_path(&h.env));
    host.write_after = Some(1);
    let mut c = cfg(&h, 5000);
    c.grace = StdDuration::from_millis(300);
    let t0 = Instant::now();
    assert_eq!(
        grok_cli::refresh(&c, &host),
        Ok(grok_cli::Outcome::Refreshed)
    );
    assert!(
        t0.elapsed() >= StdDuration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(host.killed.load(Ordering::SeqCst), 1);
    assert_eq!(grok_cli::REFRESH_GRACE, StdDuration::from_secs(5));
}
