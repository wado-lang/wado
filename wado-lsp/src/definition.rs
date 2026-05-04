//! Go-to-definition, powered by `wado_compiler::annotate`.
//!
//! Resolution flow:
//! 1. Run `annotate` to produce a fully-resolved [`Annotated`] snapshot.
//! 2. Use `Annotated::ast_id_at` to find the innermost AST node at the cursor.
//! 3. If that node is a use-site (Ident of a local), follow
//!    `Annotated::referenced_symbol` to the binding [`SymbolKey`].
//! 4. Otherwise the cursor AST id itself points at a declared symbol.
//! 5. Translate the resulting [`SymbolKey`] into a [`DefinitionResult`].

use serde::{Deserialize, Serialize};
use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::ast::{self, AstVisitor, Item, Literal, Module};
use wado_compiler::name::resolve_import_with_entry;
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};
use crate::location::{module_uri, span_to_range, symbol_uri, uri_to_filename};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    // Priority: file-path jumps first, symbol-based resolution second.
    //
    // `use { ... } from "./path"` source strings and `#include_str("./path")`
    // / `#include_bytes("./path")` literals do not carry any `AstId` that the
    // symbol table can resolve — their only target is the file on disk.
    // `resolve_def_key` would always return `None` for cursor positions inside
    // those spans, so running the file-path matcher first lets us answer
    // before falling through to `AstId`-based resolution.
    //
    // The two paths do not overlap in today's AST: path-literal spans belong
    // to `UseDecl.source_span` and `Literal::{IncludeStr,IncludeBytes}`, both
    // of which sit *outside* any identifier, type-name, or expression node
    // that could yield a symbol. Swapping the order would therefore produce
    // the same answer today — but only by accident. If a future AST change
    // introduces overlap (e.g. an `AstId` attached to the string literal
    // itself), keeping file-path resolution first preserves the current
    // behaviour: jump to the file, not to whatever symbol happens to share
    // the span.
    if let Some(result) = file_path_definition(&annotated, &module, line, col, uri) {
        return Some(result);
    }

    let cursor = annotated.cursor_at(&module, line, col)?;
    let def_key = cursor.def_key()?;

    // Most defs are registered as symbols (functions, types, globals, locals).
    // Struct fields / enum cases / variant cases / flags members / trait and
    // impl methods are addressable by `AstId` via `name_span_of` but are not
    // individually registered as symbols; the URI fallback handles both.
    let span = cursor.def_span()?;
    let def_uri = match annotated.symbol_at(&def_key) {
        Some(symbol) => symbol_uri(&annotated, symbol, uri)?,
        None => module_uri(&annotated, &def_key.module, uri)?,
    };
    Some(DefinitionResult {
        uri: def_uri,
        range: span_to_range(&span),
    })
}

/// Resolve a cursor landing on an on-disk file path (a `use ... from "./x"`
/// source string or a `#include_str("./x")` / `#include_bytes("./x")` path)
/// to the beginning of the referenced file. Returns `None` when the cursor is
/// not on such a path.
fn file_path_definition(
    annotated: &Annotated,
    module: &wado_compiler::name::ModuleSource,
    line: usize,
    col: usize,
    request_uri: &str,
) -> Option<DefinitionResult> {
    let ast_module = annotated.modules.get(module)?;
    let path = find_file_path_at_cursor(ast_module, line, col)?;
    let target_module = resolve_import_with_entry(
        &mut annotated.interner.borrow_mut(),
        module,
        &path,
        Some(&annotated.entry_module_source),
    );
    let target_uri = module_uri(annotated, &target_module, request_uri)?;
    Some(DefinitionResult {
        uri: target_uri,
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
    })
}

/// Walk the module AST looking for a file-path node whose span covers
/// `(line, col)`. Currently recognises `use ... from "source"` string literals
/// and `#include_str` / `#include_bytes` path arguments.
fn find_file_path_at_cursor(module: &Module, line: usize, col: usize) -> Option<String> {
    for item in &module.items {
        if let Item::Use(use_decl) = item
            && span_contains(&use_decl.source_span, line, col)
        {
            return Some(use_decl.source.clone());
        }
    }
    let mut finder = IncludePathFinder {
        line,
        col,
        result: None,
    };
    for item in &module.items {
        finder.visit_item(item);
        if finder.result.is_some() {
            break;
        }
    }
    finder.result
}

struct IncludePathFinder {
    line: usize,
    col: usize,
    result: Option<String>,
}

impl AstVisitor for IncludePathFinder {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        if self.result.is_some() {
            return;
        }
        if let ast::Expr::Literal(lit) = expr
            && span_contains(&lit.span, self.line, self.col)
        {
            match &lit.value {
                Literal::IncludeStr(path) | Literal::IncludeBytes(path) => {
                    self.result = Some(path.clone());
                    return;
                }
                _ => {}
            }
        }
        ast::walk_expr(self, expr);
    }
}

fn span_contains(span: &Span, line: usize, col: usize) -> bool {
    if line < span.line || line > span.end_line {
        return false;
    }
    if line == span.line && col < span.column {
        return false;
    }
    if line == span.end_line && col > span.end_column {
        return false;
    }
    true
}
