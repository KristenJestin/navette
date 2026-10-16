//! The `navette` binary end to end, on loopback and in temporary config dirs: pair with a PIN,
//! then `navette serve` and `navette agent` bring the link up on their own.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
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

    fn finish(self) -> String {
        self.finish_with_status().1
    }

    fn finish_with_status(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "navette did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        };
        std::thread::sleep(Duration::from_millis(50));
        (status, self.output.lock().unwrap().clone())
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
fn pairing_again_says_to_restart_a_running_agent() {
    // A running agent read the previous pairing (the desk's router, say) when it started.
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, printed) = paired_link(&home, &remote);
    assert!(
        printed.contains("systemctl --user enable navette && systemctl --user restart navette"),
        "{printed}"
    );
    assert!(!printed.contains("navette-agent"), "{printed}");
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
    // A table read in several `read()`s can repeat a row while sockets come and go on the
    // machine, so a socket is counted once by its inode.
    let mut established = std::collections::HashSet::new();
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
                established.insert(inode.to_owned());
            }
        }
    }
    assert_eq!(
        established.len(),
        1,
        "the agent holds exactly its link to home"
    );
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

#[test]
fn a_second_agent_refuses_to_start_while_one_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);

    let (status, output) = Navette::start(&remote, &["agent"]).finish_with_status();
    assert!(!status.success(), "{output}");
    assert!(
        output.contains("another navette agent is already running"),
        "{output}"
    );
    assert!(
        output.contains("(systemctl --user status navette)"),
        "{output}"
    );
}

#[test]
fn the_first_agent_s_link_is_untouched_by_a_refused_second_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, agent, _) = paired_link(&home, &remote);

    let refused = Navette::start(&remote, &["agent"]).finish();
    assert!(!refused.contains("link up"), "{refused}");

    // Home keeps its newest connection: a second dial would have shown as a second `link up`
    // at home and a `link down` at the first agent.
    for (who, output) in [
        ("home", serve.output()),
        ("the first agent", agent.output()),
    ] {
        assert_eq!(output.matches("link up").count(), 1, "{who}:\n{output}");
        assert!(!output.contains("link down"), "{who}:\n{output}");
    }
}

#[test]
fn an_agent_starts_after_the_previous_one_crashed() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, mut agent, _) = paired_link(&home, &remote);

    // SIGKILL: no clean-up of any kind runs, so nothing but the kernel can free the lock.
    agent.child.kill().unwrap();
    agent.child.wait().unwrap();
    serve.wait_for("link down");

    let next = Navette::start(&remote, &["agent"]);
    next.wait_for("link up");
}

#[test]
fn mode_list_json_without_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mode", "list", "--json"]).finish();
    assert_eq!(output, "[]\n", "no mode.conf: no mode");
    fs::write(
        tmp.path().join("mode.conf"),
        "[default]\nmonitor = DP-2\n[office]\nmonitor = DP-1, DP-2\n",
    )
    .unwrap();
    let (status, output) =
        Navette::start(tmp.path(), &["mode", "list", "--json"]).finish_with_status();
    assert!(status.success(), "{output}");
    assert_eq!(
        output,
        "[{\"name\":\"default\",\"monitors\":[\"DP-2\"]},\
         {\"name\":\"office\",\"monitors\":[\"DP-1\",\"DP-2\"]}]\n"
    );
}

#[test]
fn watch_once_without_serve_or_agent_says_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, output) =
        Navette::start(tmp.path(), &["watch", "--json", "--once"]).finish_with_status();
    assert!(status.success(), "{output}");
    assert_eq!(output, "{\"v\":1,\"state\":\"off\"}\n");
    let output = Navette::start(tmp.path(), &["watch", "--waybar", "--once"]).finish();
    assert!(output.contains(r#""class":"off""#), "{output}");
    let output = Navette::start(tmp.path(), &["watch", "--once"]).finish();
    assert_eq!(output, "off\n");
}

/// `navette watch --json --once` on `config`'s machine, until its state is `state`.
fn watched_until(config: &Path, state: &str) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let output = Navette::start(config, &["watch", "--json", "--once"]).finish();
        if output.contains(&format!(r#""state":"{state}""#)) {
            return output;
        }
        assert!(Instant::now() < deadline, "never {state}: {output}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn each_machine_says_where_it_stands_on_its_events_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);
    let desk = watched_until(&home, "linked");
    assert!(desk.contains(r#""role":"desk""#), "{desk}");
    assert!(desk.contains(r#""peer":"REMOTE""#), "{desk}");
    let far = watched_until(&remote, "linked");
    assert!(far.contains(r#""role":"remote""#), "{far}");
    // The remote drives nothing: a command there is refused.
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(remote.join("run/navette/events.sock")).unwrap();
    stream.write_all(b"stop\n").unwrap();
    let mut lines = BufReader::new(stream).lines();
    assert!(lines.next().unwrap().unwrap().starts_with('{'));
    assert_eq!(
        lines.next().unwrap().unwrap(),
        "error the remote does not drive the mode"
    );
}

#[test]
fn the_remote_says_dialing_once_the_desk_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, _agent, _) = paired_link(&home, &remote);
    watched_until(&remote, "linked");
    drop(serve);
    watched_until(&remote, "dialing");
}

#[test]
fn run_without_a_pairing_exits_with_the_refusal_status_and_says_to_pair() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, output) = Navette::start(tmp.path(), &["run", "--audio"]).finish_with_status();
    assert_eq!(
        status.code(),
        Some(i32::from(navette::runner::REFUSED)),
        "{output}"
    );
    assert!(
        output.contains("navette: not paired; run navette pair first"),
        "{output}"
    );
}

#[test]
fn run_with_a_broken_role_names_the_key_and_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("paired.conf"), "role = boss\n").unwrap();
    let (status, output) = Navette::start(tmp.path(), &["run"]).finish_with_status();
    assert_eq!(
        status.code(),
        Some(i32::from(navette::runner::REFUSED)),
        "{output}"
    );
    assert!(output.contains("role"), "{output}");
    assert!(output.contains("paired.conf"), "{output}");
}

/// Pairs the two config dirs, then starts `navette run` on both machines (no `--audio` at home:
/// it would reach the real PipeWire) and waits until both report the link up.
fn run_link(home: &Path, remote: &Path, remote_args: &[&str]) -> (Navette, Navette) {
    let home_pair = Navette::start(home, &["pair", "--listen", "127.0.0.1:0"]);
    let pin = home_pair.wait_for("PIN:");
    let pin = pin.rsplit(' ').next().unwrap().to_owned();
    let hint = home_pair.wait_for("navette pair ");
    let address = hint.split('`').nth(1).unwrap().rsplit(' ').next().unwrap();
    let mut remote_pair = Navette::start(remote, &["pair", address]);
    remote_pair.type_line(&pin);
    remote_pair.finish();
    home_pair.finish();

    let serve = Navette::start(home, &["run"]);
    serve.wait_for("waiting for the paired remote machine");
    let agent = Navette::start(remote, &[&["run"], remote_args].concat());
    serve.wait_for("link up");
    agent.wait_for("link up");
    (serve, agent)
}

#[test]
fn run_starts_serve_at_home_and_the_agent_on_the_remote_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent) = run_link(&home, &remote, &[]);
}

#[test]
fn run_on_the_remote_machine_says_once_that_audio_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, agent) = run_link(&home, &remote, &["--audio"]);
    let output = agent.output();
    assert_eq!(output.matches("--audio is ignored").count(), 1, "{output}");
}

#[test]
fn run_ends_on_sigterm_like_the_command_it_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, agent) = run_link(&home, &remote, &[]);
    for machine in [agent, serve] {
        let pid = machine.child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(sent.success());
        let (status, output) = machine.finish_with_status();
        assert_eq!(status.code(), Some(0), "{output}");
    }
}

#[test]
fn pairing_writes_role_desk_and_a_pairing_that_says_home_still_runs_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (serve, _agent) = run_link(&home, &remote, &[]);
    let file = home.join("paired.conf");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("role = desk\n"), "{text}");
    let far = fs::read_to_string(remote.join("paired.conf")).unwrap();
    assert!(far.contains("role = remote\n"), "{far}");
    // A pairing made by 0.1.0: nothing is paired again.
    drop(serve);
    fs::write(&file, text.replace("role = desk", "role = home")).unwrap();
    let again = Navette::start(&home, &["run"]);
    again.wait_for("waiting for the paired remote machine");
}

#[test]
fn mic_home_still_works_and_says_the_new_name_once() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mic", "home"]).finish();
    assert_eq!(output.matches("navette mic desk").count(), 1, "{output}");
    assert!(
        output.contains("`navette serve` is not running"),
        "the order itself was still sent: {output}"
    );
}

#[test]
fn mic_desk_says_nothing_of_the_old_name() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Navette::start(tmp.path(), &["mic", "desk"]).finish();
    assert!(!output.contains("mic home"), "{output}");
    assert!(
        output.contains("`navette serve` is not running"),
        "{output}"
    );
}

/// `navette watch --json --once` on `config`'s machine, until its line holds `needle`.
fn watched_with(config: &Path, needle: &str) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let output = Navette::start(config, &["watch", "--json", "--once"]).finish();
        if output.contains(needle) {
            return output;
        }
        assert!(Instant::now() < deadline, "never {needle}: {output}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_remote_says_the_label_of_the_desk_as_its_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("mode.conf"),
        "local_label = NEAR\nremote_label = FAR\nmonitor = DP-2\n",
    )
    .unwrap();
    let (_serve, _agent, _) = paired_link(&home, &remote);
    let desk = watched_until(&home, "linked");
    assert!(desk.contains(r#""peer":"FAR""#), "{desk}");
    let far = watched_with(&remote, r#""peer":"NEAR""#);
    assert!(far.contains(r#""role":"remote""#), "{far}");
}

#[test]
fn with_no_label_the_remote_has_no_peer_to_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, remote) = (tmp.path().join("home"), tmp.path().join("remote"));
    let (_serve, _agent, _) = paired_link(&home, &remote);
    let far = watched_until(&remote, "linked");
    assert!(far.contains(r#""peer":null"#), "{far}");
}
