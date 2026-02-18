//! Component Model wrapper — wraps a core Wasm module in a Component Model component.
//!
//! Delegates to `component_gen::build_component` which handles:
//! - WASI interface imports
//! - Memory module
//! - Bundled modules (FTS, libm)
//! - Canonical intrinsics
//! - WASI function lowering
//! - Core module instantiation
//! - Canonical lifting for world exports

use crate::project::Project;
use crate::wir::WirModule;

/// Build a Wasm Component from a core module and project metadata.
pub fn build_component(project: &Project, core_module: &[u8], _wir: &WirModule) -> Vec<u8> {
    crate::component_gen::build_component(project, core_module)
}
