//! Regression test for `wir_optimize::dedupe_const_globals`'s actual merge
//! behavior — that two functions hoisting byte-identical constant globals
//! (via `const_object_globalization`) end up sharing ONE global, not two.
//!
//! `tests/fixtures/const_global_dedup.wado`'s `wir_expect`/`wir_not_expect`
//! (substring presence/absence over the WIR dump) can't express an exact
//! occurrence count, so they can't directly prove merging happened — only
//! that one promoted-eager `__const_obj_0` exists (true whether or not a
//! second, unmerged duplicate also exists). This test compiles the same
//! shape and counts the constant array literal's occurrences in the
//! disassembled module directly.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

/// `black_box` keeps the loop bound opaque, so the calls — and the template
/// region around them — stay unfolded and the hoists actually form; with a
/// constant bound the whole program folds to one string and there is nothing
/// left to dedup.
const DEDUP_SOURCE: &str = r#"
use { println, Stdout } from "core:cli";

fn sum_a(n: i32) -> i32 {
    let xs: List<i32> = [10, 20, 30, 40];
    let mut s = 0;
    let mut i = 0;
    while i < n {
        s = s + xs[i % 4];
        i = i + 1;
    }
    return s;
}

fn sum_b(n: i32) -> i32 {
    let ys: List<i32> = [10, 20, 30, 40];
    let mut s = 0;
    let mut i = 0;
    while i < n {
        s = s + ys[i % 4];
        i = i + 1;
    }
    return s;
}

export fn run() with Stdout {
    println(`${sum_a(builtin::black_box(4))},${sum_b(builtin::black_box(4))}`);
}
"#;

#[test]
fn identical_hoisted_globals_merge_to_one() {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("dedupe_const_globals_test.wado"),
        DEDUP_SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wat = wasmprinter::print_bytes(&result.wasm).expect("disassemble wasm to WAT");

    // `sum_a` and `sum_b` each hoist a byte-identical `[10, 20, 30, 40]`
    // constant. If `dedupe_const_globals` merged them, exactly one global
    // declaration carries this eager `array.new_fixed` initializer (printed
    // flat, Wasm-const-expr order, in a global's own init — see
    // `codegen_flags.rs`'s `ships_assert_diagnostic` for the same flat-vs-
    // folded distinction); if the merge regressed, two independent globals
    // would each carry a copy.
    let pattern = "i32.const 10 i32.const 20 i32.const 30 i32.const 40";
    let occurrences = wat.matches(pattern).count();
    assert_eq!(
        occurrences, 1,
        "expected exactly one eager `[10, 20, 30, 40]` global after dedup, found {occurrences} in:\n{wat}"
    );
}
