//! A blocking LSP client over a child process's stdio: one request in flight
//! at a time, and `null` for every request the server sends, since the client
//! declares no capability a server would ask it about.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use wado_lsp::{Uri, UriScheme};

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// The latest `experimental/serverStatus` params, a rust-analyzer extension.
    status: Option<Value>,
    /// What the server answered `initialize` with.
    pub capabilities: Value,
}

impl Client {
    /// Spawns `program` in `root` and completes the `initialize` handshake.
    pub fn start(program: &str, root: &Path, capabilities: Value, options: Value) -> Client {
        let mut child = Command::new(program)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {program}: {e}"));
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut client = Client {
            child,
            stdin,
            stdout,
            next_id: 0,
            status: None,
            capabilities: Value::Null,
        };
        let root_uri = file_uri(root);
        let initialized = client
            .request(
                "initialize",
                json!({
                    "processId": process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "uri": root_uri, "name": "root" }],
                    "capabilities": capabilities,
                    "initializationOptions": options,
                }),
            )
            .unwrap_or_else(|e| panic!("{program} refused to initialize: {e}"));
        client.capabilities = initialized["capabilities"].clone();
        client.notify("initialized", &json!({}));
        client
    }

    /// Sends a request and reads until its response. An error response is
    /// the server's message.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.receive();
            if message.get("method").is_some() {
                self.handle_server_message(message);
                continue;
            }
            assert_eq!(message["id"], id, "responses must arrive in request order");
            if let Some(error) = message.get("error") {
                let message = error["message"].as_str();
                return Err(message
                    .expect("an error response has a message")
                    .to_string());
            }
            return Ok(message["result"].clone());
        }
    }

    pub fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Reads until the server reports `quiescent`, and returns that status.
    pub fn wait_until_quiescent(&mut self) -> Value {
        loop {
            if let Some(status) = &self.status
                && status["quiescent"] == true
            {
                return status.clone();
            }
            let message = self.receive();
            self.handle_server_message(message);
        }
    }

    pub fn shutdown(mut self) {
        self.request("shutdown", Value::Null)
            .unwrap_or_else(|e| panic!("shutdown failed: {e}"));
        self.notify("exit", &Value::Null);
        self.child.wait().expect("failed to wait for the server");
    }

    fn handle_server_message(&mut self, message: Value) {
        if message["method"] == "experimental/serverStatus" {
            self.status = Some(message["params"].clone());
        }
        if let Some(id) = message.get("id") {
            self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": null }));
        }
    }

    fn send(&mut self, message: &Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len())
            .and_then(|()| self.stdin.flush())
            .expect("failed to write to the server");
    }

    fn receive(&mut self) -> Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("failed to read from the server");
            assert!(read > 0, "the server exited");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.parse::<usize>().expect("Content-Length is a number"));
            }
        }
        let mut body = vec![0; length.expect("a message must carry Content-Length")];
        self.stdout
            .read_exact(&mut body)
            .expect("failed to read from the server");
        serde_json::from_slice(&body).expect("a message body must be JSON")
    }
}

pub fn file_uri(path: &Path) -> String {
    assert!(path.is_absolute(), "a file URI needs an absolute path");
    Uri::from_file_path(path).to_string()
}

pub fn uri_path(uri: &str) -> PathBuf {
    let uri = Uri::new(uri);
    assert_eq!(
        uri.scheme(),
        UriScheme::File,
        "the server names files by file URI"
    );
    PathBuf::from(uri.to_filename())
}
