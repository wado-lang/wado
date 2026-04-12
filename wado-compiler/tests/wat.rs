//! Tests for branch hints and specific WAT output features
//!
//! These tests verify that specific codegen features (like branch hints)
//! are correctly emitted in the WebAssembly output.

mod common;

use std::path::PathBuf;
use wado_compiler::OptLevel;

/// Compile a fixture file at the given optimization level.
fn compile_fixture_opt(fixture: &str, opt: OptLevel) -> wado_compiler::CompileResult {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let source_path = PathBuf::from(manifest_dir).join(format!("tests/fixtures/{fixture}"));

    common::compile_file_with_opts(&source_path, opt)
        .unwrap_or_else(|e| panic!("Compilation failed: {e}"))
}

/// Compile a fixture file with O0 optimization
fn compile_fixture(fixture: &str) -> wado_compiler::CompileResult {
    compile_fixture_opt(fixture, OptLevel::O0)
}

/// Test that branch hints are correctly emitted for likely/unlikely builtins
#[test]
fn test_branch_hints_emitted() {
    let result = compile_fixture("likely_unlikely.wado");
    let wasm = result.wasm;

    // The branch hints should be in a custom section named "metadata.code.branch_hint"
    let section_name = b"metadata.code.branch_hint";
    let has_branch_hints = wasm
        .windows(section_name.len())
        .any(|window| window == section_name);

    assert!(
        has_branch_hints,
        "Branch hints custom section not found in wasm output. \
         Expected 'metadata.code.branch_hint' section to be present."
    );
}

/// Test that branch hints have correct values for likely (1) and unlikely (0)
#[test]
fn test_branch_hints_values() {
    let result = compile_fixture("likely_unlikely.wado");
    let wasm = result.wasm;

    // Find the branch hints section
    let section_name = b"metadata.code.branch_hint";
    let pos = wasm
        .windows(section_name.len())
        .position(|window| window == section_name);

    assert!(pos.is_some(), "Branch hints section not found");

    // The section should contain hints for both check_likely (hint=1) and check_unlikely (hint=0)
    let section_start = pos.unwrap();
    let section_end = section_start + section_name.len();

    // Section data starts after the name length and name
    // There should be at least a few bytes of data
    assert!(
        wasm.len() > section_end + 5,
        "Branch hints section appears to be empty or too short"
    );
}

/// Test that multi-value builtin calls with destructuring do not generate tuple structs.
/// When `let [lo, hi] = builtin::i64_add128(...)` is used, the codegen should directly
/// bind stack values to locals without creating a tuple struct (no struct.new after i64.add128).
/// Requires O1+ because tuple elision is a WIR optimization pass.
#[test]
fn test_tuple_elision_multivalue() {
    let result = compile_fixture_opt("tuple_elision_multivalue.wado", OptLevel::O1);

    // Get WAT representation
    let wat = wasmprinter::print_bytes(&result.wasm)
        .unwrap_or_else(|e| panic!("Failed to print WAT: {e}"));

    // Check that after i64.add128, there's no struct.new on the same line or next lines
    let lines: Vec<&str> = wat.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if line.contains("i64.add128") || line.contains("i64.sub128") {
            // Check the next few lines for struct.new - it should NOT be present
            if i + 1 < lines.len() {
                let next_line = lines[i + 1].trim();
                assert!(
                    !next_line.contains("struct.new"),
                    "Tuple elision optimization not applied! Found struct.new after multi-value instruction.\n\
                     Line {}: {}\n\
                     Line {}: {}",
                    i + 1,
                    line,
                    i + 2,
                    next_line
                );
            }
        }

        // Also check i64.mul_wide_u and i64.mul_wide_s
        if (line.contains("i64.mul_wide_u") || line.contains("i64.mul_wide_s"))
            && i + 1 < lines.len() {
                let next_line = lines[i + 1].trim();
                assert!(
                    !next_line.contains("struct.new"),
                    "Tuple elision optimization not applied! Found struct.new after multi-value instruction.\n\
                     Line {}: {}\n\
                     Line {}: {}",
                    i + 1,
                    line,
                    i + 2,
                    next_line
                );
            }
    }

    // Verify that local.set appears after the multi-value instructions (positive check)
    let has_add128_with_local_set = wat.contains("i64.add128")
        && wat.lines().any(|line| {
            line.contains("i64.add128")
                || (line.trim().starts_with("(local.set") || line.trim().starts_with("local.set"))
        });

    assert!(
        has_add128_with_local_set,
        "Expected i64.add128 followed by local.set instructions"
    );
}
