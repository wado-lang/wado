use wado_compiler::ast::{self, Item, Stmt, Type};
use wado_compiler::lexer::Lexer;
use wado_compiler::token::{Token, TokenKind};

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
///
/// Returns a markdown string describing the symbol (signature, type, kind).
pub fn find_hover(source: &str, position: Position) -> Option<HoverResult> {
    // 1. Lex to find the token at cursor position
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let (ident_name, token_range) = find_ident_at_position(&tokens, position)?;

    // 2. Parse to get AST
    let pr = wado_compiler::parse(source).ok()?;

    // 3. Find matching declaration and format hover info
    let info = find_symbol_info(&pr.ast, &ident_name)?;

    Some(HoverResult {
        contents: format!("```wado\n{info}\n```"),
        range: token_range,
    })
}

/// Find the identifier name and range at the given cursor position.
fn find_ident_at_position(tokens: &[Token], position: Position) -> Option<(String, Range)> {
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in tokens {
        let name = match &token.kind {
            TokenKind::Ident(name) => name.clone(),
            _ => continue,
        };

        if token.span.line == target_line
            && target_col >= token.span.column
            && target_col < token.span.column + (token.span.end - token.span.start)
        {
            let range = Range {
                start: Position {
                    line: (token.span.line - 1) as u32,
                    character: (token.span.column - 1) as u32,
                },
                end: Position {
                    line: (token.span.line - 1) as u32,
                    character: (token.span.column - 1 + token.span.end - token.span.start) as u32,
                },
            };
            return Some((name, range));
        }
    }
    None
}

/// Find the declaration info for a symbol name in the AST.
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
        Item::Function(f) if f.name == name => Some(format_function_sig(f)),
        Item::Struct(s) if s.name == name => Some(format_struct_sig(s)),
        Item::Enum(e) if e.name == name => Some(format_enum_sig(e)),
        Item::Variant(v) if v.name == name => Some(format_variant_sig(v)),
        Item::Flags(fl) if fl.name == name => Some(format!("flags {name}")),
        Item::Trait(t) if t.name == name => Some(format_trait_sig(t)),
        Item::Newtype(n) if n.name == name => Some(format!("type {name} = {}", format_type(&n.ty))),
        Item::Effect(e) if e.name == name => Some(format!("effect {name}")),
        Item::Global(g) if g.name == name => {
            let mut s = String::new();
            if g.is_pub {
                s.push_str("pub ");
            }
            s.push_str("global ");
            if g.mutable {
                s.push_str("mut ");
            }
            s.push_str(&format!("{name}: {}", format_type(&g.ty)));
            Some(s)
        }
        // Check inside impl blocks for methods
        Item::Impl(imp) => {
            for method in &imp.methods {
                if method.name == name {
                    return Some(format_function_sig(method));
                }
            }
            None
        }
        // Check enum/variant cases
        Item::Enum(e) => {
            for case in &e.cases {
                if case.name == name {
                    return Some(format!("{}::{}", e.name, case.name));
                }
            }
            None
        }
        Item::Variant(v) => {
            for case in &v.cases {
                if case.name == name {
                    let payload = case
                        .payload
                        .as_ref()
                        .map(|ty| format!("({})", format_type(ty)))
                        .unwrap_or_default();
                    return Some(format!("{}::{}{}", v.name, case.name, payload));
                }
            }
            None
        }
        Item::Struct(s) => {
            for field in &s.fields {
                if field.name == name {
                    return Some(format!(
                        "{}.{}: {}",
                        s.name,
                        field.name,
                        format_type(&field.ty)
                    ));
                }
            }
            None
        }
        // Check inside trait for methods
        Item::Trait(t) => {
            for method in &t.methods {
                if method.name == name {
                    return Some(format_function_sig(method));
                }
            }
            None
        }
        // Check function params and let bindings
        Item::Function(f) => {
            for param in &f.params {
                if param.name == name && param.self_kind == ast::SelfKind::None {
                    return Some(format!("{name}: {}", format_type(&param.ty)));
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
            if let Some(ty) = &l.ty {
                let mut s = String::new();
                if l.is_mut {
                    s.push_str("let mut ");
                } else {
                    s.push_str("let ");
                }
                s.push_str(&format!("{name}: {}", format_type(ty)));
                return Some(s);
            }
            // No type annotation — can't say much without type inference
            let mut s = String::new();
            if l.is_mut {
                s.push_str("let mut ");
            } else {
                s.push_str("let ");
            }
            s.push_str(name);
            return Some(s);
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

// --- Type formatting ---

fn format_type(ty: &Type) -> String {
    match ty {
        Type::Named(n) => n.name.clone(),
        Type::Generic(g) => {
            let args: Vec<String> = g.args.iter().map(format_type).collect();
            format!("{}<{}>", g.name, args.join(", "))
        }
        Type::NamespacedGeneric(ng) => {
            let args: Vec<String> = ng.args.iter().map(format_type).collect();
            format!("{}::{}<{}>", ng.namespace, ng.name, args.join(", "))
        }
        Type::Function(ft) => {
            let params: Vec<String> = ft.params.iter().map(format_type).collect();
            format!(
                "fn({}) -> {}",
                params.join(", "),
                format_type(&ft.return_type)
            )
        }
        Type::Tuple(types) => {
            let elems: Vec<String> = types.iter().map(format_type).collect();
            format!("[{}]", elems.join(", "))
        }
        Type::Reference(inner) => format!("&{}", format_type(inner)),
        Type::MutReference(inner) => format!("&mut {}", format_type(inner)),
        Type::TypePackSpread(name, _) => format!("..{name}"),
    }
}

fn format_function_sig(f: &ast::Function) -> String {
    let mut sig = String::new();
    if f.is_pub {
        sig.push_str("pub ");
    }
    if f.is_export {
        sig.push_str("export ");
    }
    if f.is_async {
        sig.push_str("async ");
    }
    sig.push_str("fn ");
    sig.push_str(&f.name);

    // Generic params
    if !f.type_params.is_empty() {
        sig.push('<');
        let params: Vec<String> = f
            .type_params
            .iter()
            .map(|tp| {
                let mut s = String::new();
                if tp.is_effect {
                    s.push_str("effect ");
                }
                if tp.is_pack {
                    s.push_str("..");
                }
                s.push_str(&tp.name);
                if !tp.bounds.is_empty() {
                    s.push_str(": ");
                    let bounds: Vec<&str> = tp.bounds.iter().map(|b| b.name.as_str()).collect();
                    s.push_str(&bounds.join(" + "));
                }
                s
            })
            .collect();
        sig.push_str(&params.join(", "));
        sig.push('>');
    }

    sig.push('(');
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| {
            if p.self_kind == ast::SelfKind::Ref {
                "&self".to_string()
            } else if p.self_kind == ast::SelfKind::MutRef {
                "&mut self".to_string()
            } else {
                format!("{}: {}", p.name, format_type(&p.ty))
            }
        })
        .collect();
    sig.push_str(&params.join(", "));
    sig.push(')');

    if let Some(ret) = &f.return_type {
        sig.push_str(" -> ");
        sig.push_str(&format_type(ret));
    }

    if !f.effects.is_empty() {
        sig.push_str(" with ");
        sig.push_str(&f.effects.join(", "));
    }

    sig
}

fn format_struct_sig(s: &ast::StructDecl) -> String {
    let mut sig = String::new();
    if s.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("struct ");
    sig.push_str(&s.name);
    if !s.type_params.is_empty() {
        sig.push('<');
        let params: Vec<&str> = s.type_params.iter().map(|tp| tp.name.as_str()).collect();
        sig.push_str(&params.join(", "));
        sig.push('>');
    }
    sig
}

fn format_enum_sig(e: &ast::EnumDecl) -> String {
    let mut sig = String::new();
    if e.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("enum ");
    sig.push_str(&e.name);
    if !e.type_params.is_empty() {
        sig.push('<');
        let params: Vec<&str> = e.type_params.iter().map(|tp| tp.name.as_str()).collect();
        sig.push_str(&params.join(", "));
        sig.push('>');
    }
    sig
}

fn format_variant_sig(v: &ast::VariantDecl) -> String {
    let mut sig = String::new();
    if v.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("variant ");
    sig.push_str(&v.name);
    if !v.type_params.is_empty() {
        sig.push('<');
        let params: Vec<&str> = v.type_params.iter().map(|tp| tp.name.as_str()).collect();
        sig.push_str(&params.join(", "));
        sig.push('>');
    }
    sig
}

fn format_trait_sig(t: &ast::TraitDecl) -> String {
    let mut sig = String::new();
    if t.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("trait ");
    sig.push_str(&t.name);
    if !t.type_params.is_empty() {
        sig.push('<');
        let params: Vec<&str> = t.type_params.iter().map(|tp| tp.name.as_str()).collect();
        sig.push_str(&params.join(", "));
        sig.push('>');
    }
    sig
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
        // Clicking on `a` in the function body should show its type from the param
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
        // Cursor on whitespace
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
