// SPDX-License-Identifier: GPL-3.0-or-later
//! Grok through the real App: the Grok chain over fixtures (and a mock billing server), the real
//! collector threads, the separate history file, the view model, alerts and the settings toggle.
use crate::common;
use aitrack::alerts::{Notification, Notifier};
use aitrack::app::App;
use aitrack::grok::{self, GrokBilling, GrokLog};
use aitrack::live::Poller;
use aitrack::reading::Kind;
use aitrack::settings::Settings;
use aitrack::sources::{DesktopHistory, Env, Os};
use aitrack::store::{JsonlStore, Store};
use aitrack::vendor::{self, VendorId};
use aitrack::view::{self, Cell};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

const TOKEN: &str = "xai-APP-TOKEN-SECRET";
const USER: &str = "user-APP-SECRET-1";

fn t(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn billing_line(
    fetched: DateTime<Utc>,
    pct: Option<f64>,
    end: DateTime<Utc>,
    tier: Option<&str>,
) -> String {
    let mut config = json!({"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "end": t(end)}});
    if let Some(p) = pct {
        config["creditUsagePercent"] = json!(p);
    }
    let mut ctx = json!({"config": config});
    if let Some(x) = tier {
        ctx["subscriptionTier"] = json!(x);
    }
    json!({"ts": t(fetched), "level": "info", "msg": "billing: fetched credits config", "ctx": ctx})
        .to_string()
}

struct Fx {
    dir: TempDir,
    env: Env,
}

fn fx() -> Fx {
    let dir = TempDir::new();
    let mut env = Env::new(Os::Linux);
    env.home = Some(dir.join("home"));
    env.xdg_config_home = Some(dir.join("xdg"));
    env.claude_config_dir = Some(dir.join("cfg"));
    env.grok_home = Some(dir.join("grokhome"));
    Fx { dir, env }
}

impl Fx {
    fn grok_log(&self, lines: &[String]) {
        std::fs::create_dir_all(self.dir.join("grokhome/logs")).unwrap();
        std::fs::write(
            self.dir.join("grokhome/logs/unified.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn grok_auth(&self, expires: DateTime<Utc>) {
        std::fs::create_dir_all(self.dir.join("grokhome")).unwrap();
        std::fs::write(
            self.dir.join("grokhome/auth.json"),
            json!({"https://auth.x.ai::c1": {"key": TOKEN, "user_id": USER, "expires_at": t(expires)}}).to_string(),
        )
        .unwrap();
    }

    fn claude_history(&self, session: f64, weekly: f64) {
        let dir = self.dir.join("xdg/Claude");
        std::fs::create_dir_all(&dir).unwrap();
        let s = |m: i64, fh: f64| json!({"t": (Utc::now() - Duration::minutes(m)).timestamp_millis(), "org": "o", "u": {"fh": fh, "sd": weekly}});
        std::fs::write(
            dir.join("plan-usage-history.json"),
            json!({"version": 2, "samples": [s(20, session - 2.0), s(1, session)]}).to_string(),
        )
        .unwrap();
    }

    /// The real wiring: Claude's default chain, Grok's chain (billing at `url`, then the log), two files.
    fn app(&self, url: &str, settings_json: Option<&str>) -> App {
        if let Some(j) = settings_json {
            std::fs::write(self.dir.join("s.json"), j).unwrap();
        }
        let mut billing = GrokBilling::new(self.env.clone());
        billing.url = url.to_string();
        let grok_poller = Poller::with_chain_for(
            vec![
                Box::new(billing),
                Box::new(GrokLog {
                    env: self.env.clone(),
                }),
            ],
            "grok",
        );
        let env = self.env.clone();
        App::new(
            Box::new(JsonlStore::new(self.dir.join("usage.jsonl"))),
            Arc::new(Settings::load(self.dir.join("s.json"))),
            Poller::with_env(&self.env, "http://127.0.0.1:1/usage", None),
        )
        .with_grok_poller(
            Box::new(JsonlStore::new(self.dir.join("usage-grok.jsonl"))),
            grok_poller,
            Arc::new(move || grok::detected(&env)),
        )
    }
}

fn wait(what: &str, f: &dyn Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < StdDuration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    }
}

fn grok_ready(app: &App) -> bool {
    app.state()
        .grok
        .is_some_and(|g| g.plan.is_some() || g.collector.error.is_some())
}

// ---- a tiny mock of the billing endpoint -------------------------------------------------------

struct Mock {
    url: String,
    hits: Arc<Mutex<usize>>,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Mock {
    fn start(body: Value) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (status, hits, stop) = (
            Arc::new(AtomicU16::new(200)),
            Arc::new(Mutex::new(0usize)),
            Arc::new(AtomicBool::new(false)),
        );
        let (st, h, sp) = (status.clone(), hits.clone(), stop.clone());
        let body = body.to_string();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(mut c) = conn {
                    let mut r = BufReader::new(c.try_clone().unwrap());
                    let mut line = String::new();
                    while r.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                        line.clear();
                    }
                    *h.lock().unwrap() += 1;
                    let _ = write!(c, "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", st.load(Ordering::SeqCst), body.len());
                }
            }
        });
        Mock {
            url: format!("http://{addr}/v1/billing?format=credits"),
            hits,
            stop,
            addr,
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Notification>>);
impl Notifier for Recorder {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

// ------------------------------------------------------------ tests

#[test]
fn grok_only_from_the_cli_log_shows_a_weekly_only_block() {
    let f = fx();
    let end = Utc::now() + Duration::days(4);
    f.grok_log(&[billing_line(
        Utc::now() - Duration::minutes(12),
        Some(7.0),
        end,
        Some("SuperGrok Heavy"),
    )]);
    let app = f.app("http://127.0.0.1:1/v1/billing", None); // no login: the log is the source
    app.start();
    wait("grok reading", &|| grok_ready(&app));
    let st = app.state();
    assert!(st.plan.is_none(), "Claude has no data here");
    let g = st.grok.as_ref().unwrap();
    assert!(g.enabled);
    assert_eq!(g.plan.as_ref().unwrap().weekly.as_ref().unwrap().pct, 7.0);
    assert!(g.plan.as_ref().unwrap().session.is_none());

    let vendors = vendor::from_state(&st);
    assert_eq!(vendors.len(), 1);
    assert_eq!(vendors[0].id, VendorId::Grok);
    let home = view::build_home(&st, Utc::now());
    assert!(home.empty.is_none());
    let rows = &home.vendors[0].rows;
    assert_eq!(rows.len(), 1, "Grok has no Session row");
    assert_eq!(
        (rows[0].label, rows[0].value.as_str(), rows[0].stale),
        ("Weekly", "7%", false)
    );
    assert_eq!(rows[0].cells.iter().filter(|c| **c == Cell::Lit).count(), 1); // 7%: only the segment centred at 2.5% is below it
    let tip: Vec<&str> = rows[0].tip.iter().map(|l| l.text.as_str()).collect();
    assert!(tip[0].starts_with("Weekly resets in "), "{tip:?}");
    assert!(tip.contains(&"Plan: SuperGrok Heavy"), "{tip:?}");
    assert!(
        tip.iter()
            .any(|l| l.starts_with("From Grok CLI log, sample 12 min ago")),
        "{tip:?}"
    );
    // the records went to the Grok file, tagged; Claude's file has no Grok record
    let gr = JsonlStore::new(f.dir.join("usage-grok.jsonl")).read();
    assert!(!gr.is_empty() && gr.iter().all(|r| r["vendor"] == "grok"));
    let cl = JsonlStore::new(f.dir.join("usage.jsonl")).read();
    assert!(
        cl.iter()
            .all(|r| r.get("vendor").is_none() && r["source"] != "grok-log"),
        "{cl:?}"
    );
    app.stop();
    app.join();
}

#[test]
fn claude_and_grok_side_by_side_with_live_billing_preferred() {
    let f = fx();
    f.claude_history(42.0, 20.0);
    let end = Utc::now() + Duration::days(5);
    f.grok_log(&[billing_line(
        Utc::now() - Duration::minutes(40),
        Some(3.0),
        end,
        None,
    )]);
    f.grok_auth(Utc::now() + Duration::hours(6));
    let mock = Mock::start(
        json!({"config": {"creditUsagePercent": 11.5, "currentPeriod": {"end": t(end)}}, "subscriptionTier": "SuperGrok"}),
    );
    let app = f.app(&mock.url, None);
    app.start();
    wait("both vendors", &|| {
        let st = app.state();
        st.plan.is_some()
            && st.grok.as_ref().is_some_and(|g| {
                g.plan
                    .as_ref()
                    .is_some_and(|p| p.last_source.as_deref() == Some("grok-billing"))
            })
    });
    let st = app.state();
    let home = view::build_home(&st, Utc::now());
    let names: Vec<&str> = home.vendors.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Claude", "Grok"]);
    assert_eq!(home.vendors[0].rows.len(), 2);
    assert_eq!(home.vendors[1].rows[0].value, "12%"); // 11.5 rounds half up
    let tip: Vec<&str> = home.vendors[1].rows[0]
        .tip
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        tip.contains(&"Live from Grok billing") && tip.contains(&"Plan: SuperGrok"),
        "{tip:?}"
    );
    assert_eq!(*mock.hits.lock().unwrap(), 1);
    // no token or id in either file
    for file in ["usage.jsonl", "usage-grok.jsonl"] {
        let text = std::fs::read_to_string(f.dir.join(file)).unwrap();
        assert!(!text.contains(TOKEN) && !text.contains(USER), "{file}");
    }
    // the next local tick re-reads only the log: the live reading stays the newest
    app.collector.refresh();
    app.stop();
    app.join();
}

#[test]
fn grok_is_off_by_default_without_the_cli_and_can_be_switched() {
    // no Grok folder at all: nothing polled, no error row, no block (automatic mode)
    let f = fx();
    f.claude_history(10.0, 5.0);
    let app = f.app("http://127.0.0.1:1/v1/billing", None);
    app.start();
    wait("claude reading", &|| app.state().plan.is_some());
    std::thread::sleep(StdDuration::from_millis(300));
    let st = app.state();
    assert!(!st.grok.as_ref().unwrap().enabled);
    assert_eq!(vendor::from_state(&st).len(), 1);
    assert!(
        !f.dir.join("usage-grok.jsonl").exists()
            || JsonlStore::new(f.dir.join("usage-grok.jsonl"))
                .read()
                .is_empty()
    );
    app.stop();
    app.join();

    // forced off although the folder exists: still nothing
    let f = fx();
    f.claude_history(10.0, 5.0);
    f.grok_log(&[billing_line(
        Utc::now() - Duration::minutes(2),
        Some(7.0),
        Utc::now() + Duration::days(4),
        None,
    )]);
    let app = f.app(
        "http://127.0.0.1:1/v1/billing",
        Some("{\"grok_enabled\": false}"),
    );
    app.start();
    wait("claude reading", &|| app.state().plan.is_some());
    std::thread::sleep(StdDuration::from_millis(300));
    assert!(vendor::from_state(&app.state())
        .iter()
        .all(|v| v.id != VendorId::Grok));
    assert!(!app.grok_on());
    // the toggle turns it on at runtime and the settings file remembers it
    app.set_grok_enabled(true);
    wait("grok after the switch", &|| grok_ready(&app));
    assert!(app.grok_on());
    assert!(std::fs::read_to_string(f.dir.join("s.json"))
        .unwrap()
        .contains("\"grok_enabled\": true"));
    app.stop();
    app.join();
}

/// A 13 h old log sample (its period still running) is the last known value: shown, dimmed, with
/// the data-age chip, not dashes.
#[test]
fn a_stale_grok_log_sample_stays_visible_dimmed_with_the_chip() {
    let f = fx();
    f.grok_log(&[billing_line(
        Utc::now() - Duration::hours(13),
        Some(7.0),
        Utc::now() + Duration::days(3),
        None,
    )]);
    let app = f.app("http://127.0.0.1:1/v1/billing", None);
    app.start();
    wait("grok state", &|| grok_ready(&app));
    let home = view::build_home(&app.state(), Utc::now());
    let g = home
        .vendors
        .iter()
        .find(|v| v.id == VendorId::Grok)
        .unwrap();
    assert!(!g.rows[0].stale && g.rows[0].dim);
    assert_eq!(g.rows[0].value, "7%");
    assert_eq!(g.age_chip.as_ref().unwrap().compact, "13 h");
    app.stop();
    app.join();
}

#[test]
fn ended_and_missing_grok_data_is_dashes_with_the_reason() {
    // a snapshot of a billing period that has ended: last week's number, dashes
    let (name, why) = ("ended", "no current data (last reading 10 min ago: 7%)");
    {
        let f = fx();
        f.grok_log(&[billing_line(
            Utc::now() - Duration::minutes(10),
            Some(7.0),
            Utc::now() - Duration::minutes(1),
            None,
        )]);
        let app = f.app("http://127.0.0.1:1/v1/billing", None);
        app.start();
        wait("grok state", &|| grok_ready(&app));
        let home = view::build_home(&app.state(), Utc::now());
        let row = &home
            .vendors
            .iter()
            .find(|v| v.id == VendorId::Grok)
            .expect(name)
            .rows[0];
        assert!(row.stale, "{name}");
        assert_eq!(row.value, "--");
        assert!(row.cells.iter().all(|c| *c == Cell::Off));
        assert!(row.tip[0].text.contains(why), "{name}: {:?}", row.tip);
        app.stop();
        app.join();
    }
    // tracked (the folder exists) but nothing usable: a placeholder with the reason, never a number
    let f = fx();
    std::fs::create_dir_all(f.dir.join("grokhome")).unwrap();
    let app = f.app("http://127.0.0.1:1/v1/billing", None);
    app.start();
    wait("grok error", &|| {
        app.state()
            .grok
            .is_some_and(|g| g.collector.error.is_some())
    });
    let home = view::build_home(&app.state(), Utc::now());
    let v = &home.vendors[0];
    assert_eq!(v.id, VendorId::Grok);
    assert!(v.rows[0].stale && v.rows[0].value == "--");
    assert!(
        v.rows[0].tip[0].text.contains("no current data")
            && v.rows[0].tip[0].text.contains("Grok login not found"),
        "{:?}",
        v.rows[0].tip
    );
    app.stop();
    app.join();
}

#[test]
fn grok_weekly_alerts_use_their_own_file_and_fire_once() {
    let f = fx();
    f.grok_log(&[billing_line(
        Utc::now() - Duration::minutes(3),
        Some(92.0),
        Utc::now() + Duration::days(4),
        None,
    )]);
    let app = f.app("http://127.0.0.1:1/v1/billing", None);
    let rec = Arc::new(Recorder::default());
    app.set_notifier(rec.clone());
    app.grok
        .as_ref()
        .unwrap()
        .collector
        .set_local_interval_override(Some(StdDuration::from_millis(60)));
    app.start();
    wait("an alert", &|| !rec.0.lock().unwrap().is_empty());
    wait("more polls", &|| {
        app.grok.as_ref().unwrap().collector.status().polls >= 5
    });
    let titles: Vec<String> = rec
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|n| n.title.clone())
        .collect();
    assert_eq!(titles, ["Grok weekly: 8% left"]);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(f.dir.join("alerts-grok.json")).unwrap())
            .unwrap();
    assert!(
        saved["windows"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| k.starts_with("grok:weekly:")),
        "{saved}"
    );
    assert!(
        !f.dir.join("alerts.json").exists(),
        "Claude's alert state is untouched"
    );
    app.stop();
    app.join();
}

#[test]
fn the_grok_chart_reads_the_grok_history() {
    let f = fx();
    let end = Utc::now() + Duration::days(4);
    f.grok_log(&[
        billing_line(Utc::now() - Duration::hours(50), Some(1.0), end, None),
        billing_line(Utc::now() - Duration::hours(26), Some(4.0), end, None),
        billing_line(Utc::now() - Duration::minutes(5), Some(7.0), end, None),
    ]);
    let app = f.app("http://127.0.0.1:1/v1/billing", None);
    app.start();
    wait("grok history", &|| grok_ready(&app));
    let h = app.history_for(VendorId::Grok);
    let n = h.with_readings(|r| {
        view::chart_data(r, Kind::Weekly, view::Range::All, Utc::now())
            .segments
            .iter()
            .map(Vec::len)
            .sum::<usize>()
    });
    assert_eq!(n, 3);
    // Claude's history is a different object
    assert!(!Arc::ptr_eq(h, app.history_for(VendorId::Claude)));
    app.stop();
    app.join();
}

#[test]
fn settings_grok_enabled_is_tri_state() {
    let dir = TempDir::new();
    let s = Settings::load(dir.join("s.json"));
    assert_eq!(s.get().grok_enabled, None);
    assert!(s.get().grok_on(true) && !s.get().grok_on(false));
    s.set_grok_enabled(false);
    assert!(!Settings::load(dir.join("s.json")).get().grok_on(true));
    s.update(&json!({"grok_enabled": null}), true);
    assert_eq!(Settings::load(dir.join("s.json")).get().grok_enabled, None);
    let _ = DesktopHistory {
        env: Env::new(Os::Linux),
    }; // (keeps the import honest)
}
