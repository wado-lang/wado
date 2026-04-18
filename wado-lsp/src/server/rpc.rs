//! LSP JSON-RPC envelope and request/response param types used by the stdio
//! server.
//!
//! Semantic LSP types (Position, Range, Diagnostic, ...) live at the crate root
//! and are reused here. This module only defines the protocol-handling layer:
//! JSON-RPC framing, request param shapes, server capabilities.
//!
//! Spec reference:
//! <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/>.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Position;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i32>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextDocumentContentChangeEvent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentPositionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    #[serde(default)]
    pub context: ReferenceContext,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceContext {
    #[serde(default)]
    pub include_declaration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticTokens {
    pub data: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<crate::Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<ServerInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<&'static str>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_document_sync: Option<TextDocumentSyncOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_provider: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover_provider: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references_provider: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_highlight_provider: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_tokens_provider: Option<SemanticTokensOptions>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentSyncOptions {
    pub open_close: bool,
    pub change: u32,
}

pub mod text_document_sync_kind {
    pub const NONE: u32 = 0;
    pub const FULL: u32 = 1;
    pub const INCREMENTAL: u32 = 2;
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticTokensOptions {
    pub legend: SemanticTokensLegend,
    pub full: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensLegend {
    pub token_types: &'static [&'static str],
    pub token_modifiers: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse<'a, T: Serialize> {
    pub jsonrpc: &'static str,
    pub id: &'a Value,
    pub result: T,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcErrorResponse<'a> {
    pub jsonrpc: &'static str,
    pub id: &'a Value,
    pub error: JsonRpcError,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification<T: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: T,
}

pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    /// Server received a request/notification before `initialize`.
    /// LSP 3.18 §initialize.
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
}
