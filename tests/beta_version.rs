//! `tools/beta-version.sh`: the version of the beta of a workflow run, from the tags of a
//! repository made on the fly (no network, no real history).

use std::path::Path;
use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/beta-version.sh");

fn repository(tags: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "chore(repo): a commit to tag",
        ],
    );
    for tag in tags {
        git(dir.path(), &["-c", "tag.gpgSign=false", "tag", tag]);
    }
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn beta(dir: &Path, run: &str) -> String {
    let output = Command::new("sh")
        .arg(SCRIPT)
        .arg(run)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn before_any_release_the_beta_follows_zero() {
    let repo = repository(&[]);
    assert_eq!(beta(repo.path(), "7"), "0.0.1-beta.7");
}

#[test]
fn the_beta_is_the_patch_after_the_last_release() {
    let repo = repository(&["v0.1.0", "v0.2.3", "v0.2.0"]);
    assert_eq!(beta(repo.path(), "21"), "0.2.4-beta.21");
}

#[test]
fn releases_are_compared_as_numbers_not_as_text() {
    let repo = repository(&["v0.9.0", "v0.10.0", "v0.10.9", "v0.10.10"]);
    assert_eq!(beta(repo.path(), "3"), "0.10.11-beta.3");
}

#[test]
fn a_previous_beta_and_foreign_tags_are_not_a_release() {
    let repo = repository(&["v0.3.0", "v0.4.1-beta.12", "nightly", "2.0.0"]);
    assert_eq!(beta(repo.path(), "30"), "0.3.1-beta.30");
}

#[test]
fn a_run_number_that_is_not_a_number_is_refused() {
    let repo = repository(&[]);
    for bad in ["", "0", "x", "1.5"] {
        let output = Command::new("sh")
            .arg(SCRIPT)
            .arg(bad)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{bad:?} was accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
    }
}
