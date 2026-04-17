use serde_json::{Value, json};
use tokio::io::BufReader;

use crate::lsp_adapter;

/// Run the LSP server over stdio.
pub async fn run() {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    let mut engine = wado_lsp::Engine::new();
    let mut shutdown_requested = false;

    loop {
        let msg = match lsp_adapter::read_message(&mut reader).await {
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
                            "definitionProvider": true,
                            "hoverProvider": true,
                            "semanticTokensProvider": {
                                "legend": {
                                    "tokenTypes": wado_lsp::semantic_tokens::TOKEN_TYPES,
                                    "tokenModifiers": wado_lsp::semantic_tokens::TOKEN_MODIFIERS,
                                },
                                "full": true,
                            },
                        },
                        "serverInfo": {
                            "name": "wado-lsp",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    });
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "initialized" => {} // no-op
            "shutdown" => {
                shutdown_requested = true;
                if let Some(id) = id
                    && let Err(e) = lsp_adapter::send_response(&mut writer, id, Value::Null).await
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
                    if let Err(e) =
                        lsp_adapter::publish_diagnostics(&engine, uri, &mut writer).await
                    {
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
                        if let Err(e) =
                            lsp_adapter::publish_diagnostics(&engine, uri, &mut writer).await
                        {
                            eprintln!("wado-lsp: {e}");
                        }
                    }
                }
            }
            "textDocument/definition" => {
                if let Some(id) = id {
                    let uri = params
                        .get("textDocument")
                        .and_then(|td| td.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let line = params
                        .get("position")
                        .and_then(|p| p.get("line"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    let character = params
                        .get("position")
                        .and_then(|p| p.get("character"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    let position = wado_lsp::Position { line, character };
                    let host = lsp_adapter::host_for_uri(uri);
                    let result = match engine.definition(uri, position, &host).await {
                        Some(def) => json!({
                            "uri": def.uri,
                            "range": {
                                "start": {
                                    "line": def.range.start.line,
                                    "character": def.range.start.character,
                                },
                                "end": {
                                    "line": def.range.end.line,
                                    "character": def.range.end.character,
                                },
                            },
                        }),
                        None => Value::Null,
                    };
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/hover" => {
                if let Some(id) = id {
                    let uri = params
                        .get("textDocument")
                        .and_then(|td| td.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let line = params
                        .get("position")
                        .and_then(|p| p.get("line"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    let character = params
                        .get("position")
                        .and_then(|p| p.get("character"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    let position = wado_lsp::Position { line, character };
                    let host = lsp_adapter::host_for_uri(uri);
                    let result = match engine.hover(uri, position, &host).await {
                        Some(hover) => json!({
                            "contents": {
                                "kind": "markdown",
                                "value": hover.contents,
                            },
                            "range": {
                                "start": {
                                    "line": hover.range.start.line,
                                    "character": hover.range.start.character,
                                },
                                "end": {
                                    "line": hover.range.end.line,
                                    "character": hover.range.end.character,
                                },
                            },
                        }),
                        None => Value::Null,
                    };
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/semanticTokens/full" => {
                if let Some(id) = id {
                    let uri = params
                        .get("textDocument")
                        .and_then(|td| td.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let data = engine.semantic_tokens(uri);
                    let result = json!({ "data": data });
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
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
                    if let Err(e) = lsp_adapter::send_notification(
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
                if let Some(id) = id
                    && let Err(e) = lsp_adapter::send_error(
                        &mut writer,
                        id,
                        -32601,
                        format!("method not found: {method}"),
                    )
                    .await
                {
                    eprintln!("wado-lsp: {e}");
                    break;
                }
                // Notifications for unknown methods are silently ignored
            }
        }
    }
}
