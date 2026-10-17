// SPDX-License-Identifier: GPL-3.0-or-later
//! The macOS bundle pieces that can be checked on any Unix: the Info.plist template, the .icns file and the
//! shell scripts. The scripts themselves only run on a Mac (hdiutil, codesign); none of that is tested here.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn plist(version: Option<&str>) -> String {
    let mut cmd = Command::new("bash");
    cmd.arg(root().join("scripts/make-app.sh"))
        .arg("--print-plist");
    if let Some(v) = version {
        cmd.arg(v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The string value that follows `<key>name</key>`.
fn value(plist: &str, key: &str) -> String {
    let after = plist
        .split(&format!("<key>{key}</key>"))
        .nth(1)
        .unwrap_or_else(|| panic!("key {key} missing"));
    let line = after.lines().find(|l| !l.trim().is_empty()).unwrap().trim();
    if line == "<true/>" {
        return "true".into();
    }
    line.strip_prefix("<string>")
        .and_then(|s| s.strip_suffix("</string>"))
        .unwrap_or_else(|| panic!("key {key} has no string value: {line}"))
        .to_owned()
}

#[test]
fn info_plist_has_the_keys_the_app_needs() {
    let p = plist(Some("1.2.3"));
    assert_eq!(value(&p, "CFBundleIdentifier"), "com.sirgene.aitrack");
    assert_eq!(value(&p, "CFBundleName"), "aiTrack");
    assert_eq!(value(&p, "CFBundleDisplayName"), "aiTrack");
    assert_eq!(value(&p, "CFBundleExecutable"), "aitrack");
    assert_eq!(value(&p, "CFBundlePackageType"), "APPL");
    assert_eq!(value(&p, "CFBundleIconFile"), "aitrack");
    assert_eq!(value(&p, "LSMinimumSystemVersion"), "12.0");
    assert_eq!(value(&p, "NSHighResolutionCapable"), "true");
    assert_eq!(
        value(&p, "LSApplicationCategoryType"),
        "public.app-category.developer-tools"
    );
    // a normal Dock app, not a menu-bar agent
    assert!(!p.contains("LSUIElement"));
    assert!(p.trim_end().ends_with("</plist>"));
}

#[test]
fn bundle_version_drops_the_beta_suffix_but_the_short_version_keeps_it() {
    let p = plist(Some("0.3.1-beta"));
    assert_eq!(value(&p, "CFBundleShortVersionString"), "0.3.1-beta");
    assert_eq!(value(&p, "CFBundleVersion"), "0.3.1");
    let p = plist(Some("1.0.0"));
    assert_eq!(value(&p, "CFBundleShortVersionString"), "1.0.0");
    assert_eq!(value(&p, "CFBundleVersion"), "1.0.0");
}

#[test]
fn info_plist_uses_the_cargo_version_by_default() {
    let p = plist(None);
    let short = value(&p, "CFBundleShortVersionString");
    assert_eq!(short, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        value(&p, "CFBundleVersion"),
        env!("CARGO_PKG_VERSION").split('-').next().unwrap()
    );
}

#[test]
fn icns_has_the_magic_and_a_matching_length_and_whole_chunks() {
    let data = std::fs::read(root().join("assets/icon/aitrack.icns")).unwrap();
    assert_eq!(&data[..4], b"icns");
    let total = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    assert_eq!(total, data.len(), "length header must equal the file size");
    // walk the chunks: each is a 4-byte type, a big-endian length (including the 8-byte header), the data
    let mut pos = 8;
    let mut kinds = Vec::new();
    while pos < data.len() {
        let kind = std::str::from_utf8(&data[pos..pos + 4]).unwrap().to_owned();
        let len = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        assert!(
            len > 8 && pos + len <= data.len(),
            "chunk {kind} overruns the file"
        );
        assert_eq!(
            &data[pos + 8..pos + 16],
            b"\x89PNG\r\n\x1a\n",
            "{kind} holds a PNG"
        );
        kinds.push(kind);
        pos += len;
    }
    assert_eq!(pos, data.len());
    for want in ["ic07", "ic08", "ic09"] {
        assert!(kinds.iter().any(|k| k == want), "{want} chunk missing");
    }
}

#[test]
fn scripts_pass_shellcheck_when_it_is_installed() {
    if Command::new("shellcheck")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("shellcheck is not installed; skipped");
        return;
    }
    for s in [
        "make-app.sh",
        "make-dmg.sh",
        "make-icns.sh",
        "mac-contents-check.sh",
        "readme-pdf-mac.sh",
        "sign-notarize-mac.sh",
    ] {
        let out = Command::new("shellcheck")
            .arg(root().join("scripts").join(s))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{s}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn scripts_are_valid_bash() {
    for s in [
        "make-app.sh",
        "make-dmg.sh",
        "make-icns.sh",
        "mac-contents-check.sh",
        "readme-pdf-mac.sh",
        "sign-notarize-mac.sh",
    ] {
        let out = Command::new("bash")
            .arg("-n")
            .arg(root().join("scripts").join(s))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{s}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

const SECRET_VARS: [&str; 8] = [
    "APPLE_CERT_P12_BASE64",
    "APPLE_CERT_PASSWORD",
    "APPLE_API_KEY_P8_BASE64",
    "APPLE_API_KEY_ID",
    "APPLE_API_ISSUER_ID",
    "APPLE_ID",
    "APPLE_APP_PASSWORD",
    "APPLE_TEAM_ID",
];

fn mode_with(vars: &[&str]) -> String {
    let mut cmd = Command::new("bash");
    cmd.arg(root().join("scripts/sign-notarize-mac.sh"))
        .arg("--print-mode")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    for v in vars {
        cmd.env(v, "x");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn signing_mode_follows_which_secrets_are_set() {
    assert_eq!(mode_with(&[]), "adhoc");
    // an incomplete certificate, or notary credentials alone, never switch signing on
    assert_eq!(mode_with(&["APPLE_CERT_P12_BASE64"]), "adhoc");
    assert_eq!(
        mode_with(&[
            "APPLE_API_KEY_P8_BASE64",
            "APPLE_API_KEY_ID",
            "APPLE_API_ISSUER_ID"
        ]),
        "adhoc"
    );
    let cert = ["APPLE_CERT_P12_BASE64", "APPLE_CERT_PASSWORD"];
    assert_eq!(mode_with(&cert), "signed");
    let api = [
        "APPLE_API_KEY_P8_BASE64",
        "APPLE_API_KEY_ID",
        "APPLE_API_ISSUER_ID",
    ];
    let apple_id = ["APPLE_ID", "APPLE_APP_PASSWORD", "APPLE_TEAM_ID"];
    assert_eq!(
        mode_with(&[&cert[..], &apple_id[..]].concat()),
        "notarized-apple-id"
    );
    assert_eq!(
        mode_with(&[&cert[..], &api[..]].concat()),
        "notarized-api-key"
    );
    // API key wins when both are present
    assert_eq!(
        mode_with(&[&cert[..], &api[..], &apple_id[..]].concat()),
        "notarized-api-key"
    );
    // a partial set of notary credentials falls back to signed-only
    assert_eq!(mode_with(&[&cert[..], &apple_id[..2]].concat()), "signed");
}

#[test]
fn signing_script_never_prints_a_secret_and_never_traces() {
    let text = std::fs::read_to_string(root().join("scripts/sign-notarize-mac.sh")).unwrap();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let prints = line.contains("echo") || line.contains("printf");
        if prints && !line.contains("base64 --decode") {
            for v in SECRET_VARS {
                assert!(!line.contains(v), "secret {v} may be printed: {line}");
            }
        }
        assert!(
            !line.contains("set -x") && !line.contains("set -o xtrace"),
            "{line}"
        );
    }
    // the workflow gives secrets to steps only outside pull requests
    let wf = std::fs::read_to_string(root().join(".github/workflows/release-macos.yml")).unwrap();
    for line in wf.lines().filter(|l| l.contains("secrets.APPLE_")) {
        assert!(
            line.contains("github.event_name != 'pull_request'"),
            "{line}"
        );
    }
}

#[test]
fn identity_parser_picks_the_developer_id_application_hash() {
    let input = "  1) AAAA567890ABCDEF1234567890ABCDEF12345678 \"Apple Development: A (X)\"\n  \
        2) 0123456789ABCDEF0123456789ABCDEF01234567 \"Developer ID Application: Gene (ABCDE12345)\"\n     2 valid identities found\n";
    let run = |input: &str| {
        let mut child = Command::new("bash")
            .arg(root().join("scripts/sign-notarize-mac.sh"))
            .arg("--parse-identity")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run(input);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "0123456789ABCDEF0123456789ABCDEF01234567"
    );
    assert!(!run("  0 valid identities found\n").status.success());
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn readme_pack_scripts_exist_and_the_pdf_is_never_committed() {
    for s in ["readme-pdf-mac.sh", "mac-contents-check.sh"] {
        assert!(root().join("scripts").join(s).is_file(), "{s} missing");
    }
    let ignore = read(".gitignore");
    assert!(
        ignore
            .lines()
            .any(|l| l.trim() == "dist/" || l.trim() == "*.pdf"),
        "dist/ (where README.pdf is built) must be git-ignored"
    );
    let tracked = Command::new("git")
        .args(["ls-files", "*.pdf"])
        .current_dir(root())
        .output();
    if let Ok(o) = tracked {
        if o.status.success() {
            assert!(
                String::from_utf8_lossy(&o.stdout).trim().is_empty(),
                "a PDF is tracked"
            );
        }
    }
}

#[test]
fn dmg_and_zip_scripts_add_the_extras_beside_the_app_and_check_them() {
    let dmg = read("scripts/make-dmg.sh");
    for f in ["README.md", "README.pdf", "LICENSE", "docs/img"] {
        assert!(dmg.contains(f), "make-dmg.sh does not mention {f}");
    }
    // the extras go into the staging folders, never into the .app
    assert!(!dmg.contains("$app/README") && !dmg.contains("$app/docs"));
    assert!(dmg.contains("mac-contents-check.sh\" dmg"));
    assert!(dmg.contains("mac-contents-check.sh\" zip"));
    // order: dmg check after signing, zip made from a staging folder after the dmg
    let sign = dmg.find("sign-notarize-mac.sh\" dmg").unwrap();
    let check = dmg.find("mac-contents-check.sh\" dmg").unwrap();
    let zip = dmg.find("ditto -c -k \"$zstage\"").unwrap();
    assert!(sign < check && check < zip);
}

#[test]
fn workflow_makes_the_pdf_before_packing_and_checks_the_files_after() {
    let wf = read(".github/workflows/release-macos.yml");
    let pdf = wf.find("./scripts/readme-pdf-mac.sh").expect("PDF step");
    let pack = wf.find("./scripts/make-dmg.sh").expect("pack step");
    let check = wf.find("mac-contents-check.sh dmg").expect("dmg check");
    let check_zip = wf.find("mac-contents-check.sh zip").expect("zip check");
    let keep = wf.find("name: Keep the dmg").unwrap();
    assert!(pdf < pack && pack < check && check < keep && check_zip < keep);
    // valid YAML, when python's yaml module exists
    let ok = Command::new("python3")
        .args([
            "-I",
            "-c",
            "import sys,yaml;yaml.safe_load(open(sys.argv[1]))",
        ])
        .arg(root().join(".github/workflows/release-macos.yml"))
        .output();
    if let Ok(o) = ok {
        if !o.status.success() && !String::from_utf8_lossy(&o.stderr).contains("No module") {
            panic!("{}", String::from_utf8_lossy(&o.stderr));
        }
    }
}

/// Builds a zip in `dir` from the given file names (contents "x" or a 100-byte %PDF- for README.pdf).
fn fixture_zip(dir: &Path, files: &[&str]) -> Option<PathBuf> {
    let src = dir.join("src");
    for f in files {
        let p = src.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let body = if f.ends_with("README.pdf") {
            format!("%PDF-{}", "x".repeat(100))
        } else {
            "x".into()
        };
        std::fs::write(&p, body).unwrap();
    }
    let zip = dir.join("t.zip");
    let ok = Command::new("zip")
        .args(["-q", "-r"])
        .arg(&zip)
        .arg(".")
        .current_dir(&src)
        .status()
        .ok()?;
    ok.success().then_some(zip)
}

fn run_zip_check(zip: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(root().join("scripts/mac-contents-check.sh"))
        .arg("zip")
        .arg(zip)
        .env("AITRACK_MIN_PDF_BYTES", "50")
        .output()
        .unwrap()
}

#[test]
fn contents_check_passes_a_complete_zip_and_names_what_is_missing() {
    let have_zip = Command::new("zip").arg("-v").output().is_ok()
        && Command::new("unzip").arg("-v").output().is_ok();
    if !have_zip {
        eprintln!("zip/unzip not installed; skipped");
        return;
    }
    let full = [
        "aiTrack.app/Contents/MacOS/aitrack",
        "README.md",
        "README.pdf",
        "LICENSE",
        "docs/img/a.png",
    ];
    let tmp = std::env::temp_dir().join(format!("aitrack-zipcheck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let good = tmp.join("good");
    std::fs::create_dir_all(&good).unwrap();
    let zip = fixture_zip(&good, &full).expect("zip failed");
    let out = run_zip_check(&zip);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for (i, missing) in full.iter().enumerate() {
        let dir = tmp.join(format!("m{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let rest: Vec<&str> = full.iter().copied().filter(|f| f != missing).collect();
        let zip = fixture_zip(&dir, &rest).unwrap();
        let out = run_zip_check(&zip);
        assert!(!out.status.success(), "passed without {missing}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("MISSING"));
    }

    // an extra placed inside the .app breaks the signature: refused
    let dir = tmp.join("inside");
    std::fs::create_dir_all(&dir).unwrap();
    let mut inside = full.to_vec();
    inside.push("aiTrack.app/Contents/Resources/README.md");
    let zip = fixture_zip(&dir, &inside).unwrap();
    let out = run_zip_check(&zip);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("WRONG PLACE"));

    // a README.pdf that is too small is refused
    let out = Command::new("bash")
        .arg(root().join("scripts/mac-contents-check.sh"))
        .arg("zip")
        .arg(good.join("t.zip"))
        .env("AITRACK_MIN_PDF_BYTES", "5000")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&tmp);
}
