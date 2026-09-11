//! A literal coerced to `i128` / `u128` reifies to the type's constructor call,
//! whatever the literal is spelled as.
//!
//! Regression: `try_reify_int128_coercion` enumerated the literal shapes itself
//! and named only `Number`, so a byte literal fell through to a bare
//! `IntLiteral` carrying the `i128` struct type. Every byte value fits the low
//! 64 bits, so the pipeline absorbed the difference and no output moved — the
//! shape is what the test has to read.

use crate::common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world, unparse::unparse_tir};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

const SOURCE: &str = r#"
export fn run() {
    let dec_i: i128 = 65;
    let byte_i: i128 = b'A';
    let dec_u: u128 = 65;
    let byte_u: u128 = b'A';
}
"#;

#[test]
fn byte_literal_at_int128_reifies_to_the_constructor_call() {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        wado_compiler::OptOverrides::default(),
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dump succeeds");

    let modules = dump
        .tir_modules
        .expect("resolved TIR modules present after dump");

    let entry = modules
        .values()
        .map(unparse_tir)
        .find(|text| text.contains("let byte_i"))
        .expect("entry module present");

    for (binding, ctor) in [
        ("dec_i", "i128::from_i64"),
        ("byte_i", "i128::from_i64"),
        ("dec_u", "u128::from_u64"),
        ("byte_u", "u128::from_u64"),
    ] {
        let line = entry
            .lines()
            .find(|line| line.contains(&format!("let {binding}")))
            .unwrap_or_else(|| panic!("`{binding}` should be bound, got:\n{entry}"));
        assert!(
            line.contains(ctor),
            "`{binding}` should reify through `{ctor}`, got:\n{line}"
        );
    }
}
