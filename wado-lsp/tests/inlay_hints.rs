//! Integration tests for `Engine::inlay_hints` — end-to-end coverage of
//! type and parameter-name hints. The unit tests in
//! `wado-lsp/src/inlay_hints.rs` already drive each branch of the hint
//! computation directly against a `Semantics` snapshot; these tests
//! validate the public `Engine` surface (`Engine::open_document` →
//! `Engine::inlay_hints`) and the LSP-shaped wire types.

use wado_lsp::test_support::MapHost;
use wado_lsp::{Engine, InlayHint, InlayHintKind, Position, Range};

async fn hints(source: &str) -> Vec<InlayHint> {
    let path = "/test.wado";
    let uri = format!("file://{path}");
    let host = MapHost::single(path, source);
    let mut engine = Engine::new();
    engine.open_document(&uri, source.to_string());
    let range = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: u32::MAX,
            character: u32::MAX,
        },
    };
    engine.inlay_hints(&uri, range, &host).await
}

#[test]
fn engine_returns_let_type_hint() {
    futures::executor::block_on(async {
        let source = "fn f() -> i32 {\n    let x = 1;\n    return x;\n}\n";
        let hints = hints(source).await;
        let type_hints: Vec<_> = hints
            .iter()
            .filter(|h| h.kind == InlayHintKind::Type)
            .collect();
        assert!(
            type_hints.iter().any(|h| h.label == ": i32"),
            "expected `: i32` hint on `let x = 1`; got {hints:?}",
        );
    });
}

#[test]
fn engine_returns_parameter_hints_for_free_function() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn add(a: i32, b: i32) -> i32 { return a + b; }\n",
            "fn run() -> i32 { return add(1, 2); }\n",
        );
        let hints = hints(source).await;
        let labels: Vec<_> = hints
            .iter()
            .filter(|h| h.kind == InlayHintKind::Parameter)
            .map(|h| h.label.clone())
            .collect();
        assert!(
            labels.contains(&"a:".to_string()) && labels.contains(&"b:".to_string()),
            "expected `a:` and `b:` hints; got {labels:?}",
        );
    });
}

#[test]
fn engine_returns_parameter_hints_for_method_call() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Counter { value: i32 }\n",
            "impl Counter {\n",
            "    fn bump(&mut self, by: i32) { self.value = self.value + by; }\n",
            "}\n",
            "fn run() {\n",
            "    let mut c = Counter { value: 0 };\n",
            "    c.bump(5);\n",
            "}\n",
        );
        let hints = hints(source).await;
        let labels: Vec<_> = hints
            .iter()
            .filter(|h| h.kind == InlayHintKind::Parameter)
            .map(|h| h.label.clone())
            .collect();
        assert!(
            labels.contains(&"by:".to_string()),
            "expected `by:` hint at `c.bump(5)`; got {labels:?}",
        );
    });
}

#[test]
fn engine_filters_hints_outside_range() {
    futures::executor::block_on(async {
        let source = "fn f() -> i32 {\n    let x = 1;\n    let y = 2;\n    return x + y;\n}\n";
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = MapHost::single(path, source);
        let mut engine = Engine::new();
        engine.open_document(&uri, source.to_string());
        // Restrict to line 1 only (the `let x = 1` line). The `let y =
        // 2` hint on line 2 must be filtered out.
        let range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 1,
                character: 999,
            },
        };
        let hints = engine.inlay_hints(&uri, range, &host).await;
        assert!(
            hints.iter().all(|h| h.position.line == 1),
            "every hint should be anchored on line 1; got {hints:?}",
        );
    });
}

#[test]
fn engine_returns_empty_for_unknown_document() {
    futures::executor::block_on(async {
        let path = "/nonexistent.wado";
        let uri = format!("file://{path}");
        let host = MapHost::empty();
        let engine = Engine::new();
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        };
        let hints = engine.inlay_hints(&uri, range, &host).await;
        assert!(
            hints.is_empty(),
            "closed/unknown document must produce no hints; got {hints:?}",
        );
    });
}
