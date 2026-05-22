//! E2E tests for `wado serve`: per-request timeout and graceful shutdown.
//!
//! These tests spawn the `wado` binary, drive it over a real TCP socket, and
//! send Unix signals — none of which port to Windows, so the file is gated
//! `#[cfg(unix)]`.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::{project_root, wado_bin};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Parse the kernel-assigned port out of the server's startup banner
/// (`HTTP server listening on http://127.0.0.1:PORT/`).
fn parse_listening_port(stderr: &str) -> Option<u16> {
    let line = stderr
        .lines()
        .find(|l| l.contains("listening on http://"))?;
    line.rsplit_once(':')?.1.trim_end_matches('/').parse().ok()
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
//
// `clippy::zombie_processes` flags `cmd.spawn()` returning a `Child` that
// isn't `wait()`-ed on every code path, but `ServerGuard::drop` reliably
// calls `kill()` + `wait()` — clippy doesn't see across the type boundary.
#[allow(clippy::zombie_processes)]
fn start_serve(fixture: &str, extra_args: &[&str]) -> (ServerGuard, u16, Arc<Mutex<String>>) {
    // Bind a kernel-assigned port (`:0`) inside the server process itself.
    // Pre-reserving a port in the test and letting the child re-bind it
    // leaves a TOCTOU window in which a parallel test can grab the same
    // port — so one test's client connects to another test's server and
    // sees a spurious connection reset when that server is torn down.
    let mut cmd = Command::new(wado_bin());
    cmd.current_dir(project_root())
        .arg("serve")
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("-O0")
        .arg("--log-level")
        .arg("off")
        .args(extra_args)
        .arg(fixture_path(fixture))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn wado serve");

    // Drain stderr so the kernel pipe buffer never fills, and capture the
    // text for assertions and for the listening-port banner.
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
    loop {
        let listening_port = parse_listening_port(&stderr_buf.lock().unwrap());
        if let Some(port) = listening_port {
            // The banner is printed right after `bind`, so the socket is
            // already accepting; one confirming connect keeps the
            // contract that callers may connect immediately on return.
            let socket_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            while Instant::now() < deadline {
                if TcpStream::connect_timeout(&socket_addr, Duration::from_millis(200)).is_ok() {
                    return (ServerGuard { child }, port, stderr_buf);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let captured = stderr_buf.lock().unwrap().clone();
            panic!("wado serve exited before listening (status {status}). stderr:\n{captured}");
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let captured = stderr_buf.lock().unwrap().clone();
    panic!("wado serve did not report a listening port within 60s. stderr:\n{captured}");
}

/// Send a minimal HTTP/1.1 GET and return the full raw response text
/// (status line, headers, blank line, body — chunk framing intact).
fn http_get_raw(port: u16, path: &str, timeout: Duration) -> String {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();

    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// Parse `(status_code, raw_body)` from a raw HTTP/1.1 response. The body
/// is returned exactly as it came off the wire — for chunked responses
/// it still contains the chunk-size lines.
fn parse_response(response: &str) -> (u16, String) {
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

/// Convenience wrapper for tests that don't care about chunk framing.
fn http_get(port: u16, path: &str, timeout: Duration) -> (u16, String) {
    parse_response(&http_get_raw(port, path, timeout))
}

/// A guest stuck in pure wasm past `--timeout` should trap (via
/// `set_epoch_deadline`) and the client should see a 504 — not a connection
/// drop, not a 500, not a hang.
#[test]
fn timeout_returns_504_for_runaway_guest() {
    let (_guard, port, _stderr) = start_serve("serve_hang.wado", &["--timeout", "2"]);

    let start = Instant::now();
    // The client read timeout must be a generous *upper bound on the worst
    // case CI run*, not the test's actual deadline — that lives in the
    // `elapsed` assertions below. A tight client timeout panics on
    // `read_to_string` as `WouldBlock` whenever CI is slower than the
    // assertion would have allowed, hiding the useful "elapsed was Xs"
    // signal behind an opaque socket-level error.
    let (status, body) = http_get(port, "/", Duration::from_mins(1));
    let elapsed = start.elapsed();

    assert_eq!(status, 504, "expected 504 Gateway Timeout; body: {body:?}");
    assert!(
        body.contains("timed out"),
        "504 body should mention the timeout; got: {body:?}",
    );
    // The 504 reaches the client via one of two paths, and which one wins
    // depends on how CPU-starved the host is:
    //   * graceful: the first-byte timeout in `dispatch_request` fires at
    //     ~`--timeout` (2s) — this needs a free tokio worker thread;
    //   * backstop: when a runaway guest monopolises every tokio worker
    //     thread (e.g. a single-core CI runner), the first-byte timeout is
    //     itself starved, so the 504 only resolves once the epoch deadline
    //     (`--timeout` + 5s grace) traps the guest and frees the runtime.
    // So the legitimate window is wide. The lower bound proves the timeout
    // is actually enforced (CPU starvation only ever *delays* the 504, so
    // it cannot make this bound flaky); the upper bound is generous enough
    // that only a genuine "timeout never fires" regression — which would
    // instead hit the 1-minute client read timeout — trips it.
    assert!(
        elapsed >= Duration::from_millis(1500),
        "504 returned in {elapsed:?} — earlier than the configured 2s timeout, \
         which suggests the timeout isn't actually being enforced",
    );
    assert!(
        elapsed <= Duration::from_secs(20),
        "504 returned in {elapsed:?} — far later than the 2s timeout or its \
         epoch-deadline backstop, which suggests the timeout isn't enforced",
    );
}

/// The response body should leave the server one frame at a time rather
/// than being buffered into a single `Content-Length` blob. We verify
/// this end-to-end by asserting the response uses HTTP/1.1 chunked
/// transfer encoding (the only way hyper can deliver a body whose total
/// length isn't known up front), and that the literal body bytes survive
/// the trip through the streaming pipeline.
#[test]
fn responds_with_streamed_chunked_body() {
    let (_guard, port, _stderr) = start_serve("serve_hello.wado", &[]);

    let raw = http_get_raw(port, "/", Duration::from_secs(15));
    let (status, body) = parse_response(&raw);

    assert_eq!(status, 200, "expected 200 OK; got raw response:\n{raw}");
    let header_block = raw
        .split_once("\r\n\r\n")
        .map(|(h, _)| h.to_lowercase())
        .unwrap_or_default();
    assert!(
        header_block.contains("transfer-encoding: chunked"),
        "expected chunked transfer encoding (proof the streaming path is wired); \
         got headers:\n{header_block}",
    );
    assert!(
        !header_block.contains("content-length:"),
        "streaming responses should not advertise a Content-Length; got headers:\n{header_block}",
    );
    assert!(
        body.contains("Hello"),
        "expected 'Hello' to survive the streaming pipeline; got chunked body:\n{body}",
    );
}

/// Issue #1138: a client that reads the response head but then stops
/// draining the body — while keeping the connection open — must not pin
/// the worker's fiber stack forever. The body pump applies a per-frame
/// idle timeout (`--timeout`); once a `frame_tx.send` stalls past it the
/// pump aborts and logs. We assert that abort happens.
#[test]
fn non_draining_client_does_not_pin_worker_stack() {
    let (_guard, port, stderr) = start_serve("serve_big_body.wado", &["--timeout", "2"]);

    // Open the connection, send the request, read just the head plus a
    // little body, then stall: stop reading while holding the socket open.
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).unwrap();

    // Drain a small fixed amount so the response head has definitely been
    // produced, then never read again.
    let mut head = [0u8; 8192];
    let n = stream.read(&mut head).expect("read response head");
    assert!(n > 0, "expected some response bytes before stalling");

    // The connection stays open (`stream` is still in scope) but is no
    // longer drained. Within `--timeout` plus slack the body pump should
    // give up and log the abort. Without the idle timeout it blocks on
    // `frame_tx.send` forever and this loop times out.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if stderr.lock().unwrap().contains("Request body pump aborted") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "body pump did not abort within 30s for a non-draining client; \
             stderr:\n{}",
            stderr.lock().unwrap(),
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Hold the connection until the abort is observed so it is attributed
    // to the stall, not to a client disconnect (which is a separate path).
    drop(stream);
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
