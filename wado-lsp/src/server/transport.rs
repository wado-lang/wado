//! Content-Length-framed JSON-RPC transport for the LSP server.
//!
//! I/O is synchronous (via `std::io`) so the module builds for
//! `wasm32-wasip2`, where tokio's `io-std` feature is not available. The
//! wrapping runtime is still async: dispatch awaits `Engine` queries between
//! blocking `read_message` / `send_*` calls.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::Engine;
use crate::host::FilesystemCompilerHost;
use crate::server::rpc::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PublishDiagnosticsParams, error_codes,
};
use crate::uri::Uri;

const JSONRPC_VERSION: &str = "2.0";

/// Failure mode of [`read_message`].
///
/// `Parse` errors are recoverable: per LSP 3.18 (JSON-RPC 2.0 §5.1), the
/// server should respond with `-32700 ParseError` and keep processing. `Io`
/// errors are unrecoverable — the transport is broken.
#[derive(Debug)]
pub enum ReadError {
    Parse(String),
    Io(String),
}

/// Read one JSON-RPC message (Content-Length framed).
///
/// Returns `Ok(None)` on clean EOF, `Err(ReadError::Parse)` for malformed
/// framing or JSON bodies, and `Err(ReadError::Io)` for transport failures.
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<JsonRpcRequest>, ReadError> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n: usize = reader
            .read_line(&mut header)
            .map_err(|e| ReadError::Io(format!("read error: {e}")))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
            content_length = Some(
                len_str
                    .parse()
                    .map_err(|_| ReadError::Parse(format!("invalid Content-Length: {len_str}")))?,
            );
        }
    }

    let length = content_length
        .ok_or_else(|| ReadError::Parse("missing Content-Length header".to_string()))?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| ReadError::Io(format!("read body error: {e}")))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| ReadError::Parse(format!("invalid JSON: {e}")))
}

fn write_serialized<W: Write>(writer: &mut W, body: String) -> Result<(), String> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .write_all(body.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;
    Ok(())
}

/// Send a typed JSON-RPC response.
pub fn send_response<W: Write, T: Serialize>(
    writer: &mut W,
    id: &Value,
    result: T,
) -> Result<(), String> {
    let msg = JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        result,
    };
    let body = serde_json::to_string(&msg).map_err(|e| format!("serialize error: {e}"))?;
    write_serialized(writer, body)
}

/// Send a typed JSON-RPC notification.
pub fn send_notification<W: Write, T: Serialize>(
    writer: &mut W,
    method: &'static str,
    params: T,
) -> Result<(), String> {
    let msg = JsonRpcNotification {
        jsonrpc: JSONRPC_VERSION,
        method,
        params,
    };
    let body = serde_json::to_string(&msg).map_err(|e| format!("serialize error: {e}"))?;
    write_serialized(writer, body)
}

/// Send a JSON-RPC error response.
pub fn send_error<W: Write>(
    writer: &mut W,
    id: &Value,
    code: i32,
    message: String,
) -> Result<(), String> {
    let msg = JsonRpcErrorResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        error: JsonRpcError { code, message },
    };
    let body = serde_json::to_string(&msg).map_err(|e| format!("serialize error: {e}"))?;
    write_serialized(writer, body)
}

/// Decode request params. On failure, send an `InvalidParams` error response
/// to the client and return `Ok(None)` so the caller can skip the request.
/// Returns `Err` only if the error response itself fails to write.
pub fn decode_or_error<P, W>(writer: &mut W, id: &Value, params: Value) -> Result<Option<P>, String>
where
    P: DeserializeOwned,
    W: Write,
{
    match serde_json::from_value(params) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            send_error(
                writer,
                id,
                error_codes::INVALID_PARAMS,
                format!("invalid params: {e}"),
            )?;
            Ok(None)
        }
    }
}

/// Build a filesystem host rooted at the directory containing `uri`.
///
/// For non-`file:` URIs (`core:`, `wasi:`, `kiln:`, …) there is no
/// meaningful workspace root — the LSP server falls back to the current
/// working directory so a host instance always exists for the resolver
/// pipeline to consult. Resolving relative imports off such URIs is a
/// no-op in practice because the underlying schemes never carry
/// relative-import use sites.
pub fn host_for_uri(uri: &str) -> FilesystemCompilerHost {
    let parsed = Uri::new(uri);
    let base_path = parsed
        .workspace_root()
        .unwrap_or_else(|| PathBuf::from("."));
    FilesystemCompilerHost::new(base_path)
}

/// Publish diagnostics for a document.
pub async fn publish_diagnostics<W: Write>(
    engine: &Engine,
    uri: &str,
    writer: &mut W,
) -> Result<(), String> {
    let host = host_for_uri(uri);
    let diagnostics = engine.diagnostics(uri, &host).await;
    let params = PublishDiagnosticsParams {
        uri: uri.to_string(),
        diagnostics,
    };
    send_notification(writer, "textDocument/publishDiagnostics", params)
}
