//! Hover information, powered by `wado_compiler::annotate`.
//!
//! Rendering strategy: lex + parse find the identifier at the cursor and the
//! range of its on-screen span. `annotate` then looks up the name in the
//! enclosing module's scope and pulls the matching AST item from the
//! defining module. `wado_compiler::unparse` renders the signature so the
//! compiler owns formatting of every declaration kind.

use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::ast::{self, Item, Stmt};
use wado_compiler::lexer::Lexer;
use wado_compiler::symbol::Symbol;
use wado_compiler::token::{Token, TokenKind};
use wado_compiler::unparse;

use crate::diagnostics::{Position, Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverResult {
    pub contents: String,
    pub range: Range,
}

pub async fn find_hover<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Option<HoverResult> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let (ident, token_range) = find_ident_at_position(&tokens, position)?;

    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let symbol = annotated
        .symbols
        .lookup_in_module(&annotated.entry_module_source, &ident)
        .or_else(|| {
            annotated
                .modules
                .keys()
                .find_map(|ms| annotated.symbols.lookup_in_module(ms, &ident))
        })?;

    let signature = render_signature(&annotated, symbol)?;

    Some(HoverResult {
        contents: format!("```wado\n{signature}\n```"),
        range: token_range,
    })
}

fn find_ident_at_position(tokens: &[Token], position: Position) -> Option<(String, Range)> {
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in tokens {
        let TokenKind::Ident(name) = &token.kind else {
            continue;
        };
        if target_line < token.span.line || target_line > token.span.end_line {
            continue;
        }
        if target_line == token.span.line && target_col < token.span.column {
            continue;
        }
        if target_line == token.span.end_line && target_col >= token.span.end_column {
            continue;
        }
        let range = Range {
            start: Position {
                line: (token.span.line - 1) as u32,
                character: (token.span.column - 1) as u32,
            },
            end: Position {
                line: (token.span.end_line - 1) as u32,
                character: (token.span.end_column - 1) as u32,
            },
        };
        return Some((name.clone(), range));
    }
    None
}

/// Render a signature for the given symbol by locating its AST item and
/// delegating to `wado_compiler::unparse`.
fn render_signature(annotated: &Annotated, symbol: &Symbol) -> Option<String> {
    let module = annotated.modules.get(&symbol.defined_at.module)?;
    for item in &module.items {
        if let Some(rendered) = item_info(item, &symbol.name) {
            return Some(rendered);
        }
    }
    None
}

fn item_info(item: &Item, name: &str) -> Option<String> {
    match item {
        Item::Function(f) if f.name == name => Some(unparse::unparse_function_signature(f)),
        Item::Struct(s) if s.name == name => Some(unparse::unparse_struct_header(s)),
        Item::Enum(e) if e.name == name => Some(unparse::unparse_enum_header(e)),
        Item::Variant(v) if v.name == name => Some(unparse::unparse_variant_header(v)),
        Item::Flags(fl) if fl.name == name => Some(unparse::unparse_flags_header(fl)),
        Item::Trait(t) if t.name == name => Some(unparse::unparse_trait_header(t)),
        Item::Newtype(n) if n.name == name => Some(unparse::unparse_newtype_signature(n)),
        Item::Effect(e) if e.name == name => Some(format!("effect {name}")),
        Item::Global(g) if g.name == name => Some(unparse::unparse_global_signature(g)),
        Item::Impl(imp) => {
            for method in &imp.methods {
                if method.name == name {
                    return Some(unparse::unparse_function_signature(method));
                }
            }
            None
        }
        Item::Enum(e) => e
            .cases
            .iter()
            .find(|c| c.name == name)
            .map(|c| unparse::unparse_enum_case(&e.name, c)),
        Item::Variant(v) => v
            .cases
            .iter()
            .find(|c| c.name == name)
            .map(|c| unparse::unparse_variant_case(&v.name, c)),
        Item::Struct(s) => s
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| unparse::unparse_struct_field(&s.name, f)),
        Item::Trait(t) => {
            for method in &t.methods {
                if method.name == name {
                    return Some(unparse::unparse_function_signature(method));
                }
            }
            None
        }
        Item::Function(f) => {
            for param in &f.params {
                if param.name == name && param.self_kind == ast::SelfKind::None {
                    let mut out = String::new();
                    unparse::unparse_param_into(param, &mut out);
                    return Some(out);
                }
            }
            if let Some(body) = &f.body {
                return find_let_info(body, name);
            }
            None
        }
        _ => None,
    }
}

fn find_let_info(block: &ast::Block, name: &str) -> Option<String> {
    for stmt in &block.stmts {
        if let Stmt::Let(l) = stmt
            && pattern_matches(&l.pattern, name)
        {
            let mut out = String::new();
            out.push_str(if l.is_mut { "let mut " } else { "let " });
            out.push_str(name);
            if let Some(ty) = &l.ty {
                out.push_str(": ");
                unparse::unparse_type_into(ty, &mut out);
            }
            return Some(out);
        }
    }
    None
}

fn pattern_matches(pattern: &ast::Pattern, name: &str) -> bool {
    match pattern {
        ast::Pattern::Ident(n) | ast::Pattern::MutIdent(n) => n == name,
        _ => false,
    }
}

fn uri_to_filename(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        path.to_string()
    } else {
        uri.to_string()
    }
}
