//! Wasm plan — Component Model planning types and builder.
//!
//! Contains `ComponentPlan` and related structs used by `wir_build` and `codegen`.
//! The planning logic (formerly the standalone `wasm_plan` pipeline phase) now runs
//! inside `wir_build::plan_project`, called at the start of the WIR pipeline.
//!
//! Type-ordering utilities (topological sort) have moved to `wir_build::types`.

use crate::ast::Type;
use crate::project::Project;
use crate::world_registry::WorldExportInfo;

/// Plan for the Component Model structure.
///
/// Computed by `wasm_plan`, consumed by codegen. Contains all the structural
/// decisions about what the component needs, so codegen can focus on encoding.
#[derive(Debug, Clone, Default)]
pub struct ComponentPlan {
    /// Canonical intrinsics needed (e.g., "stream-new", "task-return").
    /// These are TIR imports with namespace "wasi".
    pub canonical_intrinsics: Vec<String>,
    /// Whether future intrinsics are needed (future-new, future-write, etc.).
    pub needs_future_intrinsics: bool,
    /// Bundled module function names (e.g., "`f64_to_buffer`", "`libm_sin`").
    /// These are TIR imports with namespace "bundled".
    pub bundled_functions: Vec<String>,
    /// World exports to create at the component boundary.
    pub world_exports: Vec<WorldExportPlan>,
    /// Test functions to export.
    pub test_exports: Vec<TestExportPlan>,
}

/// A world export to create at the component boundary.
#[derive(Debug, Clone)]
pub struct WorldExportPlan {
    /// Export function name (e.g., "run", "handle")
    pub name: String,
    /// Core function name in the Wasm module (e.g., `"__cm_export__run"` if adapter exists, or `"run"`)
    pub core_func_name: String,
    /// Whether this is an async export
    pub is_async: bool,
    /// Whether this is an HTTP handler (Service world)
    pub is_http_handler: bool,
    /// Parameter types from world definition
    pub params: Vec<(String, Type)>,
    /// Return type from world definition
    pub return_type: Option<Type>,
}

/// A test function to export from the component.
#[derive(Debug, Clone)]
pub struct TestExportPlan {
    /// Internal function name (e.g., "__`test_0_simple`", "__`test_trap_0_panics`", or "__`test_todo_0_not_yet`")
    pub function_name: String,
    /// Core function name in Wasm module (adapter name if adapter exists, otherwise same as `function_name`)
    pub core_func_name: String,
    /// Component export name in kebab-case (e.g., "test-0-simple", "test-trap-0-panics", "test-todo-0-not-yet")
    pub export_name: String,
    /// Whether this test is expected to trap (derived from the `#[expect_trap]` attribute)
    pub expect_trap: bool,
    /// Whether this is a TODO placeholder test (derived from the `#[TODO]` attribute).
    /// Like `expect_trap`, passes when the body traps, but the runner emits a distinct message.
    pub is_todo: bool,
}

/// Build a `ComponentPlan` from the project.
///
/// Scans TIR imports and world registry to determine what the component needs.
/// Called by `wir_build::plan_project` at the start of the WIR pipeline.
pub fn build_component_plan(project: &Project) -> ComponentPlan {
    let entry_tir = project.entry_module();

    // Collect canonical intrinsics from TIR imports with namespace "wasi"
    let canonical_intrinsics: Vec<String> = entry_tir
        .imports
        .iter()
        .filter(|i| i.namespace == "wasi")
        .map(|i| i.canonical_name.clone())
        .collect();

    // Check if future intrinsics are needed
    let needs_future_intrinsics = canonical_intrinsics.iter().any(|name| {
        matches!(
            name.as_str(),
            "future-new" | "future-write" | "future-drop-writable" | "future-drop-readable"
        )
    });

    // Collect bundled module functions from TIR imports with namespace "bundled"
    let bundled_functions: Vec<String> = entry_tir
        .imports
        .iter()
        .filter(|i| i.namespace == "bundled")
        .map(|i| i.canonical_name.clone())
        .collect();

    // Build world exports from registry.
    // For the test world, there are no world exports — only test exports.
    let world_exports = if project.is_test_world() {
        vec![]
    } else {
        build_world_export_plans(project)
    };

    // Build test exports (only when targeting the test world)
    let test_exports: Vec<TestExportPlan> = if project.is_test_world() {
        entry_tir
            .tests
            .iter()
            .map(|test| {
                let export_name = sanitize_kebab_export_name(&test.function_name);
                let core_func_name = project
                    .export_adapter_names
                    .get(&test.function_name)
                    .cloned()
                    .unwrap_or_else(|| test.function_name.clone());
                TestExportPlan {
                    function_name: test.function_name.clone(),
                    core_func_name,
                    export_name,
                    expect_trap: test.expect_trap,
                    is_todo: test.is_todo,
                }
            })
            .collect()
    } else {
        vec![]
    };

    ComponentPlan {
        canonical_intrinsics,
        needs_future_intrinsics,
        bundled_functions,
        world_exports,
        test_exports,
    }
}

/// Build world export plans from the world registry.
fn build_world_export_plans(project: &Project) -> Vec<WorldExportPlan> {
    let exports: Vec<WorldExportInfo> = project
        .world_registry
        .get(&project.target_world)
        .map(|w| w.exports.clone())
        .unwrap_or_else(|| {
            // Fallback to a default run export for unknown worlds
            vec![WorldExportInfo {
                name: "run".to_string(),
                is_async: true,
                params: vec![],
                return_type: None,
            }]
        });

    exports
        .into_iter()
        .map(|export| {
            let is_http_handler = export.returns_http_response();
            // Use export adapter if one was synthesized by synthesis::cm_adapter
            let core_func_name = project
                .export_adapter_names
                .get(&export.name)
                .cloned()
                .unwrap_or_else(|| export.name.clone());
            WorldExportPlan {
                name: export.name,
                core_func_name,
                is_async: export.is_async,
                is_http_handler,
                params: export.params,
                return_type: export.return_type,
            }
        })
        .collect()
}

/// Convert a test function name (e.g., `__test_0_my_name`) to a valid kebab-case
/// CM export name (e.g., `test-0-my-name`).
///
/// Test names may contain consecutive underscores when non-alphanumeric characters
/// (like parentheses) in the original test string are each replaced with `_` by the
/// resolver. A naive `replace('_', '-')` would produce consecutive dashes which
/// violate the kebab-case requirement of the Component Model.
fn sanitize_kebab_export_name(function_name: &str) -> String {
    let raw = function_name.trim_start_matches('_').replace('_', "-");
    // Collapse consecutive dashes and strip trailing dashes
    let mut prev_dash = false;
    let collapsed: String = raw
        .chars()
        .filter(|&c| {
            if c == '-' {
                if prev_dash {
                    return false;
                }
                prev_dash = true;
            } else {
                prev_dash = false;
            }
            true
        })
        .collect();
    collapsed.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_sanitize_kebab_export_name() {
        // Simple case
        assert_eq!(
            sanitize_kebab_export_name("__test_0_simple"),
            "test-0-simple"
        );
        // Consecutive underscores from parentheses in test name
        assert_eq!(
            sanitize_kebab_export_name("__test_23_compression_level_0__stored__round_trip"),
            "test-23-compression-level-0-stored-round-trip"
        );
        // Trailing underscores
        assert_eq!(
            sanitize_kebab_export_name("__test_1_trailing__"),
            "test-1-trailing"
        );
        // Unnamed test (no name part)
        assert_eq!(sanitize_kebab_export_name("__test_5"), "test-5");
        // expect_trap tests
        assert_eq!(
            sanitize_kebab_export_name("__test_trap_0_panics_on_zero"),
            "test-trap-0-panics-on-zero"
        );
        assert_eq!(sanitize_kebab_export_name("__test_trap_3"), "test-trap-3");
        // TODO tests
        assert_eq!(
            sanitize_kebab_export_name("__test_todo_0_not_yet_implemented"),
            "test-todo-0-not-yet-implemented"
        );
        assert_eq!(sanitize_kebab_export_name("__test_todo_2"), "test-todo-2");
    }
}
