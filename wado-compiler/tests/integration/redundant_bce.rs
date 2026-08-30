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

use wado_compiler::OptLevel;

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

/// The number of bounds-check panics inside one function's WIR body: one per
/// surviving `>= arr.used` guard.
///
/// A guard's taken arm is spelled one of two ways — an inline `core:rt/panic`,
/// or a call to the `$cold` helper `nir/cold_outline` moves that arm into. Both
/// count: what these tests are about is how many checks survive, not which
/// shape the optimizer left them in, and counting the callee name alone would
/// read the split shape as no check at all.
fn panic_count(opt_level: OptLevel, file: &str, source: &str, func: &str) -> usize {
    let body = crate::common::wir_function_body(
        Path::new(file),
        source,
        opt_level,
        &format!("fn \"{file}/{func}\""),
    );
    body.matches("core:rt/panic").count() + body.matches("$cold").count()
}

fn bump_panic_count(opt_level: OptLevel) -> usize {
    panic_count(opt_level, "redundant_bce_test.wado", SOURCE, "bump")
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

fn top_panic_count(opt_level: OptLevel) -> usize {
    panic_count(
        opt_level,
        "last_element_test.wado",
        LAST_ELEMENT_SOURCE,
        "top",
    )
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
