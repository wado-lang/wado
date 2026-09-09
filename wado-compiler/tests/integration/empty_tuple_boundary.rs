//! The empty tuple `[]` at a Component Model boundary.
//!
//! `[]` is a type of its own, not unit: the parser keeps them apart, and the
//! Component Model has no `tuple<>` to carry it. An export naming one is
//! rejected where its other unrepresentable types are.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

use crate::common::compile_source_with_compiler_options;

const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

fn rejection(source: &str) -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let error = compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .expect_err("an empty tuple has no Component Model representation");
    error.to_string()
}

/// The result itself.
#[test]
fn empty_tuple_result_is_rejected() {
    let message = rejection(
        r#"
export fn f() -> [] {
    return [];
}
"#,
    );
    assert!(
        message.contains("the empty tuple `[]` has no Component Model representation"),
        "unexpected error: {message}"
    );
}

/// Behind a `future`, whose payload the boundary lifts by value. Reaching it
/// means the walk descends rather than stopping at the handle.
#[test]
fn empty_tuple_in_a_future_payload_is_rejected() {
    let message = rejection(
        r#"
export async fn f() -> Future<Result<[], String>> {
    let [rx, tx] = Future::<Result<[], String>>::new();
    tx.write(Result::<[], String>::Ok([]));
    task return rx;
}
"#,
    );
    assert!(
        message.contains("the empty tuple `[]` has no Component Model representation"),
        "unexpected error: {message}"
    );
}
