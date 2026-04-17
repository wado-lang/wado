//! Go-to-definition, powered by `wado_compiler::annotate`.
//!
//! Resolution flow:
//! 1. Lex the source to find the identifier at the cursor.
//! 2. Run `annotate` to produce a fully-resolved [`Annotated`] snapshot.
//! 3. Look up the identifier in the entry module's scope — this traverses
//!    `use` imports, so a name defined in another file resolves to that
//!    file's definition.
//! 4. Translate the resulting [`SymbolKey`] into a [`DefinitionResult`]
//!    (module URI + identifier span).

use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::lexer::Lexer;
use wado_compiler::name::ModuleSource;
use wado_compiler::symbol::Symbol;
use wado_compiler::token::{Span, TokenKind};
use wado_compiler::CompilerHost;

use crate::diagnostics::{Position, Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionResult {
    pub uri: String,
    pub range: Range,
}

/// Find the definition of the identifier at `position` in `source`.
///
/// `uri` is the URI of the document being edited; cross-file results carry
/// their own URI (derived from the defining module's `diagnostic_filename`).
pub async fn find_definition<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Option<DefinitionResult> {
    let ident = find_ident_at_position(source, position)?;

    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let symbol = resolve_ident(&annotated, &filename, &ident)?;
    let span = annotated
        .name_span_of(&symbol.defined_at)
        .or(symbol.span)?;
    let def_uri = symbol_uri(&annotated, symbol, uri)?;
    Some(DefinitionResult {
        uri: def_uri,
        range: span_to_range(&span),
    })
}

/// Scan `tokens` for an identifier whose span covers `position`.
fn find_ident_at_position(source: &str, position: Position) -> Option<String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in &tokens {
        if !token_contains(&token.span, target_line, target_col) {
            continue;
        }
        if let TokenKind::Ident(name) = &token.kind {
            return Some(name.clone());
        }
        if let Some(name) = token.kind.as_ident_name() {
            // Contextual keywords-as-identifiers (`flags`, `type`, `of`, `from`).
            if matches!(
                token.kind,
                TokenKind::Flags | TokenKind::Type | TokenKind::Of | TokenKind::From
            ) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn token_contains(span: &Span, target_line: usize, target_col: usize) -> bool {
    if target_line < span.line || target_line > span.end_line {
        return false;
    }
    if target_line == span.line && target_col < span.column {
        return false;
    }
    if target_line == span.end_line && target_col >= span.end_column {
        return false;
    }
    true
}

fn resolve_ident<'a>(
    annotated: &'a Annotated,
    filename: &str,
    ident: &str,
) -> Option<&'a Symbol> {
    let entry = &annotated.entry_module_source;
    if let Some(sym) = annotated.symbols.lookup_in_module(entry, ident) {
        return Some(sym);
    }
    // Fall back to searching every module: catches definitions hovered while
    // editing a non-entry file.
    let _ = filename;
    annotated
        .modules
        .keys()
        .find_map(|ms| annotated.symbols.lookup_in_module(ms, ident))
}

/// Derive the URI for a symbol's defining module.
///
/// Prefers the URI the request was made against when the symbol lives in the
/// entry module (keeps the `file://` scheme the client expects); otherwise
/// synthesises a `file://` URI from the module's on-disk path.
///
/// `ModuleSource::Local` paths are stored relative to the entry module's
/// directory (see `loader::resolve_module_path`), so they are resolved against
/// the request URI's parent to produce an absolute `file://` URI.
fn symbol_uri(annotated: &Annotated, symbol: &Symbol, request_uri: &str) -> Option<String> {
    let module = &symbol.defined_at.module;
    if module == &annotated.entry_module_source {
        return Some(request_uri.to_string());
    }
    match module {
        ModuleSource::EntryPoint { filename } => Some(filename_to_uri(filename)),
        ModuleSource::Local { path } => Some(resolve_local_uri(path, request_uri)),
        // Core / WASI / Remote modules have no navigable URI.
        _ => None,
    }
}

fn uri_to_filename(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        path.to_string()
    } else {
        uri.to_string()
    }
}

fn filename_to_uri(filename: &str) -> String {
    if filename.starts_with("file://") {
        filename.to_string()
    } else if filename.starts_with('/') {
        format!("file://{filename}")
    } else {
        filename.to_string()
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
    if base_dir.is_empty() {
        filename_to_uri(normalized)
    } else {
        filename_to_uri(&format!("{base_dir}/{normalized}"))
    }
}

fn span_to_range(span: &Span) -> Range {
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

