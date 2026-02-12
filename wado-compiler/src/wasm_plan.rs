//! Wasm Plan Phase - Prepares TIR for WebAssembly code generation
//!
//! This phase runs between optimize and codegen:
//! ```text
//! lower -> optimize -> wasm_plan -> codegen
//! ```
//!
//! Responsibilities:
//! 1. CM boundary analysis - Analyze export signatures to determine required glue code
//! 2. Attach `CmExportInfo` to `TirFunctions` that are world exports
//! 3. Compute scratch locals needed for CM operations
//! 4. Analyze WASI function return types to determine required CM converters
//! 5. Component structure analysis - Determine what the CM component needs
//! 6. Type ordering analysis - Topological sort and dependency analysis for type registration
//!
//! Design principles:
//! - Metadata over TIR for glue code: CM glue uses low-level Wasm operations that
//!   don't map cleanly to TIR, so we use metadata to tell codegen what to generate
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

/// Wasm value type for CM scratch locals
///
/// This mirrors `wasm_encoder::ValType` but is simpler and doesn't require
/// the `wasm_encoder` dependency in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmValType {
    I32,
    I64,
    F32,
    F64,
    /// Nullable anyref - used for storing GC objects
    AnyRef,
}

/// A scratch local variable needed for CM glue code
#[derive(Debug, Clone)]
pub struct CmScratchLocal {
    /// Local variable name (for debugging in WAT output)
    pub name: String,
    /// Wasm value type
    pub val_type: CmValType,
}

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
    /// Scratch locals needed for CM glue code
    pub scratch_locals: Vec<CmScratchLocal>,
    /// CM functions that must be imported (e.g., "task-return", "future-new")
    pub required_imports: Vec<String>,
}

impl CmExportInfo {
    /// Create `CmExportInfo` for an async export
    pub fn async_export(is_http_handler: bool) -> Self {
        let mut required_imports = vec!["task-return".to_string()];
        let mut scratch_locals = Vec::new();

        if is_http_handler {
            // HTTP handler needs additional imports for response creation
            required_imports.extend([
                "future-new".to_string(),
                "future-write".to_string(),
                "http-fields-constructor".to_string(),
                "http-response-new".to_string(),
            ]);

            // Pre-computed scratch locals for HTTP response creation
            // These are the locals needed for CM glue code in codegen
            scratch_locals.extend([
                CmScratchLocal {
                    name: "_http_future".to_string(),
                    val_type: CmValType::I64,
                },
                CmScratchLocal {
                    name: "_trailers_rx".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_trailers_tx".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_headers_handle".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_write_result".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_result_disc".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_response_handle".to_string(),
                    val_type: CmValType::I32,
                },
            ]);
        }

        Self {
            is_async: true,
            is_http_handler,
            scratch_locals,
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

// =============================================================================
// CM Converter Analysis
// =============================================================================

/// Identifies which CM converter function is needed for a given return type.
///
/// This centralizes the CM type analysis that determines what converter functions
/// are needed to convert Component Model representations (in linear memory) to
/// Wado's GC-based types.
///
/// Returns `None` if no converter is needed, or `Some(converter_name)` where
/// `converter_name` is the function name in `core/internal` (e.g., `"cm_list_string_to_array"`).
#[must_use]
pub fn get_cm_converter_for_type(return_type: &Type) -> Option<&'static str> {
    match return_type {
        // Array<String> -> cm_list_string_to_array
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
            if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                return Some("cm_list_string_to_array");
            }
            // Array<u8> -> cm_list_u8_to_array
            if matches!(&g.args[0], Type::Named(n) if n.name == "u8") {
                return Some("cm_list_u8_to_array");
            }
            // Array<[String, String]> -> cm_list_tuple_string_string_to_array
            if let Type::Tuple(tuple_types) = &g.args[0]
                && tuple_types.len() == 2
                && matches!(&tuple_types[0], Type::Named(n) if n.name == "String")
                && matches!(&tuple_types[1], Type::Named(n) if n.name == "String")
            {
                return Some("cm_list_tuple_string_string_to_array");
            }
            // Tuple<String, String> syntax (alternative to [String, String])
            if let Type::Generic(inner_g) = &g.args[0]
                && inner_g.name == "Tuple"
                && inner_g.args.len() == 2
                && matches!(&inner_g.args[0], Type::Named(n) if n.name == "String")
                && matches!(&inner_g.args[1], Type::Named(n) if n.name == "String")
            {
                return Some("cm_list_tuple_string_string_to_array");
            }
            None
        }
        // Option<String> -> cm_option_string_to_option
        Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
            if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                return Some("cm_option_string_to_option");
            }
            None
        }
        _ => None,
    }
}

/// Types of CM converters that may be needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmConverterKind {
    /// `cm_list_string_to_array` converter
    ListString,
    /// `cm_list_u8_to_array` converter
    ListU8,
    /// `cm_list_tuple_string_string_to_array` converter
    ListTupleString,
    /// `cm_option_string_to_option` converter
    OptionString,
}

/// Information about what CM converters are needed for a set of WASI functions.
#[derive(Debug, Clone, Default)]
pub struct CmConverterRequirements {
    /// Set of required converters
    needed: IndexSet<CmConverterKind>,
}

impl CmConverterRequirements {
    /// Analyze a return type and update requirements.
    pub fn analyze_type(&mut self, return_type: &Type) {
        if let Some(converter) = get_cm_converter_for_type(return_type) {
            let kind = match converter {
                "cm_list_string_to_array" => CmConverterKind::ListString,
                "cm_list_u8_to_array" => CmConverterKind::ListU8,
                "cm_list_tuple_string_string_to_array" => CmConverterKind::ListTupleString,
                "cm_option_string_to_option" => CmConverterKind::OptionString,
                _ => return,
            };
            self.needed.insert(kind);
        }
    }

    /// Check if any converters are needed.
    #[must_use]
    pub fn any_needed(&self) -> bool {
        !self.needed.is_empty()
    }

    /// Check if a specific converter is needed.
    #[must_use]
    pub fn needs(&self, kind: CmConverterKind) -> bool {
        self.needed.contains(&kind)
    }
}

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
            let export_name = test.function_name.trim_start_matches('_').replace('_', "-");
            TestExportPlan {
                function_name: test.function_name.clone(),
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
            WorldExportPlan {
                name: export.name,
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

            let mut found_with_export = false;
            let mut found_without_export = false;

            for func_rc in &entry_module.functions {
                let mut func = func_rc.borrow_mut();
                if func.name == export.name {
                    if func.is_export {
                        func.cm_export_info = Some(cm_export_info.clone());
                        found_with_export = true;
                        break;
                    } else {
                        found_without_export = true;
                    }
                }
            }

            // Check for errors
            if !found_with_export && found_without_export {
                return Err(format!(
                    "function `{}` exists but is not marked with `export` keyword. \
                         Add `export` to make it a world entry point: `export fn {}(...)`",
                    export.name, export.name
                ));
            }
            // Note: We don't error on missing functions here because some worlds
            // have optional exports (e.g., HTTP Service's handle is optional if
            // you're just testing). The codegen will fail later if needed.
        }
    }

    // Attach CmExportInfo to test functions (__test_*)
    // Test functions are async exports that use task.return, just like CLI Command's run
    let entry_module = project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("entry module should exist");

    for func_rc in &entry_module.functions {
        let mut func = func_rc.borrow_mut();
        if func.name.starts_with("__test_") && func.cm_export_info.is_none() {
            // Test functions are async (need task.return) but not HTTP handlers
            func.cm_export_info = Some(CmExportInfo::async_export(false));
        }
    }

    // Build ComponentPlan
    project.component_plan = Some(build_component_plan(&project));

    Ok(project)
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

    #[test]
    fn test_get_cm_converter_array_string() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_string_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_array_u8() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "u8".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_u8_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_option_string() {
        let return_type = Type::Generic(GenericType {
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_option_string_to_option")
        );
    }

    #[test]
    fn test_get_cm_converter_array_tuple_string() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Tuple(vec![
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
            ])],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_tuple_string_string_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_none() {
        let return_type = Type::Named(NamedType {
            name: "i32".to_string(),
            span: make_span(),
        });
        assert_eq!(get_cm_converter_for_type(&return_type), None);
    }

    #[test]
    fn test_cm_converter_requirements_analyze() {
        let mut req = CmConverterRequirements::default();
        assert!(!req.any_needed());

        // Analyze Array<String>
        let array_string = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        req.analyze_type(&array_string);
        assert!(req.needs(CmConverterKind::ListString));
        assert!(req.any_needed());

        // Analyze Option<String>
        let option_string = Type::Generic(GenericType {
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        req.analyze_type(&option_string);
        assert!(req.needs(CmConverterKind::OptionString));
    }
}
