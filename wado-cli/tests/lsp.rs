//! Integration tests for `wado lsp` and `wado query` subcommands.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use predicates::prelude::*;
use serde_json::{Value, json};

fn wado_bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("wado").into()
}

fn project_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn wado() -> assert_cmd::Command {
    let mut cmd = Command::new(wado_bin());
    cmd.current_dir(project_root());
    cmd.into()
}

// ---------------------------------------------------------------------------
// JSON-RPC helpers
// ---------------------------------------------------------------------------

fn encode_message(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_string(msg).unwrap();
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

// ---------------------------------------------------------------------------
// LSP session helper
// ---------------------------------------------------------------------------

struct LspSession {
    child: std::process::Child,
    next_id: i64,
}

impl LspSession {
    fn start() -> Self {
        let child = Command::new(wado_bin())
            .args(["lsp"])
            .current_dir(project_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn wado lsp");
        Self { child, next_id: 1 }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(&encode_message(msg)).unwrap();
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

    fn send_notification(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Read one message from stdout with a timeout.
    fn read_message(&mut self) -> Value {
        let stdout = self.child.stdout.as_mut().unwrap();

        // Read headers
        let mut header_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stdout.read_exact(&mut byte).expect("read failed");
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        let header = std::str::from_utf8(&header_buf).unwrap();
        let length: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("missing Content-Length")
            .trim()
            .parse()
            .unwrap();

        let mut body = vec![0u8; length];
        stdout.read_exact(&mut body).expect("read body failed");
        serde_json::from_slice(&body).unwrap()
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

    let info = &resp["result"]["serverInfo"];
    assert_eq!(info["name"], "wado-lsp");

    assert_eq!(session.shutdown_and_exit(), 0);
}

#[test]
fn lsp_diagnostics_on_did_open_valid() {
    let mut session = LspSession::start();

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    // Open a valid document
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_valid.wado",
                "languageId": "wado",
                "version": 1,
                "text": "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hello\");\n}\n"
            }
        }),
    );

    let notif = session.read_message();
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

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    // Open a document with a type error
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_invalid.wado",
                "languageId": "wado",
                "version": 1,
                "text": "export fn run() {\n    let x: i32 = \"oops\";\n}\n"
            }
        }),
    );

    let notif = session.read_message();
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

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    // Open with errors
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_change.wado",
                "languageId": "wado",
                "version": 1,
                "text": "export fn run() {\n    let x: i32 = \"oops\";\n}\n"
            }
        }),
    );

    let notif1 = session.read_message();
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

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    // Open with errors
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_test_close.wado",
                "languageId": "wado",
                "version": 1,
                "text": "invalid syntax here ???"
            }
        }),
    );

    let _notif1 = session.read_message(); // diagnostics with errors

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

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    let source =
        "fn helper() -> i32 {\n    return 42;\n}\n\nexport fn run() {\n    let _ = helper();\n}\n";
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_def_same.wado",
                "languageId": "wado",
                "version": 1,
                "text": source,
            }
        }),
    );
    let _diag = session.read_message();

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
    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    let source = std::fs::read_to_string(&main_path).unwrap();
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": main_uri,
                "languageId": "wado",
                "version": 1,
                "text": source,
            }
        }),
    );
    let _diag = session.read_message();

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

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();
    session.send_notification("initialized", json!({}));

    let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\nexport fn run() {\n    let _ = add(1, 2);\n}\n";
    session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///tmp/lsp_hover.wado",
                "languageId": "wado",
                "version": 1,
                "text": source,
            }
        }),
    );
    let _diag = session.read_message();

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
fn lsp_unknown_method_returns_error() {
    let mut session = LspSession::start();

    session.send_request("initialize", json!({ "capabilities": {} }));
    let _init_resp = session.read_message();

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
fn lsp_exit_without_shutdown_returns_nonzero() {
    let mut child = Command::new(wado_bin())
        .args(["lsp"])
        .current_dir(project_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdin = child.stdin.as_mut().unwrap();

    // Send initialize
    let init_msg = encode_message(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "capabilities": {} }
    }));
    stdin.write_all(&init_msg).unwrap();
    stdin.flush().unwrap();

    // Read initialize response (consume it)
    let stdout = child.stdout.as_mut().unwrap();
    let mut header_buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stdout.read_exact(&mut byte).unwrap();
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&header_buf).unwrap();
    let length: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; length];
    stdout.read_exact(&mut body).unwrap();

    // Send exit without shutdown
    let exit_msg = encode_message(&json!({
        "jsonrpc": "2.0", "method": "exit", "params": null
    }));
    stdin.write_all(&exit_msg).unwrap();
    stdin.flush().unwrap();

    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert_eq!(
        status.code().unwrap(),
        1,
        "exit without shutdown should return 1"
    );
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
        .stderr(predicate::str::contains("--json"));
}
