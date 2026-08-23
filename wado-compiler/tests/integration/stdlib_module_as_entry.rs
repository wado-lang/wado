//! A stdlib module compiled as the entry point (#1875): its own parse and the
//! stdlib snapshot's parse of that file gave it two identities, so trait
//! synthesis re-derived an impl it writes by hand and monomorphize panicked.

use std::path::{Path, PathBuf};

fn core_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/core")
}

/// Every bundled `core:` module — the ones an editor opens like any other file.
fn core_modules() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_modules(&core_dir(), &mut paths);
    paths.sort();
    assert!(paths.len() > 20, "core modules should be discoverable");
    paths
}

/// The whole `dir` tree, minus what this sweep is not about: a `*_test.wado`,
/// an ordinary entry point `mise run test-wado` covers, and a
/// `#![wasm_module("…")]` module, not compilable as an entry at all.
fn collect_modules(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("core dir should be readable") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_modules(&path, out);
        } else if is_entry_point_module(&path) {
            out.push(path);
        }
    }
}

fn is_entry_point_module(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "wado")
        && !path
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().ends_with("_test"))
        && !std::fs::read_to_string(path)
            .expect("module should be readable")
            .contains("#![wasm_module(")
}

fn compile_as_test_world_entry(
    path: &Path,
    source: &str,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileError> {
    let options = wado_compiler::CompilerOptions {
        target_world: Some("test".to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(path, source, options)
}

fn compile_file_as_test_world_entry(
    path: &Path,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileError> {
    let source = std::fs::read_to_string(path).expect("module should be readable");
    compile_as_test_world_entry(path, &source)
}

#[test]
fn every_bundled_core_module_compiles_as_the_entry_point() {
    for path in core_modules() {
        compile_file_as_test_world_entry(&path).unwrap_or_else(|e| {
            panic!(
                "compiling {} as the entry point failed: {e}",
                path.display()
            )
        });
    }
}

/// Its code is emitted into that separate core module, leaving the program's
/// own module empty — there is no component to compile.
#[test]
fn a_wasm_module_entry_is_rejected() {
    let path = core_dir().join("allocator.wado");
    let Err(err) = compile_file_as_test_world_entry(&path) else {
        panic!("a `#![wasm_module(…)]` entry must be rejected");
    };
    assert!(
        err.to_string().contains("cannot itself be the entry point"),
        "unexpected error: {err}"
    );
}

/// The entry's own source is what gets compiled, never the snapshot's copy.
#[test]
fn a_stdlib_entry_is_compiled_from_its_source() {
    let path = core_dir().join("prelude/string.wado");
    let source = std::fs::read_to_string(&path).expect("string.wado should be readable");
    let probed =
        format!("{source}\nfn snapshot_probe() -> i32 {{\n    return \"not an i32\";\n}}\n");

    let Err(err) = compile_as_test_world_entry(&path, &probed) else {
        panic!("the injected type error must be reported");
    };
    assert!(
        err.to_string().contains("type mismatch"),
        "expected the injected type error, got: {err}"
    );
}
