//! `tools/package.sh`: the archive of a release or a beta, made around a stand-in binary.

use std::fs;
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/package.sh");

fn package(version: &str, out: &Path) -> String {
    let binary = out.join("stand-in");
    fs::write(&binary, "#!/bin/sh\necho navette\n").unwrap();
    let output = Command::new("sh")
        .arg(SCRIPT)
        .args([version, binary.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run(program: &str, args: &[&str], dir: &Path) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{program} {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn the_archive_holds_the_binary_the_unit_and_the_install_note() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("0.2.4-beta.21", tmp.path());
    assert_eq!(archive, "navette-0.2.4-beta.21-linux-x86_64.tar.gz");
    let mut files: Vec<String> = run("tar", &["-tzf", &archive], tmp.path())
        .lines()
        .map(str::to_string)
        .collect();
    files.sort();
    let dir = "navette-0.2.4-beta.21-linux-x86_64";
    assert_eq!(
        files,
        [
            format!("{dir}/"),
            format!("{dir}/INSTALL.md"),
            format!("{dir}/navette"),
            format!("{dir}/navette-agent.service"),
        ]
    );
}

#[test]
fn the_binary_comes_out_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("1.0.0", tmp.path());
    let listing = run("tar", &["-tvzf", &archive], tmp.path());
    let line = listing
        .lines()
        .find(|l| l.ends_with("/navette"))
        .expect("the binary is in the archive");
    assert!(line.starts_with("-rwxr-xr-x"), "{line}");
}

#[test]
fn the_checksum_file_verifies_from_next_to_the_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("0.1.0", tmp.path());
    let checksum = format!("{archive}.sha256");
    assert!(tmp.path().join(&checksum).is_file());
    let verdict = run("sha256sum", &["--check", &checksum], tmp.path());
    assert_eq!(verdict.trim(), format!("{archive}: OK"));
}

#[test]
fn the_install_note_names_the_version_it_ships() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("0.3.0", tmp.path());
    let note = run(
        "tar",
        &["-xzOf", &archive, "navette-0.3.0-linux-x86_64/INSTALL.md"],
        tmp.path(),
    );
    assert!(note.contains("docs/install.md"), "{note}");
    assert!(note.contains("~/.local/bin/navette"), "{note}");
}

#[test]
fn a_missing_binary_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new("sh")
        .arg(SCRIPT)
        .args([
            "1.0.0",
            "/nonexistent/navette",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
