//! Component Model planning types and builder.
//!
//! Contains `ComponentPlan` and related structs that describe the Component Model
//! structure. Built by `wir_build::plan_project`, consumed by `codegen`.

use crate::ast::Type;
use crate::component_model::{CmInterfaceRegistry, is_unit_type};
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
}

/// A world export to create at the component boundary.
#[derive(Debug, Clone)]
pub struct WorldExportPlan {
    /// Export function name (e.g., "run", "handle"). Used as-is for the core
    /// module alias (`{name}-core`).
    pub name: String,
    /// Kebab-case CM extern name for the component boundary (underscores → hyphens).
    /// Computed once here so codegen emits the plan as-is rather than owning the
    /// underscore→kebab transform. WASI names (`run`, `handle`, `generate`) are
    /// already kebab-safe, so this equals `name` for them.
    pub cm_export_name: String,
    /// Core function name in the Wasm module (e.g., `"__cm_export__run"` if adapter exists, or `"run"`)
    pub core_func_name: String,
    /// Whether this is an async export
    pub is_async: bool,
    /// CM-resolved parameter types at the component boundary. Codegen
    /// iterates this list to build the export's CM signature without
    /// needing per-world discriminators.
    pub cm_params: Vec<(String, CmExportType)>,
    /// CM-resolved return type at the component boundary.
    pub cm_result: CmExportType,
    /// Fully-qualified CM interface name this export was synthesized from
    /// (`WorldExportInfo::from_interface_fq`), e.g.
    /// `"wasi:http/handler@0.3.0-rc-2026-03-15"` or
    /// `"wasi:cli/run@0.3.0-rc-2026-03-15"`. `Some` marks the export as an
    /// interface-instance export: codegen wraps it (and the named CM types its
    /// signature references) in an instance export under this FQ. `None` is a
    /// freestanding world function export (the kiln generator's `generate`,
    /// test exports) emitted as a bare top-level function.
    pub from_interface_fq: Option<String>,
    /// Use a synchronous canonical lift (no `CanonicalOption::Async`): the core
    /// function returns the lowered value(s) directly instead of delivering them
    /// via `task.return`. Set for `--lib` world exports, whose adapters are
    /// synthesized as value-returning functions. The existing WASI worlds (CLI,
    /// HTTP, kiln) keep the async lift, so this is `false` for them.
    pub sync_lift: bool,
}

/// CM-level type at the world export boundary.
///
/// Plan-level abstraction populated by [`build_component_plan`]; resolved to
/// component type indices in codegen via the `{pkg}-{cm_name}` naming
/// convention for [`Self::Named`] and via the structural type interner for
/// [`Self::HandlerResult`].
#[derive(Debug, Clone)]
pub enum CmExportType {
    /// A primitive value type at the boundary, carrying the Wado type name
    /// (`"bool"`, `"u32"`, `"i64"`, `"f64"`, `"char"`, `"String"`, ...).
    /// Codegen maps it to the matching `PrimitiveValType`. Used by `--lib`
    /// world exports whose signatures are primitives-only.
    Primitive(String),
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
    /// `Named { interface_fq: "wasi:http/types", cm_name: "request", is_resource: true }`
    /// and codegen looks it up as `ctx.type_idx("http-request")`.
    Named {
        interface_fq: String,
        cm_name: String,
        /// Whether this names a CM resource (vs variant / record / enum /
        /// flags). Lets codegen re-export the resource type
        /// (`{pkg}-{cm_name}-resource`) vs the plain `{pkg}-{cm_name}` by match,
        /// not by probing the type registry.
        is_resource: bool,
    },
    /// `result<own<resp>, error>` synthesized for the world's handler return.
    ///
    /// Produced when the export's return type is `Result<X, Y>` with at least
    /// one non-unit component. Codegen resolves this variant through the
    /// structural type interner, keyed on the `ok`/`err` arm types, so it
    /// shares the type the import/world-type helpers interned.
    ///
    /// The resolved `ok`/`err` boundary types are carried so codegen can
    /// enumerate the named CM types to re-export in an interface-instance
    /// export (e.g. `response`, `error-code`) without re-deriving them from
    /// hardcoded type names. Either arm may be [`Self::Unit`] (e.g.
    /// `Result<(), Error>`); such arms contribute no re-exported type.
    HandlerResult {
        ok: Box<CmExportType>,
        err: Box<CmExportType>,
    },
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
    /// Original, lossless `test "name"` string (`None` for unnamed tests).
    /// `export_name` is ASCII-folded and lowercased, so this is the only
    /// faithful record of the name. Emitted into the `wado:test-names` custom
    /// section so the runner can display what the user actually wrote.
    pub original_name: Option<String>,
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
/// Called by [`crate::link::link`] before constructing `NirPackage`.
///
/// Canonical intrinsics are NOT collected here — they are discovered lazily
/// during WIR translation via `WirContext::ensure_canonical`.
#[allow(clippy::too_many_arguments)]
pub fn build_component_plan(
    is_test_world: bool,
    target_world: &str,
    tests: &[TirTest],
    test_name_filters: &[String],
    export_binding_names: &IndexMap<String, String>,
    world_registry: &WorldRegistry,
    cm_interface_registry: &CmInterfaceRegistry,
    lib_world: Option<&WorldInfo>,
    is_lib_world: bool,
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
            cm_interface_registry,
            lib_world,
            is_lib_world,
        )
    };

    // Build test exports (only when targeting the test world)
    let test_exports: Vec<TestExportPlan> = if is_test_world {
        tests
            .iter()
            // Keep the same selection as `synthesis::cm_binding` step 6, so the
            // exports we plan match the adapters that survived early DCE.
            // Unselected tests have no adapter and were dropped; planning them
            // here would reference a function that no longer exists.
            .filter(|test| crate::package::test_selected(test.name.as_deref(), test_name_filters))
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
                    original_name: test.name.clone(),
                    expect_trap: test.expect_trap,
                    is_todo: test.is_todo,
                    timeout_ms: test.timeout_ms,
                }
            })
            .collect()
    } else {
        vec![]
    };

    ComponentPlan {
        world_exports,
        test_exports,
    }
}

/// Build world export plans from the world registry.
fn build_world_export_plans(
    target_world: &str,
    export_binding_names: &IndexMap<String, String>,
    world_registry: &WorldRegistry,
    cm_interface_registry: &CmInterfaceRegistry,
    lib_world: Option<&WorldInfo>,
    is_lib_world: bool,
) -> Vec<WorldExportPlan> {
    // The synthesized library world (`--lib`) takes priority over the static
    // registry, which cannot hold a per-package world.
    let world = lib_world.or_else(|| world_registry.get(target_world));
    let world_namespace_prefix = world.map(crate::world_registry::WorldInfo::namespace_prefix);
    let exports: Vec<WorldExportInfo> = world.map(|w| w.exports.clone()).unwrap_or_else(|| {
        // Fallback to a default run export for unknown worlds
        vec![WorldExportInfo {
            name: "run".to_string(),
            is_async: true,
            params: vec![],
            return_type: None,
            from_interface_fq: None,
        }]
    });

    exports
        .into_iter()
        .map(|export| {
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
                .map(|(name, ty)| {
                    (
                        name.clone(),
                        resolve_cm_export_type(ty, cm_interface_registry, world_namespace_prefix),
                    )
                })
                .collect();
            let cm_result = export
                .return_type
                .as_ref()
                .map(|ty| resolve_cm_export_type(ty, cm_interface_registry, world_namespace_prefix))
                .unwrap_or(CmExportType::Unit);

            WorldExportPlan {
                from_interface_fq: export.from_interface_fq.clone(),
                cm_export_name: kebab_export_name(&export.name),
                name: export.name,
                core_func_name,
                is_async: export.is_async,
                cm_params,
                cm_result,
                // Library exports use a synchronous lift; the WASI worlds keep
                // the async/task-return lift.
                sync_lift: is_lib_world,
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
///   to the registry → [`CmExportType::Named`]
///
/// Panics on shapes the registry cannot resolve. World export signatures
/// originate in stdlib `lib/wasi/**/worlds.wado` and
/// `lib/core/kiln/worlds.wado`, so any unresolved name indicates a bug in the
/// stdlib bootstrap rather than user input.
/// Whether `name` is a Wado primitive that maps directly to a Component Model
/// primitive value type at the export boundary.
fn is_cm_primitive_name(name: &str) -> bool {
    crate::component_model::wado_primitive_name_to_cm(name).is_some()
}

fn resolve_cm_export_type(
    ty: &Type,
    cm_interface_registry: &CmInterfaceRegistry,
    world_namespace_prefix: Option<&str>,
) -> CmExportType {
    if is_unit_type(ty) {
        return CmExportType::Unit;
    }
    if let Type::Generic(generic) = ty
        && generic.name == "Result"
        && generic.args.len() == 2
    {
        if is_unit_type(&generic.args[0]) && is_unit_type(&generic.args[1]) {
            return CmExportType::Unit;
        }
        return CmExportType::HandlerResult {
            ok: Box::new(resolve_cm_export_type(
                &generic.args[0],
                cm_interface_registry,
                world_namespace_prefix,
            )),
            err: Box::new(resolve_cm_export_type(
                &generic.args[1],
                cm_interface_registry,
                world_namespace_prefix,
            )),
        };
    }
    if let Type::Named(named) = ty
        && is_cm_primitive_name(&named.name)
    {
        return CmExportType::Primitive(named.name.clone());
    }
    if let Type::Named(named) = ty {
        // World bodies (`lib/wasi/**/worlds.wado`, `lib/core/kiln/worlds.wado`)
        // reference type names like `Request` / `RawRequest` directly without
        // a `use { ... } from "..."` import, so `populate_named_type_sources`
        // leaves `source_interface = None`. `CmInterfaceRegistry::resolve_cm_source_for`
        // already chains the `wasi:*` and `core:kiln/*` by-name lookups for
        // exactly this case — re-use it instead of duplicating the chain.
        let interface_fq = named
            .source_interface
            .as_deref()
            .or_else(|| {
                world_namespace_prefix.and_then(|prefix| {
                    cm_interface_registry.resolve_cm_source_with_prefix(named, prefix)
                })
            })
            .or_else(|| cm_interface_registry.resolve_cm_source_for(named, None))
            .map(str::to_string)
            .unwrap_or_else(|| {
                panic!(
                    "world export type `{}` has no source interface — neither the \
                 AST source_interface nor any registry index resolved it",
                    named.name,
                )
            });
        let resource_cm =
            cm_interface_registry.get_resource_cm_name_by_source(&interface_fq, &named.name);
        let is_resource = resource_cm.is_some();
        let cm_name = resource_cm
            .or_else(|| cm_interface_registry.get_variant_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| cm_interface_registry.get_struct_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| cm_interface_registry.get_enum_cm_name_by_source(&interface_fq, &named.name))
            .or_else(|| cm_interface_registry.get_flags_cm_name_by_source(&interface_fq, &named.name))
            .unwrap_or_else(|| panic!(
                "world export type `{}` (interface `{interface_fq}`) has no CM name in the registry",
                named.name,
            ))
            .to_string();
        return CmExportType::Named {
            interface_fq,
            cm_name,
            is_resource,
        };
    }
    panic!("unsupported world export type shape: {ty:?}");
}

/// Kebab-case a world export name for the Component Model boundary: underscores
/// become hyphens. Wado identifiers are `[a-z0-9_]`, so this yields a valid CM
/// extern name. Already-kebab WASI names (`run`, `handle`) are unchanged.
fn kebab_export_name(name: &str) -> String {
    name.replace('_', "-")
}

/// Convert a test function name (e.g., `__test_0_my_name`) to a valid kebab-case
/// CM export name (e.g., `test-0-my-name`).
///
/// Test names may contain consecutive underscores when non-alphanumeric characters
/// (like parentheses) in the original test string are each replaced with `_` by the
/// elaborator. A naive `replace('_', '-')` would produce consecutive dashes which
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

    /// Helpers for the resolver tests below.
    mod resolver_helpers {
        use super::super::*;
        use crate::ast::{AstId, GenericType, NamedType};
        use crate::token::Span;

        pub fn span() -> Span {
            Span::new(0, 0, 1, 1)
        }

        pub fn unit_named() -> Type {
            Type::Named(NamedType::new(AstId::fresh(), "()".to_string(), span()))
        }

        pub fn empty_tuple() -> Type {
            Type::Tuple(vec![])
        }

        pub fn named(name: &str) -> Type {
            Type::Named(NamedType::new(AstId::fresh(), name.to_string(), span()))
        }

        pub fn result_of(ok: Type, err: Type) -> Type {
            Type::Generic(GenericType {
                id: AstId::fresh(),
                name: "Result".to_string(),
                args: vec![ok, err],
                span: span(),
            })
        }
    }

    #[test]
    fn test_resolve_unit_shapes() {
        use resolver_helpers::*;
        let (registry, _) = crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        // Bare unit forms
        assert!(matches!(
            resolve_cm_export_type(&unit_named(), registry, None),
            CmExportType::Unit
        ));
        assert!(matches!(
            resolve_cm_export_type(&empty_tuple(), registry, None),
            CmExportType::Unit
        ));

        // Result<(), ()> — both arms unit → still Unit (CLI Command::run shape)
        assert!(matches!(
            resolve_cm_export_type(&result_of(unit_named(), unit_named()), registry, None),
            CmExportType::Unit
        ));
        assert!(matches!(
            resolve_cm_export_type(&result_of(empty_tuple(), empty_tuple()), registry, None),
            CmExportType::Unit
        ));
    }

    #[test]
    fn test_resolve_handler_result() {
        use resolver_helpers::*;
        let (registry, _) = crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        // wasi:http handler shape: Result<Response, ErrorCode>, resolved in the
        // http world scope. The arms resolve to wasi:http/types.
        let http = result_of(named("Response"), named("ErrorCode"));
        match resolve_cm_export_type(&http, registry, Some("wasi:http/")) {
            CmExportType::HandlerResult { ok, err } => {
                assert!(
                    matches!(*ok, CmExportType::Named { ref cm_name, ref interface_fq, .. }
                    if cm_name == "response" && interface_fq.starts_with("wasi:http/types"))
                );
                assert!(
                    matches!(*err, CmExportType::Named { ref cm_name, ref interface_fq, .. }
                    if cm_name == "error-code" && interface_fq.starts_with("wasi:http/types"))
                );
            }
            other => panic!("expected HandlerResult, got {other:?}"),
        }

        // core:kiln generator shape: Result<Response, Error>. `Response` also
        // exists in wasi:http; the kiln world scope must resolve it to
        // core:kiln/types rather than the http resource.
        let kiln = result_of(named("Response"), named("Error"));
        match resolve_cm_export_type(&kiln, registry, Some("core:kiln/")) {
            CmExportType::HandlerResult { ok, err } => {
                assert!(
                    matches!(*ok, CmExportType::Named { ref cm_name, ref interface_fq, .. }
                    if cm_name == "response" && interface_fq.starts_with("core:kiln/types"))
                );
                assert!(
                    matches!(*err, CmExportType::Named { ref cm_name, ref interface_fq, .. }
                    if cm_name == "error" && interface_fq.starts_with("core:kiln/types"))
                );
            }
            other => panic!("expected HandlerResult, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_named_resource() {
        use resolver_helpers::*;
        let (registry, _) = crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        // `Request` is a resource declared in `wasi:http/types`.
        match resolve_cm_export_type(&named("Request"), registry, None) {
            CmExportType::Named {
                interface_fq,
                cm_name,
                is_resource,
            } => {
                assert!(
                    interface_fq.starts_with("wasi:http/types"),
                    "expected wasi:http/types prefix, got `{interface_fq}`",
                );
                assert_eq!(cm_name, "request");
                assert!(is_resource, "Request is a CM resource");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_named_kiln_record() {
        use resolver_helpers::*;
        let (registry, _) = crate::component_model::CmInterfaceRegistry::build_from_stdlib();

        // `RawRequest` is a struct declared in `core:kiln/types` — exercises
        // the `find_kiln_*` half of `resolve_cm_source_for`.
        match resolve_cm_export_type(&named("RawRequest"), registry, None) {
            CmExportType::Named {
                interface_fq,
                cm_name,
                is_resource,
            } => {
                assert!(
                    !is_resource,
                    "RawRequest is a record (struct), not a resource"
                );
                assert!(
                    interface_fq.starts_with("core:kiln/types"),
                    "expected core:kiln/types prefix, got `{interface_fq}`",
                );
                assert_eq!(cm_name, "raw-request");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }
}
