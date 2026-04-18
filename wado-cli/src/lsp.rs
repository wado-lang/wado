use serde_json::Value;
use tokio::io::{AsyncWrite, BufReader};

use crate::lsp_adapter::{self, ReadError};
use crate::lsp_rpc::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeResult, JsonRpcRequest, PublishDiagnosticsParams, ReferenceParams, SemanticTokens,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams, ServerCapabilities,
    ServerInfo, TextDocumentPositionParams, TextDocumentSyncOptions, error_codes,
    text_document_sync_kind,
};

/// Tracks server lifecycle per LSP 3.18 §Server lifecycle.
///
/// - `initialized` flips to `true` once the server has *responded* to
///   `initialize`. Before that, requests must be rejected with
///   `ServerNotInitialized` and notifications (except `exit`) must be dropped.
/// - `shutdown_requested` flips to `true` once the server has responded to
///   `shutdown`. After that, every request except `exit` must fail with
///   `InvalidRequest` and every notification except `exit` must be dropped.
#[derive(Default)]
struct Lifecycle {
    initialized: bool,
    shutdown_requested: bool,
}

/// Run the LSP server over stdio.
pub async fn run() {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    let mut engine = wado_lsp::Engine::new();
    let mut lifecycle = Lifecycle::default();

    loop {
        let request = match lsp_adapter::read_message(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(ReadError::Parse(msg)) => {
                // Per JSON-RPC 2.0 §5.1: respond with ParseError (id: null)
                // and keep the loop running so a well-formed follow-up can
                // still be processed.
                if lsp_adapter::send_error(
                    &mut writer,
                    &Value::Null,
                    error_codes::PARSE_ERROR,
                    msg,
                )
                .await
                .is_err()
                {
                    break;
                }
                continue;
            }
            Err(ReadError::Io(msg)) => {
                eprintln!("wado-lsp: {msg}");
                break;
            }
        };

        if let Err(e) = dispatch(&mut engine, &mut writer, &mut lifecycle, request).await {
            eprintln!("wado-lsp: {e}");
            break;
        }
    }
}

async fn dispatch<W: AsyncWrite + Unpin>(
    engine: &mut wado_lsp::Engine,
    writer: &mut W,
    lifecycle: &mut Lifecycle,
    request: JsonRpcRequest,
) -> Result<(), String> {
    let method = request.method.as_str();
    let id = request.id.as_ref();
    let params = request.params;

    // `exit` is always processed, regardless of lifecycle state.
    if method == "exit" {
        std::process::exit(i32::from(!lifecycle.shutdown_requested));
    }

    // Pre-initialize: only `initialize` is allowed. Other requests get
    // -32002 ServerNotInitialized; notifications are dropped silently.
    if !lifecycle.initialized && method != "initialize" {
        if let Some(id) = id {
            lsp_adapter::send_error(
                writer,
                id,
                error_codes::SERVER_NOT_INITIALIZED,
                format!("server not initialized: received {method} before initialize"),
            )
            .await?;
        }
        return Ok(());
    }

    // Post-shutdown: only `exit` is allowed (handled above). Requests get
    // -32600 InvalidRequest; notifications are dropped silently.
    if lifecycle.shutdown_requested {
        if let Some(id) = id {
            lsp_adapter::send_error(
                writer,
                id,
                error_codes::INVALID_REQUEST,
                format!("server is shutting down: rejected {method}"),
            )
            .await?;
        }
        return Ok(());
    }

    match method {
        "initialize" => {
            if let Some(id) = id {
                if lifecycle.initialized {
                    // LSP 3.18 §initialize: the `initialize` request is sent
                    // as the first request from the client. A second one is
                    // a protocol error.
                    lsp_adapter::send_error(
                        writer,
                        id,
                        error_codes::INVALID_REQUEST,
                        "server already initialized".to_string(),
                    )
                    .await?;
                    return Ok(());
                }
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
                lsp_adapter::send_response(writer, id, result).await?;
                lifecycle.initialized = true;
            }
        }
        "initialized" => {} // no-op
        "shutdown" => {
            lifecycle.shutdown_requested = true;
            if let Some(id) = id {
                lsp_adapter::send_response(writer, id, Value::Null).await?;
            }
        }
        "textDocument/didOpen" => {
            let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = &p.text_document.uri;
            engine.open_document(uri, p.text_document.text);
            lsp_adapter::publish_diagnostics(engine, uri, writer).await?;
        }
        "textDocument/didChange" => {
            let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = &p.text_document.uri;
            if let Some(change) = p.content_changes.into_iter().last() {
                engine.update_document(uri, change.text);
                lsp_adapter::publish_diagnostics(engine, uri, writer).await?;
            }
        }
        "textDocument/definition" => {
            if let Some(id) = id {
                let Some(p) = lsp_adapter::decode_or_error::<TextDocumentPositionParams, _>(
                    writer, id, params,
                )
                .await?
                else {
                    return Ok(());
                };
                let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                let result = engine
                    .definition(&p.text_document.uri, p.position, &host)
                    .await;
                lsp_adapter::send_response(writer, id, result).await?;
            }
        }
        "textDocument/hover" => {
            if let Some(id) = id {
                let Some(p) = lsp_adapter::decode_or_error::<TextDocumentPositionParams, _>(
                    writer, id, params,
                )
                .await?
                else {
                    return Ok(());
                };
                let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                let result = engine.hover(&p.text_document.uri, p.position, &host).await;
                lsp_adapter::send_response(writer, id, result).await?;
            }
        }
        "textDocument/references" => {
            if let Some(id) = id {
                let Some(p) =
                    lsp_adapter::decode_or_error::<ReferenceParams, _>(writer, id, params).await?
                else {
                    return Ok(());
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
                lsp_adapter::send_response(writer, id, refs).await?;
            }
        }
        "textDocument/documentHighlight" => {
            if let Some(id) = id {
                let Some(p) = lsp_adapter::decode_or_error::<TextDocumentPositionParams, _>(
                    writer, id, params,
                )
                .await?
                else {
                    return Ok(());
                };
                let host = lsp_adapter::host_for_uri(&p.text_document.uri);
                let highlights = engine
                    .document_highlight(&p.text_document.uri, p.position, &host)
                    .await;
                lsp_adapter::send_response(writer, id, highlights).await?;
            }
        }
        "textDocument/semanticTokens/full" => {
            if let Some(id) = id {
                let Some(p) =
                    lsp_adapter::decode_or_error::<SemanticTokensParams, _>(writer, id, params)
                        .await?
                else {
                    return Ok(());
                };
                let data = engine.semantic_tokens(&p.text_document.uri);
                let result = SemanticTokens { data };
                lsp_adapter::send_response(writer, id, result).await?;
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
            lsp_adapter::send_notification(writer, "textDocument/publishDiagnostics", params)
                .await?;
        }
        _ => {
            if let Some(id) = id {
                lsp_adapter::send_error(
                    writer,
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                )
                .await?;
            }
        }
    }
    Ok(())
}
