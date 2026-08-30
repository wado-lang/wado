//! A closure costs its captures their confinement, not its whole frame's.
//!
//! Confinement used to mark every parameter of a closure-building callee as
//! escaping, so each caller kept a defensive copy of a value the callee only read.

use std::path::Path;

use wado_compiler::OptLevel;

const SOURCE: &str = r#"
#[inline(never)]
fn total(xs: List<i32>, tag: i32) -> i32 {
    let bump = || tag + 1;
    return xs.len() + bump();
}

#[inline(never)]
fn caller(src: &List<i32>) -> i32 {
    let mine = *src;
    let n = total(mine, 5);
    return n + mine.len();
}

export fn run() { assert caller(&[1]) == 8; }
"#;

fn caller_body() -> String {
    crate::common::wir_function_body(
        Path::new("closure_confinement_test.wado"),
        SOURCE,
        OptLevel::O2,
        "fn \"closure_confinement_test.wado/caller\"",
    )
}

#[test]
fn a_closure_does_not_unconfine_the_other_parameters() {
    let body = caller_body();
    let copies = body.matches("array_copy").count();
    assert_eq!(
        copies, 1,
        "only `let mine = *src` copies; the call must not:\n{body}"
    );
}
