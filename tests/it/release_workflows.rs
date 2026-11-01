// SPDX-License-Identifier: GPL-3.0-or-later
//! Release workflow hygiene: pinned actions, the hash-pinned pip install, the same-version MSI upgrade, and
//! the release-notes section that a Replace rerun swaps without duplicating text.
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap()
}

const WORKFLOWS: [&str; 3] = [
    ".github/workflows/release-linux.yml",
    ".github/workflows/release-macos.yml",
    ".github/workflows/release-msi.yml",
];

#[test]
fn every_action_in_the_release_workflows_is_pinned_to_a_full_commit_sha() {
    for wf in WORKFLOWS {
        let text = read(wf);
        let mut seen = 0;
        for line in text.lines().filter(|l| l.trim_start().contains("uses:")) {
            let spec = line.split("uses:").nth(1).unwrap().trim();
            let (target, comment) = spec.split_once('#').unwrap_or((spec, ""));
            let sha = target.trim().split('@').nth(1).unwrap_or("");
            assert!(
                sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{wf}: not pinned to a commit: {line}"
            );
            assert!(
                comment.trim().starts_with('v'),
                "{wf}: the tag is missing as a trailing comment: {line}"
            );
            seen += 1;
        }
        assert!(seen > 0, "{wf}");
    }
}

#[test]
fn the_mac_pdf_script_installs_markdown_by_hash_and_wheel_only() {
    let sh = read("scripts/readme-pdf-mac.sh");
    assert!(sh.contains("--require-hashes") && sh.contains("--only-binary :all:"));
    assert!(sh.contains("markdown==3.7 --hash=sha256:"));
}

#[test]
fn the_msi_allows_a_same_version_upgrade_and_still_refuses_downgrades() {
    let wxs = read("installer/Package.wxs");
    let line = wxs
        .lines()
        .find(|l| l.contains("<MajorUpgrade"))
        .expect("MajorUpgrade");
    assert!(line.contains("AllowSameVersionUpgrades=\"yes\""), "{line}");
    assert!(line.contains("DowngradeErrorMessage="), "{line}");
    assert!(read("installer/aiTrack.Installer.wixproj").contains("ICE61"));
}

/// The publish jobs that run this shell are all on ubuntu-latest, and a Windows checkout has CRLF line
/// ends that bash rejects, so these run on Unix only, like the other bash tests.
#[cfg(unix)]
mod shell {
    use super::read;
    use std::process::{Command, Stdio};

    /// The text between `# strip_section begin` and `# strip_section end` in a workflow, de-indented.
    fn strip_block(wf: &str) -> String {
        let text = read(wf);
        let begin =
            text.find("# strip_section begin").expect("begin") + "# strip_section begin".len();
        let end = text.find("# strip_section end").expect("end");
        text[begin..end].to_string()
    }

    fn run_strip(wf: &str, body: &str) -> String {
        let script = format!("{}\nstrip_section\n", strip_block(wf));
        let mut child = Command::new("bash")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    }

    /// One section as a workflow writes it: prose, the SHA256 block, all inside its begin and end comments.
    fn section_of(key: &str, prose: &str, marker: &str, hash: &str) -> String {
        format!(
            "<!-- aitrack-notes:{key}:begin -->\n{prose}\nmore prose\n\n{marker}\n```\n{hash}  a.zip\n```\n<!-- aitrack-notes:{key}:end -->\n"
        )
    }

    const OSES: [(&str, &str, &str, &str); 3] = [
        (
            ".github/workflows/release-linux.yml",
            "linux",
            "Linux: Ubuntu 22.04 and newer",
            "Linux SHA256:",
        ),
        (
            ".github/workflows/release-macos.yml",
            "macos",
            "macOS 12 or newer, Apple Silicon",
            "macOS SHA256:",
        ),
        (
            ".github/workflows/release-msi.yml",
            "windows",
            "Windows 10/11, 64-bit.",
            "Windows SHA256:",
        ),
    ];

    #[test]
    fn a_notes_rerun_leaves_one_section_per_os_and_the_other_systems_alone() {
        let head = "aiTrack 0.5.17\n\nIntro text.\n\n";
        for (wf, key, prose, marker) in OSES {
            // the workflow itself writes the markers around its section
            let text = read(wf);
            assert!(
                text.contains(&format!("<!-- aitrack-notes:{key}:begin -->")),
                "{wf}"
            );
            assert!(
                text.contains(&format!("<!-- aitrack-notes:{key}:end -->")),
                "{wf}"
            );
            let mut body = String::from(head);
            for (_, k, p, m) in OSES {
                body.push_str(&section_of(k, p, m, &"a".repeat(64)));
                body.push('\n');
            }
            // a second run: strip own section, append a new one (as add_section does)
            let stripped = run_strip(wf, &body);
            let rerun = format!(
                "{}\n{}",
                stripped.trim_end(),
                section_of(key, prose, marker, &"b".repeat(64))
            );
            assert_eq!(rerun.matches(marker).count(), 1, "{wf}\n{rerun}");
            assert_eq!(rerun.matches(prose).count(), 1, "{wf}\n{rerun}");
            assert!(rerun.contains(&"b".repeat(64)), "{wf}");
            for (_, k, p, m) in OSES.iter().filter(|o| o.1 != key) {
                assert_eq!(rerun.matches(m).count(), 1, "{wf} {k}");
                assert_eq!(rerun.matches(p).count(), 1, "{wf} {k}");
            }
            assert!(rerun.starts_with(head), "{wf}");
            // and it is stable: stripping the rerun result removes exactly the new section
            let again = run_strip(wf, &rerun);
            assert_eq!(again.matches(marker).count(), 0, "{wf}");
            assert!(!again.contains(&"b".repeat(64)), "{wf}");
        }
    }

    #[test]
    fn a_section_from_before_the_markers_is_cut_from_its_prose_to_the_closing_fence() {
        for (wf, key, prose, marker) in OSES {
            let old = format!(
                "{prose} old prose\nsecond line\n\n{marker}\n```\n{}  a.zip\n```\n",
                "c".repeat(64)
            );
            let other = "Other OS text.\n\nOther SHA256:\n```\ndeadbeef  x\n```\n";
            let body = format!("aiTrack 0.5\n\n{old}\n{other}");
            let out = run_strip(wf, &body);
            assert!(
                !out.contains(marker) && !out.contains(prose),
                "{wf} {key}\n{out}"
            );
            assert!(out.contains(other), "{wf}\n{out}");
            assert!(out.starts_with("aiTrack 0.5\n"), "{wf}");
        }
    }
}

#[test]
fn each_workflow_writes_both_notes_markers_into_its_own_notes_section() {
    for (wf, key, notes) in [
        ("release-linux.yml", "linux", "linux-notes.md"),
        ("release-macos.yml", "macos", "mac-notes.md"),
        ("release-msi.yml", "windows", "win-notes.md"),
    ] {
        let text = read(&format!(".github/workflows/{wf}"));
        let begin = format!("echo \"<!-- aitrack-notes:{key}:begin -->\"");
        let end = format!("echo \"<!-- aitrack-notes:{key}:end -->\"");
        assert_eq!(text.matches(&begin).count(), 1, "{wf}: one begin marker");
        assert_eq!(text.matches(&end).count(), 1, "{wf}: one end marker");
        let b = text.find(&begin).unwrap();
        let e = text.find(&end).unwrap();
        assert!(b < e, "{wf}: begin before end");
        let section = &text[b..e];
        // Both markers belong to the block that writes the notes, never to step outputs.
        assert!(
            !section.contains("GITHUB_OUTPUT"),
            "{wf}: begin marker outside the notes block"
        );
        assert!(
            !section.contains("\n      - name:"),
            "{wf}: markers span more than one step"
        );
        let after = &text[e..];
        let close = after.find("} >").expect("notes block closes");
        assert!(
            after[close..].starts_with(&format!("}} > {notes}")),
            "{wf}: end marker writes to {notes}"
        );
    }
}
