// SPDX-License-Identifier: GPL-3.0-or-later
//! The MCP server (`aitrack --mcp`): protocol handling and the usage it reports, from saved
//! history files in a temp folder (never the real machine's).
use crate::common::TempDir;
use aitrack::mcp::{handle, parse_vendor, serve, Sources};
use aitrack::sources::{Env, Os};
use aitrack::vendor::VendorId;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::process::{Command, Stdio};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap()
}

fn ts(t: DateTime<Utc>) -> String {
    aitrack::util::iso(t)
}

fn write_lines(path: &std::path::Path, recs: &[Value]) {
    let text: String = recs.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(path, text).unwrap();
}

/// Claude (weekly at 100%), Grok (one shared pool), ChatGPT (a 5-hour window that has reset since
/// its reading) and Gemini (weekly only, switched on in the settings).
fn setup(dir: &TempDir) -> Sources {
    setup_at(dir, now())
}

fn setup_at(dir: &TempDir, n: DateTime<Utc>) -> Sources {
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".grok")).unwrap();
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    write_lines(
        &data_dir.join("usage.jsonl"),
        &[json!({
            "ts": ts(n - Duration::minutes(5)), "ok": true, "source": "oauth",
            "session": {"pct": 42.0, "resets_at": ts(n + Duration::hours(2))},
            "weekly": {"pct": 100.0, "resets_at": ts(n + Duration::days(3))},
            "limits": [{"kind": "weekly_scoped", "percent": 10, "scope": {"model": {"display_name": "Fable"}}}],
        })],
    );
    write_lines(
        &data_dir.join("usage-grok.jsonl"),
        &[json!({
            "ts": ts(n - Duration::minutes(10)), "ok": true, "source": "grok-billing",
            "weekly": {"pct": 30.0, "resets_at": ts(n + Duration::days(4))},
            "raw": {"tier": "SuperGrok", "unified": true},
        })],
    );
    write_lines(
        &data_dir.join("usage-codex.jsonl"),
        &[json!({
            "ts": ts(n - Duration::hours(6)), "ok": true, "source": "codex-usage",
            "session": {"pct": 100.0, "resets_at": ts(n - Duration::hours(1))},
            "weekly": {"pct": 20.0, "resets_at": ts(n + Duration::days(2))},
            "raw": {"tier": "Plus"},
        })],
    );
    write_lines(
        &data_dir.join("usage-antigravity.jsonl"),
        &[json!({
            "ts": ts(n - Duration::minutes(3)), "ok": true, "source": "agy-usage",
            "weekly": {"pct": 73.0},
            "raw": {"full_at": {"weekly": ts(n + Duration::hours(30))}},
        })],
    );
    let settings = dir.join("settings.json");
    std::fs::write(&settings, r#"{"antigravity_enabled": true}"#).unwrap();
    let mut env = Env::new(Os::current());
    env.home = Some(home.clone());
    env.appdata = Some(home.clone());
    env.localappdata = Some(home.clone());
    env.xdg_config_home = Some(home.join(".config"));
    env.grok_bot_dir = Some(home.join("no-grok-bot"));
    Sources {
        data: data_dir.join("usage.jsonl"),
        settings,
        env,
    }
}

fn call(src: &Sources, name: &str, args: Value) -> Value {
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": name, "arguments": args}}),
        src,
        now(),
    )
    .unwrap();
    assert_eq!(r["id"], 7);
    r["result"].clone()
}

fn ai<'a>(payload: &'a Value, id: &str) -> &'a Value {
    payload["ais"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("{id} missing in {payload:#}"))
}

#[test]
fn initialize_answers_with_a_supported_protocol_version() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26", "capabilities": {}}}),
        &src,
        now(),
    )
    .unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r["result"]["serverInfo"]["name"], "aitrack");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
                "params": {"protocolVersion": "1999-01-01"}}),
        &src,
        now(),
    )
    .unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn notifications_get_no_reply_and_unknown_methods_an_error() {
    let dir = TempDir::new();
    let src = setup(&dir);
    assert!(handle(
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        &src,
        now()
    )
    .is_none());
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": "x", "method": "sampling/createMessage"}),
        &src,
        now(),
    )
    .unwrap();
    assert_eq!(r["id"], "x");
    assert_eq!(r["error"]["code"], -32601);
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
        &src,
        now(),
    )
    .unwrap();
    assert_eq!(r["result"], json!({}));
}

#[test]
fn tools_list_names_both_read_only_tools() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &src,
        now(),
    )
    .unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["check_ai_availability", "get_ai_usage"]);
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
    }
}

#[test]
fn availability_reports_the_blocking_limit_and_when_it_resets() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let r = call(&src, "check_ai_availability", json!({}));
    assert_eq!(r["isError"], false);
    let p = &r["structuredContent"];
    let claude = ai(p, "claude");
    assert_eq!(claude["available"], false);
    assert_eq!(claude["limited_by"], json!(["weekly"]));
    assert_eq!(claude["available_again_at"], ts(now() + Duration::days(3)));
    assert_eq!(claude["five_hour_used_percent"], 42.0);
    assert_eq!(claude["weekly_used_percent"], 100.0);
    assert_eq!(claude["data_age_minutes"], 5.0);

    let grok = ai(p, "grok");
    assert_eq!(grok["available"], true);
    assert_eq!(grok["weekly_used_percent"], 30.0);
    assert!(grok.get("five_hour_used_percent").is_none());

    // the 5-hour window at 100% has reset since its reading: not a block
    let chatgpt = ai(p, "chatgpt");
    assert_eq!(chatgpt["available"], true);
    assert_eq!(chatgpt["five_hour_used_percent"], Value::Null);
    assert_eq!(chatgpt["weekly_used_percent"], 20.0);

    let gemini = ai(p, "gemini");
    assert_eq!(gemini["available"], true);
    assert_eq!(gemini["weekly_used_percent"], 73.0);
    // the text content carries the same JSON
    let text = r["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *p);
}

#[test]
fn usage_has_windows_plans_models_and_data_age() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let r = call(&src, "get_ai_usage", json!({"ai": "Claude"}));
    let p = &r["structuredContent"];
    assert_eq!(p["ais"].as_array().unwrap().len(), 1);
    let c = ai(p, "claude");
    assert_eq!(c["ai"], "Claude");
    assert_eq!(c["data_is_old"], false);
    assert_eq!(c["source"], "oauth");
    let w = c["windows"].as_array().unwrap();
    assert_eq!(w[0]["window"], "5-hour");
    assert_eq!(w[0]["used_percent"], 42.0);
    assert_eq!(w[0]["left_percent"], 58.0);
    assert_eq!(w[0]["hours_until_reset"], 2.0);
    assert_eq!(w[1]["window"], "weekly");
    assert_eq!(w[1]["used_percent"], 100.0);
    assert_eq!(c["models"][0]["name"], "Fable");

    let r = call(&src, "get_ai_usage", json!({"ai": "xai"}));
    let g = ai(&r["structuredContent"], "grok");
    assert_eq!(g["plan"], "SuperGrok");
    assert_eq!(g["shared_pool_with_grok_bot"], true);

    let r = call(&src, "get_ai_usage", json!({"ai": "codex"}));
    let x = ai(&r["structuredContent"], "chatgpt");
    assert_eq!(x["plan"], "Plus");
    assert_eq!(x["data_is_old"], true);
    assert_eq!(x["windows"][0]["reset_since_reading"], true);
    // no pace from a six-hour-old reading
    assert!(x["windows"][1].get("pace_percent_per_hour").is_none());

    let r = call(&src, "get_ai_usage", json!({"ai": "gemini"}));
    let g = ai(&r["structuredContent"], "gemini");
    assert_eq!(g["full_again_at"], ts(now() + Duration::hours(30)));
    assert_eq!(g["windows"].as_array().unwrap().len(), 1);
}

#[test]
fn unknown_ai_or_tool_is_an_error_and_untracked_ai_a_note() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let r = call(&src, "get_ai_usage", json!({"ai": "clippy"}));
    assert_eq!(r["isError"], true);
    let r = handle(
        &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": "delete_everything", "arguments": {}}}),
        &src,
        now(),
    )
    .unwrap();
    assert_eq!(r["error"]["code"], -32602);
    // Gemini switched off: not tracked, so an empty list with a note
    std::fs::write(&src.settings, r#"{"antigravity_enabled": false}"#).unwrap();
    let r = call(&src, "check_ai_availability", json!({"ai": "gemini"}));
    assert_eq!(r["structuredContent"]["ais"], json!([]));
    assert!(r["structuredContent"]["note"]
        .as_str()
        .unwrap()
        .contains("Gemini"));
}

#[test]
fn vendor_names_are_parsed_loosely() {
    assert_eq!(parse_vendor(" ChatGPT "), Some(VendorId::Codex));
    assert_eq!(parse_vendor("openai"), Some(VendorId::Codex));
    assert_eq!(parse_vendor("Antigravity"), Some(VendorId::Antigravity));
    assert_eq!(parse_vendor("anthropic"), Some(VendorId::Claude));
    assert_eq!(parse_vendor("grok bot"), Some(VendorId::Grok));
    assert_eq!(parse_vendor("bard"), None);
}

#[test]
fn serve_answers_one_line_per_request_and_survives_bad_input() {
    let dir = TempDir::new();
    let src = setup(&dir);
    let long = "x".repeat(aitrack::mcp::MAX_LINE_BYTES as usize + 10);
    let input = format!(
        "{}\n\n{}\nnot json\n{long}\n{}\n",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let mut out = Vec::new();
    serve(input.as_bytes(), &mut out, &src, now).unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["error"]["code"], -32700);
    assert_eq!(lines[2]["error"]["code"], -32600);
    assert_eq!(lines[3]["id"], 2);
}

#[test]
fn the_binary_speaks_mcp_on_stdout_only() {
    let dir = TempDir::new();
    // the binary uses the real clock
    let src = setup_at(&dir, Utc::now());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aitrack"));
    cmd.arg("--mcp")
        .arg("--data")
        .arg(&src.data)
        .env("AITRACK_SETTINGS", &src.settings)
        .env("AITRACK_GROK_BOT_DIR", dir.join("none"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(k, dir.join("home"));
    }
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "check_ai_availability", "arguments": {"ai": "claude"}}})
        )
        .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    let p = &lines[1]["result"]["structuredContent"];
    assert_eq!(p["ais"][0]["id"], "claude");
    assert_eq!(p["ais"][0]["available"], false);
}
