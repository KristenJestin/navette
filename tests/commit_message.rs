//! `tools/commit-message.sh`: a pull request title is judged as the squash subject it becomes,
//! with ` (#<number>)` appended.

use std::process::{Command, Output};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/commit-message.sh");

fn check_title(title: &str, pull_request: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("title");
    std::fs::write(&file, format!("{title}\n")).unwrap();
    Command::new("bash")
        .arg(SCRIPT)
        .args(["--pull-request", pull_request])
        .arg(&file)
        .output()
        .unwrap()
}

/// A valid title of exactly `length` characters.
fn title(length: usize) -> String {
    let prefix = "fix(repo): a";
    format!("{prefix}{}", "b".repeat(length - prefix.len()))
}

#[test]
fn a_67_character_title_on_pull_request_95_is_refused() {
    let output = check_title(&title(67), "95");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("73 characters once merged as"),
        "the message gives the merged subject's length: {stderr}"
    );
}

#[test]
fn a_66_character_title_on_pull_request_95_passes() {
    let output = check_title(&title(66), "95");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs `--range` over a throwaway repository holding one commit with `subject`.
fn check_range(subject: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    };
    git(&["init", "--quiet"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", subject]);
    Command::new("bash")
        .arg(SCRIPT)
        .args(["--range", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap()
}

#[test]
fn a_72_character_squash_subject_with_its_pull_request_suffix_passes_in_a_range() {
    let output = check_range(&format!("{} (#87)", title(72)));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_73_character_subject_before_the_pull_request_suffix_fails_in_a_range() {
    let output = check_range(&format!("{} (#87)", title(73)));
    assert!(!output.status.success());
}

#[test]
fn a_73_character_subject_without_a_suffix_fails_in_a_range() {
    let output = check_range(&title(73));
    assert!(!output.status.success());
}

#[test]
fn a_70_character_title_on_pull_request_123_still_fails() {
    let output = check_title(&title(70), "123");
    assert!(!output.status.success());
}
