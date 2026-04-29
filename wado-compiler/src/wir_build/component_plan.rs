//! Component Model planning types and builder.
//!
//! Contains `ComponentPlan` and related structs that describe the Component Model
//! structure. Built by `wir_build::plan_project`, consumed by `codegen`.

use crate::ast::Type;
use crate::component_model::WasiRegistry;
use crate::hashmap::IndexMap;
use crate::tir::TirTest;
use crate::world_registry::{WorldExportInfo, WorldInfo, WorldRegistry};

/// Plan for the Component Model structure.
///
/// Computed by `wir_build::component_plan`, consumed by codegen. Contains all the structural
/// decisions about what the component needs, so codegen can focus on encoding.
///
/// Canonical intrinsics (e.g., "stream-read", "task-return") are NOT stored here.
/// They are discovered lazily during WIR translation via `WirContext::ensure_canonical`
/// and stored in `WirPackage::needed_canonicals`.
#[derive(Debug, Clone, Default)]
pub struct ComponentPlan {
    /// World exports to create at the component boundary.
    pub world_exports: Vec<WorldExportPlan>,
    /// Test functions to export.
    pub test_exports: Vec<TestExportPlan>,
    /// CM package segment of the target world's `fq_name` (e.g., `"http"`,
    /// `"kiln"`). Empty for the test world or when the target world is not
    /// in the registry. Used by codegen to resolve [`CmExportType::HandlerResult`]
    /// to the per-world `{pkg}-handler-result` registered type.
    pub world_package: String,
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
    /// CM-resolved parameter types at the component boundary, derived from
    /// `params` via the type registry. Codegen iterates this list to build
    /// the export's CM signature without needing per-world discriminators.
    pub cm_params: Vec<(String, CmExportType)>,
    /// CM-resolved return type at the component boundary.
    pub cm_result: CmExportType,
}

/// CM-level type at the world export boundary.
///
/// Plan-level abstraction populated by [`build_component_plan`]; resolved to
/// component type indices in codegen via the per-world naming convention
/// `{pkg}-{cm_name}` for [`Self::Named`] and `{world_pkg}-handler-result` for
/// [`Self::HandlerResult`].
#[derive(Debug, Clone)]
pub enum CmExportType {
    /// `result<>` — no value at the boundary.
    ///
    /// Produced when the export has no return type or its return type is
    /// `Result<(), ()>` (CLI `Command::run`). Encoded by codegen as the
    /// shared `result-unit` component type.
    Unit,
    /// Reference to a named CM type (resource, variant, record, ...) declared
    /// by `interface_fq` with the given kebab-case `cm_name`.
    ///
    /// Examples: `Request` from `wasi:http/types` resolves to
    /// `Named { interface_fq: "wasi:http/types", cm_name: "request" }` and
    /// codegen looks it up as `ctx.type_idx("http-request")`.
    Named {
        interface_fq: String,
        cm_name: String,
    },
    /// `result<own<resp>, error>` synthesized for the world's handler return.
    ///
    /// Produced when the export's return type is `Result<X, Y>` with at least
    /// one non-unit component. Codegen registers a per-world named alias
    /// (e.g. `http-handler-result`, `kiln-handler-result`) and resolves this
    /// variant to `ctx.type_idx(&format!("{world_package}-handler-result"))`.
    HandlerResult,
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
    /// Per-test timeout in milliseconds (from `#[timeout_ms(N)]` attribute).
    /// `None` means use the default timeout.
    pub timeout_ms: Option<u64>,
}

/// Build a `ComponentPlan` from pre-link data.
///
/// Scans entry module tests and world registry to determine what the component needs.
/// Called by [`crate::link::link`] before constructing `FlatPackage`.
///
/// Canonical intrinsics are NOT collected here — they are discovered lazily
/// during WIR translation via `WirContext::ensure_canonical`.
pub fn build_component_plan(
    is_test_world: bool,
    target_world: &str,
    tests: &[TirTest],
    export_binding_names: &IndexMap<String, String>,
    world_registry: &WorldRegistry,
    wasi_registry: &WasiRegistry,
) -> ComponentPlan {
    // Build world exports from registry.
    // For the test world, there are no world exports — only test exports.
    let world_exports = if is_test_world {
        vec![]
    } else {
        build_world_export_plans(
            target_world,
            export_binding_names,
            world_registry,
            wasi_registry,
        )
    };

    // Build test exports (only when targeting the test world)
    let test_exports: Vec<TestExportPlan> = if is_test_world {
        tests
            .iter()
            .map(|test| {
                let export_name = sanitize_kebab_export_name(&test.function_name);
                let core_func_name = export_binding_names
                    .get(&test.function_name)
                    .cloned()
                    .unwrap_or_else(|| test.function_name.clone());
                TestExportPlan {
                    function_name: test.function_name.clone(),
                    core_func_name,
                    export_name,
                    expect_trap: test.expect_trap,
                    is_todo: test.is_todo,
                    timeout_ms: test.timeout_ms,
                }
            })
            .collect()
    } else {
        vec![]
    };

    let world_package = if is_test_world {
        String::new()
    } else {
        world_registry
            .get(target_world)
            .map(|w| w.package().to_string())
            .unwrap_or_default()
    };

    ComponentPlan {
        world_exports,
        test_exports,
        world_package,
    }
}

/// Build world export plans from the world registry.
fn build_world_export_plans(
    target_world: &str,
    export_binding_names: &IndexMap<String, String>,
    world_registry: &WorldRegistry,
    wasi_registry: &WasiRegistry,
) -> Vec<WorldExportPlan> {
    let world = world_registry.get(target_world);
    let exports: Vec<WorldExportInfo> = world.map(|w| w.exports.clone()).unwrap_or_else(|| {
        // Fallback to a default run export for unknown worlds
        vec![WorldExportInfo {
            name: "run".to_string(),
            is_async: true,
            params: vec![],
            return_type: None,
        }]
    });

    // Route through `WorldInfo::has_http_handler_export` so the "is this
    // the HTTP service world?" check stays in one place — no ad-hoc
    // `starts_with("wasi:http/")` string parsing scattered across the
    // codegen pipeline.
    let is_http_world = world.is_some_and(WorldInfo::has_http_handler_export);
    exports
        .into_iter()
        .map(|export| {
            // Only mark as HTTP handler when the world itself is under
            // `wasi:http/…`. Without this guard, any world whose export
            // returns `Result<Response, _>` (for example
            // `core:kiln/generator`'s `Response`) would be misrouted
            // through the HTTP codegen branch.
            let is_http_handler = is_http_world && export.returns_http_response();
            // Use export adapter if one was synthesized by synthesis::cm_binding
            let core_func_name = export_binding_names
                .get(&export.name)
                .cloned()
                .unwrap_or_else(|| export.name.clone());

            // Resolve params and return into CM-level boundary types so
            // codegen can build the export signature uniformly across
            // worlds. See [`CmExportType`].
            let cm_params = export
                .params
                .iter()
                .map(|(name, ty)| (name.clone(), resolve_cm_export_type(ty, wasi_registry)))
                .collect();
            let cm_result = export
                .return_type
                .as_ref()
                .map(|ty| resolve_cm_export_type(ty, wasi_registry))
                .unwrap_or(CmExportType::Unit);

            WorldExportPlan {
                name: export.name,
                core_func_name,
                is_async: export.is_async,
                is_http_handler,
                params: export.params,
                return_type: export.return_type,
                cm_params,
                cm_result,
            }
        })
        .collect()
}

/// Resolve a Wado [`Type`] reachable from a world export signature into the
/// CM-level boundary representation.
///
/// Recognised shapes:
///
/// - `()` / unit / `Result<(), ()>` → [`CmExportType::Unit`]
/// - `Result<X, Y>` with at least one non-unit component →
///   [`CmExportType::HandlerResult`]
/// - Any other named type whose source interface and CM kebab-name are known
///   to the WASI registry → [`CmExportType::Named`]
///
/// Panics on shapes the registry cannot resolve. World export signatures
/// originate in stdlib `lib/wasi/**/worlds.wado` and `lib/core/kiln/worlds.wado`,
/// so any unresolved name indicates a bug in the stdlib bootstrap rather than
/// user input.
fn resolve_cm_export_type(ty: &Type, wasi_registry: &WasiRegistry) -> CmExportType {
    if is_unit_like(ty) {
        return CmExportType::Unit;
    }
    if let Type::Generic(generic) = ty
        && generic.name == "Result"
        && generic.args.len() == 2
    {
        if is_unit_like(&generic.args[0]) && is_unit_like(&generic.args[1]) {
            return CmExportType::Unit;
        }
        return CmExportType::HandlerResult;
    }
    if let Type::Named(named) = ty {
        // World bodies (`lib/wasi/**/worlds.wado`, `lib/core/kiln/worlds.wado`)
        // reference type names like `Request` / `RawRequest` directly without
        // a `use { ... } from "..."` import, so `populate_named_type_sources`
        // leaves `source_interface = None`. Fall back to the by-name lookups
        // that the rest of the CM codegen uses for the same shape — trying
        // both the `wasi:*` and `core:kiln/*` namespaces.
        let interface_fq = named
            .source_interface
            .clone()
            .or_else(|| {
                wasi_registry
                    .find_wasi_resource_source(&named.name)
                    .or_else(|| wasi_registry.find_wasi_variant_source(&named.name))
                    .or_else(|| wasi_registry.find_wasi_struct_source(&named.name))
                    .or_else(|| wasi_registry.find_wasi_enum_source(&named.name))
                    .or_else(|| wasi_registry.find_wasi_flags_source(&named.name))
                    .or_else(|| wasi_registry.find_kiln_struct_source(&named.name))
                    .or_else(|| wasi_registry.find_kiln_variant_source(&named.name))
                    .or_else(|| wasi_registry.find_kiln_enum_source(&named.name))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                panic!(
                    "world export type `{}` has no source interface — neither \
                 the AST source_interface nor any WASI/kiln registry index resolved it",
                    named.name,
                )
            });
        let cm_name = wasi_registry
            .get_resource_cm_name_by_source(&interface_fq, &named.name)
            .or_else(|| wasi_registry.get_variant_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| wasi_registry.get_struct_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| wasi_registry.get_enum_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| wasi_registry.get_flags_cm_name_by_source(&interface_fq, &named.name))
            .unwrap_or_else(|| panic!(
                "world export type `{}` (interface `{interface_fq}`) has no CM name in the WASI registry",
                named.name,
            ))
            .to_string();
        return CmExportType::Named {
            interface_fq,
            cm_name,
        };
    }
    panic!("unsupported world export type shape: {ty:?}");
}

/// Returns true if `ty` is a unit/empty-tuple type at the boundary.
///
/// Mirrors the recogniser in [`crate::component_model::unwrap_async_call_if_async`]
/// — both surface syntaxes the parser may emit (`()`, `Unit`, `unit`, empty tuple)
/// must round-trip to the same CM shape.
fn is_unit_like(ty: &Type) -> bool {
    match ty {
        Type::Tuple(elems) => elems.is_empty(),
        Type::Named(named) => matches!(named.name.as_str(), "()" | "Unit" | "unit"),
        _ => false,
    }
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
        // timeout_ms tests
        assert_eq!(
            sanitize_kebab_export_name("__test_tm2000_0_slow"),
            "test-tm2000-0-slow"
        );
        assert_eq!(
            sanitize_kebab_export_name("__test_trap_tm500_0_panics"),
            "test-trap-tm500-0-panics"
        );
        assert_eq!(
            sanitize_kebab_export_name("__test_todo_tm3000_1"),
            "test-todo-tm3000-1"
        );
    }
}
