//! Content-Length-framed JSON-RPC transport for the LSP server.
//!
//! I/O is synchronous (via `std::io`) so the module builds for
//! `wasm32-wasip2`, where tokio's `io-std` feature is not available. The
//! wrapping runtime is still async: dispatch awaits `Engine` queries between
//! blocking `read_message` / `send_*` calls.

use std::future::Future;
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

/// Largest message body accepted from the client, in bytes.
///
/// The body buffer is allocated eagerly at the declared length, and a buggy
/// client or a desynchronised stream can name any. 64 MiB sits far above a
/// real message and far below an allocation failure.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Failure mode of [`read_message`].
///
/// `Parse` is recoverable: the frame was consumed whole and only its contents
/// were bad, so per LSP 3.18 (JSON-RPC 2.0 §5.1) the server answers
/// `-32700 ParseError` and keeps going.
///
/// `Io` is not: the transport is broken, or the frame boundary is lost. A
/// `Content-Length` we cannot parse or refuse to honour leaves a body we
/// never consumed, so answering and looping would read that body as headers.
#[derive(Debug)]
pub enum ReadError {
    Parse(String),
    Io(String),
}

/// Read one JSON-RPC message (Content-Length framed).
///
/// Returns `Ok(None)` on clean EOF, `Err(ReadError::Parse)` for a body that
/// was read but is not valid JSON, and `Err(ReadError::Io)` for transport
/// failures and for framing we cannot recover from.
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
        // Header names are case-insensitive and the space after the colon is
        // optional (LSP 3.18 §baseProtocol defers to the HTTP header grammar).
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            let value = value.trim();
            // Fatal: without a length we cannot say where this body ends,
            // so every later read starts at an unrecoverable offset.
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| ReadError::Io(format!("invalid Content-Length: {value}")))?,
            );
        }
    }

    let length = content_length
        .ok_or_else(|| ReadError::Parse("missing Content-Length header".to_string()))?;
    if length > MAX_CONTENT_LENGTH {
        // Fatal for the same reason: the declared body is never consumed.
        return Err(ReadError::Io(format!(
            "Content-Length {length} exceeds the {MAX_CONTENT_LENGTH}-byte limit",
        )));
    }
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

/// Run a typed request: decode the JSON params into `P`, invoke
/// `handler(p).await`, and serialize the result as a JSON-RPC response.
///
/// Skips the body when `id` is `None` (notification-shaped envelope on
/// a request method), and bails after sending an `InvalidParams` error
/// when the params don't deserialize.
pub async fn typed_request<P, R, W, F, Fut>(
    writer: &mut W,
    id: Option<&Value>,
    params: Value,
    handler: F,
) -> Result<(), String>
where
    P: DeserializeOwned,
    R: Serialize,
    W: Write,
    F: FnOnce(P) -> Fut,
    Fut: Future<Output = R>,
{
    let Some(id) = id else {
        return Ok(());
    };
    let Some(p) = decode_or_error::<P, _>(writer, id, params)? else {
        return Ok(());
    };
    let result = handler(p).await;
    send_response(writer, id, result)
}

/// Build a filesystem host rooted at the directory containing `uri`.
///
/// For non-`file:` URIs (`core:`, `wasi:`, `kiln:`, …) there is no
/// meaningful workspace root — the LSP server falls back to the current
/// working directory so a host instance always exists for the elaborator
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(input: &[u8]) -> Result<Option<JsonRpcRequest>, ReadError> {
        read_message(&mut std::io::BufReader::new(input))
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn reads_a_well_formed_frame() {
        let msg = read(&frame(r#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#))
            .expect("read")
            .expect("message");
        assert_eq!(msg.method, "shutdown");
    }

    #[test]
    fn content_length_header_is_case_insensitive_and_space_optional() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#;
        let raw = format!("content-length:{}\r\n\r\n{body}", body.len());
        let msg = read(raw.as_bytes()).expect("read").expect("message");
        assert_eq!(msg.method, "shutdown");
    }

    #[test]
    fn oversized_content_length_is_refused_without_allocating() {
        // The buffer is allocated eagerly at the declared size.
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        let err = read(raw.as_bytes()).expect_err("oversized length must be refused");
        assert!(
            matches!(&err, ReadError::Io(m) if m.contains("exceeds")),
            "expected a fatal transport error, got {err:?}",
        );
    }

    #[test]
    fn unparseable_content_length_is_fatal_not_recoverable() {
        // Recovering here would read the unconsumed body as headers.
        let raw = "Content-Length: not-a-number\r\n\r\n{}";
        let err = read(raw.as_bytes()).expect_err("an unparseable length must be refused");
        assert!(
            matches!(&err, ReadError::Io(m) if m.contains("invalid Content-Length")),
            "expected a fatal transport error, got {err:?}",
        );
    }

    #[test]
    fn malformed_json_body_stays_recoverable() {
        // The counter-case: the body was consumed, so the stream stays framed.
        let err = read(&frame("not json")).expect_err("malformed JSON must be reported");
        assert!(
            matches!(&err, ReadError::Parse(m) if m.contains("invalid JSON")),
            "expected a recoverable parse error, got {err:?}",
        );
    }

    #[test]
    fn clean_eof_is_not_an_error() {
        assert!(read(b"").expect("read").is_none());
    }
}
