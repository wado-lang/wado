use wado_compiler::ast::{self, Expr, Item, Stmt};
use wado_compiler::lexer::Lexer;
use wado_compiler::token::{Span, Token, TokenKind};

use crate::diagnostics::{Position, Range};

/// Result of a go-to-definition query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionResult {
    /// URI of the document containing the definition.
    pub uri: String,
    /// Range of the definition.
    pub range: Range,
}

/// Find the definition location for the identifier at the given position.
///
/// Uses AST-level resolution within the entry file: finds the token at the cursor,
/// then scans all declarations for a matching name.
pub fn find_definition(source: &str, position: Position, uri: &str) -> Option<DefinitionResult> {
    // 1. Lex to find the token at cursor position
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let ident_name = find_ident_at_position(&tokens, position)?;

    // 2. Parse to get AST
    let pr = wado_compiler::parse(source).ok()?;

    // 3. Collect all definitions from AST
    let defs = collect_definitions(&pr.ast);

    // 4. Find matching definition
    let def = defs.iter().find(|d| d.name == ident_name)?;

    Some(DefinitionResult {
        uri: uri.to_string(),
        range: span_to_range(&def.span),
    })
}

/// Find the identifier name at the given cursor position.
fn find_ident_at_position(tokens: &[Token], position: Position) -> Option<String> {
    let target_line = position.line as usize + 1; // LSP 0-based → compiler 1-based
    let target_col = position.character as usize + 1;

    for token in tokens {
        if let TokenKind::Ident(ref name) = token.kind
            && token.span.line == target_line
            && target_col >= token.span.column
            && target_col < token.span.column + (token.span.end - token.span.start)
        {
            return Some(name.clone());
        }
        // Also handle contextual keywords used as identifiers
        if let Some(name) = token.kind.as_ident_name()
            && token.span.line == target_line
            && target_col >= token.span.column
            && target_col < token.span.column + (token.span.end - token.span.start)
        {
            if !matches!(
                token.kind,
                TokenKind::Ident(_)
                    | TokenKind::Flags
                    | TokenKind::Type
                    | TokenKind::Of
                    | TokenKind::From
            ) {
                continue; // Skip real keywords
            }
            return Some(name.to_string());
        }
    }
    None
}

/// A definition found in the AST.
struct Definition {
    name: String,
    span: Span,
}

/// Collect all top-level and nested definitions from the AST module.
fn collect_definitions(module: &ast::Module) -> Vec<Definition> {
    let mut defs = Vec::new();
    for item in &module.items {
        collect_item_defs(&mut defs, item);
    }
    defs
}

fn collect_item_defs(defs: &mut Vec<Definition>, item: &Item) {
    match item {
        Item::Function(f) => {
            defs.push(Definition {
                name: f.name.clone(),
                span: f.span,
            });
            // Collect parameter definitions
            for param in &f.params {
                if param.self_kind == ast::SelfKind::None {
                    defs.push(Definition {
                        name: param.name.clone(),
                        span: param.span,
                    });
                }
            }
            // Collect let bindings inside the function body
            if let Some(body) = &f.body {
                collect_block_defs(defs, body);
            }
        }
        Item::Struct(s) => {
            defs.push(Definition {
                name: s.name.clone(),
                span: s.span,
            });
            for field in &s.fields {
                defs.push(Definition {
                    name: field.name.clone(),
                    span: field.span,
                });
            }
        }
        Item::Enum(e) => {
            defs.push(Definition {
                name: e.name.clone(),
                span: e.span,
            });
            for case in &e.cases {
                defs.push(Definition {
                    name: case.name.clone(),
                    span: case.span,
                });
            }
        }
        Item::Variant(v) => {
            defs.push(Definition {
                name: v.name.clone(),
                span: v.span,
            });
            for case in &v.cases {
                defs.push(Definition {
                    name: case.name.clone(),
                    span: case.span,
                });
            }
        }
        Item::Flags(f) => {
            defs.push(Definition {
                name: f.name.clone(),
                span: f.span,
            });
            for flag in &f.flags {
                defs.push(Definition {
                    name: flag.name.clone(),
                    span: flag.span,
                });
            }
        }
        Item::Trait(t) => {
            defs.push(Definition {
                name: t.name.clone(),
                span: t.span,
            });
            for method in &t.methods {
                defs.push(Definition {
                    name: method.name.clone(),
                    span: method.span,
                });
            }
        }
        Item::Newtype(n) => {
            defs.push(Definition {
                name: n.name.clone(),
                span: n.span,
            });
        }
        Item::Impl(imp) => {
            for method in &imp.methods {
                defs.push(Definition {
                    name: method.name.clone(),
                    span: method.span,
                });
                if let Some(body) = &method.body {
                    collect_block_defs(defs, body);
                }
            }
        }
        Item::Effect(e) => {
            defs.push(Definition {
                name: e.name.clone(),
                span: e.span,
            });
            for method in &e.methods {
                defs.push(Definition {
                    name: method.name.clone(),
                    span: method.span,
                });
            }
        }
        Item::Global(g) => {
            defs.push(Definition {
                name: g.name.clone(),
                span: g.span,
            });
        }
        Item::Use(_)
        | Item::Resource(_)
        | Item::World(_)
        | Item::Test(_)
        | Item::TupleTypeDecl(_) => {}
    }
}

fn collect_block_defs(defs: &mut Vec<Definition>, block: &ast::Block) {
    for stmt in &block.stmts {
        collect_stmt_defs(defs, stmt);
    }
}

fn collect_stmt_defs(defs: &mut Vec<Definition>, stmt: &Stmt) {
    match stmt {
        Stmt::Let(l) => {
            if let Some(name) = pattern_name(&l.pattern) {
                defs.push(Definition { name, span: l.span });
            }
        }
        Stmt::If(i) => {
            collect_block_defs(defs, &i.then_block);
            if let Some(else_block) = &i.else_block {
                collect_block_defs(defs, else_block);
            }
        }
        Stmt::While(w) => collect_block_defs(defs, &w.body),
        Stmt::For(f) => {
            if let Some(init) = &f.init {
                collect_stmt_defs(defs, init);
            }
            collect_block_defs(defs, &f.body);
        }
        Stmt::ForOf(fo) => {
            if let Some(name) = pattern_name(&fo.binding) {
                defs.push(Definition {
                    name,
                    span: fo.span,
                });
            }
            collect_block_defs(defs, &fo.body);
        }
        Stmt::Loop(l) => collect_block_defs(defs, &l.body),
        Stmt::Match(m) => {
            for arm in &m.arms {
                collect_expr_defs(defs, &arm.body);
            }
        }
        Stmt::LabeledBlock(lb) => collect_block_defs(defs, &lb.block),
        _ => {}
    }
}

fn collect_expr_defs(defs: &mut Vec<Definition>, expr: &Expr) {
    match expr {
        Expr::Block(b) => collect_block_defs(defs, b),
        Expr::If(i) => {
            collect_block_defs(defs, &i.then_block);
            if let Some(else_block) = &i.else_block {
                collect_block_defs(defs, else_block);
            }
        }
        Expr::Match(m) => {
            for arm in &m.arms {
                collect_expr_defs(defs, &arm.body);
            }
        }
        Expr::Closure(c) => {
            for param in &c.params {
                // Closure params don't have spans in the AST, skip for now
                let _ = param;
            }
            collect_expr_defs(defs, &c.body);
        }
        Expr::LabeledBlock(lb) => collect_block_defs(defs, &lb.block),
        _ => {}
    }
}

/// Extract the simple identifier name from a pattern, if it is a simple ident pattern.
fn pattern_name(pattern: &ast::Pattern) -> Option<String> {
    match pattern {
        ast::Pattern::Ident(name) | ast::Pattern::MutIdent(name) => Some(name.clone()),
        _ => None,
    }
}

/// Convert a compiler Span (1-based) to an LSP Range (0-based).
fn span_to_range(span: &Span) -> Range {
    Range {
        start: Position {
            line: span.line.saturating_sub(1) as u32,
            character: span.column.saturating_sub(1) as u32,
        },
        end: Position {
            line: span.end_line.saturating_sub(1) as u32,
            // end column not available in Span, use start + length
            character: span.column.saturating_sub(1) as u32 + (span.end - span.start) as u32,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_function_definition() {
        let source = "fn greet() {}\nfn main() { greet(); }";
        // Click on `greet` in the call at line 1, char 12
        let pos = Position {
            line: 1,
            character: 12,
        };
        let result = find_definition(source, pos, "file:///test.wado");
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.range.start.line, 0); // fn greet is on line 0
    }

    #[test]
    fn test_find_struct_definition() {
        let source = "struct Point { x: i32, y: i32 }\nfn foo(p: Point) {}";
        // Click on `Point` in the param type at line 1, char 10
        let pos = Position {
            line: 1,
            character: 10,
        };
        let result = find_definition(source, pos, "file:///test.wado");
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.range.start.line, 0); // struct Point is on line 0
    }

    #[test]
    fn test_no_definition_for_unknown() {
        let source = "fn foo() { bar(); }";
        let pos = Position {
            line: 0,
            character: 11,
        };
        let result = find_definition(source, pos, "file:///test.wado");
        assert!(result.is_none()); // bar is not defined
    }

    #[test]
    fn test_find_ident_at_position() {
        let source = "fn foo() {}";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();
        // `foo` is at column 3 (0-based), line 0 (0-based)
        let name = find_ident_at_position(
            &tokens,
            Position {
                line: 0,
                character: 3,
            },
        );
        assert_eq!(name.as_deref(), Some("foo"));
    }
}
