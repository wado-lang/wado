//! Integration tests for `wado lsp` and `wado query` subcommands.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use predicates::prelude::*;
use serde_json::{Value, json};

mod common;
use common::{project_root, wado, wado_bin};

/// Cap on `read_message` so a regression that stops the LSP from
/// responding fails the test in seconds rather than hanging until the
/// outer cargo timeout (often minutes). Generous enough for cold starts
/// and the bundled-stdlib parse on the first request.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// JSON-RPC helpers
// ---------------------------------------------------------------------------

fn encode_message(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_string(msg).unwrap();
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

/// Read one Content-Length-framed JSON-RPC message from `reader`. Returns
/// `Err` on EOF or a malformed frame; the caller is responsible for
/// surfacing the error (typically as a channel close).
fn read_framed<R: Read>(reader: &mut R) -> std::io::Result<Value> {
    let mut header_buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte)?;
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&header_buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let length: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
        })?
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// LSP session helper
// ---------------------------------------------------------------------------

/// Drives a `wado lsp` child process from a sync test thread. The child's
/// stdout is parsed on a background thread and queued onto an mpsc channel
/// so `read_message` can apply a timeout — without the channel, a regressed
/// server that simply stops emitting frames would hang the whole test
/// binary on a blocking `read_exact`.
struct LspSession {
    child: std::process::Child,
    next_id: i64,
    rx: Receiver<Value>,
    stderr_buf: Arc<Mutex<String>>,
}

impl LspSession {
    fn start() -> Self {
        let mut child = Command::new(wado_bin())
            .args(["lsp"])
            .current_dir(project_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn wado lsp");

        let mut stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (tx, rx) = mpsc::channel::<Value>();
        thread::spawn(move || {
            // EOF / parse errors silently terminate the reader; the test
            // thread sees `Disconnected` from `recv_timeout` and panics
            // with the captured stderr.
            while let Ok(msg) = read_framed(&mut stdout) {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });

        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        {
            let buf = Arc::clone(&stderr_buf);
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    let Ok(line) = line else { return };
                    let mut g = buf.lock().unwrap();
                    g.push_str(&line);
                    g.push('\n');
                }
            });
        }

        Self {
            child,
            next_id: 1,
            rx,
            stderr_buf,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(&encode_message(msg)).unwrap();
        stdin.flush().unwrap();
    }

    /// Write a raw byte sequence to the server (for malformed-input tests).
    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
    }

    /// Send a request with a caller-provided id (e.g. a string id).
    fn send_request_with_id(&mut self, id: Value, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
    }

    /// Initialize boilerplate — sends `initialize` + reads response + sends
    /// `initialized`. Returns the `initialize` response for callers that want
    /// to inspect the advertised capabilities.
    fn initialize(&mut self) -> Value {
        self.send_request("initialize", json!({ "capabilities": {} }));
        let resp = self.read_message();
        self.send_notification("initialized", json!({}));
        resp
    }

    /// Open a document and read+return the initial `publishDiagnostics`
    /// notification that the server emits in response.
    fn open_doc(&mut self, uri: &str, text: &str) -> Value {
        self.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "wado",
                    "version": 1,
                    "text": text,
                }
            }),
        );
        self.read_message()
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Read one message from the server with a timeout. Panics with the
    /// captured stderr if the server fails to respond — this turns a
    /// hung-server regression from a multi-minute cargo timeout into a
    /// fast, diagnosable test failure.
    fn read_message(&mut self) -> Value {
        self.read_message_within(READ_TIMEOUT)
    }

    fn read_message_within(&mut self, timeout: Duration) -> Value {
        match self.rx.recv_timeout(timeout) {
            Ok(v) => v,
            Err(RecvTimeoutError::Timeout) => {
                let stderr = self.stderr_buf.lock().unwrap().clone();
                panic!(
                    "wado lsp did not emit a framed response within {timeout:?}.\nstderr:\n{stderr}"
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Reader thread exited (server closed stdout, or framing
                // error). Surface stderr so the failure is diagnosable.
                let stderr = self.stderr_buf.lock().unwrap().clone();
                panic!("wado lsp closed stdout before emitting a response.\nstderr:\n{stderr}");
            }
        }
    }

    /// Send `exit` (with no preceding `shutdown`), close stdin, and wait.
    /// Returns the process exit code. For the happy-path `shutdown` →
    /// `exit` flow, use [`shutdown_and_exit`] instead.
    fn exit_now(mut self) -> i32 {
        self.send_notification("exit", Value::Null);
        drop(self.child.stdin.take());
        self.child.wait().unwrap().code().unwrap_or(-1)
    }

    fn shutdown_and_exit(mut self) -> i32 {
        // shutdown request
        let id = self.send_request("shutdown", Value::Null);
        let resp = self.read_message();
        assert_eq!(resp["id"], id);
        assert_eq!(resp["result"], Value::Null);

        // exit notification
        self.send_notification("exit", Value::Null);

        // Close stdin so the process can exit
        drop(self.child.stdin.take());

        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        // `std::process::Child::drop` is a documented no-op, so a test that
        // panics — e.g. inside `read_message_within` on timeout — would leak
        // the `wado lsp` subprocess and risk flaking the rest of the suite.
        // Best-effort kill+wait covers that path. On the happy
        // `shutdown_and_exit` / `exit_now` paths the child has already
        // exited; both calls return ESRCH/ECHILD, which we ignore.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ===========================================================================
// LSP tests
// ===========================================================================

#[test]
fn lsp_initialize_returns_capabilities() {
    let mut session = LspSession::start();

    let id = session.send_request("initialize", json!({ "capabilities": {} }));

    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let caps = &resp["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["textDocumentSync"]["change"], 1); // Full sync

    let schemes = caps["workspace"]["textDocumentContent"]["schemes"]
        .as_array()
        .expect("workspace.textDocumentContent.schemes should be advertised");
    let schemes: Vec<&str> = schemes.iter().filter_map(Value::as_str).collect();
    assert!(schemes.contains(&"core"));
    assert!(schemes.contains(&"wasi"));

    assert_eq!(
        caps["inlayHintProvider"], true,
        "server should advertise inlayHintProvider",
    );

    let info = &resp["result"]["serverInfo"];
    assert_eq!(info["name"], "wado-lsp");

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_workspace_text_document_content_returns_bundled_stdlib() {
    let mut session = LspSession::start();
    session.initialize();

    let id = session.send_request(
        "workspace/textDocumentContent",
        json!({ "uri": "core:cli" }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let text = resp["result"]["text"]
        .as_str()
        .expect("text field should be present");
    assert!(text.contains("println"));

    let id = session.send_request(
        "workspace/textDocumentContent",
        json!({ "uri": "wasi:filesystem/types.wado" }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let text = resp["result"]["text"].as_str().unwrap();
    assert!(text.contains("Descriptor"));

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_workspace_text_document_content_rejects_unknown_uri() {
    let mut session = LspSession::start();
    session.initialize();

    let id = session.send_request(
        "workspace/textDocumentContent",
        json!({ "uri": "core:does_not_exist" }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    // The request was syntactically valid but the URI doesn't resolve to any
    // bundled module → RequestFailed (-32803) per LSP 3.18, not InvalidParams.
    assert_eq!(resp["error"]["code"].as_i64(), Some(-32803));

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_diagnostics_on_did_open_valid() {
    let mut session = LspSession::start();

    session.initialize();

    // Open a valid document
    let notif = session.open_doc(
        "file:///tmp/lsp_test_valid.wado",
        "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hello\");\n}\n",
    );
    assert_eq!(notif["method"], "textDocument/publishDiagnostics");
    assert_eq!(notif["params"]["uri"], "file:///tmp/lsp_test_valid.wado");

    let diags = notif["params"]["diagnostics"].as_array().unwrap();
    assert!(
        diags.is_empty(),
        "valid file should produce no diagnostics, got: {diags:?}"
    );

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_diagnostics_on_did_open_invalid() {
    let mut session = LspSession::start();

    session.initialize();

    // Open a document with a type error
    let notif = session.open_doc(
        "file:///tmp/lsp_test_invalid.wado",
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    );
    assert_eq!(notif["method"], "textDocument/publishDiagnostics");

    let diags = notif["params"]["diagnostics"].as_array().unwrap();
    assert!(!diags.is_empty(), "invalid file should produce diagnostics");

    let first = &diags[0];
    assert_eq!(first["severity"], 1); // Error
    assert_eq!(first["source"], "wado");
    assert!(
        first["message"].as_str().unwrap().contains("type mismatch"),
        "expected type mismatch, got: {}",
        first["message"]
    );

    // Range should be 0-based
    let start = &first["range"]["start"];
    assert_eq!(start["line"], 1); // line 2, 0-based = 1

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_diagnostics_update_on_did_change() {
    let mut session = LspSession::start();

    session.initialize();

    // Open with errors
    let notif1 = session.open_doc(
        "file:///tmp/lsp_test_change.wado",
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    );
    let diags1 = notif1["params"]["diagnostics"].as_array().unwrap();
    assert!(!diags1.is_empty(), "should have errors initially");

    // Fix the error via didChange
    session.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_change.wado",
                "version": 2,
            },
            "contentChanges": [{
                "text": "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"fixed\");\n}\n"
            }]
        }),
    );

    let notif2 = session.read_message();
    let diags2 = notif2["params"]["diagnostics"].as_array().unwrap();
    assert!(
        diags2.is_empty(),
        "errors should be cleared after fix, got: {diags2:?}"
    );

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_did_close_clears_diagnostics() {
    let mut session = LspSession::start();

    session.initialize();

    // Open with errors — drop the initial `publishDiagnostics`.
    session.open_doc("file:///tmp/lsp_test_close.wado", "invalid syntax here ???");

    // Close the document
    session.send_notification(
        "textDocument/didClose",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_close.wado",
            }
        }),
    );

    let notif2 = session.read_message();
    assert_eq!(notif2["method"], "textDocument/publishDiagnostics");
    let diags = notif2["params"]["diagnostics"].as_array().unwrap();
    assert!(diags.is_empty(), "diagnostics should be cleared on close");

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_definition_same_file() {
    let mut session = LspSession::start();

    session.initialize();

    let source =
        "fn helper() -> i32 {\n    return 42;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n";
    session.open_doc("file:///tmp/lsp_def_same.wado", source);

    let id = session.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_def_same.wado" },
            "position": { "line": 5, "character": 13 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let result = &resp["result"];
    assert!(
        !result.is_null(),
        "definition should be found, got: {result}"
    );
    assert_eq!(result["uri"], "file:///tmp/lsp_def_same.wado");
    assert_eq!(result["range"]["start"]["line"], 0);
    assert_eq!(result["range"]["start"]["character"], 3);

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_definition_cross_file() {
    let tmpdir = tempfile::tempdir().unwrap();
    let helper_path = tmpdir.path().join("helper.wado");
    let main_path = tmpdir.path().join("main.wado");
    std::fs::write(
        &helper_path,
        "pub fn helper() -> i32 {\n    return 42;\n}\n",
    )
    .unwrap();
    std::fs::write(
        &main_path,
        "use { helper } from \"./helper.wado\";\n\nexport fn run() {\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let helper_uri = format!("file://{}", helper_path.display());
    let main_uri = format!("file://{}", main_path.display());

    let mut session = LspSession::start();
    session.initialize();

    let source = std::fs::read_to_string(&main_path).unwrap();
    session.open_doc(&main_uri, &source);

    let id = session.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": main_uri },
            "position": { "line": 3, "character": 13 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let result = &resp["result"];
    assert!(
        !result.is_null(),
        "cross-file definition should be found, got: {result}"
    );
    assert_eq!(result["uri"], helper_uri);
    assert_eq!(result["range"]["start"]["line"], 0);

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_hover_function_signature() {
    let mut session = LspSession::start();

    session.initialize();

    let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\nexport fn run() {\n    let _ = add(1, 2);\n}\n";
    session.open_doc("file:///tmp/lsp_hover.wado", source);

    let id = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_hover.wado" },
            "position": { "line": 5, "character": 13 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let result = &resp["result"];
    assert!(!result.is_null(), "hover should be found, got: {result}");
    let contents = result["contents"]["value"].as_str().unwrap();
    assert!(
        contents.contains("fn add(a: i32, b: i32) -> i32"),
        "hover should show signature, got: {contents}"
    );

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_references_includes_call_sites() {
    let mut session = LspSession::start();

    session.initialize();

    let source = "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n    let _ = helper();\n}\n";
    session.open_doc("file:///tmp/lsp_refs.wado", source);

    let id = session.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_refs.wado" },
            "position": { "line": 0, "character": 4 },
            "context": { "includeDeclaration": true }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let arr = resp["result"].as_array().unwrap();
    assert_eq!(arr.len(), 3, "decl + 2 calls, got: {arr:?}");
    for r in arr {
        assert_eq!(r["uri"], "file:///tmp/lsp_refs.wado");
    }
    // Declaration at line 0, character 3..9 (helper)
    assert_eq!(arr[0]["range"]["start"]["line"], 0);
    assert_eq!(arr[0]["range"]["start"]["character"], 3);
    // Call sites at lines 5 and 6
    assert_eq!(arr[1]["range"]["start"]["line"], 5);
    assert_eq!(arr[2]["range"]["start"]["line"], 6);

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_document_highlight_classifies_read_write() {
    let mut session = LspSession::start();

    session.initialize();

    let source = "fn f() -> i32 {\n    let mut x = 0;\n    x = 1;\n    return x;\n}\n";
    session.open_doc("file:///tmp/lsp_highlight.wado", source);

    let id = session.send_request(
        "textDocument/documentHighlight",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_highlight.wado" },
            "position": { "line": 1, "character": 12 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);

    let arr = resp["result"].as_array().unwrap();
    assert_eq!(arr.len(), 3, "decl + write + read, got: {arr:?}");
    // Declaration: write
    assert_eq!(arr[0]["kind"], 3);
    // x = 1: write
    assert_eq!(arr[1]["range"]["start"]["line"], 2);
    assert_eq!(arr[1]["kind"], 3);
    // return x: read
    assert_eq!(arr[2]["range"]["start"]["line"], 3);
    assert_eq!(arr[2]["kind"], 2);

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_unknown_method_returns_error() {
    let mut session = LspSession::start();
    session.initialize();

    let id = session.send_request(
        "textDocument/unknownMethod",
        json!({
            "textDocument": { "uri": "file:///tmp/test.wado" },
            "position": { "line": 0, "character": 0 }
        }),
    );

    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32601); // MethodNotFound

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_invalid_params_returns_error() {
    let mut session = LspSession::start();
    session.initialize();

    // Send a well-formed request with malformed params (missing required fields).
    let id = session.send_request("textDocument/hover", json!({ "bogus": true }));

    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32602); // InvalidParams

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_exit_without_shutdown_returns_nonzero() {
    // Per LSP 3.18 §exit: an `exit` notification without a preceding
    // `shutdown` request must terminate the server with a non-zero code.
    let mut session = LspSession::start();
    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init = session.read_message();
    assert_eq!(
        session.exit_now(),
        1,
        "exit without shutdown should return 1"
    );
}

// ===========================================================================
// LSP spec conformance tests — base protocol
//
// These tests pin down behaviour described by LSP 3.18 (see wado-lsp/lsp.md):
// JSON-RPC 2.0 framing, id echoing, `$/`-prefixed requests, the default
// pattern for notifications vs. requests, and lifecycle messages.
// ===========================================================================

#[test]
fn lsp_response_includes_jsonrpc_version_field() {
    let mut session = LspSession::start();
    let id = session.send_request("initialize", json!({ "capabilities": {} }));
    let resp = session.read_message();
    assert_eq!(resp["jsonrpc"], "2.0", "response must carry jsonrpc: 2.0");
    assert_eq!(resp["id"], id);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_request_with_string_id_is_echoed() {
    let mut session = LspSession::start();
    session.send_request_with_id(
        json!("init-string-id"),
        "initialize",
        json!({ "capabilities": {} }),
    );
    let resp = session.read_message();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], "init-string-id");
    assert!(resp["result"].is_object(), "got: {resp}");
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_initialize_advertises_all_providers() {
    let mut session = LspSession::start();
    let resp = session.initialize();
    let caps = &resp["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["textDocumentSync"]["change"], 1);
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["referencesProvider"], true);
    assert_eq!(caps["documentHighlightProvider"], true);
    let st = &caps["semanticTokensProvider"];
    assert_eq!(st["full"], true);
    let types = st["legend"]["tokenTypes"].as_array().unwrap();
    assert!(
        types.contains(&json!("keyword")) && types.contains(&json!("function")),
        "token types missing core entries: {types:?}"
    );
    let mods = st["legend"]["tokenModifiers"].as_array().unwrap();
    assert!(mods.contains(&json!("declaration")), "got: {mods:?}");
    assert_eq!(caps["inlayHintProvider"], true);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_inlay_hint_returns_type_and_parameter_hints() {
    // End-to-end wire test: open a document, request inlay hints, and
    // verify the JSON-RPC response carries the expected `Type` (let-binding)
    // and `Parameter` (call-site) hints.
    let mut session = LspSession::start();
    session.initialize();
    let uri = "file:///inlay_test.wado";
    let text = concat!(
        "fn add(a: i32, b: i32) -> i32 { return a + b; }\n",
        "fn run() -> i32 {\n",
        "    let total = add(1, 2);\n",
        "    return total;\n",
        "}\n",
    );
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "wado",
                "version": 1,
                "text": text,
            }
        }),
    );
    // Drain the diagnostics notification that `didOpen` triggers.
    let _ = session.read_message();
    let id = session.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 999, "character": 0 },
            },
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let hints = resp["result"]
        .as_array()
        .expect("result must be an InlayHint[]");
    let labels: Vec<&str> = hints.iter().filter_map(|h| h["label"].as_str()).collect();
    assert!(
        labels.contains(&": i32"),
        "expected `: i32` type hint for `let total = ...`; got {labels:?}",
    );
    assert!(
        labels.contains(&"a:") && labels.contains(&"b:"),
        "expected `a:`/`b:` parameter hints; got {labels:?}",
    );
    // Each hint kind is the wire integer (Type=1, Parameter=2).
    for hint in hints {
        let kind = hint["kind"].as_u64().expect("kind must be a uint");
        assert!(kind == 1 || kind == 2, "unexpected kind: {hint}");
    }
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_initialized_notification_is_accepted() {
    // `initialized` is a notification — it must not yield a response and must
    // not crash the server. We verify by issuing a known request afterwards
    // and confirming the server still responds.
    let mut session = LspSession::start();
    session.send_request("initialize", json!({ "capabilities": {} }));
    let _ = session.read_message();
    session.send_notification("initialized", json!({}));
    // Next request must still succeed.
    let id = session.send_request("shutdown", Value::Null);
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["result"], Value::Null);
    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_dollar_notification_is_silently_ignored() {
    // Per spec §$-notifications: "If a server or client receives notifications
    // starting with '$/' it is free to ignore the notification." We confirm
    // the server keeps running by issuing a normal request afterwards.
    let mut session = LspSession::start();
    session.initialize();
    session.send_notification("$/cancelRequest", json!({ "id": 99 }));
    session.send_notification("$/setTrace", json!({ "value": "off" }));
    let id = session.send_request("shutdown", Value::Null);
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["result"], Value::Null);
    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_dollar_request_returns_method_not_found() {
    // Per spec §$-notifications: "If a server or client receives a request
    // starting with '$/' it must error the request with error code
    // MethodNotFound (e.g. -32601)."
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("$/unknownDollar", json!({}));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32601);
    assert!(resp["error"]["message"].is_string());
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_unknown_notification_is_silently_ignored() {
    // Unknown notifications (no id) must not elicit any response. We confirm
    // by issuing a normal request afterwards and expecting exactly one reply.
    let mut session = LspSession::start();
    session.initialize();
    session.send_notification("workspace/didChangeSomethingBogus", json!({}));
    let id = session.send_request("shutdown", Value::Null);
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_multiple_sequential_requests_all_get_responses() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_seq.wado",
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    );

    let id1 = session.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_seq.wado" },
            "position": { "line": 4, "character": 13 }
        }),
    );
    let id2 = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_seq.wado" },
            "position": { "line": 4, "character": 13 }
        }),
    );
    let id3 = session.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_seq.wado" },
            "position": { "line": 0, "character": 4 },
            "context": { "includeDeclaration": false }
        }),
    );

    let r1 = session.read_message();
    let r2 = session.read_message();
    let r3 = session.read_message();
    assert_eq!(r1["id"], id1);
    assert_eq!(r2["id"], id2);
    assert_eq!(r3["id"], id3);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_content_type_header_is_accepted() {
    // Per spec §header: `Content-Type` is optional and defaults to
    // application/vscode-jsonrpc; charset=utf-8. The server must accept
    // messages carrying an explicit Content-Type header.
    let mut session = LspSession::start();
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "initialize",
        "params": { "capabilities": {} }
    }))
    .unwrap();
    let raw = format!(
        "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
         Content-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    session.send_raw(raw.as_bytes());
    let resp = session.read_message();
    assert_eq!(resp["id"], 7);
    assert!(resp["result"]["capabilities"].is_object());
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — lifecycle
// ===========================================================================

#[test]
fn lsp_shutdown_returns_null_result() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("shutdown", Value::Null);
    let resp = session.read_message();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], id);
    // Spec §shutdown: the response result is `null`.
    assert_eq!(resp["result"], Value::Null);
    // Exit with shutdown flag set → exit code 0.
    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_exit_after_shutdown_returns_zero() {
    let mut session = LspSession::start();
    session.initialize();
    // `shutdown_and_exit` already asserts id echo and null result; we assert
    // the exit-code = 0 contract explicitly here.
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — text document synchronization
// ===========================================================================

#[test]
fn lsp_did_change_full_sync_last_change_wins() {
    // Server advertises Full sync (change: 1). Per spec, the content change
    // array contains the full document text and the last entry is the final
    // document state.
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_full_sync.wado",
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    );
    session.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_full_sync.wado", "version": 2 },
            "contentChanges": [
                { "text": "still broken \"nope\"" },
                { "text": "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"ok\");\n}\n" }
            ]
        }),
    );
    let notif = session.read_message();
    assert_eq!(notif["method"], "textDocument/publishDiagnostics");
    let diags = notif["params"]["diagnostics"].as_array().unwrap();
    assert!(
        diags.is_empty(),
        "last change should win and clear errors, got: {diags:?}"
    );
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_did_change_empty_content_changes_is_noop() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_empty_change.wado",
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    );
    session.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_empty_change.wado", "version": 2 },
            "contentChanges": []
        }),
    );
    // With no content changes, the server should not re-publish diagnostics.
    // Confirm by issuing a request and expecting exactly that request's
    // response (not a stray publishDiagnostics notification).
    let id = session.send_request("shutdown", Value::Null);
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_did_open_on_empty_text_produces_no_errors() {
    let mut session = LspSession::start();
    session.initialize();
    let notif = session.open_doc("file:///tmp/lsp_empty.wado", "");
    assert_eq!(notif["method"], "textDocument/publishDiagnostics");
    // An empty file has no top-level items — compiler may warn or stay silent,
    // but it must not crash the server.
    assert!(notif["params"]["diagnostics"].is_array());
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — language features (empty / null results)
// ===========================================================================

#[test]
fn lsp_definition_returns_null_for_whitespace_position() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_def_null.wado",
        "fn f() -> i32 {\n    return 1;\n}\n",
    );
    let id = session.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_def_null.wado" },
            // Column 0 on the blank `return` line: no identifier at this spot.
            "position": { "line": 1, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    // Spec §definition allows `Location | Location[] | LocationLink[] | null`.
    assert!(
        resp["result"].is_null(),
        "definition on whitespace should be null, got: {}",
        resp["result"]
    );
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_hover_returns_null_for_whitespace_position() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_hover_null.wado",
        "fn f() -> i32 {\n    return 1;\n}\n",
    );
    let id = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_hover_null.wado" },
            "position": { "line": 1, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert!(
        resp["result"].is_null(),
        "hover on whitespace should be null, got: {}",
        resp["result"]
    );
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_references_returns_empty_array_for_whitespace_position() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_refs_empty.wado",
        "fn f() -> i32 {\n    return 1;\n}\n",
    );
    let id = session.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_refs_empty.wado" },
            "position": { "line": 1, "character": 0 },
            "context": { "includeDeclaration": true }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let arr = resp["result"].as_array().expect("references returns array");
    assert!(arr.is_empty(), "got: {arr:?}");
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_document_highlight_returns_empty_array_for_whitespace_position() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_hl_empty.wado",
        "fn f() -> i32 {\n    return 1;\n}\n",
    );
    let id = session.send_request(
        "textDocument/documentHighlight",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_hl_empty.wado" },
            "position": { "line": 1, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let arr = resp["result"]
        .as_array()
        .expect("documentHighlight returns array");
    assert!(arr.is_empty(), "got: {arr:?}");
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_references_without_context_defaults_to_exclude_declaration() {
    // `ReferenceContext` is optional in our RPC shape (`#[serde(default)]`),
    // which means an omitted `context` implies `includeDeclaration: false`.
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_refs_no_context.wado",
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    );
    let id = session.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_refs_no_context.wado" },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let arr = resp["result"].as_array().unwrap();
    // Only the single call-site (line 5 `    let _ = helper();`), no declaration.
    assert_eq!(arr.len(), 1, "got: {arr:?}");
    assert_eq!(arr[0]["range"]["start"]["line"], 5);
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — semantic tokens
// ===========================================================================

#[test]
fn lsp_semantic_tokens_full_returns_data_array() {
    let mut session = LspSession::start();
    session.initialize();
    session.open_doc(
        "file:///tmp/lsp_semtoks.wado",
        "fn run() -> i32 {\n    return 42;\n}\n",
    );
    let id = session.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": "file:///tmp/lsp_semtoks.wado" } }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let data = resp["result"]["data"]
        .as_array()
        .expect("data must be array");
    // Delta-encoded tokens come in 5-tuples. Must be a non-empty multiple of 5.
    assert!(!data.is_empty(), "expected semantic tokens, got none");
    assert_eq!(
        data.len() % 5,
        0,
        "semantic tokens data length must be a multiple of 5: {}",
        data.len()
    );
    for v in data {
        assert!(v.is_number(), "data must contain only integers: {v}");
    }
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_semantic_tokens_unopened_document_returns_empty() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": "file:///tmp/lsp_never_opened.wado" } }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    let data = resp["result"]["data"].as_array().unwrap();
    assert!(data.is_empty(), "got: {data:?}");
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — InvalidParams across all request methods
// ===========================================================================

#[test]
fn lsp_invalid_params_on_definition() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("textDocument/definition", json!({ "nope": true }));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_invalid_params_on_references() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("textDocument/references", json!({ "nope": true }));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_invalid_params_on_document_highlight() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("textDocument/documentHighlight", json!({ "nope": true }));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_invalid_params_on_semantic_tokens() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request(
        "textDocument/semanticTokens/full",
        json!({ "wrong": "shape" }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// LSP spec conformance tests — lifecycle enforcement
//
// Covers the "only `initialize` first" and "no work after `shutdown`" rules
// (LSP 3.18 §Server lifecycle) plus JSON-RPC 2.0 §5.1 parse-error recovery.
// ===========================================================================

#[test]
fn lsp_request_before_initialize_returns_server_not_initialized() {
    let mut session = LspSession::start();
    let id = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/x.wado" },
            "position": { "line": 0, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32002);
    assert!(resp["error"]["message"].is_string());
    // Cannot `shutdown` gracefully here — that would also be rejected with
    // ServerNotInitialized. Send `exit` directly; per spec this is allowed
    // before `initialize` and must exit non-zero when no shutdown was seen.
    assert_eq!(session.exit_now(), 1);
}

#[test]
fn lsp_notification_before_initialize_is_silently_dropped() {
    // Notifications before `initialize` must be dropped (spec §initialize).
    // We confirm by initializing afterwards and seeing a normal response.
    let mut session = LspSession::start();
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_pre_init.wado",
                "languageId": "wado",
                "version": 1,
                "text": "fn f() {}\n"
            }
        }),
    );
    // No publishDiagnostics should have been emitted for that didOpen.
    let id = session.send_request("initialize", json!({ "capabilities": {} }));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert!(resp["result"]["capabilities"].is_object());
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_duplicate_initialize_returns_invalid_request() {
    let mut session = LspSession::start();
    session.initialize();
    let id = session.send_request("initialize", json!({ "capabilities": {} }));
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32600);
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_request_after_shutdown_returns_invalid_request() {
    let mut session = LspSession::start();
    session.initialize();
    let id_shut = session.send_request("shutdown", Value::Null);
    let shut_resp = session.read_message();
    assert_eq!(shut_resp["id"], id_shut);
    assert_eq!(shut_resp["result"], Value::Null);

    let id_req = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/x.wado" },
            "position": { "line": 0, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id_req);
    assert_eq!(resp["error"]["code"], -32600);

    session.send_notification("exit", Value::Null);
    drop(session.child.stdin.take());
    // Exit after shutdown → code 0.
    assert_eq!(session.child.wait().unwrap().code().unwrap(), 0);
}

#[test]
fn lsp_notification_after_shutdown_is_silently_dropped() {
    let mut session = LspSession::start();
    session.initialize();
    let id_shut = session.send_request("shutdown", Value::Null);
    let _ = session.read_message();
    // This didOpen must not be processed and must not produce a
    // publishDiagnostics notification.
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_post_shutdown.wado",
                "languageId": "wado",
                "version": 1,
                "text": "fn f() {}\n"
            }
        }),
    );
    // Follow with a request we expect to fail with InvalidRequest — its
    // response is the *next* message the client sees, proving the stray
    // didOpen did not emit anything.
    let id = session.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///tmp/lsp_post_shutdown.wado" },
            "position": { "line": 0, "character": 0 }
        }),
    );
    let resp = session.read_message();
    assert_eq!(resp["id"], id);
    assert_eq!(resp["error"]["code"], -32600);
    let _ = id_shut;

    assert_eq!(session.exit_now(), 0);
}

#[test]
fn lsp_malformed_json_returns_parse_error_and_keeps_server_alive() {
    let mut session = LspSession::start();
    session.initialize();

    let bogus = b"not valid json at all";
    let header = format!("Content-Length: {}\r\n\r\n", bogus.len());
    session.send_raw(header.as_bytes());
    session.send_raw(bogus);

    let resp = session.read_message();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["error"]["code"], -32700);
    // JSON-RPC 2.0: when the id cannot be determined, response id is null.
    assert!(resp["id"].is_null(), "got: {}", resp["id"]);

    // Server must still be alive — follow-up requests are processed.
    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_missing_content_length_returns_parse_error() {
    let mut session = LspSession::start();
    session.initialize();

    // A terminated header block with no Content-Length: header.
    session.send_raw(b"NoLengthHeader: hello\r\n\r\n");

    let resp = session.read_message();
    assert_eq!(resp["error"]["code"], -32700);
    assert!(resp["id"].is_null());

    assert_eq!(session.shutdown_and_exit(), 0);
}

// ===========================================================================
// `wado query` tests
// ===========================================================================

#[test]
fn query_diagnostics_valid_file() {
    wado()
        .args(["query", "diagnostics", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No diagnostics."));
}

#[test]
fn query_diagnostics_invalid_file() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    )
    .unwrap();

    wado()
        .args(["query", "diagnostics", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("error"))
        .stdout(predicate::str::contains("type mismatch"));
}

#[test]
fn query_diagnostics_json_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "export fn run() {\n    let x: i32 = \"oops\";\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "diagnostics",
            "--json",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let parsed: Vec<Value> =
        serde_json::from_slice(&output).expect("output should be valid JSON array");
    assert!(!parsed.is_empty(), "should have at least one diagnostic");

    let first = &parsed[0];
    assert_eq!(first["severity"], "error");
    assert!(first["message"].as_str().unwrap().contains("type mismatch"));
    assert!(first["range"]["start"]["line"].is_number());
    assert!(first["code"].is_string());
}

#[test]
fn query_diagnostics_json_valid_file() {
    let output = wado()
        .args(["query", "diagnostics", "--json", "example/hello.wado"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: Vec<Value> = serde_json::from_slice(&output).unwrap();
    assert!(
        parsed.is_empty(),
        "valid file should produce empty JSON array"
    );
}

#[test]
fn query_missing_kind() {
    wado()
        .args(["query"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing query kind"));
}

#[test]
fn query_unknown_kind() {
    wado()
        .args(["query", "hover"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown query kind"));
}

#[test]
fn query_missing_file() {
    wado()
        .args(["query", "diagnostics"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing input file"));
}

#[test]
fn query_help() {
    wado()
        .args(["query", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("diagnostics"))
        .stderr(predicate::str::contains("references"))
        .stderr(predicate::str::contains("document-highlight"))
        .stderr(predicate::str::contains("--json"))
        .stderr(predicate::str::contains("--line"))
        .stderr(predicate::str::contains("--column"))
        .stderr(predicate::str::contains("--include-declaration"));
}

#[test]
fn query_references_text_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let path_str = tmp.path().to_str().unwrap();
    let output = wado()
        .args([
            "query",
            "references",
            "--line",
            "1",
            "--column",
            "4",
            path_str,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 call sites, got: {text}");
    assert!(lines[0].ends_with(":6:13"), "got: {}", lines[0]);
    assert!(lines[1].ends_with(":7:13"), "got: {}", lines[1]);
}

#[test]
fn query_references_include_declaration() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "references",
            "--include-declaration",
            "--line",
            "1",
            "--column",
            "4",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected decl + 1 call, got: {text}");
    assert!(lines[0].ends_with(":1:4"), "decl, got: {}", lines[0]);
    assert!(lines[1].ends_with(":6:13"), "call, got: {}", lines[1]);
}

#[test]
fn query_references_json_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "references",
            "--json",
            "--line",
            "1",
            "--column",
            "4",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Vec<Value> = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["range"]["start"]["line"], 5);
    assert_eq!(parsed[0]["range"]["start"]["character"], 12);
    assert!(parsed[0]["uri"].as_str().unwrap().starts_with("file://"));
}

#[test]
fn query_document_highlight_text_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn f() -> i32 {\n    let mut x = 0;\n    x = 1;\n    return x;\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "document-highlight",
            "--line",
            "2",
            "--column",
            "13",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains(":2:13: write"), "decl write, got: {text}");
    assert!(text.contains(":3:5: write"), "x = 1, got: {text}");
    assert!(text.contains(":4:12: read"), "return x, got: {text}");
}

#[test]
fn query_document_highlight_json_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn f() -> i32 {\n    let x = 1;\n    return x;\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "document-highlight",
            "--json",
            "--line",
            "2",
            "--column",
            "9",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Vec<Value> = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0]["kind"], "write");
    assert_eq!(parsed[1]["kind"], "read");
}

#[test]
fn query_definition_text_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let path_str = tmp.path().to_str().unwrap();
    let output = wado()
        .args([
            "query",
            "definition",
            "--line",
            "6",
            "--column",
            "13",
            path_str,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let line = text.trim();
    assert!(line.ends_with(":1:4"), "got: {line}");
}

#[test]
fn query_definition_json_output() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "definition",
            "--json",
            "--line",
            "6",
            "--column",
            "13",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed["range"]["start"]["line"], 0);
    assert_eq!(parsed["range"]["start"]["character"], 3);
    assert!(parsed["uri"].as_str().unwrap().starts_with("file://"));
}

#[test]
fn query_definition_no_match_text() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(tmp.path(), "export fn run() {\n}\n").unwrap();

    let assertion = wado()
        .args([
            "query",
            "definition",
            "--line",
            "2",
            "--column",
            "1",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let raw = assertion.get_output();
    let text = String::from_utf8(raw.stdout.clone()).unwrap();
    let err = String::from_utf8(raw.stderr.clone()).unwrap();
    assert!(text.contains("No definition"), "got: {text}");
    // Valid program — no compile-error warning should appear.
    assert!(
        !err.contains("compile errors"),
        "expected no warning, got: {err}"
    );
}

#[test]
fn query_definition_warns_on_compile_errors() {
    let tmp = tempfile::NamedTempFile::with_suffix(".wado").unwrap();
    std::fs::write(
        tmp.path(),
        "export fn run() {\n    let x: i32 = \"oops\";\n    let y = nonexistent();\n}\n",
    )
    .unwrap();

    let output = wado()
        .args([
            "query",
            "definition",
            "--line",
            "3",
            "--column",
            "14",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(output).unwrap();
    assert!(
        err.contains("warning: result may be incomplete due to compile errors:"),
        "got: {err}"
    );
    assert!(
        err.contains("type mismatch") || err.contains("nonexistent"),
        "expected error details, got: {err}"
    );
}

#[test]
fn query_definition_requires_line_and_column() {
    wado()
        .args(["query", "definition", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--line is required"));
}

#[test]
fn query_references_requires_line_and_column() {
    wado()
        .args(["query", "references", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--line is required"));
}

#[test]
fn query_unknown_kind_lists_available_kinds() {
    let output = wado()
        .args(["query", "hover"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert_eq!(
        stderr,
        "Error: unknown query kind 'hover'. Available: diagnostics, references, document-highlight, definition\n",
    );
}
