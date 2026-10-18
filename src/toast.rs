// SPDX-License-Identifier: GPL-3.0-or-later
//! The platform notifier: native Windows toasts (Windows.UI.Notifications), `osascript` banners on
//! macOS, `notify-send` banners on Linux (log fallback).
//!
//! An unpackaged desktop app must identify itself to the notification system with an
//! AppUserModelID. We register `aiTrack` under
//! `HKCU\Software\Classes\AppUserModelId\aiTrack` (display name only; no shortcut is created and
//! nothing outside HKCU is touched), the same mechanism PowerShell's BurntToast module uses. If a
//! toast does not appear, set `AITRACK_TOAST_APPID` to an AUMID that is already registered (for
//! example PowerShell's) to tell a Windows-side problem from an aiTrack one. Every failure is
//! logged to `~/.aitrack/notify.log` and never reaches the user as an error dialog or a crash.

use crate::alerts::{AlertKind, Notification, Notifier};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The vendor logos shown on toasts (`examples/make_toast_icons` draws them from the badge painters).
const CLAUDE_ICON: &[u8] = include_bytes!("../assets/toast/claude-192.png");
const GROK_ICON: &[u8] = include_bytes!("../assets/toast/grok-192.png");
/// Alerts without a vendor of their own (the test toasts, other vendors) use the app icon.
const APP_ICON: &[u8] = crate::appicon::TOAST_192;

/// The sound of a warning, limit or predictive toast (Windows' own reminder sound; it does not loop).
pub const REMINDER_SOUND: &str = "ms-winsoundevent:Notification.Reminder";

/// AppUserModelID used for toasts.
pub const AUMID: &str = "aiTrack";

/// The notifier for this platform.
pub fn default_notifier() -> Arc<dyn Notifier> {
    if crate::launch::spawning_disabled() {
        // test runs: log only, never a real banner
        return Arc::new(crate::alerts::LogNotifier);
    }
    #[cfg(windows)]
    {
        Arc::new(windows_toast::WindowsNotifier::new())
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacNotifier)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Arc::new(LinuxNotifier)
    }
}

/// Longest title and body (in characters) handed to `notify-send`.
pub const NOTIFY_TITLE_MAX: usize = 120;
pub const NOTIFY_BODY_MAX: usize = 500;

fn clean_notify_text(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max)
        .collect();
    if s.chars().filter(|c| !c.is_control() || *c == '\n').count() > max {
        out.pop();
        out.push('\u{2026}');
    }
    out
}

/// The argument list (without the program name) for `notify-send` showing `n`. Passed as separate
/// argv entries, never through a shell; title and body come after `--` so a leading dash cannot be
/// taken for an option. Control characters are dropped (a NUL cannot be passed at all), long text is
/// cut, and the body is XML-escaped because notification daemons render a markup subset.
pub fn notify_send_args(n: &Notification) -> Vec<String> {
    let urgency = match n.kind {
        AlertKind::Info | AlertKind::Recovery => "low",
        _ => "normal",
    };
    let mut args = vec![
        "--app-name=aiTrack".to_string(),
        format!("--urgency={urgency}"),
        "--icon=aitrack".to_string(),
    ];
    if !(n.kind.important() && n.options.sounds) {
        args.push("--hint=boolean:suppress-sound:true".to_string());
    }
    let body = clean_notify_text(&n.body, NOTIFY_BODY_MAX)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    args.push("--".to_string());
    args.push(clean_notify_text(&n.title, NOTIFY_TITLE_MAX));
    args.push(body);
    args
}

/// Linux banners through `notify-send` (libnotify-bin; GNOME plays its own sound). Falls back to the
/// log when the program is missing or fails. UNVERIFIED on a real desktop.
#[cfg(not(any(windows, target_os = "macos")))]
struct LinuxNotifier;

#[cfg(not(any(windows, target_os = "macos")))]
impl Notifier for LinuxNotifier {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        let fallback = |why: String| {
            crate::alerts::log_line(&why);
            crate::alerts::LogNotifier.notify(n)
        };
        match std::process::Command::new("notify-send")
            .args(notify_send_args(n))
            .output()
        {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => fallback(format!(
                "notify-send failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => fallback(format!("notify-send could not start: {e}")),
        }
    }
}

/// The system sound of a warning, limit or predictive banner on macOS (a short built-in one).
pub const MAC_REMINDER_SOUND: &str = "Glass";

/// Text for an AppleScript string literal: backslashes and double quotes escaped, line breaks and
/// other control characters turned into spaces so the text cannot end the literal or the statement.
/// Titles and bodies carry vendor names and percentages, so they are treated as untrusted.
pub fn applescript_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The AppleScript statement that shows `n` as a macOS banner: title, vendor as subtitle (when
/// known), body. Info is silent; warning, limit and predictive play [`MAC_REMINDER_SOUND`] with
/// `options.sounds` (silent otherwise); recovery keeps the default sound.
pub fn applescript_for(n: &Notification) -> String {
    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(&n.body),
        applescript_escape(&n.title)
    );
    if !n.vendor.is_empty() {
        script.push_str(&format!(" subtitle \"{}\"", applescript_escape(&n.vendor)));
    }
    match n.kind {
        AlertKind::Info => {}
        k if k.important() && n.options.sounds => {
            script.push_str(&format!(" sound name \"{MAC_REMINDER_SOUND}\""));
        }
        k if k.important() => {}
        _ => script.push_str(" sound name \"default\""),
    }
    script
}

/// True when `exe` lives inside an app bundle: `<name>.app/Contents/MacOS/<binary>`. Only then does
/// macOS know the process by a bundle id, which `UNUserNotificationCenter` requires (without one it
/// raises an Objective-C exception). Pure path logic, so it is tested on every OS.
pub fn in_app_bundle(exe: &Path) -> bool {
    let parts: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.windows(3).any(|w| {
        w[0].len() > ".app".len()
            && w[0].to_ascii_lowercase().ends_with(".app")
            && w[1] == "Contents"
            && w[2] == "MacOS"
    })
}

/// The sound of a native macOS banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSound {
    Silent,
    Default,
}

/// What a native banner shows: title, vendor as subtitle (`None` when unknown), body, sound.
/// Same rules as [`applescript_for`]: Info is silent; warning, limit and predictive play the default
/// sound with `options.sounds` (silent otherwise); recovery keeps the default sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeContent {
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
    pub sound: NativeSound,
}

/// The native banner content for `n`.
pub fn native_content_for(n: &Notification) -> NativeContent {
    let sound = match n.kind {
        AlertKind::Info => NativeSound::Silent,
        k if k.important() && n.options.sounds => NativeSound::Default,
        k if k.important() => NativeSound::Silent,
        _ => NativeSound::Default,
    };
    NativeContent {
        title: n.title.clone(),
        subtitle: (!n.vendor.is_empty()).then(|| n.vendor.clone()),
        body: n.body.clone(),
        sound,
    }
}

/// How a macOS alert is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacPath {
    /// Test runs (`AITRACK_NO_SPAWN`): log only, nothing is posted.
    LogOnly,
    /// `UNUserNotificationCenter`: aiTrack's own icon and name.
    Native,
    /// `osascript` banner (listed under "Script Editor").
    Osascript,
}

/// Choose the delivery path. `native_failed` is set once the native path has been refused or has
/// errored in this process; from then on everything goes through `osascript`.
pub fn choose_mac_path(no_spawn: bool, exe: Option<&Path>, native_failed: bool) -> MacPath {
    if no_spawn {
        MacPath::LogOnly
    } else if !native_failed && exe.is_some_and(in_app_bundle) {
        MacPath::Native
    } else {
        MacPath::Osascript
    }
}

/// What the system said about notification permission for this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    /// No answer yet (the prompt is open or the request is still in flight).
    Undecided,
    Granted,
    /// The user (or a policy) refused; an explicit opt-out.
    Denied,
}

/// Why the native path could not show an alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeFailure {
    /// Permission refused: respect it, do not go around it.
    Denied,
    /// Permission not decided yet: the alert must wait, not be dropped.
    Undecided,
    /// The native API cannot be used or errored (no bundle id, delivery error): osascript may help.
    Unavailable(String),
}

/// What to do after a native failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureAction {
    /// Fall back to `osascript`.
    Osascript,
    /// Show nothing, report `Ok` (the in-app state still shows the alert); log once.
    RespectOptOut,
    /// Report a retry-later error so the alert is offered again at the next poll.
    RetryLater,
}

/// The decision table for a native failure: only "unavailable" falls back to osascript.
pub fn failure_action(f: &NativeFailure) -> FailureAction {
    match f {
        NativeFailure::Denied => FailureAction::RespectOptOut,
        NativeFailure::Undecided => FailureAction::RetryLater,
        NativeFailure::Unavailable(_) => FailureAction::Osascript,
    }
}

/// May a native alert be posted given the permission state?
pub fn auth_gate(state: AuthState) -> Result<(), NativeFailure> {
    match state {
        AuthState::Granted => Ok(()),
        AuthState::Denied => Err(NativeFailure::Denied),
        AuthState::Undecided => Err(NativeFailure::Undecided),
    }
}

/// macOS banners: natively when running from the app bundle, otherwise (and after any native
/// failure) through `/usr/bin/osascript` (arguments are passed directly, no shell).
///
/// The osascript banners are attributed to "Script Editor" and carry its icon, not ours, and the
/// vendor logo cannot be attached; that is why the bundle build posts natively.
#[cfg(target_os = "macos")]
struct MacNotifier;

#[cfg(target_os = "macos")]
impl MacNotifier {
    fn osascript(n: &Notification) -> Result<(), String> {
        let out = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(applescript_for(n))
            .output()
            .map_err(|e| {
                let why = format!("osascript could not start: {e}");
                crate::alerts::log_line(&why);
                why
            })?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let why = format!("osascript failed ({}): {}", out.status, err.trim());
        crate::alerts::log_line(&why);
        Err(why)
    }
}

#[cfg(target_os = "macos")]
impl Notifier for MacNotifier {
    fn notify(&self, n: &Notification) -> Result<(), String> {
        let exe = std::env::current_exe().ok();
        let path = choose_mac_path(
            crate::launch::spawning_disabled(),
            exe.as_deref(),
            mac_native::failed(),
        );
        match path {
            MacPath::LogOnly => Ok(()),
            MacPath::Osascript => Self::osascript(n),
            MacPath::Native => match mac_native::post(n) {
                Ok(()) => Ok(()),
                Err(f) => match failure_action(&f) {
                    FailureAction::RespectOptOut => {
                        mac_native::log_denied_once();
                        Ok(())
                    }
                    FailureAction::RetryLater => Err(crate::alerts::retry_later(
                        "notification permission not decided yet",
                    )),
                    FailureAction::Osascript => {
                        mac_native::mark_failed();
                        crate::alerts::log_line(&format!(
                            "native notification unavailable ({f:?}); using osascript"
                        ));
                        Self::osascript(n)
                    }
                },
            },
        }
    }
}

/// `UNUserNotificationCenter` through the objc2 bindings. UNVERIFIED on a Mac at the time of
/// writing (written from the crate sources; there is no SDK to compile against on Linux).
#[cfg(target_os = "macos")]
mod mac_native {
    use super::{auth_gate, native_content_for, AuthState, NativeFailure, NativeSound};
    use crate::alerts::{log_line, Notification};
    use block2::RcBlock;
    use objc2::runtime::{Bool, ProtocolObject};
    use objc2::{define_class, msg_send, ClassType};
    use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationSound,
        UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
    use std::sync::{mpsc, Once};
    use std::time::Duration;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and this class has no ivars and no Drop.
        #[unsafe(super(NSObject))]
        struct AiTrackNotificationDelegate;

        unsafe impl NSObjectProtocol for AiTrackNotificationDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for AiTrackNotificationDelegate {
            // macOS hides banners of the frontmost app unless the delegate asks for them; the
            // test-alert buttons are pressed inside aiTrack's own window.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                completion_handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }
        }
    );

    /// Set when the native API failed (not on a refusal): later alerts use osascript.
    static FAILED: AtomicBool = AtomicBool::new(false);
    static AUTH: Once = Once::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    /// 0 undecided, 1 granted, 2 denied; set by the authorization completion handler.
    static AUTH_STATE: AtomicU8 = AtomicU8::new(0);
    static DENIED_LOGGED: AtomicBool = AtomicBool::new(false);
    /// How long an alert waits for the permission answer / the delivery result before giving up.
    const WAIT: Duration = Duration::from_secs(3);

    fn auth_state() -> AuthState {
        match AUTH_STATE.load(Ordering::SeqCst) {
            1 => AuthState::Granted,
            2 => AuthState::Denied,
            _ => AuthState::Undecided,
        }
    }

    /// Say once that alerts are being withheld because the user turned notifications off.
    pub fn log_denied_once() {
        if !DENIED_LOGGED.swap(true, Ordering::SeqCst) {
            log_line("notifications are turned off for aiTrack in System Settings; alerts are shown in the app only");
        }
    }

    pub fn failed() -> bool {
        FAILED.load(Ordering::SeqCst)
    }

    pub fn mark_failed() {
        FAILED.store(true, Ordering::SeqCst);
    }

    /// Ask for permission once per process. The completion handler runs later on a system queue
    /// (the answer is waited for in [`post`]); a refusal is recorded in [`AUTH_STATE`], not in [`FAILED`].
    fn request_authorization_once(center: &UNUserNotificationCenter) {
        AUTH.call_once(|| {
            // The delegate goes in before the permission request. The centre only holds it weakly,
            // so the retained object is leaked on purpose: it lives for the rest of the process.
            let delegate: objc2::rc::Retained<AiTrackNotificationDelegate> =
                // SAFETY: `new` is NSObject's plain allocate-and-init; the class has no ivars.
                unsafe { msg_send![AiTrackNotificationDelegate::class(), new] };
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            std::mem::forget(delegate);
            let handler = RcBlock::new(|granted: Bool, error: *mut NSError| {
                if granted.as_bool() {
                    AUTH_STATE.store(1, Ordering::SeqCst);
                    log_line("notification permission granted");
                    return;
                }
                AUTH_STATE.store(2, Ordering::SeqCst);
                // SAFETY: the system passes either null or a valid NSError for the duration of
                // this call; it is only read here.
                let reason = unsafe { error.as_ref() }
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "permission denied".to_string());
                log_line(&format!("notification permission not granted ({reason})"));
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &handler,
            );
        });
    }

    /// Post `n` and wait (a few seconds at most) for the permission answer and the delivery result.
    /// `Ok` only when the system accepted the alert; otherwise the failure says what to do next.
    pub fn post(n: &Notification) -> Result<(), NativeFailure> {
        // Without a bundle identifier the notification centre raises an Objective-C exception
        // (which would abort the process), so check first.
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return Err(NativeFailure::Unavailable("no bundle identifier".into()));
        }
        let c = native_content_for(n);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        request_authorization_once(&center);
        let t0 = std::time::Instant::now();
        while auth_state() == AuthState::Undecided && t0.elapsed() < WAIT {
            std::thread::sleep(Duration::from_millis(50));
        }
        auth_gate(auth_state())?;
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&c.title));
        if let Some(sub) = &c.subtitle {
            content.setSubtitle(&NSString::from_str(sub));
        }
        content.setBody(&NSString::from_str(&c.body));
        if c.sound == NativeSound::Default {
            content.setSound(Some(&UNNotificationSound::defaultSound()));
        }
        let id = format!(
            "aitrack-{}-{}",
            crate::util::now_iso(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        // No trigger: deliver immediately.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&id),
            &content,
            None,
        );
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let done = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: null or a valid NSError for the duration of the call; only read.
            let why = unsafe { error.as_ref() }.map(|e| e.to_string());
            if let Some(w) = &why {
                log_line(&format!("native notification not delivered: {w}"));
            }
            let _ = tx.send(why);
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
        match rx.recv_timeout(WAIT) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => Err(NativeFailure::Unavailable(why)),
            Err(_) => Err(NativeFailure::Unavailable("no delivery result".into())),
        }
    }
}

/// Escape text for XML element content.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape text for an XML attribute value.
pub fn attr_escape(s: &str) -> String {
    xml_escape(s).replace('"', "&quot;").replace('\'', "&apos;")
}

/// `file:///C:/Users/me/My%20Dir/icon.png` for an absolute path: backslashes become slashes and
/// everything outside the URI-safe set (spaces, non-ASCII, `%`, ...) is percent-encoded as UTF-8.
pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_start_matches('/');
    let mut out = String::from("file:///");
    for (i, seg) in text.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        // keep a drive letter's colon (`C:`)
        let is_drive = i == 0
            && seg.len() == 2
            && seg.ends_with(':')
            && seg.as_bytes()[0].is_ascii_alphabetic();
        for b in seg.bytes() {
            if is_drive && b == b':' || b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// The logo for a vendor's alerts: Claude's or Grok's own, the app icon for everything else.
pub fn icon_bytes(vendor: &str) -> Option<&'static [u8]> {
    Some(match vendor {
        "Claude" => CLAUDE_ICON,
        "Grok" => GROK_ICON,
        _ => APP_ICON,
    })
}

/// File name of a vendor's logo in the icon folder.
pub fn icon_file_name(vendor: &str) -> &'static str {
    match vendor {
        "Claude" => "claude.png",
        "Grok" => "grok.png",
        _ => "aitrack.png",
    }
}

/// Write the vendor's logo to `<dir>/<vendor>.png` (when missing or different from the embedded
/// bytes) and return its path.
pub fn ensure_icon(dir: &Path, vendor: &str) -> Result<Option<PathBuf>, String> {
    let Some(bytes) = icon_bytes(vendor) else {
        return Ok(None);
    };
    let path = dir.join(icon_file_name(vendor));
    if std::fs::read(&path).is_ok_and(|have| have == bytes) {
        return Ok(Some(path));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(path))
}

/// The toast XML for `n`, with the vendor logo when `icon_uri` is given.
///
/// * Every alert is an ordinary toast that goes away on its own (no scenario, no buttons).
/// * Warning, limit and predictive alerts play [`REMINDER_SOUND`] with `options.sounds`, otherwise
///   they are silent.
/// * Info alerts are silent short toasts; recovery notices keep the default sound.
pub fn toast_xml_for(n: &Notification, icon_uri: Option<&str>) -> String {
    let image = icon_uri
        .map(|u| {
            format!(
                "<image placement=\"appLogoOverride\" hint-crop=\"none\" src=\"{}\"/>",
                attr_escape(u)
            )
        })
        .unwrap_or_default();
    let audio = match n.kind {
        AlertKind::Info => "<audio silent=\"true\"/>".to_owned(),
        k if k.important() && n.options.sounds => format!("<audio src=\"{REMINDER_SOUND}\"/>"),
        k if k.important() => "<audio silent=\"true\"/>".to_owned(),
        _ => String::new(),
    };
    format!(
        "<toast><visual><binding template=\"ToastGeneric\">{image}<text>{}</text><text>{}</text></binding></visual>{audio}</toast>",
        xml_escape(&n.title),
        xml_escape(&n.body)
    )
}

/// The XML to show for `n`, writing its logo into `icon_dir` first. If the logo cannot be written
/// the toast is text-only and the reason is returned for the log; this never fails.
pub fn build_toast(n: &Notification, icon_dir: &Path) -> (String, Option<String>) {
    match ensure_icon(icon_dir, &n.vendor) {
        Ok(Some(path)) => (toast_xml_for(n, Some(&file_uri(&path))), None),
        Ok(None) => (toast_xml_for(n, None), None),
        Err(e) => (
            toast_xml_for(n, None),
            Some(format!("toast logo not available, showing text only: {e}")),
        ),
    }
}

/// The toast XML for a title and body.
pub fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )
}

#[cfg(windows)]
mod windows_toast {
    use super::{build_toast, toast_xml_for, AUMID};
    use crate::alerts::{log_line, Notification, Notifier};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    pub struct WindowsNotifier {
        aumid: String,
    }

    impl WindowsNotifier {
        pub fn new() -> Self {
            let custom = std::env::var("AITRACK_TOAST_APPID")
                .ok()
                .filter(|v| !v.is_empty());
            let aumid = custom.clone().unwrap_or_else(|| AUMID.to_string());
            // The app id is process-wide state of the shell. Every App creates a notifier (tests
            // create hundreds, in parallel), and SetCurrentProcessExplicitAppUserModelID is not safe
            // to call from several threads at once: do it once per process.
            static INIT: std::sync::Once = std::sync::Once::new();
            let id = aumid.clone();
            INIT.call_once(move || {
                if custom.is_none() {
                    if let Err(e) = register_aumid(&id) {
                        log_line(&format!("could not register the toast app id: {e}"));
                    }
                }
                let h = HSTRING::from(id.as_str());
                // SAFETY: `h` is a valid NUL-terminated wide string that outlives the call.
                if let Err(e) =
                    unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(h.as_ptr())) }
                {
                    log_line(&format!(
                        "SetCurrentProcessExplicitAppUserModelID failed: {e}"
                    ));
                }
            });
            Self { aumid }
        }
    }

    /// `HKCU\Software\Classes\AppUserModelId\<id>` with a display name, so Windows accepts toasts
    /// from this unpackaged executable and labels them "aiTrack".
    fn register_aumid(id: &str) -> Result<(), String> {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        use winreg::RegKey;
        let path = format!(r"Software\Classes\AppUserModelId\{id}");
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey_with_flags(path, KEY_SET_VALUE)
            .map_err(|e| e.to_string())?;
        key.set_value("DisplayName", &"aiTrack")
            .map_err(|e| e.to_string())?;
        key.set_value("IconBackgroundColor", &"FF1C1C1E")
            .map_err(|e| e.to_string())?;
        key.set_value("ShowInSettings", &1u32)
            .map_err(|e| e.to_string())
    }

    fn show(aumid: &str, xml: &str) -> windows::core::Result<()> {
        // WinRT needs COM on the calling thread; this runs on a short-lived thread of our own.
        // SAFETY: plain COM initialisation; an "already initialised" error is harmless.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))?.Show(&toast)
    }

    impl Notifier for WindowsNotifier {
        fn notify(&self, n: &Notification) -> Result<(), String> {
            let icon_dir = crate::util::data_dir().join("toast-icons");
            let (xml, why) = build_toast(n, &icon_dir);
            if let Some(why) = why {
                log_line(&why);
            }
            let aumid = self.aumid.clone();
            let plain = toast_xml_for(n, None);
            let had_icon = xml != plain;
            let result = std::thread::Builder::new()
                .name("aitrack-toast".into())
                .spawn(move || {
                    match show(&aumid, &xml) {
                        Ok(()) => Ok(()),
                        // a logo Windows will not take must not cost the toast
                        Err(e) if had_icon => {
                            log_line(&format!("toast with logo failed ({e}); showing text only"));
                            show(&aumid, &plain).map_err(|e| e.to_string())
                        }
                        Err(e) => Err(e.to_string()),
                    }
                })
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|_| "toast thread panicked".to_string())?;
            result
        }
    }
}
