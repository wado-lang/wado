use std::path::PathBuf;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use wado_lsp::Diagnostic;

use crate::compiler_host::FilesystemCompilerHost;

/// Read one JSON-RPC message (Content-Length framed).
pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, String> {
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

/// Write one JSON-RPC message (Content-Length framed).
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Value,
) -> Result<(), String> {
    let body = serde_json::to_string(msg).map_err(|e| format!("serialize error: {e}"))?;
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

/// Send a JSON-RPC response.
pub async fn send_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: &Value,
    result: Value,
) -> Result<(), String> {
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
    .await
}

/// Send a JSON-RPC notification (no id).
pub async fn send_notification<W: AsyncWrite + Unpin>(
    writer: &mut W,
    method: &str,
    params: Value,
) -> Result<(), String> {
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    )
    .await
}

/// Send a JSON-RPC error response.
pub async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: &Value,
    code: i32,
    message: String,
) -> Result<(), String> {
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
    .await
}

/// Convert engine diagnostics to LSP JSON format.
pub fn diagnostics_to_json(diagnostics: &[Diagnostic]) -> Value {
    Value::Array(
        diagnostics
            .iter()
            .map(|d| {
                json!({
                    "range": {
                        "start": {
                            "line": d.range.start.line,
                            "character": d.range.start.character,
                        },
                        "end": {
                            "line": d.range.end.line,
                            "character": d.range.end.character,
                        },
                    },
                    "severity": d.severity as u32,
                    "code": d.code,
                    "source": "wado",
                    "message": d.message,
                })
            })
            .collect(),
    )
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

    send_notification(
        writer,
        "textDocument/publishDiagnostics",
        json!({
            "uri": uri,
            "diagnostics": diagnostics_to_json(&diagnostics),
        }),
    )
    .await
}
