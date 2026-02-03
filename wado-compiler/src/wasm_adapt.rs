//! Wasm Adapt Phase - Prepares TIR for WebAssembly Component Model code generation
//!
//! This phase runs between optimize and codegen:
//! ```text
//! lower -> optimize -> wasm_adapt -> codegen
//! ```
//!
//! Responsibilities:
//! 1. CM boundary analysis - Analyze export signatures to determine required glue code
//! 2. Attach `CmExportInfo` to `TirFunctions` that are world exports
//! 3. (Future) Generate CM helper functions as TIR based on actual type usage
//!
//! Design principles:
//! - Metadata over TIR for glue code: CM glue uses low-level Wasm operations that
//!   don't map cleanly to TIR, so we use metadata to tell codegen what to generate
//! - Keep codegen simple: codegen should just convert TIR to Wasm without
//!   needing to analyze world definitions or export signatures

use crate::component_model::WasiRegistry;
use crate::project::Project;
use crate::tir::ScratchLocal;

/// CM export information attached to `TirFunction`
///
/// This metadata tells codegen how to generate Component Model glue code
/// for a world export function.
#[derive(Debug, Clone, Default)]
pub struct CmExportInfo {
    /// Whether this is an async export (from world definition)
    pub is_async: bool,
    /// Whether this export returns Result<Response, `ErrorCode`> (HTTP handler)
    pub is_http_handler: bool,
    /// Additional scratch locals needed for CM glue code
    pub scratch_locals: Vec<ScratchLocal>,
    /// CM functions that must be imported (e.g., "task-return", "future-new")
    pub required_imports: Vec<String>,
}

impl CmExportInfo {
    /// Create `CmExportInfo` for an async export
    pub fn async_export(is_http_handler: bool) -> Self {
        let mut required_imports = vec!["task-return".to_string()];

        if is_http_handler {
            // HTTP handler needs additional imports for response creation
            required_imports.extend([
                "future-new".to_string(),
                "future-write".to_string(),
                "http-fields-constructor".to_string(),
                "http-response-new".to_string(),
            ]);
        }

        Self {
            is_async: true,
            is_http_handler,
            scratch_locals: Vec::new(),
            required_imports,
        }
    }

    /// Create `CmExportInfo` for a sync export
    pub fn sync_export() -> Self {
        Self {
            is_async: false,
            is_http_handler: false,
            scratch_locals: Vec::new(),
            required_imports: Vec::new(),
        }
    }
}

/// Run the `wasm_adapt` phase on a Project
///
/// This analyzes world exports and attaches `CmExportInfo` to the corresponding
/// `TirFunctions`. The Project is modified in place.
pub fn wasm_adapt(mut project: Project) -> Project {
    let (wasi_registry, world_registry) = WasiRegistry::build_from_stdlib();

    // Look up the target world
    let world_info = world_registry.get(&project.target_world);

    if let Some(world_info) = world_info {
        // Update project's HTTP handler flag based on world analysis
        project.has_http_handler_export = world_info.has_http_handler_export();

        // Analyze each world export and attach CmExportInfo to corresponding TirFunction
        for export in &world_info.exports {
            let is_http_handler = export.returns_http_response();
            let cm_export_info = if export.is_async {
                CmExportInfo::async_export(is_http_handler)
            } else {
                CmExportInfo::sync_export()
            };

            // Find and update the corresponding TirFunction in the entry module
            let entry_module = project
                .tir_modules
                .get_mut(&project.entry_module_source)
                .expect("entry module should exist");

            for func_rc in &entry_module.functions {
                let mut func = func_rc.borrow_mut();
                if func.name == export.name {
                    func.cm_export_info = Some(cm_export_info.clone());
                    break;
                }
            }
        }
    }

    // Drop registries explicitly (they're not needed after this phase)
    drop(wasi_registry);
    drop(world_registry);

    project
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cm_export_info_async() {
        let info = CmExportInfo::async_export(false);
        assert!(info.is_async);
        assert!(!info.is_http_handler);
        assert!(info.required_imports.contains(&"task-return".to_string()));
    }

    #[test]
    fn test_cm_export_info_http_handler() {
        let info = CmExportInfo::async_export(true);
        assert!(info.is_async);
        assert!(info.is_http_handler);
        assert!(info.required_imports.contains(&"task-return".to_string()));
        assert!(
            info.required_imports
                .contains(&"http-response-new".to_string())
        );
        assert!(info.required_imports.contains(&"future-new".to_string()));
    }

    #[test]
    fn test_cm_export_info_sync() {
        let info = CmExportInfo::sync_export();
        assert!(!info.is_async);
        assert!(!info.is_http_handler);
        assert!(info.required_imports.is_empty());
    }
}
