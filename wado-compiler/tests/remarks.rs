//! Optimizer remark tests (WEP `wep-2026-06-03-optimizer-remarks.md`).
//!
//! A value-semantic copy that survives the optimizer is reported as an
//! info-level `remark:` diagnostic with a source span; a copy the optimizer
//! removes (scalarizes / elides) is not.

mod common;

use common::{InMemoryHost, runtime};
use wado_compiler::{CompilerOptions, OptLevel};

/// Compile `source` at `-O2` and return the `remark:` diagnostics as
/// `"line:col message"` strings.
fn remarks_for(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        ..CompilerOptions::default()
    };
    let _ = runtime().block_on(wado_compiler::compile_with_options(
        source,
        &host,
        Some("test.wado"),
        options,
    ));
    host.diagnostics()
        .into_iter()
        .filter(|d| d.message.starts_with("remark:"))
        .map(|d| {
            let loc = d
                .span
                .as_ref()
                .map(|s| format!("{}:{}", s.line, s.column))
                .unwrap_or_default();
            format!("{loc} {}", d.message)
        })
        .collect()
}

#[test]
fn surviving_list_copy_is_remarked() {
    // `b` is a deep value-copy of `a` that is then mutated, so the copy of the
    // backing array cannot be elided and survives to the final IR.
    let remarks = remarks_for(
        r#"
use { println } from "core:cli";

export fn run() with Stdout {
    let a: List<i32> = [1, 2, 3];
    let mut b = a;
    b.push(4);
    println(`{a.len()} {b.len()}`);
}
"#,
    );

    assert_eq!(
        remarks.len(),
        1,
        "expected exactly one remark, got {remarks:?}"
    );
    assert!(
        remarks[0].contains("a copy of") && remarks[0].contains("List<i32>"),
        "unexpected remark text: {remarks:?}"
    );
    // The remark points at the copying statement `let mut b = a;` (line 6).
    assert!(
        remarks[0].starts_with("6:"),
        "remark should point at the copy statement on line 6: {remarks:?}"
    );
}

#[test]
fn scalarized_struct_copy_is_not_remarked() {
    // SROA scalarizes `Point` into i32 locals, so the copy disappears entirely;
    // no heap copy executes, so there is nothing to remark on.
    let remarks = remarks_for(
        r#"
use { println } from "core:cli";

struct Point { x: i32, y: i32 }

export fn run() with Stdout {
    let a = Point { x: 1, y: 2 };
    let mut b = a;
    b.x = 9;
    println(`{a.x} {b.x}`);
}
"#,
    );

    assert!(
        remarks.is_empty(),
        "expected no remark (copy scalarized away), got {remarks:?}"
    );
}

#[test]
fn struct_field_copy_remark_points_at_copy_statement() {
    // `Bag` is SROA-decomposed and its `items` copy is reconstructed inside a
    // synthesized block whose inner statements carry placeholder spans. The
    // remark must anchor to the enclosing real statement `let mut b = a;`
    // (line 8), not to the placeholder span of the inner synthesized statement.
    let remarks = remarks_for(
        r#"
use { println } from "core:cli";

struct Bag { items: List<i32> }

export fn run() with Stdout {
    let a = Bag { items: [1, 2, 3] };
    let mut b = a;
    b.items.push(4);
    println(`{a.items.len()} {b.items.len()}`);
}
"#,
    );

    assert_eq!(
        remarks.len(),
        1,
        "expected exactly one remark, got {remarks:?}"
    );
    assert!(
        remarks[0].contains("a copy of") && remarks[0].contains("List<i32>"),
        "unexpected remark text: {remarks:?}"
    );
    assert!(
        remarks[0].starts_with("8:"),
        "remark should point at the copy statement on line 8, not a placeholder span: {remarks:?}"
    );
}
