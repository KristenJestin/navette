//! The `navette` binary end to end, on loopback and in temporary config dirs: pair with a PIN,
//! then `navette serve` and `navette agent` bring the link up on their own.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use navette::identity::Fingerprint;

const WAIT: Duration = Duration::from_secs(20);

/// A running `navette`, its stdout and stderr read line by line. Killed when dropped.
struct Navette {
    child: Child,
    lines: mpsc::Receiver<String>,
    output: Arc<Mutex<String>>,
}

impl Navette {
    fn start(config: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_navette"))
            .arg("--config-dir")
            .arg(config)
            .args(args)
            // Its own runtime dir (control socket, desk record), no compositor and no GPU: the
            // tests never reach the real desk.
            .env("XDG_RUNTIME_DIR", config.join("run"))
            .env("GLIFF_VIDEO", "cpu")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel();
        let output = Arc::new(Mutex::new(String::new()));
        let stdout: Box<dyn Read + Send> = Box::new(child.stdout.take().unwrap());
        let stderr: Box<dyn Read + Send> = Box::new(child.stderr.take().unwrap());
        for pipe in [stdout, stderr] {
            let (tx, output) = (tx.clone(), output.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    output.lock().unwrap().push_str(&format!("{line}\n"));
                    let _ = tx.send(line);
                }
            });
        }
        Self {
            child,
            lines,
            output,
        }
    }

    /// The first line holding `needle`.
    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return line,
                Ok(_) => {}
                Err(_) => panic!(
                    "no line with {needle:?} in time; output so far:\n{}",
                    self.output.lock().unwrap()
                ),
            }
        }
    }

    fn type_line(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn finish(mut self) -> String {
        let deadline = Instant::now() + WAIT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "navette did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        self.output.lock().unwrap().clone()
    }

    fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }
}

impl Drop for Navette {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pairs two config dirs through the binary, then starts `navette serve` and `navette agent`
/// and waits until both report the link up. Returns every line printed along the way.
fn paired_link(home: &Path, remote: &Path) -> (Navette, Navette, String) {
    let home_pair = Navette::start(home, &["pair", "--listen", "127.0.0.1:0"]);
    let pin = home_pair.wait_for("PIN:");
    let pin = pin.rsplit(' ').next().unwrap().to_owned();
    let hint = home_pair.wait_for("navette pair ");
    let address = hint.split('`').nth(1).unwrap().rsplit(' ').next().unwrap();

    let mut remote_pair = Navette::start(remote, &["pair", address]);
    remote_pair.type_line(&pin);
    let mut printed = remote_pair.finish();
    printed += &home_pair.finish();

    let serve = Navette::start(home, &["serve"]);
    let agent = Navette::start(remote, &["agent"]);
    serve.wait_for("link up");
    agent.wait_for("link up");
    (serve, agent, printed)
}

#[test]
fn two_machines_pair_with_a_pin_and_the_remote_connects_on_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);
}

#[test]
fn no_key_or_certificate_is_ever_printed_or_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, agent, mut printed) = paired_link(&home, &remote);
    printed += &serve.output();
    printed += &agent.output();
    assert!(printed.contains("Paired."), "{printed}");

    for dir in [&home, &remote] {
        let cert = fs::read(dir.join("identity.crt")).unwrap();
        let key = fs::read(dir.join("identity.key")).unwrap();
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let secrets = [
            Fingerprint::of(&cert).to_hex(),
            hex(&cert[..16]),
            hex(&key[..16]),
            hex(&key[key.len() - 16..]),
        ];
        for secret in secrets {
            assert!(!printed.contains(&secret), "{printed}");
        }
    }
    for marker in ["BEGIN", "PRIVATE", "CERTIFICATE"] {
        assert!(!printed.contains(marker), "{printed}");
    }
}

#[test]
fn nothing_listens_on_the_remote_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, agent, _) = paired_link(&home, &remote);
    let pid = agent.child.id();

    let sockets: Vec<String> = fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
        .filter_map(|target| {
            let target = target.to_string_lossy().into_owned();
            Some(
                target
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .to_owned(),
            )
        })
        .collect();
    let mut established = 0;
    for table in ["tcp", "tcp6", "udp", "udp6"] {
        let text = fs::read_to_string(format!("/proc/{pid}/net/{table}")).unwrap_or_default();
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (state, inode) = (fields[3], fields[9]);
            if !sockets.iter().any(|s| s == inode) {
                continue;
            }
            assert!(table.starts_with("tcp"), "the agent holds a UDP socket");
            assert_ne!(state, "0A", "the agent listens on a TCP socket");
            if state == "01" {
                established += 1;
            }
        }
    }
    assert_eq!(established, 1, "the agent holds exactly its link to home");
}

#[test]
fn home_refuses_a_wildcard_address_on_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    for wildcard in ["0.0.0.0:7420", "[::]:7420"] {
        let output = Navette::start(tmp.path(), &["pair", "--listen", wildcard]).finish();
        assert!(output.contains("LAN address"), "{output}");
    }
    assert!(fs::read_dir(tmp.path()).unwrap().next().is_none());
}

// `Cargo.toml` stays at 0.0.0: a release build names its version in NAVETTE_VERSION (the tag
// without its `v`), a local build is `0.0.0-dev`.
#[test]
fn version_comes_from_the_build_not_from_cargo_toml() {
    let output = Command::new(env!("CARGO_BIN_EXE_navette"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let expected = option_env!("NAVETTE_VERSION").unwrap_or("0.0.0-dev");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("navette {expected}\n")
    );
}

#[test]
fn mode_start_without_serve_says_to_start_it() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mode", "start"]).finish();
    assert!(
        output.contains("`navette serve` is not running"),
        "{output}"
    );
}

#[test]
fn mode_start_with_no_mode_configured_says_where_to_write_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);
    let output = Navette::start(&home, &["mode", "start"]).finish();
    assert!(output.contains("monitor = <output name>"), "{output}");
    assert!(output.contains("mode.conf"), "{output}");
}

#[test]
fn mic_status_without_serve_says_the_mic_is_off() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mic", "status", "--json"]).finish();
    assert!(output.contains(r#""alt":"off""#), "{output}");
}

#[test]
fn mic_toggle_without_serve_says_to_start_it() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mic", "toggle"]).finish();
    assert!(
        output.contains("`navette serve` is not running"),
        "{output}"
    );
}

#[test]
fn mic_toggle_without_audio_says_to_turn_it_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);
    let output = Navette::start(&home, &["mic", "toggle"]).finish();
    assert!(output.contains("navette serve --audio"), "{output}");
}
