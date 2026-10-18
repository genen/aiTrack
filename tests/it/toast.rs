// SPDX-License-Identifier: GPL-3.0-or-later
//! Toast XML per alert kind (ordinary toasts, sound only on the important ones), the vendor logos
//! (assets, files, URIs, fallbacks), the escaping and the alert-style settings.
use crate::common;
use aitrack::alerts::{AlertKind, AlertOptions, Alerts, Notification, Notifier};
use aitrack::analysis::analyze;
use aitrack::app::App;
use aitrack::reading::Reading;
use aitrack::settings::Settings;
use aitrack::store::JsonlStore;
use aitrack::toast::*;
use chrono::{Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn note(kind: AlertKind, vendor: &str) -> Notification {
    let mut n = Notification::new("Claude session: 10% left", "Resets in 3h 20m", kind);
    n.vendor = vendor.into();
    n
}

// ---- XML per kind ----------------------------------------------------------------------------

#[test]
fn every_alert_is_an_ordinary_toast_and_the_important_ones_play_the_reminder_sound() {
    for kind in [
        AlertKind::Info,
        AlertKind::Warning,
        AlertKind::Limit,
        AlertKind::Predictive,
        AlertKind::Recovery,
    ] {
        let x = toast_xml_for(&note(kind, "Claude"), None);
        assert!(x.starts_with("<toast><visual>"), "no scenario: {x}");
        assert!(
            !x.contains("scenario") && !x.contains("<actions>") && !x.contains("Dismiss"),
            "{x}"
        );
        assert!(!x.contains("Looping"), "nothing may loop: {x}");
    }
    for kind in [AlertKind::Warning, AlertKind::Limit, AlertKind::Predictive] {
        assert!(kind.important());
        let x = toast_xml_for(&note(kind, "Claude"), None);
        assert!(
            x.contains(&format!("<audio src=\"{REMINDER_SOUND}\"/>")),
            "{x}"
        );
        assert!(
            x.find("</visual>").unwrap() < x.find("<audio").unwrap(),
            "{x}"
        );
    }
}

#[test]
fn info_is_a_quiet_short_toast_and_recovery_keeps_the_default_sound() {
    let info = toast_xml_for(&note(AlertKind::Info, "Claude"), None);
    assert!(
        !info.contains("scenario") && !info.contains("<actions>"),
        "{info}"
    );
    assert!(info.contains("<audio silent=\"true\"/>"), "{info}");
    assert!(!AlertKind::Info.important() && !AlertKind::Recovery.important());
    let rec = toast_xml_for(&note(AlertKind::Recovery, "Claude"), None);
    assert!(
        !rec.contains("scenario") && !rec.contains("<actions>") && !rec.contains("<audio"),
        "{rec}"
    );
}

#[test]
fn the_sound_setting_switches_the_reminder_sound_off() {
    let mut n = note(AlertKind::Warning, "Claude");
    n.options = AlertOptions { sounds: true };
    let x = toast_xml_for(&n, None);
    assert!(x.contains(REMINDER_SOUND));
    n.options = AlertOptions { sounds: false };
    let x = toast_xml_for(&n, None);
    assert!(x.contains("<audio silent=\"true\"/>"), "{x}");
    assert!(!x.contains(REMINDER_SOUND) && !x.contains("scenario"));
}

#[test]
fn text_is_escaped_in_the_xml() {
    let n = Notification::new("A <b> & \"c\"", "x & y < z", AlertKind::Info);
    let x = toast_xml_for(&n, None);
    assert!(x.contains("<text>A &lt;b&gt; &amp; \"c\"</text>"), "{x}");
    assert!(x.contains("<text>x &amp; y &lt; z</text>"), "{x}");
    assert_eq!(attr_escape("a\"b'c&d<e>"), "a&quot;b&apos;c&amp;d&lt;e&gt;");
    // the old plain builder is unchanged
    assert!(toast_xml("t", "b").contains("<text>t</text><text>b</text>"));
}

// ---- URIs and logos --------------------------------------------------------------------------

#[test]
fn file_uris_escape_spaces_backslashes_drive_letters_and_non_ascii() {
    assert_eq!(
        file_uri(Path::new(r"C:\Users\me\.aitrack\toast-icons\claude.png")),
        "file:///C:/Users/me/.aitrack/toast-icons/claude.png"
    );
    assert_eq!(
        file_uri(Path::new(r"C:\Users\Zoë Müller\My Dir\100%\a&b.png")),
        "file:///C:/Users/Zo%C3%AB%20M%C3%BCller/My%20Dir/100%25/a%26b.png"
    );
    assert_eq!(
        file_uri(Path::new("/tmp/x y/icon.png")),
        "file:///tmp/x%20y/icon.png"
    );
    assert!(!file_uri(Path::new(r"D:\a b\c.png")).contains('\\'));
}

#[test]
fn the_logo_element_is_in_the_toast_with_an_escaped_uri() {
    let dir = TempDir::new();
    let icons = dir.join("my icons & more");
    for (vendor, file) in [("Claude", "claude.png"), ("Grok", "grok.png")] {
        let (xml, why) = build_toast(&note(AlertKind::Warning, vendor), &icons);
        assert!(why.is_none(), "{why:?}");
        assert!(
            xml.contains("<image placement=\"appLogoOverride\" hint-crop=\"none\" src=\"file:///"),
            "{xml}"
        );
        assert!(
            xml.contains(&format!("my%20icons%20%26%20more/{file}")),
            "{xml}"
        );
        assert!(
            !xml.contains("& more"),
            "the path must not appear unescaped: {xml}"
        );
        // the image comes before the text, inside the binding
        assert!(xml.find("<image").unwrap() < xml.find("<text>").unwrap());
        assert!(icons.join(file).is_file());
    }
}

#[test]
fn the_logo_follows_the_vendor_and_other_vendors_get_text_only() {
    let dir = TempDir::new();
    let (c, _) = build_toast(&note(AlertKind::Info, "Claude"), dir.path());
    let (g, _) = build_toast(&note(AlertKind::Info, "Grok"), dir.path());
    let (other, why) = build_toast(&note(AlertKind::Info, "Codex"), dir.path());
    let (empty, _) = build_toast(&note(AlertKind::Info, ""), dir.path());
    assert!(c.contains("claude.png") && !c.contains("grok.png"));
    assert!(g.contains("grok.png") && !g.contains("claude.png"));
    // a vendor without a logo of its own, and alerts without a vendor, carry the app icon
    assert!(other.contains("aitrack.png") && why.is_none());
    assert!(empty.contains("aitrack.png") && !empty.contains("claude.png"));
    assert_ne!(icon_bytes("Claude"), icon_bytes("Grok"));
    assert_ne!(icon_bytes("Claude"), icon_bytes(""));
    assert_eq!(icon_bytes(""), icon_bytes("Codex"));
    assert_eq!(icon_bytes("").unwrap(), aitrack::appicon::TOAST_192);
}

#[test]
fn an_unwritable_icon_folder_gives_a_text_only_toast_and_a_reason_for_the_log() {
    let dir = TempDir::new();
    // a *file* where the folder should be: neither creating the folder nor the icon can work
    let blocked = dir.join("toast-icons");
    std::fs::write(&blocked, b"in the way").unwrap();
    let (xml, why) = build_toast(&note(AlertKind::Limit, "Claude"), &blocked);
    assert!(!xml.contains("<image"), "{xml}");
    assert!(
        xml.contains("<toast><visual>") && xml.contains("<text>Claude session"),
        "the toast itself is intact: {xml}"
    );
    let why = why.expect("a reason to log");
    assert!(why.contains("text only"), "{why}");
}

#[test]
fn a_stale_icon_file_is_replaced_by_the_embedded_one_and_a_current_one_is_left_alone() {
    let dir = TempDir::new();
    let path = ensure_icon(dir.path(), "Claude").unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), icon_bytes("Claude").unwrap());
    std::fs::write(&path, b"old logo").unwrap();
    ensure_icon(dir.path(), "Claude").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), icon_bytes("Claude").unwrap());
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    ensure_icon(dir.path(), "Claude").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before,
        "an unchanged icon is not rewritten"
    );
}

#[test]
fn the_embedded_logos_are_real_pngs_of_the_expected_size_with_transparent_corners() {
    for name in ["claude", "grok"] {
        for px in [96u32, 192] {
            let path = format!(
                "{}/assets/toast/{name}-{px}.png",
                env!("CARGO_MANIFEST_DIR")
            );
            let dec =
                png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
            let mut r = dec.read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let info = r.next_frame(&mut buf).unwrap();
            assert_eq!((info.width, info.height), (px, px), "{path}");
            assert_eq!(info.color_type, png::ColorType::Rgba);
            assert_eq!(buf[3], 0, "{path}: the corner is transparent (squircle)");
            let centre = ((px / 2 * px + px / 2) * 4 + 3) as usize;
            assert_eq!(buf[centre], 255, "{path}: the badge is opaque");
        }
    }
    // the embedded 192 px files are the committed ones
    let on_disk = std::fs::read(format!(
        "{}/assets/toast/claude-192.png",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(icon_bytes("Claude").unwrap(), on_disk.as_slice());
}

// ---- settings and the evaluator --------------------------------------------------------------

#[test]
fn the_alert_sound_setting_defaults_on_persists_and_ignores_bad_and_legacy_values() {
    let d = TempDir::new();
    let path = d.join("s.json");
    let s = Settings::load(&path);
    assert!(s.get().alert_sounds);
    assert_eq!(s.alert_options(), AlertOptions::default());
    s.set_alert_sounds(false);
    let again = Settings::load(&path);
    assert!(!again.get().alert_sounds);
    assert_eq!(again.alert_options(), AlertOptions { sounds: false });
    again.update(&json!({"alert_sounds": "yes"}), true);
    assert!(!again.get().alert_sounds);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["alert_sounds"], false);
}

/// Settings files written by older versions still carry the removed keys: they load, change
/// nothing, and are gone after the next save.
#[test]
fn legacy_persistent_warnings_and_flash_taskbar_keys_are_ignored_and_dropped() {
    let d = TempDir::new();
    let path = d.join("s.json");
    std::fs::write(
        &path,
        json!({"interval_min": 10.0, "alert_sounds": false, "persistent_warnings": true, "flash_taskbar": true})
            .to_string(),
    )
    .unwrap();
    let s = Settings::load(&path);
    assert_eq!(s.get().interval_min, 10.0);
    assert!(!s.get().alert_sounds);
    s.set_alert_sounds(false);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.get("persistent_warnings").is_none() && saved.get("flash_taskbar").is_none());
    assert_eq!(saved["interval_min"], 10.0);
}

#[derive(Default)]
struct Rec(Mutex<Vec<Notification>>);
impl Notifier for Rec {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(n.clone());
        Ok(())
    }
}

fn plan(pct: f64) -> aitrack::analysis::Plan {
    let now = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
    let rs: Vec<Reading> = [60, 0]
        .iter()
        .map(|m| {
            Reading::from_value(&json!({
                "ok": true, "source": "oauth", "ts": (now - Duration::minutes(*m)).to_rfc3339(),
                "session": {"pct": pct, "resets_at": (now + Duration::minutes(200)).to_rfc3339()},
                "weekly": {"pct": 5.0, "resets_at": (now + Duration::hours(100)).to_rfc3339()},
            }))
        })
        .collect();
    analyze(&rs, now).unwrap()
}

#[test]
fn the_evaluator_stamps_vendor_and_options_on_what_it_sends() {
    let now = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
    let d = TempDir::new();
    let (a, rec) = (Alerts::load(d.join("a.json")), Rec::default());
    a.set_options(AlertOptions { sounds: true });
    a.evaluate("Grok", &plan(91.0), now, &rec);
    let sent = rec.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].vendor, "Grok");
    assert_eq!(sent[0].options, AlertOptions { sounds: true });
    assert_eq!(sent[0].kind, AlertKind::Warning);
}

#[test]
fn the_three_test_buttons_send_their_style_with_the_current_settings() {
    let d = TempDir::new();
    let settings = Arc::new(Settings::load(d.join("s.json")));
    settings.set_alert_sounds(false);
    let app = App::with_poll_fn(
        Box::new(JsonlStore::new(d.join("u.jsonl"))),
        settings,
        Box::new(|_| Ok(Value::Null)),
    );
    let rec = Arc::new(Rec::default());
    app.set_notifier(rec.clone());
    for kind in [AlertKind::Info, AlertKind::Warning, AlertKind::Limit] {
        app.send_test_notification(kind).unwrap();
    }
    let sent = rec.0.lock().unwrap().clone();
    assert_eq!(
        sent.iter().map(|n| n.kind).collect::<Vec<_>>(),
        [AlertKind::Info, AlertKind::Warning, AlertKind::Limit]
    );
    // test toasts carry the app icon (no vendor) and the current sound setting
    assert!(sent
        .iter()
        .all(|n| n.vendor.is_empty() && !n.options.sounds));
    // the test toast of a warning is built exactly like a real one: an ordinary toast, silent here
    let x = toast_xml_for(&sent[1], None);
    assert!(
        !x.contains("scenario") && x.contains("<audio silent=\"true\"/>"),
        "{x}"
    );
}

#[test]
fn the_process_wide_toast_app_id_is_set_once_even_when_many_notifiers_are_made() {
    // WindowsNotifier::new runs in every App, and tests make hundreds in parallel; the shell's
    // SetCurrentProcessExplicitAppUserModelID raced and corrupted the heap on Windows CI
    // (0xc0000374). The Windows-only call must stay behind a Once (checked in the source).
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/toast.rs")).unwrap();
    let sq: String = src.split_whitespace().collect();
    let once = sq
        .find("INIT.call_once(")
        .expect("a Once guards the app id");
    let call = sq
        .find("unsafe{SetCurrentProcessExplicitAppUserModelID(")
        .expect("the call");
    assert!(once < call, "the call must be inside the Once");
    assert_eq!(
        sq.matches("SetCurrentProcessExplicitAppUserModelID(PCWSTR")
            .count(),
        1
    );
}

// ---- macOS banners (the script builder is pure, so it is tested everywhere) -------------------

#[test]
fn the_macos_script_is_silent_for_info_and_sounds_only_the_important_kinds() {
    let info = applescript_for(&note(AlertKind::Info, "Claude"));
    assert_eq!(
        info,
        "display notification \"Resets in 3h 20m\" with title \"Claude session: 10% left\" subtitle \"Claude\""
    );
    for kind in [AlertKind::Warning, AlertKind::Limit, AlertKind::Predictive] {
        let on = applescript_for(&note(kind, "Grok"));
        assert!(on.ends_with(" sound name \"Glass\""), "{on}");
        let mut quiet = note(kind, "Grok");
        quiet.options = AlertOptions { sounds: false };
        assert!(!applescript_for(&quiet).contains("sound"), "{kind:?}");
    }
    let rec = applescript_for(&note(AlertKind::Recovery, ""));
    assert!(rec.ends_with(" sound name \"default\""), "{rec}");
    assert!(!rec.contains("subtitle"));
}

#[test]
fn the_macos_script_cannot_be_broken_out_of_by_quotes_backslashes_or_line_breaks() {
    assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    assert_eq!(applescript_escape("a\nb\r\tc"), "a b  c");
    let evil = "\" & (do shell script \"touch /tmp/x\") & \"";
    let mut n = Notification::new(
        evil,
        "\\\" end tell\ndo shell script \"id\"",
        AlertKind::Warning,
    );
    n.vendor = evil.into();
    let s = applescript_for(&n);
    assert!(!s.contains('\n'));
    // with the escaped backslashes and quotes removed, only the 8 delimiters are left
    // (body, title, subtitle, sound name) so no text ever closes a literal early
    let unescaped = s.replace("\\\\", "").replace("\\\"", "");
    assert_eq!(unescaped.matches('"').count(), 8, "{s}");
}

// ---- native macOS banners (bundle detection, content and path choice are pure) ----------------

#[test]
fn only_a_path_inside_an_app_bundle_counts_as_a_bundle() {
    for yes in [
        "/Applications/aiTrack.app/Contents/MacOS/aitrack",
        "/Users/me/Downloads/My Apps/aiTrack.app/Contents/MacOS/aitrack",
        "/Volumes/aiTrack/aiTrack.APP/Contents/MacOS/aitrack",
    ] {
        assert!(in_app_bundle(Path::new(yes)), "{yes}");
    }
    for no in [
        "/Users/me/aiTrack/target/release/aitrack",
        "/Users/me/aiTrack.app/target/release/aitrack",
        "/Applications/aiTrack.app/Contents/Resources/aitrack",
        "/Applications/.app/Contents/MacOS/aitrack",
        "aitrack",
        "",
    ] {
        assert!(!in_app_bundle(Path::new(no)), "{no}");
    }
}

#[test]
fn native_content_follows_the_same_sound_rules_as_the_osascript_banner() {
    let info = native_content_for(&note(AlertKind::Info, "Claude"));
    assert_eq!(info.sound, NativeSound::Silent);
    assert_eq!(info.title, "Claude session: 10% left");
    assert_eq!(info.subtitle.as_deref(), Some("Claude"));
    assert_eq!(info.body, "Resets in 3h 20m");
    for kind in [AlertKind::Warning, AlertKind::Limit, AlertKind::Predictive] {
        assert_eq!(
            native_content_for(&note(kind, "Grok")).sound,
            NativeSound::Default
        );
        let mut quiet = note(kind, "Grok");
        quiet.options = AlertOptions { sounds: false };
        assert_eq!(native_content_for(&quiet).sound, NativeSound::Silent);
    }
    let rec = native_content_for(&note(AlertKind::Recovery, ""));
    assert_eq!(rec.sound, NativeSound::Default);
    assert_eq!(rec.subtitle, None);
}

#[test]
fn the_native_path_is_chosen_only_in_a_bundle_and_never_in_tests_or_after_a_failure() {
    let bundle = Path::new("/Applications/aiTrack.app/Contents/MacOS/aitrack");
    let cargo = Path::new("/Users/me/aiTrack/target/release/aitrack");
    assert_eq!(choose_mac_path(false, Some(bundle), false), MacPath::Native);
    assert_eq!(
        choose_mac_path(false, Some(cargo), false),
        MacPath::Osascript
    );
    assert_eq!(choose_mac_path(false, None, false), MacPath::Osascript);
    // permission refused or an API error earlier in this process
    assert_eq!(
        choose_mac_path(false, Some(bundle), true),
        MacPath::Osascript
    );
    // AITRACK_NO_SPAWN: log only, whatever the path
    assert_eq!(choose_mac_path(true, Some(bundle), false), MacPath::LogOnly);
    assert_eq!(choose_mac_path(true, Some(cargo), true), MacPath::LogOnly);
}

// ---- macOS delivery decisions (pure; the native calls themselves are unverified off a Mac) ------

#[test]
fn a_denied_permission_is_respected_and_an_undecided_one_waits() {
    assert_eq!(auth_gate(AuthState::Granted), Ok(()));
    assert_eq!(auth_gate(AuthState::Denied), Err(NativeFailure::Denied));
    assert_eq!(
        failure_action(&NativeFailure::Denied),
        FailureAction::RespectOptOut
    );
    assert_eq!(
        auth_gate(AuthState::Undecided),
        Err(NativeFailure::Undecided)
    );
    assert_eq!(
        failure_action(&NativeFailure::Undecided),
        FailureAction::RetryLater
    );
    assert_eq!(
        failure_action(&NativeFailure::Unavailable("no bundle identifier".into())),
        FailureAction::Osascript
    );
}

// ---------------------------------------------------------------- Linux notify-send arguments

#[test]
fn notify_send_args_are_fixed_options_then_dash_dash_then_title_and_body() {
    let a = notify_send_args(&Notification::new("T", "B", AlertKind::Warning));
    assert_eq!(a[0], "--app-name=aiTrack");
    assert_eq!(a[1], "--urgency=normal");
    assert_eq!(a[2], "--icon=aitrack");
    assert_eq!(&a[a.len() - 3..], ["--", "T", "B"]);
    let low = notify_send_args(&Notification::new("T", "B", AlertKind::Info));
    assert_eq!(low[1], "--urgency=low");
    assert!(low.contains(&"--hint=boolean:suppress-sound:true".to_string()));
}

#[test]
fn notify_send_text_cannot_become_an_option_or_a_second_command() {
    let n = Notification::new(
        "--urgency=critical",
        "-h x; rm -rf ~ $(id) `id` \"q\" 'q' <b>&\nline2\u{0}\u{7}",
        AlertKind::Limit,
    );
    let a = notify_send_args(&n);
    let dd = a.iter().position(|s| s == "--").unwrap();
    assert_eq!(
        a.len(),
        dd + 3,
        "title and body are single entries after --"
    );
    assert_eq!(a[dd + 1], "--urgency=critical"); // stays text
    assert!(a[dd + 2].starts_with("-h x; rm -rf ~ $(id) `id` \"q\" 'q' &lt;b&gt;&amp;\nline2"));
    assert!(!a[dd + 2].contains('\u{0}') && !a[dd + 2].contains('\u{7}'));
    assert!(a[..dd].iter().all(|s| s.starts_with("--")));
}

#[test]
fn notify_send_text_is_cut_to_a_sane_length() {
    let n = Notification::new("t".repeat(10_000), "b".repeat(100_000), AlertKind::Info);
    let a = notify_send_args(&n);
    assert_eq!(a[a.len() - 2].chars().count(), NOTIFY_TITLE_MAX);
    assert_eq!(a[a.len() - 1].chars().count(), NOTIFY_BODY_MAX);
}

#[test]
fn with_spawning_disabled_the_default_notifier_runs_no_program() {
    // AITRACK_NO_SPAWN is set by scripts/test.sh and CI; the notifier then only logs.
    if aitrack::launch::spawning_disabled() {
        let n = default_notifier();
        assert!(n
            .notify(&Notification::new("t", "b", AlertKind::Info))
            .is_ok());
    }
}
