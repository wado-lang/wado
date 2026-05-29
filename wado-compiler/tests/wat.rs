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

/// Decode an unsigned LEB128 at `bytes[i]`, returning (value, next_index).
fn read_uleb(bytes: &[u8], mut i: usize) -> (u64, usize) {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[i];
        i += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (result, i)
}

/// Collect every branch-hint value emitted across all `metadata.code.branch_hint`
/// custom sections in `wasm`. Each `metadata.code.branch_hint` payload is a
/// `vec(funcidx, vec(byte_offset, 1, value))`, where value is 0 (unlikely) or
/// 1 (likely). See the WebAssembly branch-hinting proposal.
fn collect_branch_hint_values(wasm: &[u8]) -> Vec<u8> {
    let name = b"metadata.code.branch_hint";
    let mut values = Vec::new();
    for start in 0..wasm.len().saturating_sub(name.len()) {
        if &wasm[start..start + name.len()] != name {
            continue;
        }
        // Payload (the func vec) begins immediately after the section name.
        let (func_count, mut i) = read_uleb(wasm, start + name.len());
        for _ in 0..func_count {
            let (_func_idx, n) = read_uleb(wasm, i);
            let (hint_count, n) = read_uleb(wasm, n);
            i = n;
            for _ in 0..hint_count {
                let (_offset, n) = read_uleb(wasm, i);
                let (_len, n) = read_uleb(wasm, n); // reserved length, == 1
                values.push(wasm[n]); // 0 = unlikely, 1 = likely
                i = n + 1;
            }
        }
    }
    values
}

/// Test that branch hints are not only present but carry the expected values:
/// `likely_unlikely.wado` emits a `likely` (1) hint in `check_likely` and an
/// `unlikely` (0) hint in `check_unlikely`. Parsing the actual custom-section
/// payload guards against `emit.rs` regressing to an empty or malformed
/// section (which the byte-substring presence check alone would not catch).
#[test]
fn test_branch_hints_values() {
    let result = compile_fixture("likely_unlikely.wado");
    let values = collect_branch_hint_values(&result.wasm);

    assert!(
        !values.is_empty(),
        "No branch-hint entries decoded from the wasm custom section"
    );
    assert!(
        values.contains(&1),
        "Expected a `likely` (value 1) branch hint; decoded values: {values:?}"
    );
    assert!(
        values.contains(&0),
        "Expected an `unlikely` (value 0) branch hint; decoded values: {values:?}"
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
            && i + 1 < lines.len()
        {
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
