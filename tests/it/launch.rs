// SPDX-License-Identifier: GPL-3.0-or-later
//! The Claude launcher: method order, package family derivation, fake spawner, log file.
use crate::common;
use aitrack::launch::{self, Attempt, Spawner};
use common::TempDir;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    /// Calls whose text contains one of these fail.
    fail: Vec<String>,
}

impl Fake {
    fn failing(patterns: &[&str]) -> Fake {
        Fake {
            calls: Mutex::default(),
            fail: patterns.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn record(&self, what: String) -> Result<(), String> {
        let bad = self.fail.iter().any(|p| what.contains(p));
        self.calls.lock().unwrap().push(what);
        if bad {
            Err("simulated failure".into())
        } else {
            Ok(())
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Spawner for Fake {
    fn open_uri(&self, uri: &str) -> Result<(), String> {
        self.record(format!("open {uri}"))
    }
    fn run(&self, program: &Path, args: &[String]) -> Result<(), String> {
        self.record(format!(
            "run {} {}",
            program.display().to_string().replace('\\', "/"),
            args.join(" ")
        ))
    }
}

fn local_with(dirs: &[&str], files: &[&str]) -> TempDir {
    let t = TempDir::new();
    for d in dirs {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    for f in files {
        std::fs::create_dir_all(t.join(f).parent().unwrap()).unwrap();
        std::fs::write(t.join(f), b"exe").unwrap();
    }
    t
}

#[test]
fn the_protocol_is_tried_first_and_stops_there_when_it_works() {
    let t = local_with(
        &["Packages/Claude_pzs8sxrjxfjjc"],
        &["AnthropicClaude/claude.exe"],
    );
    let fake = Fake::default();
    let tried = launch::launch_claude(Some(t.path()), &fake);
    assert_eq!(fake.calls(), ["open claude://"]);
    assert_eq!(
        tried,
        [Attempt {
            method: "protocol claude://".into(),
            ok: true,
            detail: None
        }]
    );
    assert!(launch::succeeded(&tried));
}

#[test]
fn then_the_store_app_with_the_family_name_from_the_packages_folder() {
    let t = local_with(
        &[
            "Packages/Claude_pzs8sxrjxfjjc",
            "Packages/Other_abc",
            "Packages/NotClaude_1",
            "Packages/claude_lower2",
        ],
        &[],
    );
    let fake = Fake::failing(&["open "]);
    let tried = launch::launch_claude(Some(t.path()), &fake);
    // sorted, case-insensitive match; the first one that works ends it
    assert_eq!(
        fake.calls(),
        [
            "open claude://",
            "run explorer.exe shell:AppsFolder\\Claude_pzs8sxrjxfjjc!Claude"
        ]
    );
    assert!(launch::succeeded(&tried));
    assert_eq!(tried.len(), 2);
    assert!(!tried[0].ok && tried[0].detail.as_deref() == Some("simulated failure"));
    // the derivation itself
    assert_eq!(
        launch::store_families(t.path()),
        ["Claude_pzs8sxrjxfjjc", "claude_lower2"]
    );
    assert_eq!(
        launch::apps_folder_target("Claude_x"),
        "shell:AppsFolder\\Claude_x!Claude"
    );
}

#[test]
fn a_failing_store_activation_falls_back_to_the_next_family_then_to_classic_installs() {
    let t = local_with(
        &["Packages/Claude_aaa", "Packages/Claude_bbb"],
        &["Programs/Claude/Claude.exe"], // only the second classic location exists
    );
    let fake = Fake::failing(&["open ", "Claude_aaa", "Claude_bbb"]);
    let tried = launch::launch_claude(Some(t.path()), &fake);
    let calls = fake.calls();
    assert_eq!(calls[0], "open claude://");
    assert!(
        calls[1].contains("Claude_aaa!Claude") && calls[2].contains("Claude_bbb!Claude"),
        "{calls:?}"
    );
    assert!(
        calls[3].ends_with("Programs/Claude/Claude.exe "),
        "{calls:?}"
    );
    assert_eq!(calls.len(), 4);
    assert!(launch::succeeded(&tried));
    // the order of the classic defaults when both exist
    let t = local_with(
        &[],
        &["AnthropicClaude/claude.exe", "Programs/Claude/Claude.exe"],
    );
    let fake = Fake::failing(&["open ", "AnthropicClaude"]);
    launch::launch_claude(Some(t.path()), &fake);
    let calls = fake.calls();
    assert!(
        calls[1].contains("AnthropicClaude/claude.exe")
            && calls[2].contains("Programs/Claude/Claude.exe"),
        "{calls:?}"
    );
    assert_eq!(launch::classic_candidates(t.path()).len(), 2);
}

#[test]
fn everything_failing_returns_every_attempt_and_never_panics() {
    let t = local_with(&["Packages/Claude_aaa"], &["AnthropicClaude/claude.exe"]);
    let fake = Fake::failing(&["open ", "explorer", "claude.exe"]);
    let tried = launch::launch_claude(Some(t.path()), &fake);
    assert!(!launch::succeeded(&tried));
    assert_eq!(tried.len(), 3);
    assert!(tried.iter().all(|a| !a.ok));
    // no LOCALAPPDATA: only the protocol is possible
    let tried = launch::launch_claude(None, &Fake::failing(&["open "]));
    assert_eq!(tried.len(), 2);
    assert!(tried[1].detail.as_deref().unwrap().contains("LOCALAPPDATA"));
    // an empty / missing folder: nothing more to try
    let nothing = TempDir::new();
    assert_eq!(
        launch::launch_claude(Some(nothing.path()), &Fake::failing(&["open "])).len(),
        1
    );
    assert!(launch::store_families(&nothing.join("does-not-exist")).is_empty());
}

#[test]
fn the_log_names_what_was_tried_and_is_appended_to() {
    let t = local_with(&["Packages/Claude_aaa"], &[]);
    let log: PathBuf = t.join("data/launch.log");
    let tried = launch::launch_claude(Some(t.path()), &Fake::failing(&["open "]));
    launch::append_log(&log, &launch::log_text(&tried));
    let failed = launch::launch_claude(Some(t.path()), &Fake::failing(&["open ", "explorer"]));
    launch::append_log(&log, &launch::log_text(&failed));
    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("claude desktop launch: protocol claude://: failed (simulated failure); store app shell:AppsFolder\\Claude_aaa!Claude: ok"), "{}", lines[0]);
    assert!(
        lines[1].contains("store app shell:AppsFolder\\Claude_aaa!Claude: failed"),
        "{}",
        lines[1]
    );
}

#[cfg(not(any(windows, target_os = "macos")))]
#[test]
fn the_system_spawner_does_nothing_off_windows() {
    let s = launch::SystemSpawner;
    assert!(s
        .open_uri("claude://")
        .unwrap_err()
        .contains("only supported on Windows and macOS"));
    assert!(s.run(Path::new("explorer.exe"), &[]).is_err());
}

#[test]
fn the_async_launcher_reports_back_and_writes_the_log() {
    let t = TempDir::new();
    let (tx, rx) = std::sync::mpsc::channel();
    // (a fake that cannot open anything: the real spawner would start programs on the test machine,
    // and on a Windows CI runner that crashed the test process once with a heap corruption)
    let fake = std::sync::Arc::new(Fake::failing(&["claude://", "AppsFolder", "run "]));
    launch::launch_claude_async_with(t.join("launch.log"), fake.clone(), move |ok| {
        let _ = tx.send(ok);
    });
    let ok = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("callback");
    assert!(!ok);
    let first = if cfg!(target_os = "macos") {
        "run /usr/bin/open"
    } else {
        "open claude://"
    };
    assert!(fake.calls()[0].starts_with(first), "{:?}", fake.calls());
    assert!(std::fs::read_to_string(t.join("launch.log"))
        .unwrap()
        .contains("claude desktop launch:"));
}

#[test]
fn the_windows_shell_call_runs_in_an_initialised_com_apartment() {
    // ShellExecuteW is called from plain std threads; it needs COM initialised on the thread
    // (a Windows-only path, so this is checked in the source on every platform).
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/launch.rs")).unwrap();
    let sq: String = src.split_whitespace().collect();
    let init = sq
        .find("CoInitializeEx(std::ptr::null(),")
        .expect("COM is initialised");
    let call = sq
        .find("letcode=unsafe{ShellExecuteW(")
        .expect("the shell call");
    let done = sq.find("unsafe{CoUninitialize()};").expect("and balanced");
    assert!(init < call && call < done);
}

// ---------------------------------------------------------------- macOS (argv only, nothing is started)

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn mac_claude_is_opened_by_name_then_by_bundle_id() {
    assert_eq!(
        launch::claude_open_argvs_mac(),
        [
            v(&["-a", "Claude"]),
            v(&["-b", "com.anthropic.claudefordesktop"])
        ]
    );
    let fake = Fake::default();
    let tried = launch::launch_claude_mac(&fake);
    assert_eq!(fake.calls(), ["run /usr/bin/open -a Claude"]);
    assert!(launch::succeeded(&tried));
}

#[test]
fn mac_claude_falls_back_to_the_bundle_id_and_reports_failure() {
    let fake = Fake::failing(&["-a Claude"]);
    let tried = launch::launch_claude_mac(&fake);
    assert_eq!(
        fake.calls(),
        [
            "run /usr/bin/open -a Claude",
            "run /usr/bin/open -b com.anthropic.claudefordesktop"
        ]
    );
    assert!(launch::succeeded(&tried));
    assert!(!tried[0].ok && tried[1].ok);

    let fake = Fake::failing(&["open"]);
    let tried = launch::launch_claude_mac(&fake);
    assert_eq!(tried.len(), 2);
    assert!(!launch::succeeded(&tried));
}

#[test]
fn mac_grok_app_names_are_filtered_and_ordered() {
    let names = v(&[
        "Safari.app",
        "Grok.app",
        "Grok Notes.txt",
        "grok bot.app",
        "Grok Extra.app",
        "Groking.app",
    ]);
    assert_eq!(
        launch::grok_mac_app_names(&names),
        ["grok bot.app", "Grok.app", "Grok Extra.app", "Groking.app"]
    );
}

#[test]
fn mac_grok_prefers_an_app_then_the_cli_in_terminal() {
    let t = TempDir::new();
    std::fs::create_dir_all(t.join("Applications/Grok.app")).unwrap();
    let dirs = [t.join("Applications"), t.join("missing")];
    let cli = Path::new("/Users/me/.grok/bin/grok");
    let home = Path::new("/Users/me");

    let fake = Fake::default();
    let tried = launch::launch_grok_mac(&dirs, Some(home), Some(cli), &fake);
    assert!(launch::succeeded(&tried));
    assert_eq!(fake.calls().len(), 1);
    assert!(fake.calls()[0].starts_with("run /usr/bin/open -a "));
    // (Windows spells the joined path with a backslash)
    assert!(fake.calls()[0]
        .replace('\\', "/")
        .ends_with("Applications/Grok.app"));

    // no app: the CLI goes through osascript, nothing else
    let fake = Fake::default();
    let tried = launch::launch_grok_mac(&[], Some(home), Some(cli), &fake);
    assert!(launch::succeeded(&tried));
    assert_eq!(fake.calls().len(), 1);
    assert!(fake.calls()[0].starts_with("run /usr/bin/osascript -e "));

    // nothing at all
    let fake = Fake::default();
    let tried = launch::launch_grok_mac(&[], Some(home), None, &fake);
    assert!(fake.calls().is_empty());
    assert!(!launch::succeeded(&tried));
    assert_eq!(
        tried[0].detail.as_deref(),
        Some("no Grok app or grok CLI found")
    );
}

#[test]
fn mac_terminal_script_quotes_paths_and_types_no_prompt() {
    let a = launch::terminal_osascript_args(
        Path::new("/Users/o'neil/.grok/bin/grok"),
        Path::new("/Users/o'neil"),
    );
    assert_eq!(a.len(), 4);
    assert_eq!(a[0], "-e");
    assert_eq!(
        a[1],
        r#"tell application "Terminal" to do script "cd '/Users/o'\\''neil' && '/Users/o'\\''neil/.grok/bin/grok'""#
    );
    assert_eq!(a[3], r#"tell application "Terminal" to activate"#);
    assert_eq!(launch::sh_quote("a b"), "'a b'");
}

// ---------------------------------------------------------------- start at login (macOS plist)

#[test]
fn the_launch_agent_plist_runs_the_exe_at_load_and_escapes_xml() {
    use aitrack::startup;
    let p = startup::launch_agent_plist(Path::new("/Applications/a&b <x>/aitrack"));
    assert!(p.contains("<key>Label</key>\n    <string>com.sirgene.aitrack</string>"));
    assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
    assert!(p.contains("<array>\n        <string>/Applications/a&amp;b &lt;x&gt;/aitrack</string>"));
    assert!(p.starts_with("<?xml"));
    assert_eq!(
        startup::launch_agent_path(Path::new("/Users/me")),
        Path::new("/Users/me/Library/LaunchAgents/com.sirgene.aitrack.plist")
    );
}

#[test]
fn the_launch_agent_is_written_found_and_removed_under_an_explicit_home() {
    use aitrack::startup;
    let t = TempDir::new();
    assert!(!startup::agent_is_enabled(t.path()));
    startup::agent_enable(t.path(), Path::new("/x/aitrack")).unwrap();
    assert!(startup::agent_is_enabled(t.path()));
    let text = std::fs::read_to_string(startup::launch_agent_path(t.path())).unwrap();
    assert!(text.contains("<string>/x/aitrack</string>"));
    startup::agent_disable(t.path()).unwrap();
    assert!(!startup::agent_is_enabled(t.path()));
    startup::agent_disable(t.path()).unwrap(); // already gone is fine
}

#[test]
fn spawning_is_disabled_when_the_variable_is_set_to_anything() {
    use aitrack::launch::spawning_disabled_for;
    use std::ffi::OsStr;
    assert!(!spawning_disabled_for(None));
    assert!(spawning_disabled_for(Some(OsStr::new("1"))));
    assert!(spawning_disabled_for(Some(OsStr::new(""))));
}
