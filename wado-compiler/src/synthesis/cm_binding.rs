//! CM Binding Synthesis phase.
//!
//! Generates TIR binding functions for Component Model boundary crossing.
//! Each binding handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures binding functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-binding-synthesis.md` for design details.

mod export_adapter;
mod import_adapter;
mod lift;
mod lower;
mod resource_rewrite;
mod task_return;
mod type_fixup;
mod types;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::package::Package;
use crate::tir::{ResolvedType, TirFunction, TirModule, TypeTable};

pub use export_adapter::export_binding_func_name;
use export_adapter::{
    synthesize_async_export_binding, synthesize_general_export_binding,
    synthesize_result_export_binding, synthesize_void_export_binding, synthesize_void_stub_adapter,
};
pub use import_adapter::binding_func_name;
use import_adapter::synthesize_adapter;
pub use lift::synthesize_lift;
pub use lower::synthesize_lower;
use resource_rewrite::{rewrite_cm_resource_methods, synthesize_record_stream_reads};
use task_return::{expand_task_returns_in_func, strip_task_returns_in_func};
use type_fixup::{
    collect_effect_calls_in_block, collect_local_type_updates, rewrite_calls_in_block,
};
pub use types::{
    LiftContext, cm_enum_byte_size, cm_flags_byte_align, cm_flags_byte_size, flatten_param_type,
    wasi_type_to_type_id,
};
use types::{cm_val_type_to_type_id, compute_export_flat_return_types};

/// Build a `(module_source, name)` set for every effect/resource declared in
/// the loaded TIR modules. The CM binding synthesizer uses this to attach the
/// owning effect to each generated binding using the same `module_source` the
/// elaborator assigns to user-written `with E` clauses.
///
/// Keying by `(module_source, name)` (rather than name alone) prevents
/// collisions when two modules declare an effect or resource with the same
/// name — `lookup_effect_owner` selects the canonical WASI module.
fn effect_owner_module_sources(
    modules: &IndexMap<ModuleSource, TirModule>,
) -> IndexMap<(ModuleSource, String), ()> {
    let mut out: IndexMap<(ModuleSource, String), ()> = IndexMap::default();
    for (module_source, module) in modules {
        for effect in &module.effects {
            out.insert((module_source.clone(), effect.name.clone()), ());
        }
        for resource in &module.resources {
            out.insert((module_source.clone(), resource.name.clone()), ());
        }
    }
    out
}

/// Look up the canonical owning module for an effect/resource named `name`
/// whose binding targets WASI `package` (e.g. `"cli"`).
///
/// Preferred match: a `ModuleSource::Wasi { interface }` whose interface starts
/// with `"{package}/"` (e.g. `wasi:cli/stdio.wado` for package `"cli"`).
/// Falls back to any other owner with the same name if no WASI match exists.
fn lookup_effect_owner(
    owners: &IndexMap<(ModuleSource, String), ()>,
    name: &str,
    package: &str,
) -> Option<ModuleSource> {
    let wasi_prefix = format!("{package}/");
    let mut fallback: Option<ModuleSource> = None;
    for ((ms, n), ()) in owners {
        if n != name {
            continue;
        }
        if let ModuleSource::Wasi { interface } = ms
            && interface.starts_with(&wasi_prefix)
        {
            return Some(ms.clone());
        }
        if fallback.is_none() {
            fallback = Some(ms.clone());
        }
    }
    fallback
}

/// Phase entry point: generate CM binding functions and rewrite call sites.
///
/// For each WASI import function used in the program:
/// 1. Synthesizes a binding TIR function that handles CM boundary crossing
/// 2. Rewrites effect-like `Call` nodes to target the binding function
///
/// For each world export function:
/// 3. Synthesizes an export binding that wraps the user function with task-return
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Package) -> Result<Package, String> {
    let entry_source = project.entry_module_source.clone();

    // ---- Import adapters ----

    // Step 1: Collect all used WASI effect calls and resource method calls
    let mut seen_effects: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut seen_effects, project.wasi_registry);
            }
        }
    }

    if !seen_effects.is_empty() {
        // Step 2: Synthesize binding functions for each used WASI function
        let entry_type_table = project
            .tir_modules
            .get(&project.entry_module_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));
        // Map effect/resource name → defining module source. Used to attach the
        // canonical owner as an effect on each generated binding so the
        // checker's `(module_source, name)` identity matches user-written
        // `with E` clauses (which the elaborator also canonicalises to the
        // defining module).
        let owner_sources = effect_owner_module_sources(&project.tir_modules);
        let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        // Auxiliary functions returned alongside an adapter (e.g. the
        // per-import `__cm_lift__*` for async imports). Not used for
        // call-site rewriting, but added to the entry module so they
        // participate in monomorphize / lower / DCE like normal functions.
        let mut auxiliary_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        for qualified_name in &seen_effects {
            if let Some(func_info) = project.wasi_registry.get_function(qualified_name) {
                let func_info = func_info.clone();
                let binding_name =
                    binding_func_name(&func_info.interface_name, &func_info.method_name);
                let owner_module = lookup_effect_owner(
                    &owner_sources,
                    &func_info.interface_name,
                    &func_info.package,
                )
                .unwrap_or_else(|| project.interner.borrow_mut().wasi(&func_info.package));
                let produced = synthesize_adapter(
                    &func_info,
                    project.wasi_registry,
                    &entry_type_table,
                    &project.interner,
                    &owner_module,
                    &entry_source,
                );
                auxiliary_functions.extend(produced.auxiliary);
                let adapter = produced.adapter;
                adapters.insert(qualified_name.clone(), adapter.clone());
                // Also index by binding function name for lookup
                adapters.insert(binding_name, adapter);
            }
        }

        // Step 3: Add binding functions (and their auxiliaries) to the entry module
        if let Some(entry_module) = project.tir_modules.get_mut(&entry_source) {
            for (key, adapter_rc) in &adapters {
                // Only add each adapter once (skip the duplicate keyed by binding_name)
                if key.contains("::") {
                    entry_module.functions.push(adapter_rc.clone());
                }
            }
            for aux in auxiliary_functions {
                entry_module.functions.push(aux);
            }
        }

        // Step 4: Rewrite effect-like call nodes to target adapters
        let adapter_map: IndexMap<String, Rc<RefCell<TirFunction>>> = adapters
            .iter()
            .filter(|(k, _)| k.contains("::"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for module in project.tir_modules.values() {
            for func_rc in &module.functions {
                let mut func = func_rc.borrow_mut();
                if let Some(body) = &mut func.body {
                    rewrite_calls_in_block(
                        body,
                        &adapter_map,
                        &entry_source,
                        project.wasi_registry,
                        &entry_type_table,
                    );
                }
                // Sync locals with any Let stmts that were updated by the rewrite
                // (e.g., streaming binding calls changing the let binding type to i32).
                if !func.locals.is_empty() {
                    let mut updates = Vec::new();
                    if let Some(body) = &func.body {
                        collect_local_type_updates(body, &func.locals, &mut updates);
                    }
                    for (idx, type_id) in updates {
                        func.locals[idx].type_id = type_id;
                    }
                }
            }
        }
    }

    // ---- Export adapters ----

    // Step 5: Synthesize export bindings for world exports (signature-driven)
    let world_info = project.world_registry.get(&project.target_world).cloned();
    if let Some(world_info) = world_info {
        let entry_type_table = project
            .tir_modules
            .get(&entry_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));

        // Collect adapters in a read-only pass (synthesize_result_export_binding needs &tir_modules)
        let mut export_adapters: Vec<(String, String, Rc<RefCell<TirFunction>>)> = Vec::new();
        {
            let entry_module = project
                .tir_modules
                .get(&entry_source)
                .expect("entry module should exist");

            // Package hint for CM name resolution inside export adapters.
            // For `wasi:http/service` this is `"http"`; for
            // `core:kiln/generator` it is `"kiln"`. The hint biases bare-name
            // resolution towards the binding's owning package (e.g.
            // `ErrorCode` in `wasi:http` bindings) and feeds
            // `resolve_cm_source_for` as a fallback anchor. Derived from
            // the world's `fq_name` — the attribute-sourced identity is
            // the single source of truth.
            let binding_cm_package = world_info.package().to_string();

            for export in &world_info.exports {
                // Find the user's export function and check for missing `export` keyword
                let mut found_exported = None;
                let mut found_without_export = false;
                for f in &entry_module.functions {
                    let func = f.borrow();
                    if func.name == export.name {
                        if func.is_export {
                            found_exported = Some(f.clone());
                        } else {
                            found_without_export = true;
                        }
                    }
                }

                if found_exported.is_none() && found_without_export {
                    return Err(format!(
                        "function `{}` exists but is not marked with `export` keyword. \
                         Add `export` to make it a world entry point: `export fn {}(...)`",
                        export.name, export.name
                    ));
                }

                let binding_name = export_binding_func_name(&export.name);
                let adapter = if let Some(user_func_rc) = found_exported {
                    // Validate parameter count matches world declaration
                    {
                        let user_func = user_func_rc.borrow();
                        if user_func.params.len() != export.params.len() {
                            return Err(format!(
                                "export function `{}` has {} parameter(s), \
                                 but the world expects {} parameter(s)",
                                export.name,
                                user_func.params.len(),
                                export.params.len()
                            ));
                        }
                    }

                    // Validate return-type compatibility with the world. The
                    // dispatch below routes a `Result<_, _>`-returning world
                    // export to dedicated adapters for the async, Result, and
                    // `()` user return shapes; any other user return type
                    // falls through to `synthesize_general_export_binding`,
                    // which lowers the user's return value directly into the
                    // world's task-return slots. When the world expects only
                    // a discriminant (`Result<(), ()>`, the wasi:cli/command
                    // shape) but the user supplies, say, `i32`, the general
                    // adapter emits an extra flat value beyond what the
                    // runtime declares for task-return — surfacing as an
                    // opaque "values remaining on stack" wasm-validation
                    // panic at codegen. Catch the mismatch here with a
                    // readable diagnostic instead.
                    {
                        let user_func = user_func_rc.borrow();
                        let tt = entry_type_table.borrow();
                        let user_is_unit =
                            matches!(tt.get(user_func.return_type), ResolvedType::Unit);
                        let result_name = tt
                            .compiler_items()
                            .variant_name(crate::compiler_item::CompilerItem::Result)
                            .to_string();
                        let user_is_result = matches!(
                            tt.get(user_func.return_type),
                            ResolvedType::GenericInstance { name, .. } if *name == result_name
                        );
                        let world_expects_result = matches!(
                            &export.return_type,
                            Some(crate::ast::Type::Generic(g)) if g.name == result_name
                        );
                        if world_expects_result
                            && !user_func.is_async
                            && !user_is_unit
                            && !user_is_result
                        {
                            let user_return_name = tt.type_name(user_func.return_type);
                            drop(tt);
                            return Err(format!(
                                "export function `{}` has return type `{user_return_name}`, \
                                 but the world expects a `{result_name}<_, _>` (or unit, \
                                 which is automatically wrapped as `{result_name}<(), _>`). \
                                 Change the signature to return a `{result_name}` or remove \
                                 the explicit return type.",
                                export.name
                            ));
                        }
                    }

                    // Check if user function is `export async fn`
                    let is_async_export = {
                        let user_func = user_func_rc.borrow();
                        user_func.is_async
                    };

                    if is_async_export {
                        // Async export: the user function calls task-return internally via
                        // `task return expr` stmts. Expand those stmts into CM task-return
                        // calls and synthesize a simple lifting adapter.
                        if let Some(return_type) = &export.return_type {
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                return_type,
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            expand_task_returns_in_func(
                                &user_func_rc,
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                                project.wasi_registry,
                                &binding_cm_package,
                                &project.interner,
                            );
                        }
                        synthesize_async_export_binding(
                            &export.name,
                            user_func_rc,
                            &entry_source,
                            &project.tir_modules,
                            &entry_type_table,
                            &export.params,
                            project.wasi_registry,
                            &binding_cm_package,
                            &project.interner,
                        )
                    } else {
                        // Check the user function's actual return type (signature-driven)
                        let user_returns_result = {
                            let user_func = user_func_rc.borrow();
                            let tt = entry_type_table.borrow();
                            let result_name = tt
                                .compiler_items()
                                .variant_name(crate::compiler_item::CompilerItem::Result)
                                .to_string();
                            matches!(
                                tt.get(user_func.return_type),
                                ResolvedType::GenericInstance { name, .. }
                                    if *name == result_name
                            )
                        };

                        if user_returns_result {
                            // Result<T, E> return: full lowering adapter (signature-driven)
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                export.return_type.as_ref().unwrap(),
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            synthesize_result_export_binding(
                                &export.name,
                                user_func_rc,
                                &entry_source,
                                export.return_type.as_ref().unwrap(),
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                                &export.params,
                                project.wasi_registry,
                                &binding_cm_package,
                                &project.interner,
                            )
                        } else {
                            // Non-Result return: check if we can use the simple void adapter
                            // (only when no params AND unit return type)
                            let is_void_no_params = export.params.is_empty() && {
                                let user_func = user_func_rc.borrow();
                                let tt = entry_type_table.borrow();
                                matches!(tt.get(user_func.return_type), ResolvedType::Unit)
                            };

                            if is_void_no_params {
                                // Simple void adapter for () -> ()
                                synthesize_void_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                )
                            } else {
                                // General adapter: handles params (with lifting if needed)
                                // and non-void return types
                                synthesize_general_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                    &project.tir_modules,
                                    &entry_type_table,
                                    &export.params,
                                    project.wasi_registry,
                                    &binding_cm_package,
                                    &project.interner,
                                )
                            }
                        }
                    }
                } else {
                    // No user function: stub that just calls task-return(0)
                    synthesize_void_stub_adapter(&export.name)
                };
                export_adapters.push((export.name.clone(), binding_name, adapter));
            }
        }

        // Compute the correct task-return params from the export's flat return types.
        // The builtin registry defines task_return with a single i32 param, but for
        // Result-returning exports the task-return call passes the full flattened type.
        // Store on Package so optimize_dce can use it when creating the import.
        for export in &world_info.exports {
            if let Some(return_type) = &export.return_type {
                let tt = entry_type_table.borrow();
                let flat_types =
                    compute_export_flat_return_types(return_type, &project.tir_modules, &tt);
                project.task_return_flat_params = Some(
                    flat_types
                        .iter()
                        .map(|&vt| cm_val_type_to_type_id(vt))
                        .collect(),
                );
                break; // One export is enough — all share the same task-return
            }
        }

        // Push adapters with mutable access
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");
        for (export_name, binding_name, adapter) in export_adapters {
            project
                .export_binding_names
                .insert(export_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Step 6: Synthesize export bindings for test functions (__test_*)
    // Only when targeting the test world — in other worlds, tests are dead code.
    if project.is_test_world() {
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");

        // Collect test functions first to avoid borrow conflict.
        // Test functions have is_export=false (they're not world exports),
        // but they need adapters for task-return when called via `wado test`.
        let test_funcs: Vec<(String, Rc<RefCell<TirFunction>>)> = entry_module
            .functions
            .iter()
            .filter(|f| f.borrow().name.starts_with("__test_"))
            .map(|f| (f.borrow().name.clone(), f.clone()))
            .collect();

        for (test_name, user_func_rc) in test_funcs {
            let binding_name = export_binding_func_name(&test_name);
            let adapter = synthesize_void_export_binding(&test_name, user_func_rc, &entry_source);
            project.export_binding_names.insert(test_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Strip remaining TaskReturn from all modules.
    // `task return` is only valid inside `async fn` (checked by elaborator).
    // Step 5 expands TaskReturn into CM calls for async exports that match the
    // target world. Any remaining async fn (unmatched exports, imported modules)
    // will be DCE'd — strip their TaskReturn stmts so they don't reach monomorphize.
    // This is idempotent: already-expanded functions have no TaskReturn stmts left.
    for module in project.tir_modules.values() {
        for f in &module.functions {
            let needs_strip = {
                let func = f.borrow();
                func.is_async
            };
            if needs_strip {
                strip_task_returns_in_func(f);
            }
        }
    }

    // ---- Record Stream Read Adapters ----
    // Generate binding functions for Stream<T>.read() where T is a WASI record type.
    // Must run before rewrite_cm_resource_methods so the generated functions are available.
    synthesize_record_stream_reads(&mut project);

    // ---- CM Resource Method Adapters ----
    // Rewrite #[cm("...")] resource method calls to target internal/builtin binding functions.
    // This replaces the inline WIR emission in wir_build/translate.rs with pre-monomorphization
    // synthesis, so the binding functions go through the normal compilation pipeline.
    rewrite_cm_resource_methods(&mut project);

    Ok(project)
}

#[cfg(test)]
mod tests {
    use crate::module_source::ModuleSourceInterner;

    use super::export_adapter::synthesize_lift_from_flat_params;
    use super::types::{
        CmStdlibNames, compute_export_flat_param_types, export_needs_param_lifting,
        param_needs_lifting,
    };
    use super::*;
    use crate::ast::{NamedType, Type};
    use crate::cm_abi;
    use crate::component_model::WasiRegistry;
    use crate::synthesis::common::{
        builtin_call, cm_raw_call, i32_const, i64_const, internal_call, let_stmt, synth_span,
    };
    use crate::tir::{TirExpr, TirExprKind, TirStmtKind, TypeId};

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: synth_span(),
            source_interface: None,
        })
    }

    /// Register the `Option`, `Result`, `String`, and `Array` compiler
    /// items against the relevant prelude modules so `make_option` /
    /// `make_result` and the type-identity reads inside `lift` /
    /// `cm_binding` succeed in unit tests. Production resolution wires
    /// these up when the stdlib elaborator visits `core:prelude`.
    fn register_option_result_for_tests(tt: &mut TypeTable) {
        use crate::compiler_item::{CompilerItem, Resolved};
        let _ = tt.compiler_items_mut().register(
            CompilerItem::Option,
            Resolved::Variant {
                module_source: ModuleSource::prelude(),
                name: "Option".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::OptionSome,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Option".to_string(),
                name: "Some".to_string(),
                case_index: 0,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::OptionNone,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Option".to_string(),
                name: "None".to_string(),
                case_index: 1,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::Result,
            Resolved::Variant {
                module_source: ModuleSource::prelude(),
                name: "Result".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::ResultOk,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Result".to_string(),
                name: "Ok".to_string(),
                case_index: 0,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::ResultErr,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Result".to_string(),
                name: "Err".to_string(),
                case_index: 1,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::String,
            Resolved::Struct {
                module_source: ModuleSource::string(),
                name: "String".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::Array,
            Resolved::Struct {
                module_source: ModuleSource::array(),
                name: "Array".to_string(),
            },
        );
    }

    /// Test fixture: empty registry + fresh type table + empty interner.
    /// Mirrors the production `LiftContext` shape so the lift code paths
    /// run end-to-end in unit tests.
    struct LiftCtxFixture {
        registry: WasiRegistry,
        type_table: std::cell::RefCell<TypeTable>,
        interner: std::cell::RefCell<ModuleSourceInterner>,
    }

    impl LiftCtxFixture {
        fn new() -> Self {
            // Register the Option/Result compiler-items so `make_option` /
            // `make_result` succeed during unit-test lifts. Production
            // gets these registered when the stdlib elaborator visits
            // `core:prelude`.
            let mut tt = TypeTable::new();
            register_option_result_for_tests(&mut tt);
            Self {
                registry: WasiRegistry::new(),
                type_table: std::cell::RefCell::new(tt),
                interner: std::cell::RefCell::new(ModuleSourceInterner::new()),
            }
        }

        fn ctx(&self) -> LiftContext<'_> {
            LiftContext {
                wasi_registry: &self.registry,
                type_table: &self.type_table,
                cm_package: "",
                interner: &self.interner,
            }
        }
    }

    #[test]
    fn flatten_param_i32() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i32"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_i64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i64"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn flatten_param_f64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("f64"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::F64]
        );
    }

    #[test]
    fn flatten_param_string() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("String"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("bool"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        let reg = WasiRegistry::new();
        assert!(
            flatten_param_type(&Type::Tuple(vec![]), &reg, &CmStdlibNames::for_tests()).is_empty()
        );
    }

    #[test]
    fn flatten_param_newtype_u64() {
        let (reg, _) = WasiRegistry::build_from_stdlib();
        assert_eq!(
            flatten_param_type(&named_type("Duration"), reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
        assert_eq!(
            flatten_param_type(&named_type("Mark"), reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn binding_name() {
        assert_eq!(
            binding_func_name("Stdout", "write_via_stream"),
            "__cm_binding__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("i32"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
        assert!(stmts.is_empty()); // primitives need no setup
    }

    #[test]
    fn lift_bool() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("bool"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_string() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("String"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_list_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let list_ty = cm_abi::generic_type("Array", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &list_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        // Should produce setup stmts and return a local ref
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 5); // base, count, result, i, elem_addr
    }

    #[test]
    fn lift_option_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let opt_ty = cm_abi::generic_type("Option", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &opt_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 2); // disc, result
    }

    #[test]
    fn lift_result_unit_unit() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let result_ty =
            cm_abi::generic_type("Result", vec![Type::Tuple(vec![]), Type::Tuple(vec![])]);
        let expr = synthesize_lift(
            &result_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
    }

    #[test]
    fn lift_resource_handle() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let own_ty = cm_abi::generic_type("Own", vec![named_type("Fields")]);
        let expr = synthesize_lift(
            &own_ty,
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    /// `Array<IpAddress>` lifts must walk the buffer at the variant's
    /// canonical-ABI stride (1 byte disc + payload), not the 4-byte
    /// i32-handle fallback. Regression for issue #997 (#1, #2).
    #[test]
    fn lift_list_named_variant_uses_registry_stride() {
        use crate::tir::TirStmt;

        // Visit every TIR sub-expression and count `IntLiteral { value: target }`
        // occurrences. Walks through the structural recursion sites that
        // `synthesize_lift_list` emits (Let init, If condition, Loop body,
        // realloc args). Built as a semantic check so it doesn't depend on
        // any `Debug` output formatting.
        fn count_int_literals(stmts: &[TirStmt], target: u64) -> usize {
            fn visit_expr(e: &TirExpr, target: u64) -> usize {
                let mut n = match &e.kind {
                    TirExprKind::IntLiteral { value, .. } if *value == target => 1,
                    _ => 0,
                };
                match &e.kind {
                    TirExprKind::Binary { left, right, .. } => {
                        n += visit_expr(left, target) + visit_expr(right, target);
                    }
                    TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                        for a in args {
                            n += visit_expr(&a.expr, target);
                        }
                    }
                    TirExprKind::Unary { expr: inner, .. }
                    | TirExprKind::Cast { expr: inner, .. } => n += visit_expr(inner, target),
                    TirExprKind::Assign { target: t, value } => {
                        n += visit_expr(t, target) + visit_expr(value, target);
                    }
                    _ => {}
                }
                n
            }
            fn visit_stmt(s: &TirStmt, target: u64) -> usize {
                match &s.kind {
                    TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                        visit_expr(value, target)
                    }
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    } => {
                        let mut n = visit_expr(condition, target)
                            + visit_block(then_block.stmts.as_slice(), target);
                        if let Some(b) = else_block {
                            n += visit_block(b.stmts.as_slice(), target);
                        }
                        n
                    }
                    TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                        visit_block(body.stmts.as_slice(), target)
                    }
                    _ => 0,
                }
            }
            fn visit_block(stmts: &[TirStmt], target: u64) -> usize {
                stmts.iter().map(|s| visit_stmt(s, target)).sum()
            }
            visit_block(stmts, target)
        }

        let (registry, _) = WasiRegistry::build_from_stdlib();
        let elem_ty = named_type("IpAddress");
        let expected_size = u64::from(crate::component_model::cm_size_with_registry_scoped(
            &elem_ty,
            registry,
            Some("sockets"),
        ));
        // Sanity: registry-derived size differs from the 4-byte fallback.
        assert!(expected_size > 4, "registry size should exceed handle size");

        let mut tt = TypeTable::new();
        register_option_result_for_tests(&mut tt);
        let type_table = std::cell::RefCell::new(tt);
        let interner = std::cell::RefCell::new(ModuleSourceInterner::new());
        let ctx = LiftContext {
            wasi_registry: registry,
            type_table: &type_table,
            cm_package: "sockets",
            interner: &interner,
        };
        let list_ty = cm_abi::generic_type("Array", vec![elem_ty]);
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let _ = synthesize_lift(
            &list_ty,
            i32_const(0),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &ctx,
        );
        // Stride appears at element-addr offset (`i * elem_size`) and in
        // the realloc free (`count * elem_size`).
        let occurrences = count_int_literals(&stmts, expected_size);
        assert!(
            occurrences >= 2,
            "expected ≥2 `IntLiteral {{ value: {expected_size} }}` occurrences, got {occurrences}"
        );
    }

    #[test]
    fn lower_i32() {
        let stmts = synthesize_lower(
            &named_type("i32"),
            i32_const(42),
            i32_const(100),
            &mut 0,
            &mut vec![],
            &CmStdlibNames::for_tests(),
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(
            &named_type("bool"),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
            &CmStdlibNames::for_tests(),
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(
            &Type::Tuple(vec![]),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
            &CmStdlibNames::for_tests(),
        );
        assert!(stmts.is_empty());
    }

    #[test]
    fn lower_string() {
        let value = TirExpr::new(
            TirExprKind::StringLiteral("hello".to_string()),
            TypeTable::I32, // placeholder
            synth_span(),
        );
        let mut next_local = 10_u32;
        let stmts = synthesize_lower(
            &named_type("String"),
            value,
            i32_const(100),
            &mut next_local,
            &mut vec![],
            &CmStdlibNames::for_tests(),
        );
        // Should produce: let __packed = cm_lower_string(value); store ptr; store len
        assert_eq!(stmts.len(), 3);
        assert_eq!(next_local, 11); // one local allocated for __packed
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ---- Parameter lifting tests ----
    //
    // These exercise the TypeTable-driven classification in
    // `param_needs_lifting`. Each test constructs the minimum TIR shape
    // needed to reach a specific `ResolvedType` arm.

    fn mk_param(type_id: TypeId) -> crate::tir::TirParam {
        crate::tir::TirParam {
            name: String::new(),
            type_id,
            local_index: 0,
            is_mut: false,
            default_expr: None,
            span: crate::token::Span::new(0, 0, 0, 0),
        }
    }

    #[test]
    fn param_needs_lifting_primitives_passthrough() {
        let tt = TypeTable::new();
        // All Wasm-native primitive shapes flow through unchanged.
        assert!(!param_needs_lifting(TypeTable::I32, &tt));
        assert!(!param_needs_lifting(TypeTable::I64, &tt));
        assert!(!param_needs_lifting(TypeTable::F32, &tt));
        assert!(!param_needs_lifting(TypeTable::F64, &tt));
        assert!(!param_needs_lifting(TypeTable::U8, &tt));
        assert!(!param_needs_lifting(TypeTable::U16, &tt));
        assert!(!param_needs_lifting(TypeTable::CHAR, &tt));
    }

    #[test]
    fn param_needs_lifting_bool_lifts() {
        // `bool` needs a 0/!=0 widening at the CM boundary, so it
        // counts as needing a lift step.
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::BOOL, &tt));
    }

    #[test]
    fn param_needs_lifting_unit_lifts() {
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::UNIT, &tt));
    }

    #[test]
    fn param_needs_lifting_string() {
        let mut tt = TypeTable::new();
        let s = tt.make_struct("String".to_string(), ModuleSource::string());
        assert!(param_needs_lifting(s, &tt));
    }

    #[test]
    fn param_needs_lifting_resource() {
        // Resources are i32 handles — no lift.
        let mut tt = TypeTable::new();
        let r = tt.intern(crate::tir::ResolvedType::Resource {
            name: "Request".to_string(),
            module_source: ModuleSource::wasi_http(),
        });
        assert!(!param_needs_lifting(r, &tt));
    }

    #[test]
    fn param_needs_lifting_enum() {
        let mut tt = TypeTable::new();
        let mut interner = ModuleSourceInterner::new();
        let e = tt.intern(crate::tir::ResolvedType::Enum {
            name: "Color".to_string(),
            module_source: interner.entry_point("<test>"),
        });
        assert!(!param_needs_lifting(e, &tt));
    }

    #[test]
    fn param_needs_lifting_option() {
        // Option<T> is a GenericInstance under the hood; build it directly
        // (avoids `make_option`'s dependency on comp-feature registration,
        // which isn't present in a bare `TypeTable::new()`).
        let mut tt = TypeTable::new();
        let opt = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "Option".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(opt, &tt));
    }

    #[test]
    fn param_needs_lifting_array() {
        let mut tt = TypeTable::new();
        let arr = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "Array".to_string(),
            module_source: ModuleSource::prelude(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(arr, &tt));
    }

    #[test]
    fn export_needs_lifting_empty() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        assert!(!export_needs_param_lifting(&[], &tt));
    }

    #[test]
    fn export_needs_lifting_primitives_only() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        let params = vec![mk_param(TypeTable::I32), mk_param(TypeTable::F64)];
        assert!(!export_needs_param_lifting(&params, &tt));
    }

    #[test]
    fn export_needs_lifting_with_string() {
        let tt_cell = std::cell::RefCell::new(TypeTable::new());
        let string_id = tt_cell
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::string());
        let params = vec![mk_param(string_id)];
        assert!(export_needs_param_lifting(&params, &tt_cell));
    }

    #[test]
    fn compute_flat_params_empty() {
        let params: Vec<(String, Type)> = vec![];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert!(flat.is_empty());
    }

    #[test]
    fn compute_flat_params_primitives() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("b".to_string(), named_type("f64")),
        ];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::F64]);
    }

    #[test]
    fn compute_flat_params_string() {
        let params = vec![("name".to_string(), named_type("String"))];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::I32]);
    }

    #[test]
    fn compute_flat_params_mixed() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("name".to_string(), named_type("String")),
            ("b".to_string(), named_type("f32")),
        ];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(
            flat,
            vec![
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::F32,
            ]
        );
    }

    // ---- Lift from flat params tests ----

    #[test]
    fn lift_from_flat_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("i32"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &fix.type_table,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_from_flat_string() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 2_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("String"),
            &[0, 1],
            &[cm_abi::CmValType::I32, cm_abi::CmValType::I32],
            TypeTable::I32, // placeholder
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &fix.type_table,
            fix.ctx(),
        );
        assert_eq!(consumed, 2);
        // Should be a call to memory_to_gc_string
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_from_flat_bool() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("bool"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::BOOL,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &fix.type_table,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_from_flat_unit() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &Type::Tuple(vec![]),
            &[],
            &[],
            TypeTable::UNIT,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &fix.type_table,
            fix.ctx(),
        );
        assert_eq!(consumed, 0);
        assert!(matches!(expr.kind, TirExprKind::Unit));
    }

    #[test]
    fn lift_from_flat_resource() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("Request"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &fix.type_table,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }
}
