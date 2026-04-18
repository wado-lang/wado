use serde_json::Value;
use tokio::io::BufReader;

use crate::lsp_adapter;
use crate::lsp_rpc::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeResult, PublishDiagnosticsParams, ReferenceParams, SemanticTokens,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams, ServerCapabilities,
    ServerInfo, TextDocumentPositionParams, TextDocumentSyncOptions, error_codes,
    text_document_sync_kind,
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
                engine.open_document(uri, p.text_document.text);
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
                    let Ok(p) = serde_json::from_value::<TextDocumentPositionParams>(params) else {
                        continue;
                    };
                    let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                    let result = engine
                        .definition(&p.text_document.uri, p.position, &host)
                        .await;
                    let body = serde_json::to_value(result).unwrap_or(Value::Null);
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, body).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/hover" => {
                if let Some(id) = id {
                    let Ok(p) = serde_json::from_value::<TextDocumentPositionParams>(params) else {
                        continue;
                    };
                    let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                    let result = engine.hover(&p.text_document.uri, p.position, &host).await;
                    let body = serde_json::to_value(result).unwrap_or(Value::Null);
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, body).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/references" => {
                if let Some(id) = id {
                    let Ok(p) = serde_json::from_value::<ReferenceParams>(params) else {
                        continue;
                    };
                    let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                    let refs = engine
                        .references(
                            &p.text_document.uri,
                            p.position,
                            p.context.include_declaration,
                            &host,
                        )
                        .await;
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, refs).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/documentHighlight" => {
                if let Some(id) = id {
                    let Ok(p) = serde_json::from_value::<TextDocumentPositionParams>(params) else {
                        continue;
                    };
                    let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                    let highlights = engine
                        .document_highlight(&p.text_document.uri, p.position, &host)
                        .await;
                    if let Err(e) = lsp_adapter::send_response(&mut writer, id, highlights).await {
                        eprintln!("wado-lsp: {e}");
                        break;
                    }
                }
            }
            "textDocument/semanticTokens/full" => {
                if let Some(id) = id {
                    let Ok(p) = serde_json::from_value::<SemanticTokensParams>(params) else {
                        continue;
                    };
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
