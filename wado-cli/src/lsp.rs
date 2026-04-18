use serde_json::Value;
use tokio::io::BufReader;

use crate::lsp_adapter;
use crate::lsp_type::{
    self, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentHighlight, Hover, InitializeResult, Location, MarkupContent, MarkupKind, Position,
    PublishDiagnosticsParams, Range, ReferenceParams, SemanticTokens, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, ServerCapabilities, ServerInfo,
    TextDocumentPositionParams, TextDocumentSyncOptions, error_codes, text_document_sync_kind,
};

/// Run the LSP server over stdio.
pub async fn run() {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    let mut engine = wado_lsp::Engine::new();
    let mut shutdown_requested = false;

    loop {
        let request = match lsp_adapter::read_message(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(e) => {
                eprintln!("wado-lsp: {e}");
                break;
            }
        };

        let method = request.method.as_str();
        let id = request.id.as_ref();
        let params = request.params;

        match method {
            "initialize" => {
                if let Some(id) = id {
                    let result = InitializeResult {
                        capabilities: ServerCapabilities {
                            text_document_sync: Some(TextDocumentSyncOptions {
                                open_close: true,
                                change: text_document_sync_kind::FULL,
                            }),
                            definition_provider: Some(true),
                            hover_provider: Some(true),
                            references_provider: Some(true),
                            document_highlight_provider: Some(true),
                            semantic_tokens_provider: Some(SemanticTokensOptions {
                                legend: SemanticTokensLegend {
                                    token_types: wado_lsp::semantic_tokens::TOKEN_TYPES,
                                    token_modifiers: wado_lsp::semantic_tokens::TOKEN_MODIFIERS,
                                },
                                full: true,
                            }),
                        },
                        server_info: Some(ServerInfo {
                            name: "wado-lsp",
                            version: Some(env!("CARGO_PKG_VERSION")),
                        }),
                    };
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
                let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
                    continue;
                };
                let uri = &p.text_document.uri;
                engine.open_document(uri, p.text_document.text.clone());
                if let Err(e) = lsp_adapter::publish_diagnostics(&engine, uri, &mut writer).await {
                    eprintln!("wado-lsp: {e}");
                }
            }
            "textDocument/didChange" => {
                let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else {
                    continue;
                };
                let uri = &p.text_document.uri;
                if let Some(change) = p.content_changes.into_iter().last() {
                    engine.update_document(uri, change.text);
                    if let Err(e) =
                        lsp_adapter::publish_diagnostics(&engine, uri, &mut writer).await
                    {
                        eprintln!("wado-lsp: {e}");
                    }
                }
            }
            "textDocument/definition" => {
                if let Some(id) = id {
                    let p: TextDocumentPositionParams =
                        serde_json::from_value(params).unwrap_or_else(|_| {
                            TextDocumentPositionParams {
                                text_document: lsp_type::TextDocumentIdentifier {
                                    uri: String::new(),
                                },
                                position: Position {
                                    line: 0,
                                    character: 0,
                                },
                            }
                        });
                    let uri = &p.text_document.uri;
                    let position = wado_lsp::Position {
                        line: p.position.line,
                        character: p.position.character,
                    };
                    let host = lsp_adapter::host_for_uri(uri);
                    let result = match engine.definition(uri, position, &host).await {
                        Some(def) => serde_json::to_value(Location {
                            uri: def.uri,
                            range: Range {
                                start: Position {
                                    line: def.range.start.line,
                                    character: def.range.start.character,
                                },
                                end: Position {
                                    line: def.range.end.line,
                                    character: def.range.end.character,
                                },
                            },
                        })
                        .unwrap_or(Value::Null),
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
                    let p: TextDocumentPositionParams =
                        serde_json::from_value(params).unwrap_or_else(|_| {
                            TextDocumentPositionParams {
                                text_document: lsp_type::TextDocumentIdentifier {
                                    uri: String::new(),
                                },
                                position: Position {
                                    line: 0,
                                    character: 0,
                                },
                            }
                        });
                    let uri = &p.text_document.uri;
                    let position = wado_lsp::Position {
                        line: p.position.line,
                        character: p.position.character,
                    };
                    let host = lsp_adapter::host_for_uri(uri);
                    let result = match engine.hover(uri, position, &host).await {
                        Some(hover) => serde_json::to_value(Hover {
                            contents: MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: hover.contents,
                            },
                            range: Some(Range {
                                start: Position {
                                    line: hover.range.start.line,
                                    character: hover.range.start.character,
                                },
                                end: Position {
                                    line: hover.range.end.line,
                                    character: hover.range.end.character,
                                },
                            }),
                        })
                        .unwrap_or(Value::Null),
                        None => Value::Null,
                    };
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/references" => {
                if let Some(id) = id {
                    let p: ReferenceParams = serde_json::from_value(params).unwrap_or_else(|_| {
                        ReferenceParams {
                            text_document: lsp_type::TextDocumentIdentifier {
                                uri: String::new(),
                            },
                            position: Position {
                                line: 0,
                                character: 0,
                            },
                            context: lsp_type::ReferenceContext::default(),
                        }
                    });
                    let uri = &p.text_document.uri;
                    let position = wado_lsp::Position {
                        line: p.position.line,
                        character: p.position.character,
                    };
                    let host = lsp_adapter::host_for_uri(uri);
                    let refs = engine
                        .references(uri, position, p.context.include_declaration, &host)
                        .await;
                    let locations: Vec<Location> = refs
                        .into_iter()
                        .map(|r| Location {
                            uri: r.uri,
                            range: Range {
                                start: Position {
                                    line: r.range.start.line,
                                    character: r.range.start.character,
                                },
                                end: Position {
                                    line: r.range.end.line,
                                    character: r.range.end.character,
                                },
                            },
                        })
                        .collect();
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, locations).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/documentHighlight" => {
                if let Some(id) = id {
                    let p: TextDocumentPositionParams =
                        serde_json::from_value(params).unwrap_or_else(|_| {
                            TextDocumentPositionParams {
                                text_document: lsp_type::TextDocumentIdentifier {
                                    uri: String::new(),
                                },
                                position: Position {
                                    line: 0,
                                    character: 0,
                                },
                            }
                        });
                    let uri = &p.text_document.uri;
                    let position = wado_lsp::Position {
                        line: p.position.line,
                        character: p.position.character,
                    };
                    let host = lsp_adapter::host_for_uri(uri);
                    let highlights = engine.document_highlight(uri, position, &host).await;
                    let result: Vec<DocumentHighlight> = highlights
                        .into_iter()
                        .map(|h| DocumentHighlight {
                            range: Range {
                                start: Position {
                                    line: h.range.start.line,
                                    character: h.range.start.character,
                                },
                                end: Position {
                                    line: h.range.end.line,
                                    character: h.range.end.character,
                                },
                            },
                            kind: h.kind as u32,
                        })
                        .collect();
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/semanticTokens/full" => {
                if let Some(id) = id {
                    let p: SemanticTokensParams =
                        serde_json::from_value(params).unwrap_or_else(|_| SemanticTokensParams {
                            text_document: lsp_type::TextDocumentIdentifier {
                                uri: String::new(),
                            },
                        });
                    let data = engine.semantic_tokens(&p.text_document.uri);
                    let result = SemanticTokens { data };
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, result).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/didClose" => {
                let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(params) else {
                    continue;
                };
                let uri = &p.text_document.uri;
                engine.close_document(uri);
                let params = PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: Vec::new(),
                };
                if let Err(e) = lsp_adapter::send_notification(
                    &mut writer,
                    "textDocument/publishDiagnostics",
                    params,
                )
                .await
                {
                    eprintln!("wado-lsp: {e}");
                }
            }
            _ => {
                if let Some(id) = id
                    && let Err(e) = lsp_adapter::send_error(
                        &mut writer,
                        id,
                        error_codes::METHOD_NOT_FOUND,
                        format!("method not found: {method}"),
                    )
                    .await
                {
                    eprintln!("wado-lsp: {e}");
                    break;
                }
            }
        }
    }
}
