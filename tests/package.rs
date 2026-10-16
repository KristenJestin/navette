//! `tools/package.sh`: the archive of a release or a beta, made around a stand-in binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
fn the_archive_holds_the_binary_the_units_the_icon_the_install_script_and_the_install_note() {
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
            format!("{dir}/install.sh"),
            format!("{dir}/navette"),
            format!("{dir}/navette-agent.service"),
            format!("{dir}/navette.service"),
            format!("{dir}/navette.svg"),
        ]
    );
}

#[test]
fn the_icon_in_the_archive_is_the_pixel_icon_of_the_brand() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("1.0.0", tmp.path());
    run("tar", &["-xzf", &archive], tmp.path());
    let packed = fs::read(tmp.path().join("navette-1.0.0-linux-x86_64/navette.svg")).unwrap();
    let brand = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/brand/icon-pixel.svg"
    ))
    .unwrap();
    assert_eq!(packed, brand);
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

fn unit() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/dist/navette.service")).unwrap()
}

/// The value of the first `key=` line of the unit.
fn setting(unit: &str, key: &str) -> String {
    unit.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in the unit"))
        .to_string()
}

#[test]
fn the_one_unit_runs_what_the_pairing_says_after_the_graphical_session() {
    let unit = unit();
    assert_eq!(
        setting(&unit, "ExecStart"),
        "%h/.local/bin/navette run --audio"
    );
    assert_eq!(setting(&unit, "PartOf"), "graphical-session.target");
    assert!(setting(&unit, "After").contains("graphical-session.target"));
    assert_eq!(setting(&unit, "WantedBy"), "graphical-session.target");
}

#[test]
fn the_unit_does_not_restart_on_a_refusal() {
    let unit = unit();
    assert_eq!(setting(&unit, "Restart"), "on-failure");
    assert_eq!(
        setting(&unit, "RestartPreventExitStatus"),
        navette::runner::REFUSED.to_string()
    );
}

fn unpack(version: &str, out: &Path) -> PathBuf {
    let archive = package(version, out);
    run("tar", &["-xzf", &archive], out);
    out.join(archive.trim_end_matches(".tar.gz"))
}

/// A machine to install on: a home folder, fake `systemctl` and `id` ahead on the `PATH`, and a
/// log of what `systemctl` was asked.
struct Machine {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    fakes: PathBuf,
    log: PathBuf,
    archive: PathBuf,
    /// Whether the old `navette-agent.service` is enabled here.
    old_agent_enabled: bool,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (home, fakes) = (tmp.path().join("home"), tmp.path().join("fakes"));
        let (log, packed) = (tmp.path().join("systemctl.log"), tmp.path().join("packed"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&fakes).unwrap();
        fs::create_dir_all(&packed).unwrap();
        fake(
            &fakes,
            "systemctl",
            "echo \"$*\" >> \"$FAKE_SYSTEMCTL_LOG\"\n",
        );
        let archive = unpack("1.2.3", &packed);
        Self {
            _tmp: tmp,
            home,
            fakes,
            log,
            archive,
            old_agent_enabled: false,
        }
    }

    /// `install.sh` from `dir`, run from the home folder (not from where it lives).
    fn install_from(&self, dir: &Path) -> Output {
        let path = format!(
            "{}:{}",
            self.fakes.display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("bash")
            .arg(dir.join("install.sh"))
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("FAKE_SYSTEMCTL_LOG", &self.log)
            .env(
                "FAKE_AGENT_ENABLED",
                if self.old_agent_enabled { "1" } else { "" },
            )
            .output()
            .unwrap()
    }

    fn install(&self) -> Output {
        self.install_from(&self.archive)
    }

    fn systemctl_calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn mode(&self, path: &str) -> u32 {
        fs::metadata(self.home.join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }
}

fn fake(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn the_install_script_comes_out_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = package("1.0.0", tmp.path());
    let listing = run("tar", &["-tvzf", &archive], tmp.path());
    let line = listing
        .lines()
        .find(|l| l.ends_with("/install.sh"))
        .expect("the script is in the archive");
    assert!(line.starts_with("-rwxr-xr-x"), "{line}");
}

#[test]
fn the_install_script_puts_the_binary_the_icon_and_the_units_in_the_home_folder() {
    let machine = Machine::new();
    let output = machine.install();
    assert!(output.status.success(), "{}", said(&output));
    for (file, mode) in [
        (".local/bin/navette", 0o755),
        (
            ".local/share/icons/hicolor/scalable/apps/navette.svg",
            0o644,
        ),
        (".config/systemd/user/navette.service", 0o644),
        (".config/systemd/user/navette-agent.service", 0o644),
    ] {
        assert!(machine.home.join(file).is_file(), "{file} is missing");
        assert_eq!(machine.mode(file), mode, "{file}");
    }
    assert_eq!(
        fs::read(machine.home.join(".config/systemd/user/navette.service")).unwrap(),
        fs::read(machine.archive.join("navette.service")).unwrap()
    );
}

#[test]
fn the_install_script_reloads_systemd_and_enables_and_starts_nothing() {
    let machine = Machine::new();
    assert!(machine.install().status.success());
    // It reloads and looks at the old unit; `enable`, `start` and `restart` never come.
    assert_eq!(
        machine.systemctl_calls(),
        [
            "--user daemon-reload",
            "--user is-enabled navette-agent.service"
        ]
    );
}

#[test]
fn the_install_script_says_what_it_installed_and_the_next_step() {
    let machine = Machine::new();
    let said = said(&machine.install());
    assert!(said.contains(".local/bin/navette"), "{said}");
    assert!(said.contains("navette.service"), "{said}");
    assert!(said.contains("navette pair"), "{said}");
    assert!(
        said.contains("systemctl --user enable --now navette"),
        "{said}"
    );
}

#[test]
fn running_the_install_script_twice_gives_the_same_result() {
    let machine = Machine::new();
    let first = machine.install();
    let files = |machine: &Machine| {
        let listing = run(
            "find",
            &[".", "-type", "f", "-printf", "%p %m\\n"],
            &machine.home,
        );
        let mut lines: Vec<_> = listing.lines().map(str::to_string).collect();
        lines.sort();
        lines
    };
    let installed = files(&machine);
    let second = machine.install();
    assert!(second.status.success(), "{}", said(&second));
    assert_eq!(files(&machine), installed);
    assert_eq!(said(&second), said(&first));
    assert_eq!(
        machine.systemctl_calls(),
        [
            "--user daemon-reload",
            "--user is-enabled navette-agent.service",
            "--user daemon-reload",
            "--user is-enabled navette-agent.service"
        ]
    );
}

#[test]
fn the_install_script_refuses_to_run_as_root() {
    let machine = Machine::new();
    fake(&machine.fakes, "id", "echo 0\n");
    let output = machine.install();
    assert!(!output.status.success());
    assert!(said(&output).contains("root"), "{}", said(&output));
    assert!(!machine.home.join(".local").exists());
    assert!(machine.systemctl_calls().is_empty());
}

#[test]
fn the_install_script_names_a_file_missing_next_to_it_and_installs_nothing() {
    let machine = Machine::new();
    let half = machine.home.join("half");
    fs::create_dir(&half).unwrap();
    for name in [
        "install.sh",
        "navette",
        "navette.svg",
        "navette-agent.service",
    ] {
        fs::copy(machine.archive.join(name), half.join(name)).unwrap();
    }
    let output = machine.install_from(&half);
    assert!(!output.status.success());
    assert!(
        said(&output).contains("navette.service"),
        "{}",
        said(&output)
    );
    assert!(!machine.home.join(".local").exists());
}

#[test]
fn the_install_script_tells_to_disable_the_old_agent_unit_when_it_is_enabled() {
    let mut machine = Machine::new();
    machine.old_agent_enabled = true;
    let output = machine.install();
    assert!(output.status.success(), "{}", said(&output));
    assert!(
        said(&output).contains("systemctl --user disable --now navette-agent"),
        "{}",
        said(&output)
    );
    // It only says so: no call but the reload and the question.
    assert_eq!(
        machine.systemctl_calls(),
        [
            "--user daemon-reload",
            "--user is-enabled navette-agent.service"
        ]
    );
}

#[test]
fn the_install_script_does_not_mention_disabling_when_the_old_agent_unit_is_not_enabled() {
    let machine = Machine::new();
    let said = said(&machine.install());
    assert!(!said.contains("disable --now"), "{said}");
}
