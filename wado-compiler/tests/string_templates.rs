//! Tests for string template interpolation
//!
//! String templates use backticks and allow interpolation with {expr} syntax.
//! They also support Python-like format specifiers, e.g., {pi:.2f}

use wado_compiler::{Lexer, Parser};

/// Parse a simple expression and return the AST
fn parse_expr(source: &str) -> Result<wado_compiler::ast::Module, String> {
    // Wrap the expression in a function to make it a valid module
    let wrapped = format!("fn test() {{ let x = {source}; }}");

    let mut lexer = Lexer::new(&wrapped);
    let tokens = lexer.tokenize().map_err(|e| e.message)?;

    let mut parser = Parser::new(tokens);
    let module = parser.parse();
    match parser.take_errors().into_iter().next() {
        Some(e) => Err(e.message),
        None => Ok(module),
    }
}

/// Extract the expression from a parsed let statement
fn extract_expr(module: &wado_compiler::ast::Module) -> Option<&wado_compiler::ast::Expr> {
    use wado_compiler::ast::{Item, Stmt};

    let item = module.items.first()?;
    let Item::Function(func) = item else {
        return None;
    };
    let body = func.body.as_ref()?;
    let stmt = body.stmts.first()?;
    let Stmt::Let(let_stmt) = stmt else {
        return None;
    };
    let_stmt.value.as_ref()
}

#[test]
fn test_template_string_empty() {
    let module = parse_expr("``").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(
                template.parts.len(),
                0,
                "expected no parts in empty template"
            );
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_string_plain_text() {
    let module = parse_expr("`hello world`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(template.parts.len(), 1, "expected 1 part");
            match &template.parts[0] {
                wado_compiler::ast::TemplatePart::String(s) => {
                    assert_eq!(s, "hello world");
                }
                other => panic!("expected String part, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_string_single_interpolation() {
    let module = parse_expr("`Hello, {name}!`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(
                template.parts.len(),
                3,
                "expected 3 parts: text, interp, text"
            );

            // Part 0: "Hello, "
            match &template.parts[0] {
                wado_compiler::ast::TemplatePart::String(s) => {
                    assert_eq!(s, "Hello, ");
                }
                other => panic!("expected String part, got {other:?}"),
            }

            // Part 1: {name}
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, format } => {
                    assert!(format.is_none(), "expected no format spec");
                    match &**expr {
                        wado_compiler::ast::Expr::Ident(ident) => {
                            assert_eq!(ident.name, "name");
                        }
                        other => panic!("expected Ident, got {other:?}"),
                    }
                }
                other => panic!("expected Interpolation part, got {other:?}"),
            }

            // Part 2: "!"
            match &template.parts[2] {
                wado_compiler::ast::TemplatePart::String(s) => {
                    assert_eq!(s, "!");
                }
                other => panic!("expected String part, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_string_multiple_interpolations() {
    let module = parse_expr("`{a} + {b} = {c}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            // Should have 5 parts: {a}, " + ", {b}, " = ", {c}
            assert_eq!(template.parts.len(), 5, "expected 5 parts");

            // Verify it's alternating interpolations and strings
            assert!(matches!(
                &template.parts[0],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
            assert!(matches!(
                &template.parts[1],
                wado_compiler::ast::TemplatePart::String(_)
            ));
            assert!(matches!(
                &template.parts[2],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
            assert!(matches!(
                &template.parts[3],
                wado_compiler::ast::TemplatePart::String(_)
            ));
            assert!(matches!(
                &template.parts[4],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_string_expression_interpolation() {
    let module = parse_expr("`Result: {x + y}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(template.parts.len(), 2, "expected 2 parts");

            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, .. } => {
                    // Should be a binary expression (x + y)
                    assert!(matches!(&**expr, wado_compiler::ast::Expr::Binary(_)));
                }
                other => panic!("expected Interpolation with binary expr, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_format_simple() {
    let module = parse_expr("`Pi: {pi:.2f}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, format } => {
                    // Check expression is 'pi'
                    match &**expr {
                        wado_compiler::ast::Expr::Ident(ident) => {
                            assert_eq!(ident.name, "pi");
                        }
                        other => panic!("expected Ident, got {other:?}"),
                    }

                    // Check format spec
                    let format_spec = format.as_ref().expect("expected format spec");
                    assert_eq!(format_spec.spec, ".2f", "expected '.2f' format spec");
                }
                other => panic!("expected Interpolation, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_format_zero_padding() {
    let module = parse_expr("`Value: {x:0.3f}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => match &template.parts[1] {
            wado_compiler::ast::TemplatePart::Interpolation { format, .. } => {
                let format_spec = format.as_ref().expect("expected format spec");
                assert_eq!(format_spec.spec, "0.3f");
            }
            other => panic!("expected Interpolation, got {other:?}"),
        },
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_format_width() {
    let module = parse_expr("`{value:10}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => match &template.parts[0] {
            wado_compiler::ast::TemplatePart::Interpolation { format, .. } => {
                let format_spec = format.as_ref().expect("expected format spec");
                assert_eq!(format_spec.spec, "10");
            }
            other => panic!("expected Interpolation, got {other:?}"),
        },
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_double_colon_not_format() {
    // Module::function() should parse :: as scope resolution, not format spec
    let module = parse_expr("`Value: {Module::function()}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, format } => {
                    // Should have NO format spec
                    assert!(
                        format.is_none(),
                        "expected no format spec, :: should be part of expression"
                    );

                    // The expression should be a function call
                    assert!(matches!(&**expr, wado_compiler::ast::Expr::Call(_)));
                }
                other => panic!("expected Interpolation, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_colon_alone_is_format() {
    // Single colon should start a format spec
    let module = parse_expr("`{x:d}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => match &template.parts[0] {
            wado_compiler::ast::TemplatePart::Interpolation { format, .. } => {
                let format_spec = format.as_ref().expect("expected format spec");
                assert_eq!(format_spec.spec, "d");
            }
            other => panic!("expected Interpolation, got {other:?}"),
        },
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_nested() {
    // Nested template: `Outer {`Inner {x}`}`
    let module = parse_expr("`Outer {`Inner {x}`}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(
                template.parts.len(),
                2,
                "expected 2 parts: text and interpolation"
            );

            // Second part should be an interpolation with a nested template
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, .. } => {
                    // The interpolated expression should itself be a template string
                    assert!(matches!(
                        &**expr,
                        wado_compiler::ast::Expr::TemplateString(_)
                    ));
                }
                other => panic!("expected Interpolation, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_consecutive_interpolations() {
    // No text between interpolations
    let module = parse_expr("`{a}{b}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(template.parts.len(), 2, "expected 2 interpolations");
            assert!(matches!(
                &template.parts[0],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
            assert!(matches!(
                &template.parts[1],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_starts_with_interpolation() {
    let module = parse_expr("`{x} is the value`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(template.parts.len(), 2);
            assert!(matches!(
                &template.parts[0],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_ends_with_interpolation() {
    let module = parse_expr("`The value is {x}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            assert_eq!(template.parts.len(), 2);
            assert!(matches!(
                &template.parts[1],
                wado_compiler::ast::TemplatePart::Interpolation { .. }
            ));
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_escape_sequences() {
    let module = parse_expr(r"`Line 1\nLine 2\ttab`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => match &template.parts[0] {
            wado_compiler::ast::TemplatePart::String(s) => {
                assert_eq!(s, r"Line 1\nLine 2\ttab");
            }
            other => panic!("expected String part, got {other:?}"),
        },
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_unterminated() {
    let result = parse_expr("`unterminated");
    assert!(result.is_err(), "expected error for unterminated template");
}

#[test]
fn test_template_empty_interpolation() {
    // {} without expression should be an error
    let result = parse_expr("`empty {}`");
    assert!(result.is_err(), "expected error for empty interpolation");
}

#[test]
fn test_template_unclosed_interpolation() {
    // { without closing } should be an error
    let result = parse_expr("`unclosed {x`");
    assert!(result.is_err(), "expected error for unclosed interpolation");
}

#[test]
fn test_template_complex_expression() {
    let module = parse_expr("`Sum: {a + b * c}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, .. } => {
                    // Should parse as a binary expression
                    assert!(matches!(&**expr, wado_compiler::ast::Expr::Binary(_)));
                }
                other => panic!("expected Interpolation, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}

#[test]
fn test_template_method_call() {
    let module = parse_expr("`Length: {name.len()}`").expect("parse failed");
    let expr = extract_expr(&module).expect("no expression found");

    match expr {
        wado_compiler::ast::Expr::TemplateString(template) => {
            match &template.parts[1] {
                wado_compiler::ast::TemplatePart::Interpolation { expr, .. } => {
                    // Should be a method call
                    assert!(matches!(&**expr, wado_compiler::ast::Expr::MethodCall(_)));
                }
                other => panic!("expected Interpolation, got {other:?}"),
            }
        }
        other => panic!("expected TemplateString, got {other:?}"),
    }
}
