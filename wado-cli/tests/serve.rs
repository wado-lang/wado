//! E2E tests for `wado serve`: per-request timeout and graceful shutdown.
//!
//! These tests spawn the `wado` binary, drive it over a real TCP socket, and
//! send Unix signals — none of which port to Windows, so the file is gated
//! `#[cfg(unix)]`.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Reserve a free TCP port by binding to `127.0.0.1:0`, reading the assigned
/// port, then dropping the listener. There is a tiny TOCTOU window between
/// drop and the spawned `wado serve` re-binding the port, but in practice
/// the kernel re-uses ports rarely enough for this to be reliable in CI.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener.local_addr().unwrap().port()
}

/// Drop guard that kills the spawned server on test exit so a panic
/// mid-test doesn't leak a process holding the listening port.
struct ServerGuard {
    child: Child,
}

impl ServerGuard {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `wado serve` on a free port using `fixture` (relative to
/// `wado-cli/tests/fixtures`), wait for the listening socket to accept
/// connections, and return the guard, the bound port, and a handle to the
/// captured stderr buffer.
///
/// stderr is drained on a background thread so the child never blocks on a
/// full pipe; the captured text is also used for assertions on shutdown
/// messages.
fn start_serve(fixture: &str, extra_args: &[&str]) -> (ServerGuard, u16, Arc<Mutex<String>>) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("wado"));
    cmd.current_dir(project_root())
        .arg("serve")
        .arg("--addr")
        .arg(&addr)
        .arg("-O0")
        .arg("--log-level")
        .arg("off")
        .args(extra_args)
        .arg(fixture_path(fixture))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn wado serve");

    // Drain stderr so the kernel pipe buffer never fills, and capture the
    // text for assertions.
    let stderr = child.stderr.take().unwrap();
    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    {
        let buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let mut guard = buf.lock().unwrap();
                guard.push_str(&line);
                guard.push('\n');
            }
        });
    }

    // Compile time at -O0 is a few seconds; 60s is generous for slow CI.
    let deadline = Instant::now() + Duration::from_mins(1);
    let socket_addr: SocketAddr = addr.parse().unwrap();
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&socket_addr, Duration::from_millis(200)).is_ok() {
            return (ServerGuard { child }, port, stderr_buf);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let captured = stderr_buf.lock().unwrap().clone();
    panic!("wado serve did not accept connections within 60s. stderr:\n{captured}");
}

/// Send a minimal HTTP/1.1 GET and return `(status_code, body)`.
fn http_get(port: u16, path: &str, timeout: Duration) -> (u16, String) {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();

    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let status_line = response
        .lines()
        .next()
        .expect("status line in HTTP response");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse status from: {status_line}"));

    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();

    (status, body)
}

/// A guest stuck in pure wasm past `--timeout` should trap (via
/// `set_epoch_deadline`) and the client should see a 504 — not a connection
/// drop, not a 500, not a hang.
#[test]
fn timeout_returns_504_for_runaway_guest() {
    let (_guard, port, _stderr) = start_serve("serve_hang.wado", &["--timeout", "2"]);

    let start = Instant::now();
    let (status, body) = http_get(port, "/", Duration::from_secs(15));
    let elapsed = start.elapsed();

    assert_eq!(status, 504, "expected 504 Gateway Timeout; body: {body:?}");
    assert!(
        body.contains("timed out"),
        "504 body should mention the timeout; got: {body:?}",
    );
    // The 504 should fire near the configured 2s deadline. Allow a generous
    // window: epoch ticker granularity is 1s, and CI machines can be slow.
    assert!(
        elapsed >= Duration::from_millis(1500),
        "504 returned in {elapsed:?} — earlier than the configured 2s timeout, \
         which suggests the timeout isn't actually being enforced",
    );
    assert!(
        elapsed <= Duration::from_secs(8),
        "504 returned in {elapsed:?} — later than expected for a 2s timeout",
    );
}

/// SIGTERM should trigger the shutdown path: the accept loop stops, the
/// drain phase runs, and the process exits with status 0. Verifies both
/// the exit status and the operator-facing log line so a regression that
/// merely SIGKILLs the child (skipping drain) would fail this test.
#[test]
fn sigterm_triggers_graceful_shutdown() {
    let (mut guard, _port, stderr) = start_serve("serve_hello.wado", &[]);

    let pid = guard.pid();
    // SAFETY: pid was just observed for our owned child; the worst case
    // from a stale pid would be ESRCH, which we ignore.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: errno={}",
        std::io::Error::last_os_error()
    );

    // Without an in-flight request the drain phase is essentially a no-op,
    // so 15s is far more than the shutdown should ever need.
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        match guard.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "server did not exit within 15s after SIGTERM"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };

    assert!(
        exit.success(),
        "graceful shutdown should exit with status 0; got {exit:?}",
    );
    let captured = stderr.lock().unwrap().clone();
    assert!(
        captured.contains("Shutdown signal received"),
        "expected the shutdown log line in stderr; got:\n{captured}",
    );
}
