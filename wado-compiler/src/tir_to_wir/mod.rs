//! TIR-to-WIR translation — converts optimized TIR (Project) into a WIR module.
//!
//! This module is the entry point for the WIR pipeline:
//!   `compile_with_wir(&Project) -> Vec<u8>`
//!
//! Currently a stub that returns a minimal valid Wasm component.
//! Phase 3 will incrementally implement the actual translation.

use crate::project::Project;

mod emit;

/// Compile a Project to Wasm bytes using the WIR pipeline.
///
/// This is the WIR pipeline entry point, parallel to `Codegen::generate_wasm`.
/// Pipeline: Project → `tir_to_wir` (`WirModule`) → `wir_emit` (Wasm bytes)
///
/// Currently returns a minimal stub component. Phase 3 will implement
/// the actual translation.
pub fn compile_with_wir(_project: &Project) -> Vec<u8> {
    // Phase 2 stub: return a minimal valid Wasm component.
    // The component exports a "run" function that returns Ok(()) with no output.
    // This allows the test infrastructure to exercise the full pipeline
    // (compile → instantiate → run → verify) rather than failing at Wasm loading.
    emit::build_stub_component()
}
