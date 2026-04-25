//! Shared helpers for translating between compiler positions/symbols and LSP
//! types. Used by go-to-definition, find-references, and document highlight.

use wado_compiler::annotate::Annotated;
use wado_compiler::name::ModuleSource;
use wado_compiler::symbol::{Symbol, SymbolKey};
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};

pub(crate) fn uri_to_filename(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        path.to_string()
    } else {
        uri.to_string()
    }
}

pub(crate) fn filename_to_uri(filename: &str) -> String {
    if filename.starts_with("file://") {
        filename.to_string()
    } else if filename.starts_with('/') {
        format!("file://{filename}")
    } else {
        filename.to_string()
    }
}

/// Resolve the URI of a module relative to the requesting document's URI.
///
/// `request_uri` provides the base directory used to anchor `./relative.wado`
/// imports. Returns the request URI itself when the module is the entry point.
pub(crate) fn module_uri(
    annotated: &Annotated,
    module: &ModuleSource,
    request_uri: &str,
) -> Option<String> {
    if module == &annotated.entry_module_source {
        return Some(request_uri.to_string());
    }
    match module {
        ModuleSource::EntryPoint { filename } => Some(filename_to_uri(filename)),
        ModuleSource::Local { path } => Some(resolve_local_uri(path, request_uri)),
        ModuleSource::Core { name } => Some(format!("core:{name}")),
        ModuleSource::Wasi { interface } => Some(format!("wasi:{interface}")),
        ModuleSource::Remote { url } => Some(url.clone()),
        // Kiln-redirected modules already carry a fully-qualified URI;
        // hand it to the LSP client unchanged.
        ModuleSource::Redirected { uri } => Some(uri.clone()),
    }
}

fn resolve_local_uri(module_path: &str, request_uri: &str) -> String {
    if module_path.starts_with('/') || module_path.starts_with("file://") {
        return filename_to_uri(module_path);
    }
    let request_path = uri_to_filename(request_uri);
    let base_dir = request_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let normalized = module_path.strip_prefix("./").unwrap_or(module_path);
    // When the request path is rooted at "/" (e.g. "/test.wado"), rsplit_once
    // yields an empty base_dir, so preserve the leading slash explicitly.
    if base_dir.is_empty() {
        if request_path.starts_with('/') {
            filename_to_uri(&format!("/{normalized}"))
        } else {
            filename_to_uri(normalized)
        }
    } else {
        filename_to_uri(&format!("{base_dir}/{normalized}"))
    }
}

/// URI of the module defining `symbol`, relative to the requesting document.
pub(crate) fn symbol_uri(
    annotated: &Annotated,
    symbol: &Symbol,
    request_uri: &str,
) -> Option<String> {
    module_uri(annotated, &symbol.defined_at.module, request_uri)
}

pub(crate) fn span_to_range(span: &Span) -> Range {
    Range {
        start: Position {
            line: span.line.saturating_sub(1) as u32,
            character: span.column.saturating_sub(1) as u32,
        },
        end: Position {
            line: span.end_line.saturating_sub(1) as u32,
            character: span.end_column.saturating_sub(1) as u32,
        },
    }
}

/// Resolve the cursor at `(line, col)` to the [`SymbolKey`] of the binding it
/// names — either via use→def lookup or because the cursor lands directly on a
/// declared symbol. Returns `None` when the cursor is not on a name.
///
/// `line` and `col` are 1-based to match compiler conventions.
pub(crate) fn resolve_def_key(
    annotated: &Annotated,
    module: &ModuleSource,
    line: usize,
    col: usize,
) -> Option<SymbolKey> {
    let ast_id = annotated.ast_id_at(module, line, col)?;
    let cursor_key = SymbolKey::new(module.clone(), ast_id);
    if let Some(def) = annotated.referenced_symbol(&cursor_key) {
        return Some(def);
    }
    if annotated.symbol_at(&cursor_key).is_some() {
        return Some(cursor_key);
    }
    None
}
