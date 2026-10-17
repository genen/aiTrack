// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared test helpers: temp dirs and a tiny mock HTTP server for the usage endpoint.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("aitrack-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn join(&self, s: &str) -> PathBuf {
        self.0.join(s)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn response() -> Value {
    json!({
        "five_hour": {"utilization": 42.0, "resets_at": "2026-10-14T15:00:00.123456+00:00"},
        "seven_day": {"utilization": 21.5, "resets_at": "2026-10-20T08:00:00+00:00"},
        "spend": {"enabled": true, "used": {"amount_minor": 1359, "exponent": 2}, "limit": {"amount_minor": 5000, "exponent": 2}},
        "limits": [{"kind": "weekly_scoped", "percent": 10, "scope": {"model": {"display_name": "Fable"}}}],
        "future_field": {"keep": "me"},
    })
}

pub struct Mock {
    pub url: String,
    pub status: Arc<AtomicU16>,
    pub non_json: Arc<AtomicBool>,
    /// (Authorization, anthropic-beta, Accept, User-Agent) of each request.
    pub seen: Arc<Mutex<Vec<[Option<String>; 4]>>>,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Mock {
    pub fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = Arc::new(AtomicU16::new(200));
        let non_json = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, nj, sn, sp) = (status.clone(), non_json.clone(), seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(c) = conn {
                    handle(c, &st, &nj, &sn);
                }
            }
        });
        Mock {
            url: format!("http://{addr}/usage"),
            status,
            non_json,
            seen,
            stop,
            addr,
        }
    }

    pub fn set_status(&self, s: u16) {
        self.status.store(s, Ordering::SeqCst);
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn handle(
    mut c: TcpStream,
    status: &AtomicU16,
    non_json: &AtomicBool,
    seen: &Mutex<Vec<[Option<String>; 4]>>,
) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(c.try_clone().unwrap());
    let (mut auth, mut beta, mut accept, mut ua) = (None, None, None, None);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            let v = Some(v.trim().to_string());
            match k.to_ascii_lowercase().as_str() {
                "authorization" => auth = v,
                "anthropic-beta" => beta = v,
                "accept" => accept = v,
                "user-agent" => ua = v,
                _ => {}
            }
        }
    }
    seen.lock().unwrap().push([auth, beta, accept, ua]);
    let code = status.load(Ordering::SeqCst);
    let body = if non_json.load(Ordering::SeqCst) {
        "<html>nope</html>".to_string()
    } else {
        response().to_string()
    };
    let extra = if code == 429 {
        "Retry-After: 120\r\n"
    } else {
        ""
    };
    let _ = write!(
        c,
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

pub fn write_creds(dir: &Path, token: &str, expires_at_ms: f64) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(".credentials.json");
    std::fs::write(
        &p,
        json!({"claudeAiOauth": {"accessToken": token, "expiresAt": expires_at_ms}}).to_string(),
    )
    .unwrap();
    p
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0
}
