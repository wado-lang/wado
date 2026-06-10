//! Tests for the Wado formatter
//!
//! These tests verify:
//! - Idempotency: format(format(x)) == format(x)
//! - Comment preservation: comments remain in formatted output
//! - Round-trip AST equivalence: parse(format(x)) ≡ parse(x) (ignoring spans)
//! - Canonical formatting style

use std::fs;
use std::path::Path;

/// Strip semantically-irrelevant fields from Debug output so we can compare
/// ASTs structurally. This normalizes:
/// - `span: Span { ... }` → `span: SPAN` (source positions differ after format)
/// - `has_trailing_comma: ...` → removed (formatting hint, not semantic)
/// - `type_params: [...]` → `type_params: NORM` (formatter normalizes `impl<T> Foo<T>` to `impl Foo<T>`)
///
/// The goal is to catch **semantic** changes (dropped expressions, changed precedence)
/// while ignoring **syntactic normalizations** that the formatter intentionally performs.
fn normalize_ast_debug(debug: &str) -> String {
    // Work on bytes since Debug output is always ASCII
    let bytes = debug.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        // Normalize any "Span {" occurrence (span:, op_span:, or bare Span in enums)
        if bytes[i..].starts_with(b"Span {") {
            result.extend_from_slice(b"SPAN");
            i += b"Span ".len();
            let mut depth = 0i32;
            while i < bytes.len() {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
            continue;
        }

        // Strip "has_trailing_comma: ..." lines
        if bytes[i..].starts_with(b"has_trailing_comma:") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }

        // Normalize any `AstIdSpace(N)` occurrence (the Module's
        // `ast_id_space` field): every parse mints a fresh space, so the
        // original and formatted parses always differ here by design.
        // Multi-line in `{:#?}` output, so consume through the `)`.
        if bytes[i..].starts_with(b"AstIdSpace(") {
            result.extend_from_slice(b"ASPACE");
            i += b"AstIdSpace(".len();
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }

        // Normalize any `AstId(S:N)` occurrence — AstIds are parse-order
        // allocations (in a per-parse space) and must not affect AST
        // equivalence. Whitespace changes by the formatter can cause
        // legitimately different id counts when the parser's token
        // consumption changes slightly.
        if bytes[i..].starts_with(b"AstId(") {
            result.extend_from_slice(b"AID");
            i += b"AstId(".len();
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }

        // Note: type_params are NOT normalized — the formatter now always emits
        // explicit `impl<T>` (matching Rust), so type_params must round-trip exactly.

        result.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(result).expect("Debug output should be valid UTF-8")
}

/// Assert that formatting preserves AST semantics: parse(source) ≡ parse(format(source)).
///
/// This catches bugs where the formatter changes program meaning (e.g., dropping
/// parentheses, reordering operators, losing expressions).
///
/// Panics on parse errors in either `source` or the formatter output: the
/// source passed in is expected to be valid, and a silent skip would let
/// typos in the test itself report success while testing nothing.
fn assert_format_preserves_ast(source: &str) {
    let formatted = wado_compiler::format(source)
        .unwrap_or_else(|e| panic!("test source failed to format: {e:?}\n\nSource:\n{source}"));

    let original_ast = wado_compiler::parse(source)
        .into_fail_fast()
        .unwrap_or_else(|e| panic!("test source failed to parse: {e:?}\n\nSource:\n{source}"))
        .ast;
    let formatted_ast = wado_compiler::parse(&formatted)
        .into_fail_fast()
        .unwrap_or_else(|e| panic!(
            "formatted code failed to parse: {e:?}\n\nOriginal:\n{source}\n\nFormatted:\n{formatted}"
        ))
        .ast;

    let original_debug = normalize_ast_debug(&format!("{original_ast:#?}"));
    let formatted_debug = normalize_ast_debug(&format!("{formatted_ast:#?}"));

    if original_debug != formatted_debug {
        // Find first differing line for a useful error message
        let orig_lines: Vec<&str> = original_debug.lines().collect();
        let fmt_lines: Vec<&str> = formatted_debug.lines().collect();
        let mut diff_context = String::new();
        for (i, (o, f)) in orig_lines.iter().zip(fmt_lines.iter()).enumerate() {
            if o != f {
                let start = i.saturating_sub(3);
                let end = (i + 4).min(orig_lines.len()).min(fmt_lines.len());
                diff_context.push_str(&format!("First difference at line {i}:\n"));
                for j in start..end {
                    let marker = if j == i { ">>>" } else { "   " };
                    if j < orig_lines.len() {
                        diff_context.push_str(&format!("{marker} orig[{j}]: {}\n", orig_lines[j]));
                    }
                    if j < fmt_lines.len() {
                        diff_context.push_str(&format!("{marker}  fmt[{j}]: {}\n", fmt_lines[j]));
                    }
                }
                break;
            }
        }
        if diff_context.is_empty() && orig_lines.len() != fmt_lines.len() {
            diff_context = format!(
                "Line count differs: original={}, formatted={}",
                orig_lines.len(),
                fmt_lines.len()
            );
        }
        panic!(
            "Formatter changed AST semantics!\n\n{diff_context}\n\nOriginal source:\n{source}\n\nFormatted source:\n{formatted}"
        );
    }
}

#[test]
fn test_format_idempotent_simple() {
    let source = r"
fn run() {
    let x = 1;
}
";
    let formatted1 = wado_compiler::format(source).expect("format failed");
    let formatted2 = wado_compiler::format(&formatted1).expect("format failed");
    assert_eq!(formatted1, formatted2, "format should be idempotent");
}

/// `reactive` is a prefix keyword that must precede `let`. The formatter used
/// to emit `let reactive ...`, which the parser rejects ("expected pattern,
/// found Reactive"), so formatting any `reactive let` binding produced output
/// that no longer reparsed. Pin the keyword order and round-trip stability.
#[test]
fn test_format_reactive_let_keeps_keyword_order() {
    let source = "fn run() {\n    reactive let mut r = 1;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("reactive let mut r"),
        "expected `reactive let` order, got:\n{formatted}"
    );
    // Formatted output must reparse and stay idempotent.
    assert_format_preserves_ast(source);
    let formatted2 = wado_compiler::format(&formatted).expect("reformat failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

/// A `use { ... } from "..."` whose item list fits in 120 chars but whose
/// full line (including the ` from "..."` clause) exceeds it must wrap the
/// item list one-per-line. Regression test for the width check that used to
/// exclude the from-clause (issue #1234).
#[test]
fn test_format_use_wraps_when_from_clause_overflows() {
    let source = "use { Ordering, Eq, Ord, IndexValue, IndexAssign, Iterator, IntoIterator, FromIterator, Display } from \"core:prelude/traits.wado\";\n";
    let expected = r#"use {
    Ordering,
    Eq,
    Ord,
    IndexValue,
    IndexAssign,
    Iterator,
    IntoIterator,
    FromIterator,
    Display,
} from "core:prelude/traits.wado";
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

/// A namespace import with a long `with { ... }` clause wraps the with-clause
/// onto its own line and expands the attribute object, recursively keeping
/// short nested objects inline.
#[test]
fn test_format_use_wraps_long_with_clause() {
    let source = "use sqlite from \"../../package-gale/tests/grammars/SQLite.g4\" with { generator: { module: \"../../package-gale/src/generator.wado\", options: { highlight: false } } };\n";
    let expected = r#"use sqlite from "../../package-gale/tests/grammars/SQLite.g4"
    with {
        generator: {
            module: "../../package-gale/src/generator.wado",
            options: { highlight: false },
        },
    };
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

/// A long item list paired with a short `with { ... }` clause wraps the item
/// list but keeps the (short) with-clause inline on the closing line.
#[test]
fn test_format_use_wraps_items_keeps_short_with_inline() {
    let source = "use { sin, cos, tan, asin, acos, atan, atan2, sinh, cosh, tanh, exp, log, log2, log10, sqrt, cbrt, floor } from \"../libm.wat\" with { type: \"wat\" };\n";
    let expected = r#"use {
    sin,
    cos,
    tan,
    asin,
    acos,
    atan,
    atan2,
    sinh,
    cosh,
    tanh,
    exp,
    log,
    log2,
    log10,
    sqrt,
    cbrt,
    floor,
} from "../libm.wat" with { type: "wat" };
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

/// A long nested array inside a `with { ... }` clause wraps one element per
/// line; short scalar values stay inline.
#[test]
fn test_format_use_with_wraps_nested_array() {
    let source = "use g from \"./x.g4\" with { generator: { inputs: [\"./grammars/AAAAAAAAAAAAAAAA.g4\", \"./grammars/BBBBBBBBBBBBBBBB.g4\", \"./grammars/CCCCCCCCCCCCCCCC.g4\"], output_dir: \"tests/generated/x\" } };\n";
    let expected = r#"use g from "./x.g4"
    with {
        generator: {
            inputs: [
                "./grammars/AAAAAAAAAAAAAAAA.g4",
                "./grammars/BBBBBBBBBBBBBBBB.g4",
                "./grammars/CCCCCCCCCCCCCCCC.g4",
            ],
            output_dir: "tests/generated/x",
        },
    };
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

/// A `test` item carrying a leading comment and an outer attribute must format
/// idempotently — the blank line between the comment and the attribute must not
/// grow on repeated passes. Regression test: `Item::Test` was missing from
/// `get_item_first_line`, so the attribute line was miscounted as a blank.
#[test]
fn test_format_test_attr_blank_line_idempotent() {
    let source = "// note\n\n#[TODO]\ntest \"x\" {\n    assert true;\n}\n";
    let once = wado_compiler::format(source).expect("format failed");
    let twice = wado_compiler::format(&once).expect("format failed");
    assert_eq!(
        once, twice,
        "format must be idempotent\n--- once ---\n{once}"
    );
    // Exactly one blank line (the source's) sits between comment and attribute.
    assert_eq!(
        once,
        "// note\n\n#[TODO]\ntest \"x\" {\n    assert true;\n}\n"
    );
}

/// A function type with a unit return omits the `-> ()`, matching the rule for
/// function declarations. The arrow is kept for non-unit returns.
#[test]
fn test_format_fn_type_omits_unit_return() {
    let source = "trait Each {\n    fn run(&self, f: fn mut(i32));\n    fn map(&self, g: fn(i32) -> i32);\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        !formatted.contains("-> ()"),
        "unit return should be omitted, got:\n{formatted}"
    );
    assert!(formatted.contains("fn mut(i32)"), "got:\n{formatted}");
    assert!(formatted.contains("fn(i32) -> i32"), "got:\n{formatted}");
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "idempotent");
}

/// An array whose elements are themselves containers (here 2-tuples) is forced
/// one entry per line by the depth rule — the old KV-list special case.
#[test]
fn test_format_array_of_tuples_one_per_line() {
    let source = "fn run() {\n    let m = [[\"a\", 1], [\"b\", 2]];\n}\n";
    let expected = r#"fn run() {
    let m = [
        ["a", 1],
        ["b", 2],
    ];
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "idempotent");
}

/// The depth rule generalizes beyond 2-tuples: an array of 3-element tuples
/// also wraps one entry per line (the old KV-list rule only handled 2-tuples).
#[test]
fn test_format_array_of_triples_one_per_line() {
    let source = "fn run() {\n    let m = [[1, 2, 3], [4, 5, 6]];\n}\n";
    let expected = r"fn run() {
    let m = [
        [1, 2, 3],
        [4, 5, 6],
    ];
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "idempotent");
}

/// A flat scalar array that fits stays inline (no nested container, no calls).
#[test]
fn test_format_flat_array_stays_inline() {
    let source = "fn run() {\n    let m = [1, 2, 3];\n}\n";
    let expected = "fn run() {\n    let m = [1, 2, 3];\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
}

/// A nested struct literal forces the outer struct multi-line, one field per
/// line, while the (flat) inner struct stays inline.
#[test]
fn test_format_nested_struct_breaks_outer() {
    let source = "fn run() {\n    let c = Config { server: Server { host: \"h\", port: 8080 }, debug: true };\n}\n";
    let expected = r#"fn run() {
    let c = Config {
        server: Server { host: "h", port: 8080 },
        debug: true,
    };
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "idempotent");
}

/// A flat struct literal that fits stays inline.
#[test]
fn test_format_flat_struct_stays_inline() {
    let source = "fn run() {\n    let p = Point { x: 1, y: 2 };\n}\n";
    let expected = "fn run() {\n    let p = Point { x: 1, y: 2 };\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert_eq!(formatted, expected);
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
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

fn run() {
    let p = Point { x: 1, y: 2 };
}
";
    let formatted1 = wado_compiler::format(source).expect("format failed");
    let formatted2 = wado_compiler::format(&formatted1).expect("format failed");
    assert_eq!(formatted1, formatted2, "format should be idempotent");
}

#[test]
fn test_format_preserves_line_comment() {
    let source = r"
// This is a comment
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// This is a comment"),
        "line comment should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_block_comment() {
    let source = r"
/* This is a block comment */
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/* This is a block comment */"),
        "block comment should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_trailing_comment() {
    let source = r"
fn run() {
    let x = 1; // trailing comment
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// trailing comment"),
        "trailing comment should be preserved: {formatted}"
    );
}

/// `matches` is a postfix-level operator, so it binds tighter than the
/// unary operators. `(!x) matches { ... }` is `Matches(Unary(x))` and must
/// keep its parentheses: dropping them yields `!x matches { ... }`, which
/// reparses as `Unary(Matches(x))` — a different expression. The unparser
/// used to emit the scrutinee with no parens, silently changing semantics.
#[test]
fn test_format_matches_scrutinee_keeps_parens() {
    // Scrutinee is a lower-precedence expression → parens are load-bearing.
    for source in [
        "fn run() -> bool {\n    return (!x) matches { Some(_) };\n}\n",
        "fn run() -> bool {\n    return (-x) matches { 0 };\n}\n",
        "fn run() -> bool {\n    return (a + b) matches { 0 };\n}\n",
        "fn run() -> bool {\n    return (x as i32) matches { 0 };\n}\n",
    ] {
        assert_format_preserves_ast(source);
    }
    // The unary-outer form drops its now-redundant parens but keeps meaning.
    let unary_outer = "fn run() -> bool {\n    return !(x matches { Some(_) });\n}\n";
    let formatted = wado_compiler::format(unary_outer).expect("format failed");
    assert!(
        formatted.contains("!x matches { Some(_) }"),
        "redundant parens around a matches-inside-unary should be dropped:\n{formatted}"
    );
    assert_format_preserves_ast(unary_outer);
    // And the paren-free unary-outer form is stable / preserved.
    assert_format_preserves_ast("fn run() -> bool {\n    return !x matches { Some(_) };\n}\n");
}

/// A trailing comment after a declaration whose type is a generic
/// (`Foo<...>`) used to be dropped: the generic type's span ended at its
/// name token rather than at the closing `>`, so an inner type-argument
/// node (textually to the right) became the comment's owner and the
/// unparser never emitted trailing comments for inner type arguments.
/// Regression guard for variant cases and struct fields.
#[test]
fn test_format_preserves_trailing_comment_on_generic_type() {
    let source = r"pub variant Value {
    List(List<Value>),  // a list value
    Object(TreeMap<String, Value>),  // string keys only
    Other,  // plain case
}

struct Config {
    entries: TreeMap<String, Value>,  // the entries
    count: i32,  // how many
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    for comment in [
        "// a list value",
        "// string keys only",
        "// plain case",
        "// the entries",
        "// how many",
    ] {
        assert!(
            formatted.contains(comment),
            "trailing comment `{comment}` should be preserved:\n{formatted}"
        );
    }
    // Comments must not migrate onto the wrong line, and format is stable.
    assert!(
        formatted.contains("Object(TreeMap<String, Value>),  // string keys only"),
        "comment must stay on its own declaration line:\n{formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("reformat failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_at_file_start() {
    let source = r"// First line comment
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("// First line comment\n"),
        "comment at file start should be preserved at start: {formatted}"
    );
    // Idempotency check
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_multiple_consecutive_comments() {
    let source = r"// Comment 1
// Comment 2
// Comment 3
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Comment 1\n// Comment 2\n// Comment 3"),
        "consecutive comments should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_between_items() {
    let source = r"fn foo() {
    let x = 1;
}

// Comment between functions
fn bar() {
    let y = 2;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Comment between functions"),
        "comment between items should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_preserves_assoc_const_leading_comment() {
    let source = r"struct Foo {
    x: i32,
}

impl Foo {
    /// Doc on a const.
    pub const A: i32 = 1;
    // Line comment on another const.
    pub const B: i32 = 2;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/// Doc on a const."),
        "assoc const doc comment should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("// Line comment on another const."),
        "assoc const line comment should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_preserves_method_comment_after_const() {
    // Regression: an associated const must not swallow the leading comment of
    // the item that follows it.
    let source = r"struct Foo {
    x: i32,
}

impl Foo {
    pub const A: i32 = 1;

    /// Doc on the method after a const.
    pub fn m(&self) -> i32 {
        return self.x;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/// Doc on the method after a const."),
        "method doc after a const should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_preserves_assoc_type_leading_comment() {
    let source = r"trait Container {
    type Item;
}

impl Container for Foo {
    /// Doc on an associated type.
    type Item = i32;

    /// Doc on the method after a type.
    pub fn first(&self) -> i32 {
        return 0;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/// Doc on an associated type."),
        "assoc type doc comment should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("/// Doc on the method after a type."),
        "method doc after an assoc type should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_inside_nested_block() {
    let source = r"fn run() {
    if true {
        // Inside if
        let x = 1;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Inside if"),
        "comment inside nested block should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_multiline_block_comment() {
    let source = r"/*
 * Multi-line
 * block comment
 */
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("Multi-line"),
        "multiline block comment should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("block comment"),
        "multiline block comment should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_on_closing_brace_line() {
    let source = r"fn run() {
    let x = 1;
} // end run
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // Canonical format uses two spaces before trailing comments
    assert!(
        formatted.contains("}  // end run"),
        "comment on closing brace line should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_after_last_statement() {
    let source = r"fn run() {
    let x = 1;
    // Last comment in block
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// Last comment in block"),
        "comment after last statement should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_comment_in_struct() {
    let source = r"struct Point {
    // X coordinate
    x: i32,
    // Y coordinate
    y: i32,
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// X coordinate"),
        "comment in struct should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("// Y coordinate"),
        "comment in struct should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_trailing_comment_on_field() {
    let source = r"struct Point {
    x: i32, // horizontal
    y: i32, // vertical
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("// horizontal"),
        "trailing comment on field should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("// vertical"),
        "trailing comment on field should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

/// Inline `/*name=*/` comments before a function-call argument are common
/// as ad-hoc named-argument hints when calling a function with bare bool /
/// integer literals. The formatter must keep the comment attached to the
/// argument (not silently drop it nor strand it as a free-standing comment
/// after the statement). Found 2026-05 while landing the package-gale
/// Phase 5 LL `FollowEnv` work — see `package-gale/antlr4-compatibility.md`.
#[test]
fn test_format_preserves_inline_block_comment_in_call_args() {
    let source = r"fn caller() {
    foo(1, /*flag=*/true);
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("/*flag=*/"),
        "inline `/*flag=*/` comment must survive formatting: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_inline_call_arg_comment_does_not_strand() {
    let source = r"fn caller() {
    very_long_function_name(some_long_argument, another_long_argument, /*flag=*/false);
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    for line in formatted.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed != "/*flag=*/",
            "inline `/*flag=*/` must not become a stranded standalone comment: {formatted}"
        );
    }
    assert!(
        formatted.contains("/*flag=*/"),
        "inline `/*flag=*/` comment must survive formatting: {formatted}"
    );
}

#[test]
fn test_format_comment_blank_line_preservation() {
    let source = r"// Top comment

fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // There should be exactly one blank line between comment and function
    assert!(
        formatted.contains("// Top comment\n\nfn run()"),
        "blank line after comment should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_use_braces_spacing() {
    // User preference: spaces inside braces
    let source = r#"use {foo} from "bar.wado";"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("use { foo }"),
        "use should have spaces inside braces: {formatted}"
    );
}

#[test]
fn test_format_use_multiple_items() {
    let source = r#"use {foo,bar,baz} from "bar.wado";"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("use { foo, bar, baz }"),
        "multiple items should be comma-separated with spaces: {formatted}"
    );
}

#[test]
fn test_format_indentation() {
    let source = r"
fn run() {
let x = 1;
let y = 2;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // Should use 4-space indentation
    assert!(
        formatted.contains("    let x = 1;"),
        "should use 4-space indentation: {formatted}"
    );
}

#[test]
fn test_format_wildcard_import() {
    let source = r#"use _ from "core:prelude/primitive.wado";

fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains(r#"use _ from "core:prelude/primitive.wado";"#),
        "wildcard import should be preserved: {formatted}"
    );
    // Verify idempotency
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_wildcard_import_not_braces() {
    // Wildcard import should NOT use braces
    let source = r#"use _ from "some/module.wado";"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("use _ from"),
        "wildcard import should use `use _ from` syntax: {formatted}"
    );
    assert!(
        !formatted.contains("use {"),
        "wildcard import should NOT use braces: {formatted}"
    );
}

#[test]
fn test_format_namespace_import() {
    let source = r#"use utils from "./utils.wado";

fn run() {
    let x = 1;
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains(r#"use utils from "./utils.wado";"#),
        "namespace import should be preserved: {formatted}"
    );
    assert!(
        !formatted.contains("use {"),
        "namespace import should NOT use braces: {formatted}"
    );
    // Verify idempotency
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_preserves_compound_assign() {
    let source = r"
fn run() {
    let mut x = 1;
    x += 5;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("x += 5;"),
        "compound assignment should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_all_compound_ops() {
    let source = r"
fn run() {
    let mut a = 10;
    a += 1;
    a -= 2;
    a *= 3;
    a /= 4;
    a %= 5;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(formatted.contains("a += 1;"), "+=: {formatted}");
    assert!(formatted.contains("a -= 2;"), "-=: {formatted}");
    assert!(formatted.contains("a *= 3;"), "*=: {formatted}");
    assert!(formatted.contains("a /= 4;"), "/=: {formatted}");
    assert!(formatted.contains("a %= 5;"), "%=: {formatted}");
}

#[test]
fn test_format_preserves_comparison_chain() {
    let source = r"
fn run() {
    let x = 5;
    if 0 < x < 10 {
        let y = 1;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("0 < x < 10"),
        "comparison chain should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_struct_shorthand() {
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

fn run() {
    let x = 1;
    let y = 2;
    let p = Point { x, y };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // Shorthand should be preserved (not expanded to x: x, y: y)
    assert!(
        formatted.contains("Point { x, y }"),
        "struct shorthand should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_self_shorthand() {
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn get_x(&self) -> i32 {
        return self.x;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // &self should be preserved as shorthand, not expanded to self: &Self
    assert!(
        formatted.contains("fn get_x(&self)"),
        "&self shorthand should be preserved: {formatted}"
    );
    assert!(
        !formatted.contains("self: &Self"),
        "self: &Self should not appear: {formatted}"
    );
}

#[test]
fn test_format_normalizes_explicit_self_type() {
    // Explicit `self: &Self` should be normalized to `&self` shorthand
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn get_x(self: &Self) -> i32 {
        return self.x;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("fn get_x(&self)"),
        "explicit self: &Self should be normalized to &self: {formatted}"
    );
    assert!(
        !formatted.contains("self: &Self"),
        "self: &Self should not appear after normalization: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_preserves_mut_self_shorthand() {
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn set_x(&mut self, x: i32) {
        self.x = x;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // &mut self should be preserved as shorthand
    assert!(
        formatted.contains("fn set_x(&mut self"),
        "&mut self shorthand should be preserved: {formatted}"
    );
    assert!(
        !formatted.contains("self: &mut Self"),
        "self: &mut Self should not appear: {formatted}"
    );
}

#[test]
fn test_format_normalizes_explicit_mut_self_type() {
    // Explicit `self: &mut Self` should be normalized to `&mut self` shorthand
    let source = r"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn set_x(self: &mut Self, x: i32) {
        self.x = x;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("fn set_x(&mut self"),
        "explicit self: &mut Self should be normalized to &mut self: {formatted}"
    );
    assert!(
        !formatted.contains("self: &mut Self"),
        "self: &mut Self should not appear after normalization: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

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
        "data section marker should be preserved: {formatted}"
    );
    assert!(
        formatted.contains(r#"{"stdout": "hello"}"#),
        "data section content should be preserved: {formatted}"
    );
}

#[test]
fn test_format_preserves_shebang() {
    let source = r"#!/usr/bin/env wado
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("#!/usr/bin/env wado\n"),
        "shebang should be preserved at start: {formatted}"
    );
}

#[test]
fn test_format_preserves_shebang_with_args() {
    let source = r"#!/usr/bin/wado --some-flag
fn run() {
    let x = 1;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.starts_with("#!/usr/bin/wado --some-flag\n"),
        "shebang with args should be preserved: {formatted}"
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
        "shebang should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("__DATA__"),
        "data section should be preserved: {formatted}"
    );
    assert!(
        formatted.contains(r#"{"stdout": "hello"}"#),
        "data section content should be preserved: {formatted}"
    );
}

// NOTE: Currently, all integer literals are normalized to decimal format.
// This is a known limitation - preserving binary/hex/octal format would
// require storing the original representation in the AST.

#[test]
fn test_format_decimal_literal() {
    let source = r"fn run() {
    let x = 42;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 42;"),
        "decimal literal should be preserved: {formatted}"
    );
}

#[test]
fn test_format_binary_literal_preserved() {
    let source = r"fn run() {
    let x = 0b1100;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0b1100;"),
        "binary literal should be preserved: {formatted}"
    );
    // Verify idempotency
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_hex_literal_preserved() {
    let source = r"fn run() {
    let x = 0xFF;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0xFF;"),
        "hex literal should be preserved: {formatted}"
    );
}

#[test]
fn test_format_octal_literal_preserved() {
    let source = r"fn run() {
    let x = 0o755;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 0o755;"),
        "octal literal should be preserved: {formatted}"
    );
}

#[test]
fn test_format_underscore_literal_preserved() {
    let source = r"fn run() {
    let x = 1_000_000;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1_000_000;"),
        "underscores should be preserved: {formatted}"
    );
}

#[test]
fn test_format_float_preserves_decimal() {
    let source = r"fn run() {
    let x = 3.14159;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 3.14159;"),
        "float literal should be preserved: {formatted}"
    );
}

#[test]
fn test_format_integer_as_float() {
    // When parsing 3.0, it might be stored as 3 internally, but should format with .0
    let source = r"fn run() {
    let x: f64 = 3.0;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("3.0") || formatted.contains('3'),
        "should be able to format: {formatted}"
    );
}

#[test]
fn test_format_negative_literal() {
    let source = r"fn run() {
    let x = -42;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = -42;"),
        "negative literal should be preserved: {formatted}"
    );
}

#[test]
fn test_format_float_scientific_preserved() {
    let source = r"fn run() {
    let x = 6.022e23;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 6.022e23;"),
        "scientific notation should be preserved: {formatted}"
    );
}

#[test]
fn test_format_float_underscore_preserved() {
    let source = r"fn run() {
    let x = 1_000_000.5;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1_000_000.5;"),
        "underscore in float should be preserved: {formatted}"
    );
}

#[test]
fn test_format_float_negative_exponent_preserved() {
    let source = r"fn run() {
    let x = 1.6e-19;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("let x = 1.6e-19;"),
        "negative exponent should be preserved: {formatted}"
    );
}

#[test]
fn test_format_nested_comparison_preserves_parens() {
    // Comparisons chain in Wado: `a == b == c` parses as `a == b && b == c`,
    // not `(a == b) == c`. So a comparison nested as an operand of another
    // comparison must keep its parens or the formatter silently rewrites the
    // meaning. Same for `<` chains and mixed groups that would become invalid
    // chains (`a < b < c == d`).
    let source = r"fn run() {
    let x = (a == 0) == b;
    let y = (a < b) == c;
    let z = (a < b < c) == d;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("(a == 0) == b"),
        "nested == must keep parens: {formatted}"
    );
    assert!(
        formatted.contains("(a < b) == c"),
        "nested comparison must keep parens: {formatted}"
    );
    assert!(
        formatted.contains("(a < b < c) == d"),
        "nested comparison chain must keep parens: {formatted}"
    );
    // The AST round-trip is the real invariant.
    assert_format_preserves_ast(source);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_long_signature_wraps_params() {
    // A function signature whose inline form exceeds the 120-col width must
    // wrap its parameters one-per-line, like every other comma list, instead
    // of emitting a single over-width line.
    let source = "fn keyword_carrier_admits(kw: KeywordInfo, kw_index: i32, carrier_indices: List<i32>, carrier_first_chars: List<List<char>>, carrier_is_wildcard: List<bool>) -> bool {\n    return true;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    for line in formatted.lines() {
        assert!(
            line.chars().count() <= 120,
            "line exceeds 120 cols: {line:?}\n\nformatted:\n{formatted}"
        );
    }
    assert!(
        formatted.contains("fn keyword_carrier_admits(\n"),
        "long signature params should wrap one-per-line: {formatted}"
    );
    assert_format_preserves_ast(source);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_long_interface_method_wraps_params() {
    // An interface method whose inline signature overflows must wrap its
    // parameters one-per-line, like a free function.
    let source = "interface Service {\n    fn handle_request(alpha: i32, beta: i32, gamma: i32, delta: i32, epsilon: i32, zeta: i32, eta: i32, theta: i32) -> bool;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    for line in formatted.lines() {
        assert!(
            line.chars().count() <= 120,
            "line exceeds 120 cols: {line:?}\n\nformatted:\n{formatted}"
        );
    }
    assert!(
        formatted.contains("fn handle_request(\n"),
        "long interface method params should wrap one-per-line: {formatted}"
    );
    assert_format_preserves_ast(source);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_long_world_export_fn_wraps_params() {
    // A world's `export fn` whose inline signature overflows must wrap its
    // parameters one-per-line too.
    let source = "world app {\n    export fn run(alpha: i32, beta: i32, gamma: i32, delta: i32, epsilon: i32, zeta: i32, eta: i32, theta: i32, iota: i32) -> bool;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    for line in formatted.lines() {
        assert!(
            line.chars().count() <= 120,
            "line exceeds 120 cols: {line:?}\n\nformatted:\n{formatted}"
        );
    }
    assert!(
        formatted.contains("fn run(\n"),
        "long world export fn params should wrap one-per-line: {formatted}"
    );
    assert_format_preserves_ast(source);
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_deref_index_preserves_parens() {
    // (*p)[i] must keep parens — *p[i] means *(p[i])
    let source = r"fn foo(data: &List<i32>) -> i32 {
    return (*data)[0];
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("(*data)[0]"),
        "deref-index parens must be preserved: {formatted}"
    );
    // Idempotency
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_deref_field_preserves_parens() {
    // (*p).field must keep parens — *p.field means *(p.field)
    let source = r"fn foo(p: &Point) -> i32 {
    return (*p).x;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("(*p).x"),
        "deref-field parens must be preserved: {formatted}"
    );
}

#[test]
fn test_format_deref_method_preserves_parens() {
    // (*p).method() must keep parens
    let source = r"fn foo(data: &List<i32>) -> i32 {
    return (*data).len();
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("(*data).len()"),
        "deref-method parens must be preserved: {formatted}"
    );
}

#[test]
fn test_format_doc_comment_before_attribute_no_extra_blank() {
    // Doc comment immediately before an attribute must not grow a blank line
    let source = "/// Doc\n#[compiler_item(\"tuple\")]\npub type [..T];\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        !formatted.contains("/// Doc\n\n#["),
        "doc comment + attribute must not have a blank line between them: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_impl_explicit_type_params() {
    // Formatter must always emit explicit `impl<T>`, never compact `impl Foo<T>`
    let source = r"
impl<T> WrapBox<T> {
    pub fn new(value: T) -> WrapBox<T> {
        return WrapBox { value };
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("impl<T> WrapBox<T>"),
        "impl type params must be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_impl_bounded_type_params() {
    // Bounded compact form `impl Foo<T: Ord>` should be normalized to `impl<T: Ord> Foo<T>`
    let source = r"
impl MyArray<T: Ord> {
    fn sort(&mut self) {
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("impl<T: Ord> MyArray<T>"),
        "bounded compact form should be expanded: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_impl_trait_for_type_preserves_params() {
    let source = r"
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return true;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("impl<T: Eq> Eq for Pair<T>"),
        "trait impl type params must be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

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

        // Skip files that expect compile errors or are TODO modules
        if source.contains("compile_error")
            || source.contains("\"TODO\": true")
            || source.contains("#![TODO]")
        {
            continue;
        }

        // First format
        let formatted1 = match wado_compiler::format(&source) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{filename}: format error: {e}"));
                continue;
            }
        };

        // Second format
        let formatted2 = match wado_compiler::format(&formatted1) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{filename}: second format error: {e}"));
                continue;
            }
        };

        // Check idempotency
        if formatted1 != formatted2 {
            failures.push(format!(
                "{filename}: not idempotent\nFirst:\n{formatted1}\nSecond:\n{formatted2}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Format idempotency failures:\n{}",
        failures.join("\n\n---\n\n")
    );
}

#[test]
fn test_format_break_with_label() {
    let source = r"fn run() {
    outer: {
        break outer;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break outer;"),
        "break with label should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_break_with_label_and_value() {
    let source = r"fn run() {
    let x = foo: {
        break foo: 42;
    };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break foo: 42;"),
        "break with label and value should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_break_with_label_and_expression() {
    let source = r"fn run() {
    let result = compute: {
        let a = 10;
        let b = 20;
        break compute: a + b;
    };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break compute: a + b;"),
        "break with label and expression should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

#[test]
fn test_format_nested_labeled_blocks() {
    let source = r"fn run() {
    let result = outer: {
        let x = inner: {
            break inner: 10;
        };
        break outer: x * 2;
    };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("break inner: 10;"),
        "break inner with value should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("break outer: x * 2;"),
        "break outer with expression should be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "should be idempotent");
}

/// Golden test: format(dirty) == clean, and the result is idempotent.
///
/// `format.fixtures/all-dirty.wado` exercises all the syntax constructs whose
/// formatting was fixed. `generated/format.fixtures/all.clean.wado` is the expected
/// canonical output. The test verifies:
///   1. format(dirty) == clean   (fixes are applied)
///   2. format(clean) == clean   (idempotency)
#[test]
fn test_format_golden() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/format.fixtures");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/generated/format.fixtures");
    let dirty = fs::read_to_string(fixtures.join("all-dirty.wado")).expect("read dirty");
    let clean = fs::read_to_string(golden.join("all.clean.wado")).expect("read clean");

    let formatted = wado_compiler::format(&dirty).expect("format dirty failed");
    assert_eq!(
        formatted, clean,
        "format(dirty) should equal clean\n\nActual:\n{formatted}\n\nExpected:\n{clean}"
    );

    let formatted2 = wado_compiler::format(&formatted).expect("format clean failed");
    assert_eq!(formatted, formatted2, "format(clean) should be idempotent");
}

/// Golden test for "messy" inputs: excessive blank lines, block comments in
/// unusual positions, missing spaces in use{}, implicit self normalization, etc.
///
/// `format.fixtures/mess-dirty.wado` has many deliberately weird patterns.
/// `generated/format.fixtures/mess.clean.wado` is the expected canonical output.
#[test]
fn test_format_golden_mess() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/format.fixtures");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/generated/format.fixtures");
    let dirty = fs::read_to_string(fixtures.join("mess-dirty.wado")).expect("read mess dirty");
    let clean = fs::read_to_string(golden.join("mess.clean.wado")).expect("read mess clean");

    let formatted = wado_compiler::format(&dirty).expect("format mess dirty failed");
    assert_eq!(
        formatted, clean,
        "format(mess dirty) should equal mess clean\n\nActual:\n{formatted}\n\nExpected:\n{clean}"
    );

    let formatted2 = wado_compiler::format(&formatted).expect("format mess clean failed");
    assert_eq!(
        formatted, formatted2,
        "format(mess clean) should be idempotent"
    );
}

/// Golden test for `no_prelude` constructs: effects, resources, WASI attributes,
/// and other syntax that requires `#![no_prelude]` or cannot compile.
///
/// `format.fixtures/no_prelude-dirty.wado` has dirty patterns (e.g. `self: &Self`).
/// `generated/format.fixtures/no_prelude.clean.wado` is the expected canonical output.
#[test]
fn test_format_golden_no_prelude() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/format.fixtures");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/generated/format.fixtures");
    let dirty =
        fs::read_to_string(fixtures.join("no_prelude-dirty.wado")).expect("read no_prelude dirty");
    let clean =
        fs::read_to_string(golden.join("no_prelude.clean.wado")).expect("read no_prelude clean");

    let formatted = wado_compiler::format(&dirty).expect("format no_prelude dirty failed");
    assert_eq!(
        formatted, clean,
        "format(no_prelude dirty) should equal no_prelude clean\n\nActual:\n{formatted}\n\nExpected:\n{clean}"
    );

    let formatted2 = wado_compiler::format(&formatted).expect("format no_prelude clean failed");
    assert_eq!(
        formatted, formatted2,
        "format(no_prelude clean) should be idempotent"
    );
}

/// Golden test for operator precedence: redundant parens are removed, necessary
/// parens are kept, and spacing is normalized.
///
/// `format.fixtures/ops.all-dirty.wado` has redundant parens and inconsistent
/// spacing. `generated/format.fixtures/ops.all.clean.wado` is the expected canonical output.
#[test]
fn test_format_golden_ops_all() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/format.fixtures");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/generated/format.fixtures");
    let dirty =
        fs::read_to_string(fixtures.join("ops.all-dirty.wado")).expect("read ops.all dirty");
    let clean = fs::read_to_string(golden.join("ops.all.clean.wado")).expect("read ops.all clean");

    let formatted = wado_compiler::format(&dirty).expect("format ops.all dirty failed");
    assert_eq!(
        formatted, clean,
        "format(ops.all dirty) should equal ops.all clean\n\nActual:\n{formatted}\n\nExpected:\n{clean}"
    );

    let formatted2 = wado_compiler::format(&formatted).expect("format ops.all clean failed");
    assert_eq!(
        formatted, formatted2,
        "format(ops.all clean) should be idempotent"
    );
}

/// Golden test for operator precedence with messy inputs: block comments,
/// extra blank lines, and no spacing around operators.
///
/// `format.fixtures/ops.mess-dirty.wado` has messy formatting around operators.
/// `generated/format.fixtures/ops.mess.clean.wado` is the expected canonical output.
#[test]
fn test_format_golden_ops_mess() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/format.fixtures");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/generated/format.fixtures");
    let dirty =
        fs::read_to_string(fixtures.join("ops.mess-dirty.wado")).expect("read ops.mess dirty");
    let clean =
        fs::read_to_string(golden.join("ops.mess.clean.wado")).expect("read ops.mess clean");

    let formatted = wado_compiler::format(&dirty).expect("format ops.mess dirty failed");
    assert_eq!(
        formatted, clean,
        "format(ops.mess dirty) should equal ops.mess clean\n\nActual:\n{formatted}\n\nExpected:\n{clean}"
    );

    let formatted2 = wado_compiler::format(&formatted).expect("format ops.mess clean failed");
    assert_eq!(
        formatted, formatted2,
        "format(ops.mess clean) should be idempotent"
    );
}

#[test]
fn test_format_match_arm_trailing_comment() {
    let source = r"
fn foo(c: char) -> bool {
    return match c {
        ' ' => true,              // SP
        '\t' => true,             // HT
        '\n' => true,             // LF
        _ => false,
    };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("true,  // SP"),
        "trailing comment should be preserved on match arm: {formatted}"
    );
    assert!(
        formatted.contains("true,  // HT"),
        "trailing comment should be preserved on match arm: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_if_else_chain_all_multiline() {
    // When the first branch of an if-else chain is multiline, all branches should be multiline
    let source = r"
fn foo(x: i32) -> i32 {
    let result = if x < 10 {
        '0' as i32 + x
    } else if x < 100 {
        'A' as i32 + x - 10
    } else {
        'a' as i32 + x - 100
    };
    return result;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // The else-if and else blocks should NOT be inlined when the first branch is multiline
    assert!(
        !formatted.contains("} else if x < 100 { "),
        "else-if should not be inlined when first branch is multiline: {formatted}"
    );
    assert!(
        formatted.contains("} else if x < 100 {\n"),
        "else-if should be multiline: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_single_arg_wide_wraps_multiline() {
    // A single argument that exceeds MAX_LINE_WIDTH should trigger multiline call formatting
    let source = r#"
fn run() {
    println(match classify(i) { Fizz => "Fizz", Buzz => "Buzz", FizzBuzz => "FizzBuzz", Number(n) => `{n}` });
}
"#;
    let formatted = wado_compiler::format(source).expect("format failed");
    // The match should have been broken out to multiline since it exceeds 120 chars
    assert!(
        formatted.contains('\n'),
        "wide single-arg call should wrap: {formatted}"
    );
    // Should be idempotent
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_trait_assoc_type_bounds_preserved() {
    let source = r"
trait Serializer {
    type SeqSerializer: SerializeSeq;
    type MapSerializer: SerializeMap;

    fn serialize_i32(&mut self, v: i32) -> Result<(), SerializeError>;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("type SeqSerializer: SerializeSeq;"),
        "assoc type bounds should be preserved: {formatted}"
    );
    assert!(
        formatted.contains("type MapSerializer: SerializeMap;"),
        "assoc type bounds should be preserved: {formatted}"
    );
    // Should be idempotent
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_logical_chain_multiline() {
    // A long && chain should be broken with operator at start of continuation lines
    let source = r"
fn run() {
    if self.input.get_byte(self.pos) as i32 == 'n' as i32 && self.input.get_byte(self.pos + 1) as i32 == 'u' as i32 && self.input.get_byte(self.pos + 2) as i32 == 'l' as i32 && self.input.get_byte(self.pos + 3) as i32 == 'l' as i32 {
        return null;
    }
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // Each continuation line should start with && (after indentation)
    let lines: Vec<&str> = formatted.lines().collect();
    let continuation_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("&&"))
        .collect();
    assert!(
        !continuation_lines.is_empty(),
        "&& should appear at start of continuation lines: {formatted}"
    );
    // Should be idempotent
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

// ============================================================
// Round-trip AST equivalence tests
//
// These verify that parse(format(source)) produces the same AST
// as parse(source), ignoring span positions. This catches bugs
// where the formatter changes program semantics (e.g., dropping
// parens, reordering expressions, losing constructs).
// ============================================================

#[test]
fn test_roundtrip_ast_simple() {
    assert_format_preserves_ast(
        r"
fn run() {
    let x = 1;
    let y = (x + 2) * 3;
}
",
    );
}

#[test]
fn test_roundtrip_ast_operator_precedence() {
    assert_format_preserves_ast(
        r"
fn run() {
    let a = (1 + 2) * 3;
    let b = 1 + 2 * 3;
    let c = (a || b) && c;
    let d = a || (b && c);
    let e = !(a && b);
    let f = -(a + b);
}
",
    );
}

#[test]
fn test_roundtrip_ast_deref_parens() {
    assert_format_preserves_ast(
        r"
fn foo(data: &List<i32>, p: &Point) -> i32 {
    let a = (*data)[0];
    let b = (*p).x;
    let c = (*data).len();
    return a + b + c;
}
",
    );
}

#[test]
fn test_roundtrip_ast_complex_expressions() {
    assert_format_preserves_ast(
        r#"
fn run() {
    let x = if true { 1 } else { 2 };
    let y = match x {
        1 => "one",
        2 => "two",
        _ => "other",
    };
}
"#,
    );
}

#[test]
fn test_roundtrip_ast_struct_and_methods() {
    assert_format_preserves_ast(
        r"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn sum(&self) -> i32 {
        return self.x + self.y;
    }

    fn origin() -> Point {
        return Point { x: 0, y: 0 };
    }
}
",
    );
}

#[test]
fn test_roundtrip_ast_closures() {
    assert_format_preserves_ast(
        r"
fn run() {
    let add = |a: i32, b: i32| a + b;
    let compute = |x: i32| {
        let doubled = x * 2;
        return doubled + x;
    };
    let direct = (|x: i32| x + 1)(41);
}
",
    );
}

#[test]
fn test_roundtrip_ast_call_on_compound_callee() {
    // Each statement here uses a callee shape that lives above the
    // postfix level (or is otherwise call-ambiguous). The formatter must
    // keep the parens around the callee so the AST round-trips.
    assert_format_preserves_ast(
        r"
type FnI = fn(i32) -> i32;

fn make() -> FnI {
    return |x: i32| x + 1;
}

fn flag() -> bool {
    return true;
}

fn run() {
    let cast_call = (make() as FnI)(10);
    let if_call = (if flag() { make() } else { make() })(10);
    let match_call = (match flag() { true => make(), false => make() })(10);
    let unary_call = (-make())(10);
    let binary_call = (make() + make())(10);
}
",
    );
}

#[test]
fn test_roundtrip_ast_control_flow() {
    assert_format_preserves_ast(
        r"
fn run() {
    for let mut i = 0; i < 10; i += 1 {
        if i % 2 == 0 {
            continue;
        }
    }

    while true {
        break;
    }

    outer: {
        break outer;
    }
}
",
    );
}

#[test]
fn test_roundtrip_ast_generics_and_traits() {
    assert_format_preserves_ast(
        r"
trait Container {
    type Item;
    fn get(&self) -> Self::Item;
}

fn identity<T>(x: T) -> T {
    return x;
}

struct Box<T> {
    value: T,
}
",
    );
}

#[test]
fn test_roundtrip_ast_variants_and_match() {
    assert_format_preserves_ast(
        r"
variant Shape {
    Circle(f64),
    Rectangle([f64, f64]),
    Point,
}

fn area(s: Shape) -> f64 {
    return match s {
        Circle(r) => 3.14 * r * r,
        Rectangle(dims) => dims.0 * dims.1,
        Point => 0.0,
    };
}
",
    );
}

#[test]
fn test_roundtrip_ast_imports_and_effects() {
    assert_format_preserves_ast(
        r#"
use { println } from "core:cli";

fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}
"#,
    );
}

#[test]
fn test_roundtrip_ast_bitwise_and_cast() {
    assert_format_preserves_ast(
        r"
fn run() {
    let a = 0xFF & 0x0F;
    let b = (1 << 4) | 3;
    let c = ~a;
    let d = 42 as f64;
    let e = 'A' as i32;
}
",
    );
}

#[test]
fn test_roundtrip_ast_all_fixtures() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut failures = Vec::new();

    for entry in fs::read_dir(&fixtures_dir).expect("cannot read fixtures dir") {
        let entry = entry.expect("cannot read entry");
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) != Some("wado") {
            continue;
        }
        if path.is_dir() {
            continue;
        }

        let source = fs::read_to_string(&path).expect("cannot read file");
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip files that expect compile errors or are TODO tests
        if source.contains("compile_error") || source.contains("\"TODO\": true") {
            continue;
        }

        let formatted = match wado_compiler::format(&source) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let original_ast = match wado_compiler::parse(&source).into_fail_fast() {
            Ok(r) => r.ast,
            Err(_) => continue,
        };
        let formatted_ast = match wado_compiler::parse(&formatted).into_fail_fast() {
            Ok(r) => r.ast,
            Err(e) => {
                failures.push(format!("{filename}: formatted code failed to parse: {e:?}"));
                continue;
            }
        };

        let original_debug = normalize_ast_debug(&format!("{original_ast:#?}"));
        let formatted_debug = normalize_ast_debug(&format!("{formatted_ast:#?}"));

        if original_debug != formatted_debug {
            let orig_lines: Vec<&str> = original_debug.lines().collect();
            let fmt_lines: Vec<&str> = formatted_debug.lines().collect();
            let mut diff_line = String::new();
            for (i, (o, f)) in orig_lines.iter().zip(fmt_lines.iter()).enumerate() {
                if o != f {
                    diff_line = format!(
                        "line {i}:\n  orig: {}\n   fmt: {}",
                        &o[..o.len().min(120)],
                        &f[..f.len().min(120)]
                    );
                    break;
                }
            }
            failures.push(format!(
                "{filename}: AST changed by formatter ({diff_line})"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Round-trip AST equivalence failures ({} files):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Regression test: the formatter must preserve parentheses in `if let ... && (expr)`
/// and `while let ... && (expr)` conditions. Without parens, the let-chain element
/// count changes on re-parse (e.g., 2 elements become 3), violating round-trip
/// AST equivalence.
#[test]
fn test_roundtrip_ast_let_chain_paren_preservation() {
    // if let with && boolean guard containing &&
    assert_format_preserves_ast(
        r"
fn test() {
    let opt = Option::<i32>::Some(5);
    let flag1 = true;
    let flag2 = true;
    if let Some(x) = opt && (flag1 && flag2) {
        println(`{x}`);
    }
}
",
    );

    // while let with && guard containing &&
    assert_format_preserves_ast(
        r"
fn test() {
    let mut iter = [1, 2, 3].iter();
    let min = 0;
    let max = 10;
    while let Some(v) = iter.next() && (v > min && v < max) {
        println(`{v}`);
    }
}
",
    );

    // if let with || inside guard
    assert_format_preserves_ast(
        r"
fn test() {
    let opt = Option::<i32>::Some(5);
    if let Some(x) = opt && (x > 10 || x < 0) {
        println(`{x}`);
    }
}
",
    );
}

#[test]
fn test_roundtrip_ast_all_stdlib() {
    let lib_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    let mut failures = Vec::new();

    fn visit_dir(dir: &Path, failures: &mut Vec<String>) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries {
            let entry = entry.expect("cannot read entry");
            let path = entry.path();
            if path.is_dir() {
                visit_dir(&path, failures);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("wado") {
                continue;
            }

            let source = fs::read_to_string(&path).expect("cannot read file");
            let rel = path.display().to_string();

            let formatted = match wado_compiler::format(&source) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let original_ast = match wado_compiler::parse(&source).into_fail_fast() {
                Ok(r) => r.ast,
                Err(_) => continue,
            };
            let formatted_ast = match wado_compiler::parse(&formatted).into_fail_fast() {
                Ok(r) => r.ast,
                Err(e) => {
                    failures.push(format!("{rel}: formatted code failed to parse: {e:?}"));
                    continue;
                }
            };

            let original_debug = normalize_ast_debug(&format!("{original_ast:#?}"));
            let formatted_debug = normalize_ast_debug(&format!("{formatted_ast:#?}"));

            if original_debug != formatted_debug {
                let orig_lines: Vec<&str> = original_debug.lines().collect();
                let fmt_lines: Vec<&str> = formatted_debug.lines().collect();
                let mut diff_line = String::new();
                for (i, (o, f)) in orig_lines.iter().zip(fmt_lines.iter()).enumerate() {
                    if o != f {
                        diff_line = format!(
                            "line {i}:\n  orig: {}\n   fmt: {}",
                            &o[..o.len().min(120)],
                            &f[..f.len().min(120)]
                        );
                        break;
                    }
                }
                failures.push(format!("{rel}: AST changed by formatter ({diff_line})"));
            }
        }
    }

    visit_dir(&lib_dir, &mut failures);

    assert!(
        failures.is_empty(),
        "Round-trip AST equivalence failures in stdlib ({} files):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn test_format_attribute_numeric_arg_preserved() {
    // Numeric attribute arguments must not be quoted: #[timeout_ms(120000)] stays as-is
    let source = "#[timeout_ms(120000)]\ntest \"bench\" {\n    assert 1 == 1;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("#[timeout_ms(120000)]"),
        "numeric attribute argument must not be quoted: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_template_string_escape_sequences_preserved() {
    // Escape sequences in template strings must be preserved, not decoded
    let source = "fn foo() {\n    let s = `hello\\nworld`;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("`hello\\nworld`"),
        "\\n escape in template string must be preserved: {formatted}"
    );
    let formatted2 = wado_compiler::format(&formatted).expect("format failed");
    assert_eq!(formatted, formatted2, "format should be idempotent");
}

#[test]
fn test_format_template_string_escape_sequences_all() {
    // All escape sequences: \n, \r, \t, \0, \\, \{, \}
    let source = "fn foo() {\n    let s = `a\\nb\\rc\\td\\0e\\\\f\\{g\\}`;\n}\n";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("`a\\nb\\rc\\td\\0e\\\\f\\{g\\}`"),
        "all escape sequences in template string must be preserved: {formatted}"
    );
}

#[test]
fn test_format_match_two_arms_always_multiline() {
    // Match with 2+ arms must always be formatted multiline, never collapsed to one line
    let source = r"
fn foo(x: Option<i32>) -> i32 {
    return match x { Some(v) => v, None => 0 };
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    // The match should be expanded to multiple lines
    assert!(
        formatted.contains("Some(v) => v,\n"),
        "match with 2 arms must be multiline:\n{formatted}"
    );
}

#[test]
fn test_format_match_multiline_scrutinee_no_extra_blanks() {
    // Regression: when the match scrutinee spans multiple source lines, the
    // blank-line tracker used to count those source lines as if they were
    // blank, inserting spurious blanks between `{` and the first arm.
    //
    // Asserting on the `{` → first-arm transition (rather than on the token
    // that closes the scrutinee) keeps the test honest for scrutinee shapes
    // that don't end with `)` — `] {`, `" {`, identifiers, method chains, etc.
    let source = r"
fn caller(a: i32) -> i32 {
    return a;
}

fn foo() -> i32 {
    let x = match caller(
        1,
    ) {
        1 => 1,
        _ => 0,
    };
    return x;
}
";
    let formatted = wado_compiler::format(source).expect("format failed");
    assert!(
        formatted.contains("{\n        1 => 1,"),
        "expected exactly one newline between match `{{` and first arm:\n{formatted}",
    );
}
