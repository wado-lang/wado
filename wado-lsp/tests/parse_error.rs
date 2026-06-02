//! Behavior of LSP queries when the entry source has a syntax error.
//!
//! The Wado parser is error-recovering: a syntax error no longer discards
//! the whole module. A single broken item (or a missing brace) is recovered,
//! and the surrounding well-formed regions still analyze. This file pins that
//! recovery behavior:
//!
//! - `Engine::diagnostics` reports at least one parse/lex error, each with
//!   span attribution to the entry filename.
//! - Position-bearing semantic queries resolve in the healthy regions on
//!   either side of the error.
//! - `semantic_tokens` keeps producing lexer-level tokens.
//!
//! Lexer errors are also recovered: a stray character or a malformed numeric
//! prefix surfaces as a `lexer error: …` diagnostic but does not abort
//! analysis, so position queries still resolve on the surrounding well-formed
//! regions. The dedicated test `lexer_error_recovers_around_stray_character`
//! pins this.

use wado_lsp::test_support::MapHost;
use wado_lsp::{Diagnostic, Engine, Position, Severity};

const PATH: &str = "/test.wado";

/// Source with a deliberate syntax error: `f` is missing its closing `}`
/// before `g`. Error recovery closes `f`'s body at `fn g` so that `g` parses
/// as a complete item and `f`'s prefix (`let x`, `return x`) survives.
const BROKEN_SOURCE: &str = "\
fn f() -> i32 {
    let x: i32 = 1;
    return x;

fn g() -> i32 {
    return 2;
}
";

fn uri() -> String {
    format!("file://{PATH}")
}

fn engine_with_broken_source() -> (Engine, MapHost) {
    let mut engine = Engine::new();
    engine.open_document(&uri(), BROKEN_SOURCE.to_string());
    let host = MapHost::single(PATH, BROKEN_SOURCE);
    (engine, host)
}

fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

/// A syntax error surfaces as at least one `Severity::Error` diagnostic,
/// attributed to the entry filename and using the `parse error: …` /
/// `lexer error: …` wire format the loader emits.
#[test]
fn parse_error_emits_diagnostics_attributed_to_entry() {
    futures::executor::block_on(async {
        let (engine, host) = engine_with_broken_source();
        let diags = engine.diagnostics(&uri(), &host).await;
        let errs = errors(&diags);
        assert!(
            !errs.is_empty(),
            "expected at least one parse error, got none",
        );
        for e in &errs {
            assert!(
                e.message.starts_with("parse error:") || e.message.starts_with("lexer error:"),
                "unexpected diagnostic message: {}",
                e.message,
            );
        }
    });
}

/// With error recovery, semantic queries resolve in the well-formed regions
/// around the syntax error: both `g` (after the broken brace) and `x` (in
/// `f`'s surviving prefix) hover/resolve.
#[test]
fn position_queries_resolve_in_healthy_regions() {
    futures::executor::block_on(async {
        let (engine, host) = engine_with_broken_source();

        // `x` in `let x: i32 = 1;` (line 1, col 8) — inside the recovered
        // prefix of the brace-less `f`.
        let x_pos = Position {
            line: 1,
            character: 8,
        };
        assert!(
            engine.hover(&uri(), x_pos, &host).await.is_some(),
            "hover on `x` should resolve in f's surviving prefix",
        );

        // `g` in `fn g() -> i32` (line 4, col 3) — a complete item recovered
        // after the missing brace.
        let g_pos = Position {
            line: 4,
            character: 3,
        };
        assert!(
            engine.hover(&uri(), g_pos, &host).await.is_some(),
            "hover on `g` should resolve after the recovered brace",
        );
    });
}

/// Semantic tokens degrade gracefully to lexer-level classification.
/// Highlighting must keep working even with a syntax error present.
#[test]
fn semantic_tokens_survive_parse_error() {
    let mut engine = Engine::new();
    engine.open_document(&uri(), BROKEN_SOURCE.to_string());
    let tokens = engine.semantic_tokens(&uri());
    assert!(
        !tokens.is_empty(),
        "semantic_tokens should still produce lexer-based highlights",
    );
    // LSP delta-encoded form: every token is 5 u32s.
    assert_eq!(
        tokens.len() % 5,
        0,
        "semantic_tokens must be a multiple of 5 (deltaLine, deltaStart, length, type, mods)",
    );
}

/// A stray character is now a recovered *lexer* error, not a fatal one. The
/// LSP must surface a `lexer error: …` diagnostic and still resolve hovers
/// elsewhere in the file.
#[test]
fn lexer_error_recovers_around_stray_character() {
    futures::executor::block_on(async {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n@\nfn g() -> i32 { return 2; }\n";
        let mut engine = Engine::new();
        engine.open_document(&uri(), source.to_string());
        let host = MapHost::single(PATH, source);

        let diags = engine.diagnostics(&uri(), &host).await;
        let lex_errs: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error && d.message.starts_with("lexer error:"))
            .collect();
        assert!(
            !lex_errs.is_empty(),
            "expected a lexer-error diagnostic for the stray '@'",
        );

        // `x` in `let x: i32 = 1;` still hovers — recovery preserves the
        // surrounding healthy region.
        let hover = engine
            .hover(
                &uri(),
                Position {
                    line: 1,
                    character: 8,
                },
                &host,
            )
            .await;
        assert!(hover.is_some(), "hover should resolve before the lex error");
    });
}

/// Re-opening with a fix recovers all semantic features. Sanity check that
/// the partial-Semantics path doesn't poison the cache.
#[test]
fn fixing_the_parse_error_recovers_semantics() {
    futures::executor::block_on(async {
        let host = MapHost::single(PATH, BROKEN_SOURCE);
        let mut engine = Engine::new();
        engine.open_document(&uri(), BROKEN_SOURCE.to_string());
        // First query on the broken source still resolves `x`.
        assert!(
            engine
                .hover(
                    &uri(),
                    Position {
                        line: 1,
                        character: 8,
                    },
                    &host,
                )
                .await
                .is_some(),
        );

        // Edit to fix the missing brace.
        let fixed = "\
fn f() -> i32 {
    let x: i32 = 1;
    return x;
}

fn g() -> i32 {
    return 2;
}
";
        let host = MapHost::single(PATH, fixed);
        engine.update_document(&uri(), fixed.to_string());
        let hover = engine
            .hover(
                &uri(),
                Position {
                    line: 1,
                    character: 8,
                },
                &host,
            )
            .await;
        assert!(
            hover.is_some(),
            "hover on `x` should resolve once the parse error is fixed",
        );
    });
}

/// A body-level syntax error leaves an `Expr::Error` placeholder inside an
/// otherwise reachable function body. The LSP `semantics` path must tolerate
/// it without panicking: `build_tir_from_state` skips reify (Stage 5) for a
/// module with syntax errors, so reify never walks the placeholder, while the
/// Phase-1 body walk still records diagnostics and edges. Pins the
/// reify-skip guard that makes the reify `Error` arms `unreachable!`.
#[test]
fn body_error_node_does_not_panic_semantics() {
    futures::executor::block_on(async {
        let path = "file:///body_err.wado";
        // `let b = a + ;` reifies to `Binary { a, +, Expr::Error }` in a
        // reachable, otherwise-valid body — the case that reaches reify.
        let src = "export fn f() -> i32 {\n    let a = 1;\n    let b = a + ;\n    return 0;\n}\n";
        let mut engine = Engine::new();
        engine.open_document(path, src.to_string());
        let host = MapHost::single("/body_err.wado", src);
        let diags = engine.diagnostics(path, &host).await;
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.starts_with("parse error:")),
            "the body syntax error must surface as a parse-error diagnostic",
        );
    });
}
