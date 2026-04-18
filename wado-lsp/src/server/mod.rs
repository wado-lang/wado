//! Stdio LSP server for Wado.
//!
//! This module hosts the JSON-RPC transport, request dispatcher, and wire
//! types. The entrypoint [`run_stdio`] is shared by the native `wado-lsp`
//! binary, by `wado lsp` (which delegates here), and by the `wasm32-wasip2`
//! build used by the VS Code extension.
//!
//! I/O uses `std::io` (blocking) rather than `tokio::io`: tokio's `io-std`
//! feature is not available on `wasm32-*` targets, and stdin/stdout is the
//! single synchronous channel in the LSP loop anyway. Engine queries remain
//! `async` and are awaited between reads.

pub mod dispatch;
pub mod rpc;
pub mod transport;

use std::io::BufReader;

use crate::Engine;
use crate::server::dispatch::Lifecycle;
use crate::server::rpc::error_codes;
use crate::server::transport::ReadError;

/// Run the LSP server over stdio until the client disconnects.
pub async fn run_stdio() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut engine = Engine::new();
    let mut lifecycle = Lifecycle::default();

    loop {
        let request = match transport::read_message(&mut reader) {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(ReadError::Parse(msg)) => {
                // Per JSON-RPC 2.0 §5.1: respond with ParseError (id: null)
                // and keep the loop running so a well-formed follow-up can
                // still be processed.
                if transport::send_error(
                    &mut writer,
                    &serde_json::Value::Null,
                    error_codes::PARSE_ERROR,
                    msg,
                )
                .is_err()
                {
                    break;
                }
                continue;
            }
            Err(ReadError::Io(msg)) => {
                eprintln!("wado-lsp: {msg}");
                break;
            }
        };

        if let Err(e) = dispatch::dispatch(&mut engine, &mut writer, &mut lifecycle, request).await
        {
            eprintln!("wado-lsp: {e}");
            break;
        }
    }
}
