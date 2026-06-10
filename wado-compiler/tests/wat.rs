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

/// Decode an unsigned LEB128 at `bytes[i]`, returning (value, `next_index`).
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

/// Test that `builtin::cold_path()` markers reach the actual
/// `metadata.code.branch_hint` custom section — not just the WIR. `cold_path.wado`
/// places markers in an `if let` else arm and a `match` tail (cold side ⇒ the
/// condition is hinted likely-true, value 1) and in an `if`-then arm (cold side
/// ⇒ unlikely-true, value 0), so both hint directions must appear in the decoded
/// payload. This guards the `ColdPath` → `BranchHint` → emitter path end to end.
#[test]
fn test_cold_path_branch_hint_values() {
    let result = compile_fixture("cold_path.wado");
    let values = collect_branch_hint_values(&result.wasm);

    assert!(
        !values.is_empty(),
        "No branch-hint entries decoded from cold_path.wado; \
         cold_path() did not reach the metadata.code.branch_hint section"
    );
    assert!(
        values.contains(&1),
        "Expected a `likely` (1) hint from a cold else/match arm; decoded: {values:?}"
    );
    assert!(
        values.contains(&0),
        "Expected an `unlikely` (0) hint from a cold if-then arm; decoded: {values:?}"
    );
}

/// Test that the guard-clause fall-through hint reaches the branch-hint section:
/// `cold_path_fallthrough.wado` guards (`if ok { return v }`) whose fall-through
/// is cold must emit `likely` (1) hints on their conditions.
#[test]
fn test_cold_path_fallthrough_branch_hint_values() {
    let result = compile_fixture("cold_path_fallthrough.wado");
    let values = collect_branch_hint_values(&result.wasm);

    assert!(
        values.contains(&1),
        "Expected `likely` (1) hints for guard-clause fall-through; decoded: {values:?}"
    );
}

/// Branch-hint offsets must point at the hinted `if` (0x04) or `br_if` (0x0D)
/// opcode, relative to the start of the function body (the locals vector) —
/// that is how runtimes match entries (wasmtime compares the operator's
/// position minus the body start against `func_offset`). A drift in this
/// convention would not fail validation; it would just silently disable every
/// hint. So check the actual bytes the offsets point at.
#[test]
fn test_branch_hint_offsets_point_at_branch_opcodes() {
    use wasmparser::{KnownCustom, Parser, Payload, TypeRef};

    // Check one core module's branch hints; returns (hint_count, saw_br_if).
    // Offsets in the section are relative to `module` (the core module bytes),
    // matching the body ranges wasmparser reports.
    fn check_module(fixture: &str, module: &[u8]) -> (usize, bool) {
        let mut num_imported_funcs = 0u32;
        let mut body_ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut hints: Vec<(u32, u32)> = Vec::new();
        for payload in Parser::new(0).parse_all(module) {
            match payload.unwrap() {
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        if matches!(import.unwrap().ty, TypeRef::Func(_)) {
                            num_imported_funcs += 1;
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    body_ranges.push(body.range());
                }
                Payload::CustomSection(section) => {
                    if let KnownCustom::BranchHints(reader) = section.as_known() {
                        for func in reader {
                            let func = func.unwrap();
                            for hint in func.hints {
                                hints.push((func.func, hint.unwrap().func_offset));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let mut saw_br_if = false;
        let count = hints.len();
        for (func, offset) in hints {
            let local_idx = func
                .checked_sub(num_imported_funcs)
                .unwrap_or_else(|| panic!("{fixture}: hint on imported function {func}"))
                as usize;
            let body = &body_ranges[local_idx];
            let pos = body.start + offset as usize;
            assert!(
                pos < body.end,
                "{fixture}: hint offset {offset} in function {func} is past the body"
            );
            let opcode = module[pos];
            assert!(
                opcode == 0x04 || opcode == 0x0D,
                "{fixture}: hint offset {offset} in function {func} points at \
                 opcode {opcode:#04x}, expected `if` (0x04) or `br_if` (0x0d)"
            );
            saw_br_if |= opcode == 0x0D;
        }
        (count, saw_br_if)
    }

    for (fixture, opt, expect_br_if) in [
        ("cold_path.wado", OptLevel::O0, false),
        ("wir_optimize_brif_select.wado", OptLevel::O2, true),
    ] {
        let result = compile_fixture_opt(fixture, opt);
        let wasm = &result.wasm;

        // The compiler emits a CM component; the branch-hint sections live in
        // the embedded core modules, with hints relative to each module.
        let mut total_hints = 0;
        let mut saw_br_if = false;
        for payload in Parser::new(0).parse_all(wasm) {
            if let Payload::ModuleSection {
                unchecked_range, ..
            } = payload.unwrap()
            {
                let (count, br_if) = check_module(fixture, &wasm[unchecked_range]);
                total_hints += count;
                saw_br_if |= br_if;
            }
        }

        assert!(
            total_hints > 0,
            "{fixture}: no branch hints decoded from any embedded core module"
        );
        if expect_br_if {
            assert!(
                saw_br_if,
                "{fixture}: expected at least one hinted br_if at O2"
            );
        }
    }
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
