//! Tests for the `-f <flag>` codegen feature flags plumbed through
//! [`CompilerOptions::codegen_flags`].
//!
//! Covers `array-copy`, `branch-hinting`, `bare-asserts`, and
//! `wide-arithmetic`. Each test asserts on the disassembled WAT (or the run
//! output) so it pins the actual codegen difference rather than an internal
//! detail.

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

/// A string-building loop. `String` append (`+`) lowers to
/// `builtin::array_copy`, giving codegen something to lower as either a loop
/// or the native instruction. `assert s.len() == 50` keeps the loop live
/// through DCE.
const ARRAY_COPY_SOURCE: &str = r#"
export fn run() {
    let mut s = "";
    let mut i = 0;
    while i < 50 {
        s = s + "x";
        i += 1;
    }
    assert s.len() == 50;
}
"#;

fn compile_to_wat(codegen_flags: Vec<String>) -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        codegen_flags,
        ..Default::default()
    };
    let result = common::compile_source_with_compiler_options(
        Path::new("codegen_flags_test.wado"),
        ARRAY_COPY_SOURCE,
        options,
    )
    .expect("compilation should succeed");
    wasmprinter::print_bytes(&result.wasm).expect("disassemble wasm to WAT")
}

#[test]
fn default_emits_native_array_copy() {
    let wat = compile_to_wat(Vec::new());
    assert!(
        wat.contains("array.copy"),
        "default codegen must emit the native Wasm array.copy instruction"
    );
}

#[test]
fn no_array_copy_flag_lowers_to_a_loop() {
    let wat = compile_to_wat(vec!["no-array-copy".to_string()]);
    assert!(
        !wat.contains("array.copy"),
        "`-f no-array-copy` must lower builtin::array_copy to a loop, not the native instruction"
    );
}

#[test]
fn explicit_array_copy_flag_is_redundant_but_valid() {
    let wat = compile_to_wat(vec!["array-copy".to_string()]);
    assert!(
        wat.contains("array.copy"),
        "`-f array-copy` must keep the native Wasm array.copy instruction"
    );
}

#[test]
fn last_flag_wins() {
    // `array-copy` after `no-array-copy` re-enables the native instruction.
    let wat = compile_to_wat(vec!["no-array-copy".to_string(), "array-copy".to_string()]);
    assert!(
        wat.contains("array.copy"),
        "a trailing `-f array-copy` must override an earlier `-f no-array-copy`"
    );
}

/// Sources covering every branch-hint producer: an explicit `cold_path()`
/// marker, a synthesized one (`assert`), and a trap branch that the WIR-level
/// inference would hint on its own. `-f no-branch-hinting` must silence all
/// of them.
const BRANCH_HINT_SOURCE: &str = r#"
#[inline(never)]
fn guarded(x: i32) -> i32 {
    if x < 0 {
        builtin::cold_path();
        return -1;
    }
    if x > 1000 {
        builtin::unreachable();
    }
    return x + 1;
}

export fn run() {
    assert guarded(5) == 6;
}
"#;

fn compile_branch_hints(codegen_flags: Vec<String>) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        codegen_flags,
        ..Default::default()
    };
    common::compile_source_with_compiler_options(
        Path::new("codegen_flags_branch_hint_test.wado"),
        BRANCH_HINT_SOURCE,
        options,
    )
    .expect("compilation should succeed")
    .wasm
}

fn has_branch_hint_section(wasm: &[u8]) -> bool {
    let name = b"metadata.code.branch_hint";
    wasm.windows(name.len()).any(|w| w == name)
}

#[test]
fn default_emits_branch_hint_section() {
    let wasm = compile_branch_hints(Vec::new());
    assert!(
        has_branch_hint_section(&wasm),
        "default codegen must emit the metadata.code.branch_hint custom section"
    );
}

#[test]
fn no_branch_hinting_flag_drops_every_hint() {
    let wasm = compile_branch_hints(vec!["no-branch-hinting".to_string()]);
    assert!(
        !has_branch_hint_section(&wasm),
        "`-f no-branch-hinting` must drop cold_path markers and disable hint \
         inference, so no metadata.code.branch_hint section is emitted"
    );
}

/// Indexing a `List` lowers to `assert i < used`, whose failure path formats
/// the operands and so drags in the `Formatter` / `Inspect` stack. The
/// `"\ncondition:"` literal is part of every power-assert diagnostic template,
/// so its presence in the wasm is a reliable proxy for "the diagnostic shipped".
const ASSERT_SOURCE: &str = r#"
export fn run() {
    let xs = [1, 2, 3] as List<i32>;
    let mut sum = 0;
    let mut i = 0;
    while i < 3 {
        sum += xs[i];
        i += 1;
    }
    assert sum == 6;
}
"#;

fn compile_assert_source(opt_level: OptLevel, codegen_flags: Vec<String>) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        codegen_flags,
        ..Default::default()
    };
    common::compile_source_with_compiler_options(
        Path::new("codegen_flags_assert_test.wado"),
        ASSERT_SOURCE,
        options,
    )
    .expect("compilation should succeed")
    .wasm
}

fn ships_assert_diagnostic(wasm: &[u8]) -> bool {
    let marker = b"\ncondition:";
    wasm.windows(marker.len()).any(|w| w == marker)
}

#[test]
fn default_o2_ships_the_assert_diagnostic() {
    let wasm = compile_assert_source(OptLevel::O2, Vec::new());
    assert!(
        ships_assert_diagnostic(&wasm),
        "default -O2 must keep the power-assert diagnostic"
    );
}

#[test]
fn bare_asserts_flag_strips_the_diagnostic() {
    let with = compile_assert_source(OptLevel::O2, Vec::new());
    let without = compile_assert_source(OptLevel::O2, vec!["bare-asserts".to_string()]);
    assert!(
        !ships_assert_diagnostic(&without),
        "`-f bare-asserts` must drop the assert diagnostic, lowering it to a bare trap"
    );
    assert!(
        without.len() < with.len(),
        "`-f bare-asserts` must shrink the binary ({} >= {})",
        without.len(),
        with.len()
    );
}

#[test]
fn os_enables_bare_asserts_by_default() {
    let wasm = compile_assert_source(OptLevel::Os, Vec::new());
    assert!(
        !ships_assert_diagnostic(&wasm),
        "-Os must enable bare-asserts by default, dropping the diagnostic"
    );
}

#[test]
fn no_bare_asserts_restores_the_diagnostic_at_os() {
    let wasm = compile_assert_source(OptLevel::Os, vec!["no-bare-asserts".to_string()]);
    assert!(
        ships_assert_diagnostic(&wasm),
        "`-f no-bare-asserts` must restore the diagnostic even at -Os"
    );
}

/// Exercises every 128-bit wide-arithmetic op the codegen can emit:
/// `i64.mul_wide_u` / `mul_wide_s` (via the builtins, which `core:prelude`
/// uses for float formatting and `i128`) and `i64.add128` / `sub128`. The
/// inputs hit carry/borrow and sign edges so the `-f no-wide-arithmetic`
/// software lowering is checked against the native instructions, not just
/// happy-path values. The trailing float keeps a realistic `fpfmt` user of
/// `mul_wide_u` in the program too.
const WIDE_ARITH_SOURCE: &str = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let [lo1, hi1] = builtin::i64_mul_wide_u(0xFFFFFFFFFFFFFFFF as i64, 3 as i64);
    println(`{lo1} {hi1}`);
    let [lo2, hi2] = builtin::i64_mul_wide_s(0 - 5, 7);
    println(`{lo2} {hi2}`);
    let [lo3, hi3] = builtin::i64_add128(0xFFFFFFFFFFFFFFFF as i64, 0, 2, 0);
    println(`{lo3} {hi3}`);
    let [lo4, hi4] = builtin::i64_sub128(1, 5, 2, 2);
    println(`{lo4} {hi4}`);
    println(`{3.141592653589793}`);
}
"#;

fn compile_wide_arith(codegen_flags: Vec<String>) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        codegen_flags,
        ..Default::default()
    };
    common::compile_source_with_compiler_options(
        Path::new("wide_arith_test.wado"),
        WIDE_ARITH_SOURCE,
        options,
    )
    .expect("compilation should succeed")
    .wasm
}

const WIDE_OPS: [&str; 4] = [
    "i64.mul_wide_u",
    "i64.mul_wide_s",
    "i64.add128",
    "i64.sub128",
];

#[test]
fn default_emits_wide_arithmetic() {
    let wat = wasmprinter::print_bytes(compile_wide_arith(Vec::new())).expect("disassemble");
    for op in WIDE_OPS {
        assert!(wat.contains(op), "default codegen must emit `{op}`");
    }
}

#[test]
fn no_wide_arithmetic_flag_lowers_to_plain_i64() {
    let wat = wasmprinter::print_bytes(compile_wide_arith(vec!["no-wide-arithmetic".to_string()]))
        .expect("disassemble");
    for op in WIDE_OPS {
        assert!(
            !wat.contains(op),
            "`-f no-wide-arithmetic` must not emit `{op}` (V8 has no wide-arithmetic support)"
        );
    }
}

#[test]
fn no_wide_arithmetic_preserves_results() {
    let native = common::run_wasm(compile_wide_arith(Vec::new())).expect("run native");
    let soft = common::run_wasm(compile_wide_arith(vec!["no-wide-arithmetic".to_string()]))
        .expect("run software-lowered");
    assert!(
        !native.trapped && !soft.trapped,
        "neither build should trap"
    );
    assert!(!native.stdout.is_empty(), "expected output");
    assert_eq!(
        soft.stdout, native.stdout,
        "software wide-arithmetic must match the native instructions"
    );
}

#[test]
fn unknown_codegen_flag_is_rejected() {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        codegen_flags: vec!["bogus".to_string()],
        ..Default::default()
    };
    let result = common::compile_source_with_compiler_options(
        Path::new("codegen_flags_test.wado"),
        ARRAY_COPY_SOURCE,
        options,
    );
    let err = result.expect_err("an unknown codegen flag must fail compilation");
    assert!(
        err.to_string().contains("unknown codegen flag"),
        "expected an 'unknown codegen flag' diagnostic, got: {err}"
    );
}
