//! Tests for the `-f <flag>` codegen feature flags plumbed through
//! [`CompilerOptions::codegen_flags`].
//!
//! The only flag so far is `array-copy`, which switches the
//! `builtin::array_copy` lowering between the native Wasm `array.copy`
//! instruction (the default) and an open-coded loop (`-f no-array-copy`). We
//! assert on the disassembled WAT so the test pins the actual codegen
//! difference rather than an internal detail.

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
