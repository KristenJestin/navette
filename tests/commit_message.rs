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
