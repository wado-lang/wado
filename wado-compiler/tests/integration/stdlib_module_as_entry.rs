//! A stdlib module compiled as the entry point (#1875): its own parse and the
//! stdlib snapshot's parse of that file gave it two identities, so trait
//! synthesis re-derived an impl it writes by hand and monomorphize panicked.

use std::path::{Path, PathBuf};

fn core_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/core")
}

/// Every bundled `core:` module — the ones carrying `#![stdlib("core:…")]`,
/// which an editor opens like any other file. `*_test.wado` siblings are
/// ordinary entry points already covered by `mise run test-wado`, and a
/// `#![wasm_module("…")]` module is not compilable as an entry at all (see
/// `a_wasm_module_entry_is_rejected`).
fn core_modules() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [core_dir(), core_dir().join("prelude")]
        .into_iter()
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .expect("core dir should be readable")
                .map(|e| e.expect("dir entry").path())
        })
        .filter(|p| {
            p.extension().is_some_and(|e| e == "wado")
                && !p
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy().ends_with("_test"))
                && !std::fs::read_to_string(p)
                    .expect("module should be readable")
                    .contains("#![wasm_module(")
        })
        .collect();
    paths.sort();
    assert!(paths.len() > 20, "core modules should be discoverable");
    paths
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

/// A `#![wasm_module("mem")]` module's code is emitted into that separate core
/// module, leaving the program's own module empty — there is no component to
/// compile, and saying so beats the WIR-emit panic it used to hit.
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

/// The entry's own source is what gets compiled — never the snapshot's copy of
/// the module whose identity it claims.
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
