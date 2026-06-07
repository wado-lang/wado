//! Integration tests for `Engine::semantic_tokens` — the end-to-end
//! highlighting path that classifies identifiers by their resolved symbol
//! kind via the cached `Semantics` snapshot.
//!
//! These exercise the wire-facing entry point (delta-encoded `Vec<u32>`),
//! decoding the 5-tuples back to absolute positions so classification can be
//! asserted per token. The unit tests in `src/semantic_tokens.rs` cover the
//! classifier internals; this file pins that the snapshot is actually
//! threaded through `Engine`.

use wado_lsp::Engine;
use wado_lsp::semantic_tokens::{token_modifier, token_type};
use wado_lsp::test_support::MapHost;

/// One decoded semantic token at an absolute position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decoded {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

/// Decode the LSP delta-encoded `Vec<u32>` back to absolute positions.
fn decode(data: &[u32]) -> Vec<Decoded> {
    assert_eq!(data.len() % 5, 0, "data must be a multiple of 5");
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut start = 0u32;
    for chunk in data.chunks_exact(5) {
        let (delta_line, delta_start, length, token_type, modifiers) =
            (chunk[0], chunk[1], chunk[2], chunk[3], chunk[4]);
        if delta_line == 0 {
            start += delta_start;
        } else {
            line += delta_line;
            start = delta_start;
        }
        out.push(Decoded {
            line,
            start,
            length,
            token_type,
            modifiers,
        });
    }
    out
}

fn tokens_for(source: &str) -> Vec<Decoded> {
    let path = "/test.wado";
    let uri = format!("file://{path}");
    let host = MapHost::single(path, source);
    let mut engine = Engine::new();
    engine.open_document(&uri, source.to_string());
    let data = futures::executor::block_on(engine.semantic_tokens(&uri, &host));
    decode(&data)
}

fn at(tokens: &[Decoded], line: u32, start: u32) -> &Decoded {
    tokens
        .iter()
        .find(|t| t.line == line && t.start == start)
        .unwrap_or_else(|| panic!("no token at {line}:{start} in {tokens:?}"))
}

#[test]
fn classifies_by_resolved_symbol_kind() {
    // `Point` is a struct; `area` is a function; `p` is a local; the field
    // type `i32` is a type. The heuristic path cannot reach this accuracy.
    let src = "\
struct Point { x: i32, y: i32 }
fn area(p: Point) -> i32 {
    return p.x;
}
";
    let tokens = tokens_for(src);

    // `Point` declaration (line 0, col 7) → struct + declaration.
    let point_decl = at(&tokens, 0, 7);
    assert_eq!(point_decl.token_type, token_type::STRUCT);
    assert_ne!(point_decl.modifiers & token_modifier::DECLARATION, 0);

    // `Point` in the parameter type position (line 1, after `p: `) → struct.
    let point_use = tokens
        .iter()
        .find(|t| t.line == 1 && t.token_type == token_type::STRUCT)
        .expect("Point use in parameter type");
    assert_eq!(point_use.token_type, token_type::STRUCT);

    // `area` declaration → function + declaration.
    let area = at(&tokens, 1, 3);
    assert_eq!(area.token_type, token_type::FUNCTION);
    assert_ne!(area.modifiers & token_modifier::DECLARATION, 0);

    // `p` parameter (line 1, col 8) → parameter.
    let p_param = at(&tokens, 1, 8);
    assert_eq!(p_param.token_type, token_type::PARAMETER);

    // `p` use in `return p.x` (line 2) → parameter (resolved, not variable).
    let p_use = tokens
        .iter()
        .find(|t| t.line == 2 && t.token_type == token_type::PARAMETER)
        .expect("p use site");
    assert_eq!(p_use.token_type, token_type::PARAMETER);
}

#[test]
fn enum_and_readonly_modifiers() {
    let src = "\
enum Color { Red, Green }
fn f() {
    let c = Color::Red;
}
";
    let tokens = tokens_for(src);

    // `Color` declaration → enum.
    let color_decl = at(&tokens, 0, 5);
    assert_eq!(color_decl.token_type, token_type::ENUM);

    // `c` immutable local → variable + readonly.
    let c = at(&tokens, 2, 8);
    assert_eq!(c.token_type, token_type::VARIABLE);
    assert_ne!(c.modifiers & token_modifier::READONLY, 0);
}
