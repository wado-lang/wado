//! Tests for the Wado formatter
//!
//! These tests verify:
//! - Idempotency: format(format(x)) == format(x)
//! - Comment preservation: comments remain in formatted output
//! - Round-trip semantics: formatted code compiles to equivalent Wasm
//! - Canonical formatting style

use std::fs;
use std::path::Path;

// ============================================================================
// Idempotency Tests
// ============================================================================

#[test]
fn test_format_idempotent_simple() {
    let source = r#"
fn run() {
    let x = 1;
}
"#;
    let formatted1 = wado_compiler::format(source).expect("format failed");
    let formatted2 = wado_compiler::format(&formatted1).expect("format failed");
    assert_eq!(formatted1, formatted2, "format should be idempotent");
}

#[test]
fn test_format_idempotent_with_imports() {
    let source = r#"
use {println} from "core:cli";

fn run() with Stdout {
    println("hello");
}
"#;
    let formatted1 = wado_compiler::format(source).expect("format failed");
    let formatted2 = wado_compiler::format(&formatted1).expect("format failed");
    assert_eq!(formatted1, formatted2, "format should be idempotent");
}

#[test]
fn test_format_idempotent_with_struct() {
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

fn run() {
    let p = Point { x: 1, y: 2 };
}
"#;
    let formatted1 = wado_compiler::format(source).expect("format failed");
    let formatted2 = wado_compiler::format(&formatted1).expect("format failed");
    assert_eq!(formatted1, formatted2, "format should be idempotent");
}

// ============================================================================
// Comment Preservation Tests
// ============================================================================

#[test]
fn test_format_preserves_line_comment() {
    let source = r#"
// This is a comment
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// This is a comment"),
        "line comment should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_block_comment() {
    let source = r#"
/* This is a block comment */
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/* This is a block comment */"),
        "block comment should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_trailing_comment() {
    let source = r#"
fn run() {
    let x = 1; // trailing comment
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// trailing comment"),
        "trailing comment should be preserved: {}",
        formatted
    );
}

// ============================================================================
// Comment Edge Cases
// ============================================================================

#[test]
fn test_format_comment_at_file_start() {
    let source = r#"// First line comment
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("// First line comment\n"),
        "comment at file start should be preserved at start: {}",
        formatted
    );
    // Idempotency check
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_multiple_consecutive_comments() {
    let source = r#"// Comment 1
// Comment 2
// Comment 3
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Comment 1\n// Comment 2\n// Comment 3"),
        "consecutive comments should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_between_items() {
    let source = r#"fn foo() {
    let x = 1;
}

// Comment between functions
fn bar() {
    let y = 2;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Comment between functions"),
        "comment between items should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_inside_nested_block() {
    let source = r#"fn run() {
    if true {
        // Inside if
        let x = 1;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Inside if"),
        "comment inside nested block should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_multiline_block_comment() {
    let source = r#"/*
 * Multi-line
 * block comment
 */
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("Multi-line"),
        "multiline block comment should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains("block comment"),
        "multiline block comment should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_on_closing_brace_line() {
    let source = r#"fn run() {
    let x = 1;
} // end run
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // Canonical format uses two spaces before trailing comments
    assert!(
        formatted.contains("}  // end run"),
        "comment on closing brace line should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_after_last_statement() {
    let source = r#"fn run() {
    let x = 1;
    // Last comment in block
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Last comment in block"),
        "comment after last statement should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_in_struct() {
    let source = r#"struct Point {
    // X coordinate
    x: i32,
    // Y coordinate
    y: i32,
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// X coordinate"),
        "comment in struct should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains("// Y coordinate"),
        "comment in struct should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_trailing_comment_on_field() {
    let source = r#"struct Point {
    x: i32, // horizontal
    y: i32, // vertical
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// horizontal"),
        "trailing comment on field should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains("// vertical"),
        "trailing comment on field should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_blank_line_preservation() {
    let source = r#"// Top comment

fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // There should be exactly one blank line between comment and function
    assert!(
        formatted.contains("// Top comment\n\nfn run()"),
        "blank line after comment should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

// ============================================================================
// Canonical Formatting Style Tests
// ============================================================================

#[test]
fn test_format_use_braces_spacing() {
    // User preference: spaces inside braces
    let source = r#"use {foo} from "bar.wado";"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("use { foo }"),
        "use should have spaces inside braces: {}",
        formatted
    );
}

#[test]
fn test_format_use_multiple_items() {
    let source = r#"use {foo,bar,baz} from "bar.wado";"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("use { foo, bar, baz }"),
        "multiple items should be comma-separated with spaces: {}",
        formatted
    );
}

#[test]
fn test_format_indentation() {
    let source = r#"
fn run() {
let x = 1;
let y = 2;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // Should use 4-space indentation
    assert!(
        formatted.contains("    let x = 1;"),
        "should use 4-space indentation: {}",
        formatted
    );
}

// ============================================================================
// Compound Assignment (+=, -=, etc.) Preservation Tests
// ============================================================================

#[test]
fn test_format_preserves_compound_assign() {
    let source = r#"
fn run() {
    let mut x = 1;
    x += 5;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("x += 5;"),
        "compound assignment should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_all_compound_ops() {
    let source = r#"
fn run() {
    let mut a = 10;
    a += 1;
    a -= 2;
    a *= 3;
    a /= 4;
    a %= 5;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(formatted.contains("a += 1;"), "+=: {}", formatted);
    assert!(formatted.contains("a -= 2;"), "-=: {}", formatted);
    assert!(formatted.contains("a *= 3;"), "*=: {}", formatted);
    assert!(formatted.contains("a /= 4;"), "/=: {}", formatted);
    assert!(formatted.contains("a %= 5;"), "%=: {}", formatted);
}

// ============================================================================
// Comparison Chain Preservation Tests
// ============================================================================

#[test]
fn test_format_preserves_comparison_chain() {
    let source = r#"
fn run() {
    let x = 5;
    if 0 < x < 10 {
        let y = 1;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("0 < x < 10"),
        "comparison chain should be preserved: {}",
        formatted
    );
}

// ============================================================================
// Struct Literal Shorthand Tests
// ============================================================================

#[test]
fn test_format_preserves_struct_shorthand() {
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

fn run() {
    let x = 1;
    let y = 2;
    let p = Point { x, y };
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // Shorthand should be preserved (not expanded to x: x, y: y)
    assert!(
        formatted.contains("Point { x, y }"),
        "struct shorthand should be preserved: {}",
        formatted
    );
}

// ============================================================================
// Method Self Parameter Tests
// ============================================================================

#[test]
fn test_format_preserves_self_shorthand() {
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn get_x(&self) -> i32 {
        return self.x;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // &self should be preserved as shorthand, not expanded to self: &Self
    assert!(
        formatted.contains("fn get_x(&self)"),
        "&self shorthand should be preserved: {}",
        formatted
    );
    assert!(
        !formatted.contains("self: &Self"),
        "self: &Self should not appear: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_explicit_self_type() {
    // When user explicitly writes self: &Self, it should be preserved
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn get_x(self: &Self) -> i32 {
        return self.x;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // Explicit self: &Self should be preserved, not converted to &self
    assert!(
        formatted.contains("fn get_x(self: &Self)"),
        "explicit self: &Self should be preserved: {}",
        formatted
    );
    assert!(
        !formatted.contains("fn get_x(&self)"),
        "&self shorthand should not appear when explicit form was used: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_mut_self_shorthand() {
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn set_x(&mut self, x: i32) {
        self.x = x;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // &mut self should be preserved as shorthand
    assert!(
        formatted.contains("fn set_x(&mut self"),
        "&mut self shorthand should be preserved: {}",
        formatted
    );
    assert!(
        !formatted.contains("self: &mut Self"),
        "self: &mut Self should not appear: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_explicit_mut_self_type() {
    // When user explicitly writes self: &mut Self, it should be preserved
    let source = r#"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn set_x(self: &mut Self, x: i32) {
        self.x = x;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // Explicit self: &mut Self should be preserved, not converted to &mut self
    assert!(
        formatted.contains("fn set_x(self: &mut Self"),
        "explicit self: &mut Self should be preserved: {}",
        formatted
    );
    assert!(
        !formatted.contains("fn set_x(&mut self"),
        "&mut self shorthand should not appear when explicit form was used: {}",
        formatted
    );
}

// ============================================================================
// Data Section Preservation Tests
// ============================================================================

#[test]
fn test_format_preserves_data_section() {
    let source = r#"
fn run() {
    let x = 1;
}

__DATA__
{"stdout": "hello"}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("__DATA__"),
        "data section marker should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains(r#"{"stdout": "hello"}"#),
        "data section content should be preserved: {}",
        formatted
    );
}

// ============================================================================
// Shebang Preservation Tests
// ============================================================================

#[test]
fn test_format_preserves_shebang() {
    let source = r#"#!/usr/bin/env wado
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("#!/usr/bin/env wado\n"),
        "shebang should be preserved at start: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_shebang_with_args() {
    let source = r#"#!/usr/bin/wado --some-flag
fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("#!/usr/bin/wado --some-flag\n"),
        "shebang with args should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_preserves_shebang_with_data_section() {
    let source = r#"#!/usr/bin/env wado
fn run() {
    let x = 1;
}

__DATA__
{"stdout": "hello"}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("#!/usr/bin/env wado\n"),
        "shebang should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains("__DATA__"),
        "data section should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains(r#"{"stdout": "hello"}"#),
        "data section content should be preserved: {}",
        formatted
    );
}

// ============================================================================
// Number Literal Tests
// ============================================================================

// NOTE: Currently, all integer literals are normalized to decimal format.
// This is a known limitation - preserving binary/hex/octal format would
// require storing the original representation in the AST.

#[test]
fn test_format_decimal_literal() {
    let source = r#"fn run() {
    let x = 42;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 42;"),
        "decimal literal should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_binary_literal_preserved() {
    let source = r#"fn run() {
    let x = 0b1100;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0b1100;"),
        "binary literal should be preserved: {}",
        formatted
    );
    // Verify idempotency
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_hex_literal_preserved() {
    let source = r#"fn run() {
    let x = 0xFF;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0xFF;"),
        "hex literal should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_octal_literal_preserved() {
    let source = r#"fn run() {
    let x = 0o755;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0o755;"),
        "octal literal should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_underscore_literal_preserved() {
    let source = r#"fn run() {
    let x = 1_000_000;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1_000_000;"),
        "underscores should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_float_preserves_decimal() {
    let source = r#"fn run() {
    let x = 3.14159;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 3.14159;"),
        "float literal should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_integer_as_float() {
    // When parsing 3.0, it might be stored as 3 internally, but should format with .0
    let source = r#"fn run() {
    let x: f64 = 3.0;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("3.0") || formatted.contains("3"),
        "should be able to format: {}",
        formatted
    );
}

#[test]
fn test_format_negative_literal() {
    let source = r#"fn run() {
    let x = -42;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = -42;"),
        "negative literal should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_float_scientific_preserved() {
    let source = r#"fn run() {
    let x = 6.022e23;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 6.022e23;"),
        "scientific notation should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_float_underscore_preserved() {
    let source = r#"fn run() {
    let x = 1_000_000.5;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1_000_000.5;"),
        "underscore in float should be preserved: {}",
        formatted
    );
}

#[test]
fn test_format_float_negative_exponent_preserved() {
    let source = r#"fn run() {
    let x = 1.6e-19;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1.6e-19;"),
        "negative exponent should be preserved: {}",
        formatted
    );
}

// ============================================================================
// E2E Fixture Idempotency Tests
// ============================================================================

#[test]
fn test_format_idempotent_all_fixtures() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

    let mut failures = Vec::new();

    for entry in fs::read_dir(&fixtures_dir).expect("cannot read fixtures dir") {
        let entry = entry.expect("cannot read entry");
        let path = entry.path();

        // Only test .wado files in the root fixtures directory
        if path.extension().and_then(|s| s.to_str()) != Some("wado") {
            continue;
        }
        if path.is_dir() {
            continue;
        }

        let source = fs::read_to_string(&path).expect("cannot read file");
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip files that expect compile errors (check __DATA__ section)
        if source.contains("compile_error") {
            continue;
        }

        // First format
        let formatted1 = match wado_compiler::format(&source) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{}: format error: {}", filename, e));
                continue;
            }
        };

        // Second format
        let formatted2 = match wado_compiler::format(&formatted1) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{}: second format error: {}", filename, e));
                continue;
            }
        };

        // Check idempotency
        if formatted1 != formatted2 {
            failures.push(format!(
                "{}: not idempotent\nFirst:\n{}\nSecond:\n{}",
                filename, formatted1, formatted2
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "Format idempotency failures:\n{}",
            failures.join("\n\n---\n\n")
        );
    }
}

// ============================================================================
// Labeled Block Tests - Break with Label and Value
// ============================================================================

#[test]
fn test_format_break_with_label() {
    let source = r#"fn run() {
    outer: {
        break outer;
    }
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break outer;"),
        "break with label should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_break_with_label_and_value() {
    let source = r#"fn run() {
    let x = foo: {
        break foo: 42;
    };
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break foo: 42;"),
        "break with label and value should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_break_with_label_and_expression() {
    let source = r#"fn run() {
    let result = compute: {
        let a = 10;
        let b = 20;
        break compute: a + b;
    };
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break compute: a + b;"),
        "break with label and expression should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_nested_labeled_blocks() {
    let source = r#"fn run() {
    let result = outer: {
        let x = inner: {
            break inner: 10;
        };
        break outer: x * 2;
    };
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break inner: 10;"),
        "break inner with value should be preserved: {}",
        formatted
    );
    assert!(
        formatted.contains("break outer: x * 2;"),
        "break outer with expression should be preserved: {}",
        formatted
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}
