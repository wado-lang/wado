use wado_compiler::ast::{self, Item, Stmt};
use wado_compiler::lexer::Lexer;
use wado_compiler::token::{Token, TokenKind};
use wado_compiler::unparse;

use crate::diagnostics::{Position, Range};

/// Result of a hover query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverResult {
    /// Markdown content to display.
    pub contents: String,
    /// Range of the hovered token.
    pub range: Range,
}

/// Compute hover information for the identifier at the given position.
pub fn find_hover(source: &str, position: Position) -> Option<HoverResult> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let (ident_name, token_range) = find_ident_at_position(&tokens, position)?;

    let pr = wado_compiler::parse(source).ok()?;
    let info = find_symbol_info(&pr.ast, &ident_name)?;

    Some(HoverResult {
        contents: format!("```wado\n{info}\n```"),
        range: token_range,
    })
}

fn find_ident_at_position(tokens: &[Token], position: Position) -> Option<(String, Range)> {
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in tokens {
        let name = match &token.kind {
            TokenKind::Ident(name) => name.clone(),
            _ => continue,
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
        return Some((name, range));
    }
    None
}

fn find_symbol_info(module: &ast::Module, name: &str) -> Option<String> {
    for item in &module.items {
        if let Some(info) = item_info(item, name) {
            return Some(info);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hover_function() {
        let source = "fn add(a: i32, b: i32) -> i32 { return a + b; }";
        let result = find_hover(
            source,
            Position {
                line: 0,
                character: 3,
            },
        );
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.contents.contains("fn add(a: i32, b: i32) -> i32"));
    }

    #[test]
    fn test_hover_struct() {
        let source = "struct Point { x: i32, y: i32 }";
        let result = find_hover(
            source,
            Position {
                line: 0,
                character: 7,
            },
        );
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.contents.contains("struct Point"));
    }

    #[test]
    fn test_hover_variable_usage() {
        let source = "fn foo(a: i32) { return a; }";
        let result = find_hover(
            source,
            Position {
                line: 0,
                character: 24,
            },
        );
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.contents.contains("a: i32"));
    }

    #[test]
    fn test_hover_no_result() {
        let source = "fn foo() {}";
        let result = find_hover(
            source,
            Position {
                line: 0,
                character: 2,
            },
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_hover_generic_function() {
        let source = "fn identity<T>(x: T) -> T { return x; }";
        let result = find_hover(
            source,
            Position {
                line: 0,
                character: 3,
            },
        );
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.contents.contains("fn identity<T>(x: T) -> T"));
    }
}
