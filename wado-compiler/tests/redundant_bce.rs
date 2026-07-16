//! Regression test for `condition_implication`'s redundant bounds-check
//! elimination via a dominating panic-guard.
//!
//! When the same index into the same (unmodified) array is accessed twice, the
//! first access's implicit bounds-check panic-guard proves the index in bounds,
//! so the second access's re-check is redundant. `wir_expect`/`wir_not_expect`
//! (substring presence over the WIR dump) can't express an occurrence count, so
//! this test counts the `bump` function's bounds-check panic calls directly:
//! four accesses (`end[child]`/`end[parent]` in the condition and again in the
//! body) collapse to two remaining guards.

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

const SOURCE: &str = r#"
#[inline(never)]
fn bump(end: &mut List<i32>, child: i32, parent: i32) {
    if end[child] > end[parent] {
        end[parent] = end[child];
    }
}

export fn run() {
    let mut e: List<i32> = [5, 9, 2, 7];
    bump(&mut e, 0, 2);
    assert e[2] == 5;
    bump(&mut e, 1, 3);
    assert e[3] == 9;
}
"#;

/// The number of bounds-check panic calls inside `bump`'s WIR body: one per
/// surviving `>= arr.used` guard.
fn bump_panic_count(opt_level: OptLevel) -> usize {
    let options = CompilerOptions {
        opt_level,
        retain_wir: true,
        ..Default::default()
    };
    let result = common::compile_source_with_compiler_options(
        Path::new("redundant_bce_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"redundant_bce_test.wado/bump\"")
        .expect("bump function in WIR");
    let rest = &wir_text[start..];
    // The next top-level `\nfn ` marks the end of `bump`'s body.
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].matches("core:rt/panic").count()
}

#[test]
fn redundant_index_recheck_is_eliminated() {
    // Each of `end[child]` / `end[parent]` is accessed in the `if` condition
    // and again in the body; the body re-checks collapse into the condition's
    // guards, leaving exactly two bounds-check panics.
    let count = bump_panic_count(OptLevel::O2);
    assert_eq!(
        count, 2,
        "expected 2 surviving bounds-check panics in `bump` at O2, found {count}"
    );
}
