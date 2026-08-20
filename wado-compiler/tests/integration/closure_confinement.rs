//! A closure costs its captures their confinement, not its whole frame's.
//!
//! Confinement used to mark every parameter of a closure-building callee as
//! escaping, so each caller kept a defensive copy of a value the callee only read.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

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
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("closure_confinement_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"closure_confinement_test.wado/caller\"")
        .expect("caller function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
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
