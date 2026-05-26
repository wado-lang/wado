//! Tests for parsing primitive literals
//!
//! This module tests the lexer and parser's handling of primitive literal values
//! as defined in the Wado language specification.
//!
//! With the raw-string AST design, `Literal::Number`, `Literal::String`, and
//! `Literal::Char` all carry raw source text. Escape interpretation happens in
//! the elaborator, not here.

use wado_compiler::{Lexer, Parser};

/// Parse a simple expression and return the AST
fn parse_expr(source: &str) -> Result<wado_compiler::ast::Module, String> {
    // Wrap the expression in a function to make it a valid module
    let wrapped = format!("fn test() {{ let x = {source}; }}");

    let mut lexer = Lexer::new(&wrapped);
    let tokens = lexer.tokenize().map_err(|e| e.message)?;

    let mut parser = Parser::new(tokens);
    parser.parse().map_err(|e| e.message)
}

/// Extract the literal value from a parsed let statement
fn extract_literal(module: &wado_compiler::ast::Module) -> Option<&wado_compiler::ast::Literal> {
    use wado_compiler::ast::{Expr, Item, Stmt};

    let item = module.items.first()?;
    let Item::Function(func) = item else {
        return None;
    };
    let body = func.body.as_ref()?;
    let stmt = body.stmts.first()?;
    let Stmt::Let(let_stmt) = stmt else {
        return None;
    };
    let Some(value) = &let_stmt.value else {
        return None;
    };
    let Expr::Literal(lit_expr) = value else {
        return None;
    };
    Some(&lit_expr.value)
}

#[test]
fn test_bool_true() {
    let module = parse_expr("true").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Bool(true) => {}
        other => panic!("expected Bool(true), got {other:?}"),
    }
}

#[test]
fn test_bool_false() {
    let module = parse_expr("false").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Bool(false) => {}
        other => panic!("expected Bool(false), got {other:?}"),
    }
}

#[test]
fn test_integer_zero() {
    let module = parse_expr("0").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0" => {}
        other => panic!("expected Number(\"0\"), got {other:?}"),
    }
}

#[test]
fn test_integer_positive() {
    let module = parse_expr("42").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "42" => {}
        other => panic!("expected Number(\"42\"), got {other:?}"),
    }
}

#[test]
fn test_integer_large() {
    let module = parse_expr("9223372036854775807").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "9223372036854775807" => {}
        other => panic!("expected Number(\"9223372036854775807\"), got {other:?}"),
    }
}

#[test]
fn test_integer_with_separator() {
    let module = parse_expr("1_000_000").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "1_000_000" => {}
        other => panic!("expected Number(\"1_000_000\"), got {other:?}"),
    }
}

#[test]
fn test_integer_hex() {
    let module = parse_expr("0xFF").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0xFF" => {}
        other => panic!("expected Number(\"0xFF\"), got {other:?}"),
    }
}

#[test]
fn test_integer_binary() {
    let module = parse_expr("0b1010").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0b1010" => {}
        other => panic!("expected Number(\"0b1010\"), got {other:?}"),
    }
}

#[test]
fn test_integer_octal() {
    let module = parse_expr("0o755").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0o755" => {}
        other => panic!("expected Number(\"0o755\"), got {other:?}"),
    }
}

#[test]
fn test_float_simple() {
    let module = parse_expr("3.25").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "3.25" => {}
        other => panic!("expected Number(\"3.25\"), got {other:?}"),
    }
}

#[test]
fn test_float_zero() {
    let module = parse_expr("0.0").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0.0" => {}
        other => panic!("expected Number(\"0.0\"), got {other:?}"),
    }
}

#[test]
fn test_float_leading_zero() {
    let module = parse_expr("0.5").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "0.5" => {}
        other => panic!("expected Number(\"0.5\"), got {other:?}"),
    }
}

#[test]
fn test_float_many_decimals() {
    let module = parse_expr("1.23456789012345").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "1.23456789012345" => {}
        other => panic!("expected Number(\"1.23456789012345\"), got {other:?}"),
    }
}

#[test]
fn test_float_scientific() {
    let module = parse_expr("6.022e23").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "6.022e23" => {}
        other => panic!("expected Number(\"6.022e23\"), got {other:?}"),
    }
}

#[test]
fn test_float_with_separator() {
    let module = parse_expr("1_000_000.5").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Number(repr) if repr == "1_000_000.5" => {}
        other => panic!("expected Number(\"1_000_000.5\"), got {other:?}"),
    }
}

// String literal tests check the *raw* text stored between the quotes.
// Escape interpretation (e.g. `\n` → newline) happens in the elaborator.

#[test]
fn test_string_empty() {
    let module = parse_expr(r#""""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, "", "expected empty raw string, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_simple() {
    let module = parse_expr(r#""hello""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, "hello", "expected raw \"hello\", got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_with_spaces() {
    let module = parse_expr(r#""hello world""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, "hello world",
                "expected raw \"hello world\", got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_newline() {
    // Source: "line1\nline2" — raw text between quotes is `line1\nline2` (backslash-n)
    let module = parse_expr(r#""line1\nline2""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, r"line1\nline2",
                "expected raw escape text, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_tab() {
    let module = parse_expr(r#""col1\tcol2""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"col1\tcol2", "expected raw escape text, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_carriage_return() {
    let module = parse_expr(r#""line\r""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"line\r", "expected raw escape text, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_backslash() {
    let module = parse_expr(r#""path\\to\\file""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            // Raw text between the quotes: path\\to\\file (two escaped backslashes)
            assert_eq!(
                raw, r"path\\to\\file",
                "expected raw escape text, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_quote() {
    let module = parse_expr(r#""say \"hello\"""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            // Raw text: say \"hello\"
            assert_eq!(
                raw, r#"say \"hello\""#,
                "expected raw escape text, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_multiple_escapes() {
    let module = parse_expr(r#""line1\nline2\ttab""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, r"line1\nline2\ttab",
                "expected raw escape text, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_forward_slash() {
    let module = parse_expr(r#""path\/to""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"path\/to", "expected raw escape text, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_backspace() {
    let module = parse_expr(r#""hello\b""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"hello\b", "expected raw escape text, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_form_feed() {
    let module = parse_expr(r#""page\f""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"page\f", "expected raw escape text, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_unicode_bmp() {
    // Source: "\u0041" — raw text between quotes is `\u0041`
    let module = parse_expr(r#""\u0041""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(raw, r"\u0041", "expected raw unicode escape, got {raw:?}");
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_escape_unicode_full() {
    // Source: "\u{1F600}" — raw text between quotes is `\u{1F600}`
    let module = parse_expr(r#""\u{1F600}""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, r"\u{1F600}",
                "expected raw unicode escape, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_string_surrogate_pair() {
    // Source: "\uD83D\uDE00" — raw text between quotes is `\uD83D\uDE00`
    let module = parse_expr(r#""\uD83D\uDE00""#).expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, r"\uD83D\uDE00",
                "expected raw surrogate pair text, got {raw:?}"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn test_unit_literal() {
    let module = parse_expr("()").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");

    match lit {
        wado_compiler::ast::Literal::Unit => {}
        other => panic!("expected Unit, got {other:?}"),
    }
}

#[test]
fn test_string_unterminated() {
    let result = parse_expr(r#""unterminated"#);
    assert!(result.is_err(), "expected error for unterminated string");
}

#[test]
fn test_string_unknown_escape_stored_as_raw() {
    // With the raw-string design, unknown escapes like `\x` are stored verbatim at the
    // AST level. Validation happens in the elaborator.
    let module = parse_expr(r#""invalid\x""#).expect("parse should succeed at AST level");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::String(raw) => {
            assert_eq!(
                raw, r"invalid\x",
                "expected raw text including unknown escape"
            );
        }
        other => panic!("expected String, got {other:?}"),
    }
}

// Char literals store raw text between the single quotes.

#[test]
fn test_char_simple() {
    let module = parse_expr("'A'").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Char(raw) if raw == "A" => {}
        other => panic!("expected Char(\"A\"), got {other:?}"),
    }
}

#[test]
fn test_char_escape_newline() {
    let module = parse_expr(r"'\n'").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Char(raw) if raw == r"\n" => {}
        other => panic!("expected Char(\"\\n\"), got {other:?}"),
    }
}

#[test]
fn test_char_unicode() {
    // '\u0041' — raw text is `\u0041`; the elaborator interprets it as 'A'
    let module = parse_expr(r"'\u0041'").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    match lit {
        wado_compiler::ast::Literal::Char(raw) if raw == r"\u0041" => {}
        other => panic!("expected Char(\"\\u0041\"), got {other:?}"),
    }
}

#[test]
fn test_null_literal() {
    let module = parse_expr("null").expect("parse failed");
    let lit = extract_literal(&module).expect("no literal found");
    // null should be equivalent to None
    match lit {
        wado_compiler::ast::Literal::Null => {}
        other => panic!("expected Null, got {other:?}"),
    }
}
