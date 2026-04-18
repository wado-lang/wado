use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::compiler_host::FilesystemCompilerHost;
use crate::lsp_type::{
    self, JsonRpcError, JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PublishDiagnosticsParams,
};

const JSONRPC_VERSION: &str = "2.0";

/// Read one JSON-RPC message (Content-Length framed).
pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<JsonRpcRequest>, String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n: usize = reader
            .read_line(&mut header)
            .await
            .map_err(|e| format!("read error: {e}"))?;
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
                    .map_err(|_| format!("invalid Content-Length: {len_str}"))?,
            );
        }
    }

    let length = content_length.ok_or("missing Content-Length header")?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("read body error: {e}"))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("invalid JSON: {e}"))
}

async fn write_serialized<W: AsyncWrite + Unpin>(writer: &mut W, body: String) -> Result<(), String> {
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

/// Convert engine diagnostics to LSP diagnostics.
pub fn diagnostics_to_lsp(diagnostics: &[wado_lsp::Diagnostic]) -> Vec<lsp_type::Diagnostic> {
    diagnostics
        .iter()
        .map(|d| lsp_type::Diagnostic {
            range: lsp_type::Range {
                start: lsp_type::Position {
                    line: d.range.start.line,
                    character: d.range.start.character,
                },
                end: lsp_type::Position {
                    line: d.range.end.line,
                    character: d.range.end.character,
                },
            },
            severity: Some(d.severity as u32),
            code: Some(d.code.clone()),
            source: Some("wado".to_string()),
            message: d.message.clone(),
        })
        .collect()
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
        diagnostics: diagnostics_to_lsp(&diagnostics),
    };
    send_notification(writer, "textDocument/publishDiagnostics", params).await
}
