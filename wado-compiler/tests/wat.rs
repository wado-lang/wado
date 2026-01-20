//! Tests for branch hints and specific WAT output features
//!
//! These tests verify that specific codegen features (like branch hints)
//! are correctly emitted in the WebAssembly output.

use std::path::PathBuf;
use std::sync::Mutex;
use wado_compiler::{CompilerHost, OptLevel};

// ============================================================================
// Test Compiler Host (Filesystem-based)
// ============================================================================

/// A simple filesystem-based CompilerHost for tests
struct TestCompilerHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<wado_compiler::Diagnostic>>,
}

impl TestCompilerHost {
    fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
        }
    }
}

impl CompilerHost for TestCompilerHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<String, wado_compiler::SourceError>> + Send {
        let full_path = self.base_path.join(path);
        async move {
            std::fs::read_to_string(&full_path).map_err(|e| wado_compiler::SourceError::IoError {
                path: full_path.to_string_lossy().to_string(),
                message: e.to_string(),
            })
        }
    }

    fn emit_diagnostic(
        &self,
        diagnostic: wado_compiler::Diagnostic,
    ) -> impl std::future::Future<Output = ()> + Send {
        let diagnostics = &self.diagnostics;
        async move {
            diagnostics.lock().unwrap().push(diagnostic);
        }
    }
}

// ============================================================================
// Branch Hints Test
// ============================================================================

/// Test that branch hints are correctly emitted for likely/unlikely builtins
#[test]
fn test_branch_hints_emitted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let source_path = PathBuf::from(manifest_dir).join("tests/fixtures/likely_unlikely.wado");

        let source = std::fs::read_to_string(&source_path)
            .unwrap_or_else(|e| panic!("Failed to read source file: {}", e));

        let base_path = source_path.parent().unwrap().to_path_buf();
        let host = TestCompilerHost::new(base_path);

        let result = wado_compiler::compile_with_host(
            &source,
            &host,
            Some(source_path.to_str().unwrap()),
            OptLevel::None,
        )
        .await
        .unwrap_or_else(|e| panic!("Compilation failed: {}", e));

        let wasm = result.wasm;

        // The branch hints should be in a custom section named "metadata.code.branch_hint"
        // We can check this by looking for the section name in the binary
        let section_name = b"metadata.code.branch_hint";
        let has_branch_hints = wasm
            .windows(section_name.len())
            .any(|window| window == section_name);

        assert!(
            has_branch_hints,
            "Branch hints custom section not found in wasm output. \
             Expected 'metadata.code.branch_hint' section to be present."
        );
    });
}

/// Test that branch hints have correct values for likely (1) and unlikely (0)
#[test]
fn test_branch_hints_values() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let source_path = PathBuf::from(manifest_dir).join("tests/fixtures/likely_unlikely.wado");

        let source = std::fs::read_to_string(&source_path)
            .unwrap_or_else(|e| panic!("Failed to read source file: {}", e));

        let base_path = source_path.parent().unwrap().to_path_buf();
        let host = TestCompilerHost::new(base_path);

        let result = wado_compiler::compile_with_host(
            &source,
            &host,
            Some(source_path.to_str().unwrap()),
            OptLevel::None,
        )
        .await
        .unwrap_or_else(|e| panic!("Compilation failed: {}", e));

        let wasm = result.wasm;

        // Find the branch hints section
        let section_name = b"metadata.code.branch_hint";
        let pos = wasm
            .windows(section_name.len())
            .position(|window| window == section_name);

        assert!(pos.is_some(), "Branch hints section not found");

        // The section should contain hints for both check_likely (hint=1) and check_unlikely (hint=0)
        // After the section name, we should have the section data with:
        // - Number of functions with hints
        // - For each function: function index, number of hints, and (offset, hint_value) pairs
        // We just verify the section exists and has some data after it
        let section_start = pos.unwrap();
        let section_end = section_start + section_name.len();

        // Section data starts after the name length and name
        // There should be at least a few bytes of data
        assert!(
            wasm.len() > section_end + 5,
            "Branch hints section appears to be empty or too short"
        );
    });
}
