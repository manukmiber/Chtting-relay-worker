//! One relay per port, proved against the real binary.
//!
//! The bug this is written against does not crash and does not log. Two copies
//! bind the same port — `SO_REUSEPORT` lets them — and the kernel splits new
//! connections between them, so the relay answers out of whichever config the
//! process that got the connection happens to hold. A key minted in one is
//! unknown to the other, and the caller sees `invalid API key` on a fraction of
//! requests exactly equal to the stale instance's share of the sockets. Nothing
//! in the logs says so.
//!
//! So it is checked the only way that means anything: start the binary, start
//! it again, and look at what the second one does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// What a start exits with when another instance already owns the port.
const EXIT_PORT_BUSY: i32 = 3;

const RELAY: &str = env!("CARGO_BIN_EXE_chtting-relay");

/// A port nothing else is on, found by letting the OS pick one and giving it
/// straight back.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port to borrow");
    listener.local_addr().unwrap().port()
}

/// A home directory with a config that starts a relay and nothing else: no
/// dashboard, no tunnel, no log file to fight over.
fn home_on(port: u16) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chtting-solo-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() + u128::from(port)
    ));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let mut file = std::fs::File::create(dir.join("config/config.json")).unwrap();
    write!(
        file,
        r#"{{
          "server": {{ "port": {port}, "rotateHours": 0, "rotateMinutes": 0 }},
          "dashboard": {{ "enabled": false }},
          "tunnel": {{ "mode": "off", "autoStart": false }},
          "logging": {{ "fileEnabled": false }}
        }}"#
    )
    .unwrap();
    dir
}

fn start_relay(home: &Path, extra: &[&str]) -> Child {
    Command::new(RELAY)
        .arg("start")
        .arg("--home")
        .arg(home)
        .arg("--no-dashboard")
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the relay binary must be runnable")
}

/// Wait until something is answering on the port.
fn wait_until_serving(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Wait for a process to finish, killing it if it will not.
fn wait_for_exit(child: &mut Child, patience: Duration) -> Option<i32> {
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        match child.try_wait().expect("the child must be waitable") {
            Some(status) => return status.code(),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_second_relay_on_one_port_refuses_to_start() {
    let port = free_port();
    let first_home = home_on(port);
    let mut first = Reaped(start_relay(&first_home, &[]));
    assert!(
        wait_until_serving(port),
        "the first relay never came up, so nothing was proved"
    );

    // A *different* home, which is the case the old pidfile could never catch:
    // the lock lived under the data directory, so two copies started with
    // different `--home` values each read their own empty lock and both bound
    // the same port. The keeper passes a home explicitly and a hand-run
    // start-termux.sh resolves one from the working directory, so the two
    // disagreeing is not hypothetical.
    let second_home = home_on(port);
    let mut second = start_relay(&second_home, &[]);
    let code = wait_for_exit(&mut second, Duration::from_secs(60));

    assert_eq!(
        code,
        Some(EXIT_PORT_BUSY),
        "a second relay started on a port that already had one"
    );

    // And the first one is untouched by having been asked.
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "the refused start took the running relay down with it"
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

    let _ = std::fs::remove_dir_all(&first_home);
    let _ = std::fs::remove_dir_all(&second_home);
}

#[test]
fn the_refusal_names_the_port_and_the_way_out() {
    let port = free_port();
    let first_home = home_on(port);
    let _first = Reaped(start_relay(&first_home, &[]));
    assert!(wait_until_serving(port), "the first relay never came up");

    let second_home = home_on(port);
    let second = start_relay(&second_home, &[])
        .wait_with_output()
        .expect("the second relay must finish");

    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        said.contains(&port.to_string()),
        "the refusal did not say which port: {said}"
    );
    assert!(
        said.contains("--replace"),
        "the refusal did not say how to take the port over: {said}"
    );
    // The line that used to appear before the check ran, and made a refused
    // duplicate look like a relay that had started.
    assert!(
        !said.contains("starting —"),
        "a relay that never started announced that it had: {said}"
    );

    let _ = std::fs::remove_dir_all(&first_home);
    let _ = std::fs::remove_dir_all(&second_home);
}

/// The rotation is the one case where two relays on one port is correct, and it
/// has to keep working — a guard that also blocks the hourly handover would
/// trade a duplicate for an outage. So: a successor carrying a token its
/// predecessor minted comes up beside it, and takes the port over the moment
/// the predecessor is gone.
#[test]
fn a_successor_carrying_a_real_token_is_let_through_and_then_takes_over() {
    let port = free_port();
    let home = home_on(port);
    let mut predecessor = start_relay(&home, &[]);
    assert!(wait_until_serving(port), "the first relay never came up");

    let pidfile = home.join("data/run/serving.pid");
    let predecessor_pid = read_pid(&pidfile).expect("the relay records who is serving");

    // Exactly what `rotate::spawn_successor` leaves behind: a one-shot token
    // and the pid it is replacing.
    std::fs::create_dir_all(home.join("data/run")).unwrap();
    std::fs::write(
        home.join("data/run/handover-1"),
        format!("ho_testtoken {predecessor_pid}"),
    )
    .unwrap();

    let mut successor = Reaped(
        Command::new(RELAY)
            .arg("start")
            .arg("--home")
            .arg(&home)
            .arg("--no-dashboard")
            .env("CHTTING_GENERATION", "1")
            .env("CHTTING_HANDOVER", "ho_testtoken")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the relay binary must be runnable"),
    );

    // It is allowed to serve beside its predecessor, which is the whole point
    // of a zero-gap handover.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        successor.0.try_wait().unwrap().is_none(),
        "the successor of a rotation was refused, which would end the hourly \
         handover and leave the relay to be killed by Android instead"
    );

    // And once the predecessor goes, the successor is the relay.
    let _ = predecessor.kill();
    let _ = predecessor.wait();
    let successor_pid = successor.0.id();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if read_pid(&pidfile) == Some(successor_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        read_pid(&pidfile),
        Some(successor_pid),
        "the successor never took the port over after its predecessor left"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn replace_takes_the_port_and_leaves_one_relay_running() {
    let port = free_port();
    let first_home = home_on(port);
    let mut first = start_relay(&first_home, &[]);
    assert!(wait_until_serving(port), "the first relay never came up");

    let second_home = home_on(port);
    let mut second = Reaped(start_relay(&second_home, &["--replace"]));

    // The incumbent is asked to stop, and goes.
    assert!(
        wait_for_exit(&mut first, Duration::from_secs(60)).is_some(),
        "--replace did not move the running relay along"
    );
    // And the one that replaced it is serving.
    assert!(
        wait_until_serving(port),
        "--replace took the port and then nobody was on it"
    );
    assert!(
        second.0.try_wait().unwrap().is_none(),
        "the replacement exited instead of taking over"
    );

    let _ = std::fs::remove_dir_all(&first_home);
    let _ = std::fs::remove_dir_all(&second_home);
}
