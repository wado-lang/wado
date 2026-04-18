use std::path::PathBuf;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::compiler_host::FilesystemCompilerHost;
use crate::lsp_rpc::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PublishDiagnosticsParams, error_codes,
};

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
pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<JsonRpcRequest>, ReadError> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n: usize = reader
            .read_line(&mut header)
            .await
            .map_err(|e| ReadError::Io(format!("read error: {e}")))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
            content_length = Some(len_str.parse().map_err(|_| {
                ReadError::Parse(format!("invalid Content-Length: {len_str}"))
            })?);
        }
    }

    let length = content_length
        .ok_or_else(|| ReadError::Parse("missing Content-Length header".to_string()))?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| ReadError::Io(format!("read body error: {e}")))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| ReadError::Parse(format!("invalid JSON: {e}")))
}

async fn write_serialized<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: String,
) -> Result<(), String> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .write_all(body.as_bytes())
        .await
        .map_err(|e| format!("write error: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("flush error: {e}"))?;
    Ok(())
}

/// Send a typed JSON-RPC response.
pub async fn send_response<W: AsyncWrite + Unpin, T: Serialize>(
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
    write_serialized(writer, body).await
}

/// Send a typed JSON-RPC notification.
pub async fn send_notification<W: AsyncWrite + Unpin, T: Serialize>(
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
    write_serialized(writer, body).await
}

/// Send a JSON-RPC error response.
pub async fn send_error<W: AsyncWrite + Unpin>(
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
    write_serialized(writer, body).await
}

/// Decode request params. On failure, send an `InvalidParams` error response
/// to the client and return `Ok(None)` so the caller can skip the request.
/// Returns `Err` only if the error response itself fails to write.
pub async fn decode_or_error<P, W>(
    writer: &mut W,
    id: &Value,
    params: Value,
) -> Result<Option<P>, String>
where
    P: DeserializeOwned,
    W: AsyncWrite + Unpin,
{
    match serde_json::from_value(params) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            send_error(
                writer,
                id,
                error_codes::INVALID_PARAMS,
                format!("invalid params: {e}"),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Build a silent filesystem host rooted at the directory containing `uri`.
pub fn host_for_uri(uri: &str) -> FilesystemCompilerHost {
    let filename = uri.strip_prefix("file://").unwrap_or(uri);
    let base_path = std::path::Path::new(filename)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    FilesystemCompilerHost::silent(base_path)
}

/// Publish diagnostics for a document.
pub async fn publish_diagnostics<W: AsyncWrite + Unpin>(
    engine: &wado_lsp::Engine,
    uri: &str,
    writer: &mut W,
) -> Result<(), String> {
    let host = host_for_uri(uri);
    let diagnostics = engine.diagnostics(uri, &host).await;
    let params = PublishDiagnosticsParams {
        uri: uri.to_string(),
        diagnostics,
    };
    send_notification(writer, "textDocument/publishDiagnostics", params).await
}
