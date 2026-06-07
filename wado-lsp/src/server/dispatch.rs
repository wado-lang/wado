//! LSP request dispatcher: routes incoming JSON-RPC methods to `Engine` calls
//! and enforces the server-lifecycle rules from LSP 3.18.

use std::io::Write;

use serde_json::Value;

use crate::Engine;
use crate::server::rpc::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, InitializeResult, InlayHintParams, JsonRpcRequest, PublishDiagnosticsParams,
    ReferenceParams, SemanticTokens, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensParams, ServerCapabilities, ServerInfo, TextDocumentContentOptions,
    TextDocumentContentParams, TextDocumentContentResult, TextDocumentPositionParams,
    TextDocumentSyncOptions, WorkspaceServerCapabilities, error_codes, text_document_sync_kind,
};
use crate::server::transport;
use crate::text::PositionEncoding;

const STDLIB_SCHEMES: &[&str] = &["core", "wasi"];

/// Tracks server lifecycle per LSP 3.18 §Server lifecycle.
///
/// - `initialized` flips to `true` once the server has *responded* to
///   `initialize`. Before that, requests must be rejected with
///   `ServerNotInitialized` and notifications (except `exit`) must be dropped.
/// - `shutdown_requested` flips to `true` once the server has responded to
///   `shutdown`. After that, every request except `exit` must fail with
///   `InvalidRequest` and every notification except `exit` must be dropped.
#[derive(Default)]
pub struct Lifecycle {
    pub initialized: bool,
    pub shutdown_requested: bool,
}

pub async fn dispatch<W: Write>(
    engine: &mut Engine,
    writer: &mut W,
    lifecycle: &mut Lifecycle,
    request: JsonRpcRequest,
) -> Result<(), String> {
    let method = request.method.as_str();
    let id = request.id.as_ref();
    let params = request.params;

    if method == "exit" {
        std::process::exit(i32::from(!lifecycle.shutdown_requested));
    }

    if !lifecycle.initialized && method != "initialize" {
        if let Some(id) = id {
            transport::send_error(
                writer,
                id,
                error_codes::SERVER_NOT_INITIALIZED,
                format!("server not initialized: received {method} before initialize"),
            )?;
        }
        return Ok(());
    }

    if lifecycle.shutdown_requested {
        if let Some(id) = id {
            transport::send_error(
                writer,
                id,
                error_codes::INVALID_REQUEST,
                format!("server is shutting down: rejected {method}"),
            )?;
        }
        return Ok(());
    }

    match method {
        "initialize" => {
            if let Some(id) = id {
                if lifecycle.initialized {
                    transport::send_error(
                        writer,
                        id,
                        error_codes::INVALID_REQUEST,
                        "server already initialized".to_string(),
                    )?;
                    return Ok(());
                }
                let parsed: InitializeParams = serde_json::from_value(params).unwrap_or_default();
                let client_encodings = parsed
                    .capabilities
                    .general
                    .as_ref()
                    .and_then(|g| g.position_encodings.clone())
                    .unwrap_or_default();
                let encoding = PositionEncoding::negotiate(&client_encodings);
                engine.set_position_encoding(encoding);
                let result = InitializeResult {
                    capabilities: ServerCapabilities {
                        position_encoding: Some(encoding.as_wire()),
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
                                token_types: crate::semantic_tokens::TOKEN_TYPES,
                                token_modifiers: crate::semantic_tokens::TOKEN_MODIFIERS,
                            },
                            full: true,
                        }),
                        inlay_hint_provider: Some(true),
                        workspace: Some(WorkspaceServerCapabilities {
                            text_document_content: Some(TextDocumentContentOptions {
                                schemes: STDLIB_SCHEMES,
                            }),
                        }),
                    },
                    server_info: Some(ServerInfo {
                        name: "wado-lsp",
                        version: Some(env!("CARGO_PKG_VERSION")),
                    }),
                };
                transport::send_response(writer, id, result)?;
                lifecycle.initialized = true;
            }
        }
        "initialized" => {}
        "shutdown" => {
            lifecycle.shutdown_requested = true;
            if let Some(id) = id {
                transport::send_response(writer, id, Value::Null)?;
            }
        }
        "textDocument/didOpen" => {
            let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = &p.text_document.uri;
            engine.open_document(uri, p.text_document.text);
            transport::publish_diagnostics(engine, uri, writer).await?;
        }
        "textDocument/didChange" => {
            let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = &p.text_document.uri;
            if let Some(change) = p.content_changes.into_iter().last() {
                engine.update_document(uri, change.text);
                transport::publish_diagnostics(engine, uri, writer).await?;
            }
        }
        "textDocument/definition" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: TextDocumentPositionParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                engine
                    .definition(&p.text_document.uri, p.position, &host)
                    .await
            })
            .await?;
        }
        "textDocument/hover" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: TextDocumentPositionParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                engine.hover(&p.text_document.uri, p.position, &host).await
            })
            .await?;
        }
        "textDocument/references" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: ReferenceParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                engine
                    .references(
                        &p.text_document.uri,
                        p.position,
                        p.context.include_declaration,
                        &host,
                    )
                    .await
            })
            .await?;
        }
        "textDocument/documentHighlight" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: TextDocumentPositionParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                engine
                    .document_highlight(&p.text_document.uri, p.position, &host)
                    .await
            })
            .await?;
        }
        "textDocument/semanticTokens/full" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: SemanticTokensParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                SemanticTokens {
                    data: engine.semantic_tokens(&p.text_document.uri, &host).await,
                }
            })
            .await?;
        }
        "textDocument/inlayHint" => {
            let engine = &*engine;
            transport::typed_request(writer, id, params, async |p: InlayHintParams| {
                let host = transport::host_for_uri(&p.text_document.uri);
                engine
                    .inlay_hints(&p.text_document.uri, p.range, &host)
                    .await
            })
            .await?;
        }
        "workspace/textDocumentContent" => {
            if let Some(id) = id {
                let Some(p) =
                    transport::decode_or_error::<TextDocumentContentParams, _>(writer, id, params)?
                else {
                    return Ok(());
                };
                match engine.text_document_content(&p.uri) {
                    Some(text) => {
                        let result = TextDocumentContentResult {
                            text: text.to_string(),
                        };
                        transport::send_response(writer, id, result)?;
                    }
                    None => {
                        transport::send_error(
                            writer,
                            id,
                            error_codes::REQUEST_FAILED,
                            format!("no bundled content for URI: {}", p.uri),
                        )?;
                    }
                }
            }
        }
        "textDocument/didClose" => {
            let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = &p.text_document.uri;
            engine.close_document(uri);
            let params = PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: Vec::new(),
            };
            transport::send_notification(writer, "textDocument/publishDiagnostics", params)?;
        }
        _ => {
            if let Some(id) = id {
                transport::send_error(
                    writer,
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                )?;
            }
        }
    }
    Ok(())
}
