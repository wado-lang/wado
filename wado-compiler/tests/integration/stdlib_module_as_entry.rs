//! A stdlib module compiled as the entry point (#1875): its own parse and the
//! stdlib snapshot's parse of that file gave it two identities, so trait
//! synthesis re-derived an impl it writes by hand and monomorphize panicked.

use std::path::{Path, PathBuf};

fn prelude_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/core/prelude")
}

/// Every `core:prelude` module, `*_test.wado` siblings excluded — those are
/// ordinary entry points already covered by `mise run test-wado`.
fn prelude_modules() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(prelude_dir())
        .expect("prelude dir should be readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "wado")
                && !p
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy().ends_with("_test"))
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "prelude modules should be discoverable");
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

#[test]
fn every_prelude_module_compiles_as_the_entry_point() {
    for path in prelude_modules() {
        let source = std::fs::read_to_string(&path).expect("prelude module should be readable");
        compile_as_test_world_entry(&path, &source).unwrap_or_else(|e| {
            panic!(
                "compiling {} as the entry point failed: {e}",
                path.display()
            )
        });
    }
}

/// The entry's own source is what gets compiled — never the snapshot's copy of
/// the module whose identity it claims.
#[test]
fn a_stdlib_entry_is_compiled_from_its_source() {
    let path = prelude_dir().join("string.wado");
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
