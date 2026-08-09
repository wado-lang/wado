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
    let result = crate::common::compile_source_with_compiler_options(
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

const LAST_ELEMENT_SOURCE: &str = r#"
#[inline(never)]
fn top(stack: &List<i32>) -> i32 {
    if stack.len() > 0 {
        return stack[stack.len() - 1];
    }
    return -1;
}

export fn run() {
    // `black_box` keeps CTFE from running `top` and erasing the shape under test.
    let s0: List<i32> = [3, 7, 4];
    let s = builtin::black_box(s0);
    assert top(&s) == 4;
    let e0: List<i32> = [];
    let e = builtin::black_box(e0);
    assert top(&e) == -1;
}
"#;

/// Bounds-check panic calls inside `top`'s WIR body.
fn top_panic_count(opt_level: OptLevel) -> usize {
    let options = CompilerOptions {
        opt_level,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("last_element_test.wado"),
        LAST_ELEMENT_SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);
    let start = wir_text
        .find("fn \"last_element_test.wado/top\"")
        .expect("top function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].matches("core:rt/panic").count()
}

#[test]
fn last_element_index_check_is_eliminated() {
    // `stack[stack.len() - 1]` is unconditionally in bounds (a length is
    // non-negative, so `len - 1 < len`), so its guard is dropped entirely.
    let count = top_panic_count(OptLevel::O2);
    assert_eq!(
        count, 0,
        "expected 0 bounds-check panics in `top` at O2, found {count}"
    );
}
