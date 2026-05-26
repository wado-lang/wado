//! Behavior of LSP queries when the entry source fails to lex/parse.
//!
//! The Wado parser is currently fail-fast (no error-recovery), so a
//! single syntax error in the entry kills the entry's AST. This file
//! pins the **current** behavior so we notice when it changes:
//!
//! - `Engine::diagnostics` returns exactly one diagnostic: the
//!   lex/parse error with span attribution to the entry filename.
//! - Every position-bearing semantic query (`definition`, `hover`,
//!   `references`, `document_highlight`) returns `None` / empty
//!   because the snapshot's `Semantics` is empty.
//! - `semantic_tokens` still produces lexer-level tokens: highlighting
//!   degrades gracefully and does not blank out on a typo.
//!
//! See the module doc comment for the planned upgrade path.

use wado_lsp::test_support::MapHost;
use wado_lsp::{Diagnostic, Engine, Position, Severity};

const PATH: &str = "/test.wado";

/// Source with a deliberate syntax error: missing `}` after the body
/// of `f`. Everything around it is well-formed, which (under a
/// future error-recovery parser) would let semantic queries still
/// resolve in the surrounding regions.
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

async fn engine_with_broken_source() -> (Engine, MapHost) {
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

/// A lex/parse error on the entry surfaces as exactly one
/// `Severity::Error` diagnostic, attributed to the entry filename.
#[test]
fn parse_error_emits_one_diagnostic_attributed_to_entry() {
    futures::executor::block_on(async {
        let (engine, host) = engine_with_broken_source().await;
        let diags = engine.diagnostics(&uri(), &host).await;
        let errs = errors(&diags);
        assert_eq!(
            errs.len(),
            1,
            "expected exactly one parse error, got {}: {:#?}",
            errs.len(),
            errs
        );
        // Diagnostic message uses the "parse error: …" / "lexer error: …"
        // wire format that the loader emits, so editor pane labels match
        // what `wado compile` shows for the same input.
        assert!(
            errs[0].message.starts_with("parse error:")
                || errs[0].message.starts_with("lexer error:"),
            "unexpected diagnostic message: {}",
            errs[0].message,
        );
    });
}

/// With no parse, the snapshot has no AST → every position-bearing
/// query returns the empty answer. This is the current LSP-as-fail-fast
/// behavior; a future error-recovery parser should let these resolve
/// in the surviving regions (see module doc).
#[test]
fn position_queries_return_empty_on_parse_error() {
    futures::executor::block_on(async {
        let (engine, host) = engine_with_broken_source().await;
        let pos = Position {
            line: 1,
            character: 8,
        };
        assert!(
            engine.definition(&uri(), pos, &host).await.is_none(),
            "definition should be None when the entry failed to parse",
        );
        assert!(
            engine.hover(&uri(), pos, &host).await.is_none(),
            "hover should be None when the entry failed to parse",
        );
        assert!(
            engine.references(&uri(), pos, true, &host).await.is_empty(),
            "references should be empty when the entry failed to parse",
        );
        assert!(
            engine
                .document_highlight(&uri(), pos, &host)
                .await
                .is_empty(),
            "document_highlight should be empty when the entry failed to parse",
        );
    });
}

/// Semantic tokens degrade gracefully to lexer-level classification.
/// Highlighting must keep working even when the parser bails, so users
/// editing toward a missing brace don't see their colours disappear.
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

/// Re-opening with a fix recovers all semantic features. Sanity check
/// that the empty-Semantics path doesn't poison the cache.
#[test]
fn fixing_the_parse_error_recovers_semantics() {
    futures::executor::block_on(async {
        let host = MapHost::single(PATH, BROKEN_SOURCE);
        let mut engine = Engine::new();
        engine.open_document(&uri(), BROKEN_SOURCE.to_string());
        // First query: broken.
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
                .is_none(),
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
