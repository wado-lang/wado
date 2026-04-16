use std::path::PathBuf;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use wado_lsp::Diagnostic;

use crate::compiler_host::FilesystemCompilerHost;

/// Read one JSON-RPC message from stdin (Content-Length framed).
async fn read_message(reader: &mut BufReader<tokio::io::Stdin>) -> Result<Option<Value>, String> {
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

/// Write one JSON-RPC message to stdout (Content-Length framed).
async fn write_message(writer: &mut tokio::io::Stdout, msg: &Value) -> Result<(), String> {
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
async fn send_response(
    writer: &mut tokio::io::Stdout,
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
async fn send_notification(
    writer: &mut tokio::io::Stdout,
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

/// Convert engine diagnostics to LSP JSON format.
fn diagnostics_to_json(diagnostics: &[Diagnostic]) -> Value {
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

/// Publish diagnostics for a document.
async fn publish_diagnostics(
    engine: &wado_lsp::Engine,
    uri: &str,
    writer: &mut tokio::io::Stdout,
) -> Result<(), String> {
    let filename = if let Some(path) = uri.strip_prefix("file://") {
        path
    } else {
        uri
    };
    let base_path = std::path::Path::new(filename)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let host = FilesystemCompilerHost::silent(base_path);

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

/// Run the LSP server over stdio.
pub async fn run() {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    let mut engine = wado_lsp::Engine::new();
    let mut shutdown_requested = false;

    loop {
        let msg = match read_message(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(e) => {
                eprintln!("wado-lsp: {e}");
                break;
            }
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                if let Some(id) = id {
                    let result = json!({
                        "capabilities": {
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 1, // Full sync
                            },
                        },
                        "serverInfo": {
                            "name": "wado-lsp",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    });
                    if let Err(e) = send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "initialized" => {} // no-op
            "shutdown" => {
                shutdown_requested = true;
                if let Some(id) = id
                    && let Err(e) = send_response(&mut writer, id, Value::Null).await
                {
                    eprintln!("wado-lsp: {e}");
                    break;
                }
            }
            "exit" => {
                std::process::exit(i32::from(!shutdown_requested));
            }
            "textDocument/didOpen" => {
                if let (Some(uri), Some(text)) = (
                    params
                        .get("textDocument")
                        .and_then(|td| td.get("uri"))
                        .and_then(Value::as_str),
                    params
                        .get("textDocument")
                        .and_then(|td| td.get("text"))
                        .and_then(Value::as_str),
                ) {
                    engine.open_document(uri, text.to_string());
                    if let Err(e) = publish_diagnostics(&engine, uri, &mut writer).await {
                        eprintln!("wado-lsp: {e}");
                    }
                }
            }
            "textDocument/didChange" => {
                if let Some(uri) = params
                    .get("textDocument")
                    .and_then(|td| td.get("uri"))
                    .and_then(Value::as_str)
                {
                    // Full sync: take the last content change
                    if let Some(text) = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|changes| changes.last())
                        .and_then(|change| change.get("text"))
                        .and_then(Value::as_str)
                    {
                        engine.update_document(uri, text.to_string());
                        if let Err(e) = publish_diagnostics(&engine, uri, &mut writer).await {
                            eprintln!("wado-lsp: {e}");
                        }
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params
                    .get("textDocument")
                    .and_then(|td| td.get("uri"))
                    .and_then(Value::as_str)
                {
                    engine.close_document(uri);
                    // Clear diagnostics for closed document
                    if let Err(e) = send_notification(
                        &mut writer,
                        "textDocument/publishDiagnostics",
                        json!({
                            "uri": uri,
                            "diagnostics": [],
                        }),
                    )
                    .await
                    {
                        eprintln!("wado-lsp: {e}");
                    }
                }
            }
            _ => {
                // Unknown method — send MethodNotFound error for requests (with id)
                if let Some(id) = id {
                    let err_resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("method not found: {method}"),
                        },
                    });
                    if let Err(e) = write_message(&mut writer, &err_resp).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
                // Notifications for unknown methods are silently ignored
            }
        }
    }
}
