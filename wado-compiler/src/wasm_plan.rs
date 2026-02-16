//! Wasm Plan Phase - Prepares TIR for WebAssembly code generation
//!
//! This phase runs between optimize and codegen:
//! ```text
//! lower -> optimize -> wasm_plan -> codegen
//! ```
//!
//! Responsibilities:
//! 1. Set project flags based on world analysis (e.g., `has_http_handler_export`)
//! 2. Build `ComponentPlan` for codegen (structure, imports, exports)
//! 3. Type ordering analysis - Topological sort and dependency analysis for type registration
//!
//! Note: CM export adapter synthesis (task-return wrapping, Result lowering) is handled
//! by `cm_adapter_gen` at the TIR level. This phase focuses on metadata and planning.
//!
//! Design principles:
//! - Keep codegen simple: codegen should just convert TIR to Wasm without
//!   needing to analyze world definitions or export signatures
//! - Centralize Wasm-related analysis: All pre-codegen analysis should be in this
//!   module to avoid duplication between optimize and codegen phases
//! - Pure analysis functions: Topological sort, dependency analysis, and self-referential
//!   detection are pure functions that don't depend on codegen state

use crate::ast::Type;
use crate::project::Project;
use crate::tir::{ResolvedType, TirStruct, TirVariantDecl, TypeId, TypeTable};
use crate::world_registry::WorldExportInfo;
use indexmap::IndexMap;
use indexmap::IndexSet;

// =============================================================================
// Component Plan
// =============================================================================

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
    /// Internal function name (e.g., "__`test_0_simple`")
    pub function_name: String,
    /// Core function name in Wasm module (adapter name if adapter exists, otherwise same as `function_name`)
    pub core_func_name: String,
    /// Component export name in kebab-case (e.g., "test-0-simple")
    pub export_name: String,
}

/// Build a `ComponentPlan` from the project.
///
/// Scans TIR imports and world registry to determine what the component needs.
fn build_component_plan(project: &Project) -> ComponentPlan {
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

    // Build world exports from registry
    let world_exports = build_world_export_plans(project);

    // Build test exports
    let test_exports: Vec<TestExportPlan> = entry_tir
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
            }
        })
        .collect();

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
            // Use export adapter if one was synthesized by cm_adapter_gen
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

// =============================================================================
// Type Ordering Analysis
// =============================================================================

/// A type declaration in topological order (struct or variant).
pub enum TypeDecl<'a> {
    Struct(&'a TirStruct),
    Variant(&'a TirVariantDecl),
}

/// Get type dependencies (struct and variant names) for a given type.
/// Used for topological sorting of type declarations.
///
/// Returns mangled names for `GenericInstance` types (e.g., "`BTreeNode<String,i32>`").
pub fn get_type_dependencies(type_table: &TypeTable, type_id: TypeId) -> Vec<String> {
    match type_table.get(type_id) {
        ResolvedType::Struct { name, .. } => vec![name.clone()],
        ResolvedType::Variant { name, .. } => vec![name.clone()],
        ResolvedType::GenericInstance { type_args, .. } => {
            let mangled_name = type_table.mangle_type_name(type_id);
            let mut deps = vec![mangled_name];
            for arg in type_args {
                deps.extend(get_type_dependencies(type_table, *arg));
            }
            deps
        }
        ResolvedType::BuiltinArray(inner)
        | ResolvedType::Option(inner)
        | ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Stream(inner)
        | ResolvedType::Future(inner)
        | ResolvedType::Reactive(inner) => get_type_dependencies(type_table, *inner),
        ResolvedType::Tuple(elems) => elems
            .iter()
            .flat_map(|e| get_type_dependencies(type_table, *e))
            .collect(),
        _ => vec![],
    }
}

/// Check if a struct has self-referential fields (directly or through Array/Ref/MutRef).
/// Returns the list of field type IDs that create the self-reference cycle.
pub fn get_self_referential_field_types(
    struct_name: &str,
    tir_struct: &TirStruct,
    type_table: &TypeTable,
) -> Vec<TypeId> {
    let mut self_ref_fields = Vec::new();
    for field in &tir_struct.fields {
        if type_references_struct(field.type_id, struct_name, type_table) {
            self_ref_fields.push(field.type_id);
        }
    }
    self_ref_fields
}

/// Check if a type references a struct by name (transitively through Array/Ref/MutRef).
/// The `struct_name` should be the full mangled name (e.g., "`AANode<String,i32>`").
pub fn type_references_struct(type_id: TypeId, struct_name: &str, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { name, .. } => name == struct_name,
        ResolvedType::GenericInstance { type_args, .. } => {
            let mangled_name = type_table.mangle_type_name(type_id);
            if mangled_name == struct_name {
                return true;
            }
            type_args
                .iter()
                .any(|arg| type_references_struct(*arg, struct_name, type_table))
        }
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            type_references_struct(*inner, struct_name, type_table)
        }
        ResolvedType::BuiltinArray(inner) => {
            type_references_struct(*inner, struct_name, type_table)
        }
        ResolvedType::Option(inner) => type_references_struct(*inner, struct_name, type_table),
        _ => false,
    }
}

/// Sort structs and variants together topologically so dependencies are registered
/// before dependents. This handles mutual dependencies between structs and variants
/// (e.g., struct with variant field, variant with struct payload).
pub fn sort_types_topologically<'a>(
    structs: &'a [TirStruct],
    variants: &'a [TirVariantDecl],
    type_table: &TypeTable,
) -> Vec<TypeDecl<'a>> {
    // Collect all type names
    let struct_names: IndexSet<String> = structs.iter().map(|s| s.name.clone()).collect();
    let variant_names: IndexSet<String> = variants.iter().map(|v| v.name.clone()).collect();
    let all_names: IndexSet<String> = struct_names.union(&variant_names).cloned().collect();

    // Build dependency graph: deps[A] = [B] means A depends on B (B must come before A)
    let mut deps: IndexMap<String, Vec<String>> = IndexMap::new();

    for s in structs {
        let mut type_deps = Vec::new();
        for field in &s.fields {
            let field_deps = get_type_dependencies(type_table, field.type_id);
            for dep in field_deps {
                if all_names.contains(&dep) && dep != s.name {
                    type_deps.push(dep);
                }
            }
        }
        deps.insert(s.name.clone(), type_deps);
    }

    for v in variants {
        let mut type_deps = Vec::new();
        for case in &v.cases {
            let payload_deps = get_type_dependencies(type_table, case.payload);
            for dep in payload_deps {
                if all_names.contains(&dep) && dep != v.name {
                    type_deps.push(dep);
                }
            }
        }
        deps.insert(v.name.clone(), type_deps);
    }

    // Topological sort using Kahn's algorithm
    let mut in_degree: IndexMap<String, usize> = IndexMap::new();
    for name in &all_names {
        let type_deps = deps.get(name).map(std::vec::Vec::len).unwrap_or(0);
        in_degree.insert(name.clone(), type_deps);
    }

    let mut dependents: IndexMap<String, Vec<String>> = IndexMap::new();
    for (name, type_deps) in &deps {
        for dep in type_deps {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(name.clone());
        }
    }

    let mut queue: Vec<String> = in_degree
        .iter()
        .filter(|&(_, deg)| *deg == 0)
        .map(|(name, _)| name.clone())
        .collect();

    let mut sorted_names = Vec::new();
    while let Some(name) = queue.pop() {
        sorted_names.push(name.clone());
        if let Some(deps_on_name) = dependents.get(&name) {
            for dependent in deps_on_name {
                let deg = in_degree.get_mut(dependent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push(dependent.clone());
                }
            }
        }
    }

    // Map names back to TypeDecl
    let name_to_struct: IndexMap<&str, &TirStruct> =
        structs.iter().map(|s| (s.name.as_str(), s)).collect();
    let name_to_variant: IndexMap<&str, &TirVariantDecl> =
        variants.iter().map(|v| (v.name.as_str(), v)).collect();

    sorted_names
        .iter()
        .filter_map(|name| {
            if let Some(s) = name_to_struct.get(name.as_str()) {
                Some(TypeDecl::Struct(s))
            } else {
                name_to_variant
                    .get(name.as_str())
                    .map(|v| TypeDecl::Variant(v))
            }
        })
        .collect()
}

// =============================================================================
// Main wasm_plan phase
// =============================================================================

/// Run the `wasm_plan` phase on a Project
///
/// This analyzes world exports and attaches `CmExportInfo` to the corresponding
/// `TirFunctions`. It also builds the `ComponentPlan` for codegen.
///
/// Returns an error if a required world export function is missing or not marked with `export`.
pub fn wasm_plan(mut project: Project) -> Result<Project, String> {
    // Look up the target world from the registry in Project
    let world_info = project.world_registry.get(&project.target_world).cloned();

    if let Some(world_info) = world_info {
        // Update project's HTTP handler flag based on world analysis
        project.has_http_handler_export = world_info.has_http_handler_export();
        // All world exports are handled by CM adapter synthesis (cm_adapter_gen).
        // The adapter functions contain task-return calls in their TIR bodies.
    }

    // Build ComponentPlan
    project.component_plan = Some(build_component_plan(&project));

    Ok(project)
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
    use crate::ast::{GenericType, NamedType};
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 0, 0)
    }

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
    }
}
