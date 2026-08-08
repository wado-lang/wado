//! Function collection — gathers all reachable functions from the `NirPackage`,
//! registers their types and creates `WirFunction` stubs (bodies filled later).

use crate::const_eval::{Value, eval_binary, eval_cast, eval_unary, is_f32_type, prim_of};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::MangledName;
use crate::name::global_name;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprKind, Operand};
use crate::nir_value_graph::ValueKind;
use crate::tir::{PrimitiveType, TypeTable};
use crate::wir::{
    CanonicalIntrinsic, WirFunction, WirGlobal, WirImport, WirImportDesc, WirMeta, WirName, WirType,
};

use super::context::{PendingFunctionBody, WirContext};

/// Collect all functions from the `NirPackage`, register imports, and create function stubs.
pub fn collect_functions(ctx: &mut WirContext<'_>) {
    // `FuncId == position` holds end-to-end: `lower` mints `id = index`, `dce`
    // marks dead in place without renumbering (Phase 4), and synthesized
    // functions append at `id = next_func_id() = len`. Phase 5's `store[id]`
    // descriptor reads depend on this; the check is O(n), once.
    {
        use cranelift_entity::EntityRef;
        for (i, func_rc) in ctx.package.functions.iter().enumerate() {
            assert_eq!(
                func_rc.borrow().id,
                Some(crate::nir::FuncId::new(i)),
                "FuncId must equal store position at codegen (function #{i})"
            );
        }
    }

    // Step 1: Register builtin + bundled imports
    register_imports(ctx);

    // Step 2: Register WASI function imports
    register_wasi_imports(ctx);

    // Step 2.5: Register memory import from "mem" module
    register_memory_import(ctx);

    // Step 2.7: Register global variables from all modules
    register_globals(ctx);

    // Step 3: Collect and register entry module functions
    register_entry_functions(ctx);

    // Step 4: Collect and register loaded module functions
    register_loaded_functions(ctx);

    // Step 5: Collect and register methods from all modules
    register_methods(ctx);

    // Step 6: Register data segments for string and bytes literals
    register_literal_data(ctx);

    // Step 7: Register exports
    register_exports(ctx);
}

/// Register builtin and bundled function imports.
fn register_imports(ctx: &mut WirContext<'_>) {
    let type_table = &*ctx.package.type_table.borrow();

    for import in &ctx.package.imports {
        // Register the function type
        let params: Vec<WirType> = import
            .params
            .iter()
            .map(|&p| ctx.type_id_to_wir_type(type_table, p))
            .collect();
        let results: Vec<WirType> =
            if import.return_type == TypeTable::UNIT || import.return_type == TypeTable::NEVER {
                Vec::new()
            } else {
                vec![ctx.type_id_to_wir_type(type_table, import.return_type)]
            };

        let type_fq = crate::name::wir_func_type_key(&format!(
            "{}/{}",
            import.namespace, import.canonical_name
        ));
        let type_id = ctx.register_func_type(type_fq, params, results);

        let name = WirName {
            fq: format!("{}/{}", import.namespace, import.canonical_name),
        };

        let func_id = ctx.register_import_func(
            import.namespace.clone(),
            import.canonical_name.clone(),
            type_id,
            name,
        );

        // Track "wasi" namespace imports as needed canonical intrinsics.
        // This ensures they appear in WirPackage::needed_canonicals, which is
        // the single source of truth for component codegen.
        if import.namespace == "wasi"
            && let Some(intrinsic) = CanonicalIntrinsic::from_import_name(&import.canonical_name)
        {
            // Future-related canonicals with default Trailers payload are not tracked here.
            // They are registered with the correct CmFuturePayload during WIR translation
            // via CM method dispatch (cm_future_payload), which has access to the actual
            // Future<T> type parameter. Tracking them with the wrong payload would force
            // unnecessary HTTP type definitions.
            if intrinsic.future_payload().is_none() {
                ctx.needed_canonicals.insert(intrinsic, func_id.clone());
            }
        }

        // Also register under the TIR builtin function name so call sites can resolve.
        // e.g., "builtin/realloc" → same WirFuncId as "mem/realloc"
        if !import.func_name.is_empty() {
            let alias = MangledName::builtin_alias(&import.func_name);
            ctx.func_map.insert(alias, func_id);
        }
    }
}

/// Register WASI function imports.
///
/// WASI functions are already lowered at the component level;
/// the core module imports them from the "wasi" namespace.
/// Uses `flatten_cm_param_type` / `cm_return_needs_outptr` for CM ABI type flattening.
fn register_wasi_imports(ctx: &mut WirContext<'_>) {
    let cm_interface_registry = &ctx.package.cm_interface_registry;

    for interface_info in cm_interface_registry.interfaces() {
        for func in &interface_info.functions {
            let local_name = func.local_alias_name();

            // Only register functions that are actually used (per-function check).
            // This is more precise than has_interface() which includes ALL functions
            // for a used effect. Per-function filtering avoids importing unused
            // WASI functions that the component builder doesn't support (e.g.,
            // [static] HTTP functions like consume_body).
            let wasi_func_key = format!("{}::{}", func.interface_name, func.method_name);
            if !ctx.package.used_wasi_functions.contains(&wasi_func_key) {
                continue;
            }

            if !cm_interface_registry.is_function_supported(func) {
                continue;
            }

            // Async functions with void return are supported (e.g., MonotonicClock::wait_for).
            // The adapter handles them by calling the lowered function, waiting for the subtask,
            // and freeing the async results buffer.

            // Build param types using CM ABI type flattening.
            // WASI P3 async functions with > MAX_FLAT_ASYNC_PARAMS (4) flat params use
            // indirect calling: all params are passed via a single params_ptr (i32) plus
            // a results_ptr (i32). This matches what `canon lower async` produces.
            const MAX_FLAT_ASYNC_PARAMS: usize = 4;
            let mut param_vts: Vec<wasm_encoder::ValType> = Vec::new();
            for (_, _, ty) in &func.params {
                let resolved_ty = cm_interface_registry.resolve_type(ty);
                crate::component_model::flatten_cm_param_type(
                    &resolved_ty,
                    &mut param_vts,
                    cm_interface_registry,
                );
            }

            // Async functions: per CM spec flatten_functype('lower'), the outptr (i32) is
            // only appended when len(flat_results) > 0 (i.e., when there is a return type).
            // Async void functions (e.g., wait_for, wait_until) have no results and no outptr.
            // Only truly async imports use canon lower async.
            let needs_async_lower = func.is_async;
            if needs_async_lower {
                let has_results = func.return_type.is_some();
                if param_vts.len() > MAX_FLAT_ASYNC_PARAMS {
                    // Indirect convention: collapse all params to a single params_ptr.
                    // Add outptr only if there are results.
                    if has_results {
                        param_vts = vec![wasm_encoder::ValType::I32, wasm_encoder::ValType::I32];
                    } else {
                        param_vts = vec![wasm_encoder::ValType::I32];
                    }
                } else if has_results {
                    // Direct convention: params passed directly + outptr.
                    param_vts.push(wasm_encoder::ValType::I32);
                }
            }
            // Sync functions with complex return types also need an outptr
            else if let Some(ret_ty) = &func.return_type {
                let resolved_ret_ty = cm_interface_registry.resolve_type(ret_ty);
                if crate::component_model::cm_return_needs_outptr(
                    &resolved_ret_ty,
                    cm_interface_registry,
                ) {
                    param_vts.push(wasm_encoder::ValType::I32);
                }
            }

            let params: Vec<WirType> = param_vts.into_iter().map(valtype_to_wir_type).collect();

            // Build result types using CM ABI type flattening
            let results: Vec<WirType> = if needs_async_lower {
                // Async/streaming functions always return i32 (subtask handle)
                vec![WirType::I32]
            } else if let Some(ret_ty) = &func.return_type {
                let resolved_ret_ty = cm_interface_registry.resolve_type(ret_ty);
                if crate::component_model::cm_return_needs_outptr(
                    &resolved_ret_ty,
                    cm_interface_registry,
                ) {
                    // Complex return via outptr — function returns nothing
                    Vec::new()
                } else {
                    // Simple return — flatten the return type
                    let mut out = Vec::new();
                    crate::component_model::flatten_cm_param_type(
                        &resolved_ret_ty,
                        &mut out,
                        cm_interface_registry,
                    );
                    out.into_iter().map(valtype_to_wir_type).collect()
                }
            } else {
                Vec::new()
            };

            let type_fq = format!("functype//wasi/{local_name}");
            let type_id = ctx.register_func_type(type_fq, params, results);

            let name = WirName {
                fq: format!("wasi/{local_name}"),
            };

            ctx.register_import_func("wasi".to_string(), local_name.clone(), type_id, name);

            ctx.available_wasi_funcs.insert(local_name);
        }
    }

    // World-level function imports (Phase 9): register the raw core import like
    // a sync interface method so the synthesized adapter's `CmRawCall` resolves.
    let world_funcs: Vec<crate::component_model::CmFunctionInfo> = cm_interface_registry
        .world_import_functions()
        .map(|(_, f)| f.clone())
        .collect();
    for func in &world_funcs {
        let local_name = func.local_alias_name();

        let mut param_vts: Vec<wasm_encoder::ValType> = Vec::new();
        for (_, _, ty) in &func.params {
            let resolved_ty = cm_interface_registry.resolve_type(ty);
            crate::component_model::flatten_cm_param_type(
                &resolved_ty,
                &mut param_vts,
                cm_interface_registry,
            );
        }
        let results: Vec<WirType> = if let Some(ret_ty) = &func.return_type {
            let resolved_ret_ty = cm_interface_registry.resolve_type(ret_ty);
            if crate::component_model::cm_return_needs_outptr(
                &resolved_ret_ty,
                cm_interface_registry,
            ) {
                param_vts.push(wasm_encoder::ValType::I32);
                Vec::new()
            } else {
                let mut out = Vec::new();
                crate::component_model::flatten_cm_param_type(
                    &resolved_ret_ty,
                    &mut out,
                    cm_interface_registry,
                );
                out.into_iter().map(valtype_to_wir_type).collect()
            }
        } else {
            Vec::new()
        };

        let params: Vec<WirType> = param_vts.into_iter().map(valtype_to_wir_type).collect();
        let type_fq = format!("functype//wasi/{local_name}");
        let type_id = ctx.register_func_type(type_fq, params, results);
        let name = WirName {
            fq: format!("wasi/{local_name}"),
        };
        ctx.register_import_func("wasi".to_string(), local_name.clone(), type_id, name);
        ctx.available_wasi_funcs.insert(local_name);
    }
}

/// Register memory import from "mem" module.
///
/// The core module imports memory and realloc from the "mem" instance,
/// which is provided by the component model wrapper.
fn register_memory_import(ctx: &mut WirContext<'_>) {
    ctx.imports.push(WirImport {
        module: "mem".to_string(),
        field: "memory".to_string(),
        desc: WirImportDesc::Memory { min: 1, max: None },
    });
}

/// Register entry module free functions.
fn register_entry_functions(ctx: &mut WirContext<'_>) {
    let type_table_rc = ctx.package.type_table.clone();
    let type_table = &*type_table_rc.borrow();
    let entry_source = &ctx.package.entry_module_source;

    for func_rc in &ctx.package.functions {
        let tir_func = func_rc.borrow();
        let module_source = &tir_func.module_source;
        if module_source != entry_source {
            continue;
        }

        // Skip bodyless functions
        if tir_func.body.is_none() {
            continue;
        }

        // Skip methods (handled in register_methods, which selects on the same
        // `method_info` field — so the two registrars partition functions).
        if tir_func.method_info.is_some() {
            continue;
        }

        // Skip generic template functions (effect-only params don't count)
        if tir_func.has_real_type_params() && tir_func.monomorph_info.is_none() {
            continue;
        }

        register_single_function(
            ctx,
            &tir_func,
            type_table,
            module_source,
            func_rc.clone(),
            type_table_rc.clone(),
        );
    }
}

/// Register loaded module functions (core:*, etc.).
fn register_loaded_functions(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.package.entry_module_source;
    let type_table_rc = ctx.package.type_table.clone();
    let type_table = &*type_table_rc.borrow();

    for func_rc in &ctx.package.functions {
        let tir_func = func_rc.borrow();
        let module_source = &tir_func.module_source;
        // The entry module is handled by `register_entry_functions`. A wasi
        // module contributes only bodyless CM imports from source, filtered by
        // the `body.is_none()` check below — but the reflect derivation
        // synthesizes body-carrying free-function helpers (`$field_get$…`) homed
        // in a wasi CM record's own module (e.g. `wasi:clocks#Instant`). Those
        // must register here, mirroring how `register_methods` (no wasi filter)
        // already emits a wasi CM record's synthesized `Eq`/`Ord` methods.
        if module_source == entry_source {
            continue;
        }

        // Methods are registered by `register_methods` (same `method_info`
        // selector), so they partition cleanly against the free functions here.
        if tir_func.name == "run" || tir_func.body.is_none() || tir_func.method_info.is_some() {
            continue;
        }

        // Skip generic template functions
        if !tir_func.type_params.is_empty() && tir_func.monomorph_info.is_none() {
            continue;
        }

        register_single_function(
            ctx,
            &tir_func,
            type_table,
            module_source,
            func_rc.clone(),
            type_table_rc.clone(),
        );
    }
}

/// Register methods from all modules.
fn register_methods(ctx: &mut WirContext<'_>) {
    let type_table_rc = ctx.package.type_table.clone();
    let type_table = &*type_table_rc.borrow();

    for func_rc in &ctx.package.functions {
        let tir_func = func_rc.borrow();
        let module_source = &tir_func.module_source;

        // Skip dead functions. A dead `FnCanonicalDispatch` is bodyless yet would
        // otherwise pass the `body.is_none()` exception below; `is_dead`
        // distinguishes it from a live bodyless dispatch (Phase 4).
        if tir_func.is_dead {
            continue;
        }

        // Only methods
        if tir_func.method_info.is_none() {
            continue;
        }

        // Bodyless methods are normally external/CM-import declarations and
        // get skipped here. The exception is `FnCanonicalDispatch`: WIR build
        // supplies its body in `translate_function_bodies`, so the entry must
        // still be registered so call sites resolve.
        if tir_func.body.is_none() && tir_func.fn_canonical_dispatch().is_none() {
            continue;
        }

        // Skip generic template methods
        if type_table.contains_type_param(tir_func.return_type)
            || tir_func
                .params
                .iter()
                .any(|p| type_table.contains_type_param(p.type_id))
        {
            continue;
        }

        register_single_function(
            ctx,
            &tir_func,
            type_table,
            module_source,
            func_rc.clone(),
            type_table_rc.clone(),
        );
    }
}

/// Register a single function with its type and create a `WirFunction` stub.
fn register_single_function(
    ctx: &mut WirContext<'_>,
    tir_func: &NirFunction,
    type_table: &TypeTable,
    module_source: &ModuleSource,
    tir_func_rc: std::rc::Rc<std::cell::RefCell<NirFunction>>,
    type_table_rc: std::rc::Rc<std::cell::RefCell<TypeTable>>,
) {
    let mangled_name = build_mangled_name(tir_func, module_source);

    // Skip if already registered
    let fq = MangledName::in_module(module_source, &mangled_name);
    if ctx.func_map.contains_key(&fq) {
        return;
    }

    // Build param types, filtering out unit-type params (unit has no Wasm representation).
    // WIR locals are looked up by name during codegen, so any two params sharing a
    // name would clobber each other in `current_locals`. Disambiguate duplicates by
    // suffixing `_{local_index}`; matches `FunctionTranslator::local_name`.
    let mut params: Vec<WirType> = Vec::new();
    let mut param_names: Vec<String> = Vec::new();
    let mut name_counts: IndexMap<String, u32> = IndexMap::default();
    for p in &tir_func.params {
        *name_counts.entry(p.name.clone()).or_insert(0) += 1;
    }
    for p in &tir_func.params {
        let wir_type = ctx.type_id_to_wir_type(type_table, p.type_id);
        if matches!(wir_type, WirType::Unit) {
            continue;
        }
        params.push(wir_type);
        let unique_name = if name_counts.get(&p.name).copied().unwrap_or(0) > 1 {
            format!("{}_{}", p.name, p.local_index)
        } else {
            p.name.clone()
        };
        param_names.push(unique_name);
    }

    // Build result types. Honour the function's `return_abi`:
    //
    // - `Single`: a single Wasm result whose type is the TIR return type
    //   (or empty for unit/never). This is the historical behaviour.
    // - `MultiValue { result_types, .. }`: a Wasm multi-value result with
    //   one slot per tuple element / struct field. Set by
    //   `optimize::multi_value_return` for aggregate-returning functions
    //   whose every call site destructures the result.
    let results: Vec<WirType> = match &tir_func.return_abi {
        crate::nir::ReturnAbi::MultiValue { result_types, .. } => result_types
            .iter()
            .map(|&t| ctx.type_id_to_wir_type(type_table, t))
            .filter(|t| !matches!(t, WirType::Unit))
            .collect(),
        crate::nir::ReturnAbi::Single => {
            if tir_func.return_type == TypeTable::UNIT || tir_func.return_type == TypeTable::NEVER {
                Vec::new()
            } else {
                vec![ctx.type_id_to_wir_type(type_table, tir_func.return_type)]
            }
        }
    };
    let effects = tir_func.effects.clone();

    // Register function type
    let fq = fq.into_string();
    let type_fq = crate::name::wir_func_type_key(&fq);
    let type_id = ctx.register_func_type(type_fq, params, results);

    let wir_func = WirFunction {
        name: WirName { fq },
        type_id,
        param_names,
        body: None, // Filled later by translate phase
        locals: crate::wir::WirLocals::default(),
        value_copy_mangle: tir_func
            .value_copy_type()
            .map(|t| type_table.mangle_type_arg_for_generic(t)),
        meta: WirMeta {
            module_source: Some(module_source.clone()),
            ..WirMeta::default()
        },
        generic_origin: tir_func.monomorph_info.as_ref().map(|info| {
            let mut type_arg_names: Vec<String> = info
                .impl_type_args
                .iter()
                .map(|&ta| type_table.mangle_type_name(ta))
                .collect();
            type_arg_names.extend(
                info.method_type_args
                    .iter()
                    .map(|&ta| type_table.mangle_type_name(ta)),
            );
            WirGenericOrigin {
                base_name: info.generic_name.clone(),
                type_args: type_arg_names,
            }
        }),
        effects,
        stores: tir_func.stores.clone(),
        compiler_item: tir_func.compiler_item,
        export_name: tir_func.export_name.clone(),
    };

    let _func_id = ctx.register_function(wir_func, tir_func.id);
    let wir_func_index = ctx.functions.len() - 1;

    // Register as pending body for translation
    ctx.pending_bodies.push(PendingFunctionBody {
        wir_func_index,
        tir_func: tir_func_rc,
        type_table: type_table_rc,
    });
}

/// Register passive data segments for string and bytes literals.
///
/// Both lower to a packed `Array<u8>` `repr` (see `translate_packed_array`).
/// Only payloads longer than `string_inline_max_bytes` need a segment; shorter
/// ones materialize inline as a constant `array.new_fixed<u8>`, so a segment for
/// them would be dead. String and bytes payloads share one content-keyed map,
/// so equal bytes dedup to one segment.
fn register_literal_data(ctx: &mut WirContext<'_>) {
    // `package` is a `&NirPackage` whose lifetime outlives `ctx`, so copying the
    // reference lets us iterate the source literals while calling `&mut self`.
    let package = ctx.package;
    let threshold = package.string_inline_max_bytes;
    let recorded: Vec<&[u8]> = package
        .string_literals
        .iter()
        .map(String::as_bytes)
        .chain(package.bytes_literals.iter().map(Vec::as_slice))
        .filter(|payload| payload.len() > threshold)
        .collect();
    for payload in recorded {
        ctx.register_packed_data(payload);
    }
    for payload in synthesized_packed_payloads(ctx.package, threshold) {
        ctx.register_packed_data(&payload);
    }
}

/// Every `PackedArray` payload in the program that needs a segment.
///
/// The lower phase's recorded literal lists are not the whole set: the
/// optimizer writes literals back that no source literal accounts for — a
/// compile-time call producing a constant `String` becomes one — so the NIR is
/// the authority. Registering after the recorded lists keeps their segment
/// indices, appending only what they missed.
///
/// Only what the module will emit counts: a dead function is never lowered,
/// and the arena keeps every node an in-place rewrite displaced. Counting
/// either would put a segment in the binary that nothing reads.
fn synthesized_packed_payloads(
    package: &crate::nir_package::NirPackage,
    threshold: usize,
) -> Vec<Vec<u8>> {
    fn collect(body: &crate::nir_arena::Body, threshold: usize, out: &mut Vec<Vec<u8>>) {
        for e in crate::nir_visitor::reachable_exprs(body) {
            if let crate::nir_arena::ExprKind::PackedArray(bytes) = &body.exprs[e].kind
                && bytes.len() > threshold
            {
                out.push(bytes.clone());
            }
        }
    }
    let mut out = Vec::new();
    for func in &package.functions {
        let func = func.borrow();
        if func.is_dead {
            continue;
        }
        if let Some(body) = func.body.as_ref() {
            collect(body, threshold, &mut out);
        }
    }
    for global in &package.globals {
        collect(global.init.slot_expr().body(), threshold, &mut out);
    }
    out
}

/// Register function exports (world exports like "run").
fn register_exports(ctx: &mut WirContext<'_>) {
    let component_plan = &ctx.package.component_plan;

    for export in &component_plan.world_exports {
        let core_func_name = &export.core_func_name;
        // Find the function in the map
        let entry_source = &ctx.package.entry_module_source;
        let fq = MangledName::in_module(entry_source, core_func_name);
        if let Some(func_id) = ctx.func_map.get(&fq) {
            // Export with export.name (component-level name), using core_func_name's function
            ctx.exports.push(crate::wir::WirExport {
                name: export.name.clone(),
                desc: crate::wir::WirExportDesc::Func {
                    func_id: func_id.clone(),
                },
            });
        } else {
            panic!(
                "[WIR] export function '{core_func_name}' not found (fq: {fq}); available: {:?}",
                ctx.func_map.keys().collect::<Vec<_>>()
            );
        }

        if let Some(post_return) = &export.post_return_core_name {
            let fq = MangledName::in_module(entry_source, post_return);
            let Some(func_id) = ctx.func_map.get(&fq) else {
                panic!(
                    "[WIR] post-return function '{post_return}' not found (fq: {fq}); available: {:?}",
                    ctx.func_map.keys().collect::<Vec<_>>()
                );
            };
            ctx.exports.push(crate::wir::WirExport {
                name: post_return.clone(),
                desc: crate::wir::WirExportDesc::Func {
                    func_id: func_id.clone(),
                },
            });
        }
    }

    // Also export test functions
    for test in &component_plan.test_exports {
        let entry_source = &ctx.package.entry_module_source;
        let fq = MangledName::in_module(entry_source, &test.core_func_name);
        if let Some(func_id) = ctx.func_map.get(&fq) {
            // Export test function with its core function name
            ctx.exports.push(crate::wir::WirExport {
                name: test.function_name.clone(),
                desc: crate::wir::WirExportDesc::Func {
                    func_id: func_id.clone(),
                },
            });
        }
    }
}

/// Register global variables from all TIR modules.
///
/// Global naming convention:
/// - Entry module: `"global:{name}"`
/// - Other modules: `"global:{mod_path}::{name}"`
fn register_globals(ctx: &mut WirContext<'_>) {
    let type_table = &*ctx.package.type_table.borrow();

    for global in &ctx.package.globals {
        let module_source = &global.module_source;
        // A WASI module never hosts a `NirGlobal` (no `wasi/*.wado` stdlib
        // file declares a top-level `global`), mirroring the exclusion in
        // `register_loaded_functions` above. Assert here, the one place
        // this can be checked authoritatively — a pass that violates it
        // would otherwise leave the name unregistered, and `resolve_global`
        // in `codegen::emit` silently falls back to Wasm global index 0.
        assert!(
            !module_source.is_wasi(),
            "[WIR] global '{}' has a WASI module_source ({module_source}) — \
             WASI modules cannot host globals; whichever pass created this \
             global must exclude WASI-sourced functions from its candidates",
            global.name
        );

        let global_name = global_name(module_source, &global.name);

        let mut wir_type = ctx.type_id_to_wir_type(type_table, global.ty);

        // Both cases need a nullable slot, but only a deferred one narrows its
        // reads: a declared `null` is a value the program observes, and
        // narrowing would trap on every `None`.
        let deferred = global.init.is_deferred();
        let lazy_init = deferred && is_wir_reference(&wir_type);
        let declared_null = global
            .init
            .declared()
            .is_some_and(|d| is_null_operand(d.body(), d.expr()));
        if lazy_init || declared_null {
            match &mut wir_type {
                WirType::Ref { nullable, .. } | WirType::AbstractRef { nullable, .. } => {
                    *nullable = true;
                }
                _ => {}
            }
        }

        let slot = global.init.slot_expr();
        let init_body = slot.body();
        let init_op = slot.expr();
        let init = translate_global_init(
            init_body,
            init_op,
            init_body.operand_type(init_op),
            type_table,
            &wir_type,
        );

        let idx = u32::try_from(ctx.globals.len()).expect("too many globals");
        ctx.global_map.insert(global_name.clone(), idx);

        ctx.globals.push(WirGlobal {
            name: WirName { fq: global_name },
            ty: wir_type,
            mutable: global.wado_mutable || deferred,
            wado_mutable: global.wado_mutable,
            init,
            lazy_init,
            meta: WirMeta {
                module_source: Some(module_source.clone()),
                ..WirMeta::default()
            },
        });
    }
}

/// Whether the slot needs a `ref.null` placeholder, and so a narrowing read.
fn is_wir_reference(ty: &WirType) -> bool {
    matches!(ty, WirType::Ref { .. } | WirType::AbstractRef { .. })
}

/// Whether the declared value is `null` itself.
fn is_null_operand(body: &Body, op: Operand) -> bool {
    match op {
        Operand::Value(v) => matches!(body.values.kind(v), ValueKind::Null),
        // `null` reaches the pool as a value, never as a skeleton node.
        Operand::Expr(_) => false,
    }
}

/// The primitive a value of `type_id` is stored as. `enum` and `flags` hold a
/// discriminant, so their slot is an `i32` even though the declared type is
/// not a primitive — without this an enum-valued initializer is not constant
/// and the global falls back to a runtime assignment.
fn storage_prim_of(type_id: crate::tir::TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    use crate::tir::ResolvedType;

    if let Some(prim) = prim_of(type_id, type_table) {
        return Some(prim);
    }
    match type_table.get(type_id) {
        ResolvedType::Enum { .. } | ResolvedType::Flags { .. } => Some(PrimitiveType::I32),
        _ => None,
    }
}

/// Evaluate a global's initializer to a compile-time value.
///
/// Every node is evaluated at its *own* type, which is what makes a cast a
/// conversion rather than a relabelling: `(2147483647 + 1) as i64` wraps at
/// `i32` before it widens. Shares the evaluators with compile-time function
/// evaluation, so a global and an equivalent local expression agree.
fn global_init_value(body: &Body, op: Operand, type_table: &TypeTable) -> Option<Value> {
    match op {
        Operand::Value(v) => {
            let ty = body.values.type_of(v)?;
            match body.values.kind(v) {
                ValueKind::Bool(b) => Some(Value::Bool(*b)),
                ValueKind::Char(c) => Some(Value::Char(*c)),
                ValueKind::Int(value, _) => Some(Value::Int {
                    value: *value,
                    prim: storage_prim_of(ty, type_table)?,
                }),
                ValueKind::Float(bits, _) => Some(Value::Float {
                    value: f64::from_bits(*bits),
                    prim: if is_f32_type(ty, type_table) {
                        PrimitiveType::F32
                    } else {
                        PrimitiveType::F64
                    },
                }),
                _ => None,
            }
        }
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Cast { expr: inner, .. } => {
                let value = global_init_value(body, *inner, type_table)?;
                eval_cast(value, prim_of(body.exprs[e].type_id, type_table)?)
            }
            ExprKind::Unary {
                op: NirUnaryOp::Neg,
                expr: inner,
            } => eval_unary(
                NirUnaryOp::Neg,
                global_init_value(body, *inner, type_table)?,
            ),
            ExprKind::Binary { left, op, right } => eval_binary(
                global_init_value(body, *left, type_table)?,
                *op,
                global_init_value(body, *right, type_table)?,
            ),
            _ => None,
        },
    }
}

/// The value a slot starts at when its initializer is assigned by the module
/// initialization function instead of being reduced here. It has to inhabit
/// the slot's own Wasm type: `ref.null` is a value only for a reference slot,
/// and an `enum` or `flags` slot is an `i32`.
fn init_placeholder(wir_type: &WirType) -> crate::wir::WirInstr {
    use crate::wir::WirInstr;

    match wir_type {
        WirType::Ref { .. } | WirType::AbstractRef { .. } => WirInstr::RefNull {
            heap_type: crate::wir::WirAbstractHeapType::None,
        },
        WirType::I64 | WirType::U64 => WirInstr::I64Const(0),
        WirType::F32 => WirInstr::F32Const(0.0),
        WirType::F64 => WirInstr::F64Const(0.0),
        WirType::I8
        | WirType::I16
        | WirType::I32
        | WirType::U8
        | WirType::U16
        | WirType::U32
        | WirType::Bool
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. } => WirInstr::I32Const(0),
        WirType::V128 => WirInstr::V128Const(0),
        // A unit global occupies no Wasm slot, so nothing reaches here.
        WirType::Unit => panic!("[WIR] unit-typed global has no Wasm slot to initialize"),
    }
}

/// Convert a NIR global initializer operand to a WIR constant instruction.
/// `type_id` is the global's declared type, which decides the constant's
/// width; `wir_type` is the slot it has to inhabit.
fn translate_global_init(
    body: &Body,
    op: Operand,
    type_id: crate::tir::TypeId,
    type_table: &TypeTable,
    wir_type: &WirType,
) -> crate::wir::WirInstr {
    use crate::tir::ResolvedType;
    use crate::wir::WirInstr;

    // What the evaluator cannot reduce is assigned by the initialization
    // function instead, so the slot starts at a placeholder.
    let Some(value) = global_init_value(body, op, type_table) else {
        return init_placeholder(wir_type);
    };
    let bits = match value {
        Value::Int { value, .. } => value,
        Value::Bool(b) => u64::from(b),
        Value::Char(c) => u64::from(c),
        Value::Float { value, .. } => {
            return match type_table.get(type_id) {
                ResolvedType::Primitive(PrimitiveType::F32) => WirInstr::F32Const(value as f32),
                _ => WirInstr::F64Const(value),
            };
        }
        Value::Aggregate { .. } | Value::Seq { .. } | Value::Variant { .. } => {
            return init_placeholder(wir_type);
        }
    };
    match type_table.get(type_id) {
        ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
            WirInstr::I64Const(bits as i64)
        }
        _ => WirInstr::I32Const(bits as i32),
    }
}

/// Build a mangled function name from TIR function and module source.
fn build_mangled_name(tir_func: &NirFunction, _module_source: &ModuleSource) -> String {
    // For monomorphized functions, prefer `tir_func.name` because the
    // monomorphizer set it to the canonical mangled name produced by
    // `function_instantiation_name` / `method_instantiation_name`. The
    // string-typed `method_info.method_type_args` field is populated by
    // elaborator/monomorphizer call sites that historically used
    // `mangle_type_name` (unqualified for `Variant`/`Newtype`/etc.); calling
    // `method_info.to_mangled_name()` on a monomorphized function would
    // therefore drop the module qualification that the call-rewrite path
    // already baked into `tir_func.name`, leaving the func_map registered
    // under a name no call site looks up.
    if tir_func.monomorph_info.is_some() {
        return tir_func.name.clone();
    }
    if let Some(ref method_info) = tir_func.method_info {
        method_info.to_mangled_name()
    } else {
        tir_func.name.clone()
    }
}

/// Convert a `wasm_encoder` `ValType` to `WirType`.
fn valtype_to_wir_type(vt: wasm_encoder::ValType) -> WirType {
    match vt {
        wasm_encoder::ValType::I32 => WirType::I32,
        wasm_encoder::ValType::I64 => WirType::I64,
        wasm_encoder::ValType::F32 => WirType::F32,
        wasm_encoder::ValType::F64 => WirType::F64,
        wasm_encoder::ValType::Ref(_) => WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Any,
            nullable: true,
        },
        _ => WirType::I32,
    }
}

use crate::wir::WirGenericOrigin;
