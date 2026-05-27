//! Function collection — gathers all reachable functions from the `NirPackage`,
//! registers their types and creates `WirFunction` stubs (bodies filled later).

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::NirFunction;
use crate::tir::TypeTable;
use crate::wir::{
    CanonicalIntrinsic, WirFunction, WirGlobal, WirImport, WirImportDesc, WirMeta, WirName, WirType,
};

use super::context::{PendingFunctionBody, WirContext};

/// Collect all functions from the `NirPackage`, register imports, and create function stubs.
pub fn collect_functions(ctx: &mut WirContext<'_>) {
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
    register_string_data(ctx);
    register_bytes_data(ctx);

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

        let type_fq = format!("functype//{}/{}", import.namespace, import.canonical_name);
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
            let alias = format!("builtin/{}", import.func_name);
            ctx.func_map.insert(alias, func_id);
        }
    }
}

/// Register WASI function imports.
///
/// WASI functions are already lowered at the component level;
/// the core module imports them from the "wasi" namespace.
/// Uses `flatten_wasi_param_type` / `return_type_requires_outptr` for CM ABI type flattening.
fn register_wasi_imports(ctx: &mut WirContext<'_>) {
    let cm_interface_registry = ctx.package.cm_interface_registry;

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

            // Skip unsupported functions (for non-HTTP interfaces).
            // HTTP functions are handled separately by import_http_types_for_service
            // in the component builder, which has its own type support (ast_type_to_cm).
            // The general is_function_supported check would reject HTTP [method] functions
            // like Fields::append because their &Resource params aren't supported
            // in the general type checker.
            if func.package != "http" && !cm_interface_registry.is_function_supported(func) {
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
                crate::component_model::flatten_wasi_param_type(
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
                if crate::component_model::return_type_requires_outptr(&resolved_ret_ty)
                    || crate::component_model::wasi_named_type_return_needs_outptr(
                        &resolved_ret_ty,
                        cm_interface_registry,
                    )
                {
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
                if crate::component_model::return_type_requires_outptr(&resolved_ret_ty)
                    || crate::component_model::wasi_named_type_return_needs_outptr(
                        &resolved_ret_ty,
                        cm_interface_registry,
                    )
                {
                    // Complex return via outptr — function returns nothing
                    Vec::new()
                } else {
                    // Simple return — flatten the return type
                    let mut out = Vec::new();
                    crate::component_model::flatten_wasi_param_type(
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

        // Skip methods (handled in register_methods)
        if tir_func.name.contains("::") {
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
        if module_source == entry_source || module_source.is_wasi() {
            continue;
        }

        if tir_func.name == "run" || tir_func.body.is_none() || tir_func.name.contains("::") {
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
    let fq = format!("{module_source}/{mangled_name}");
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
    let type_fq = format!("functype//{fq}");
    let type_id = ctx.register_func_type(type_fq, params, results);

    let wir_func = WirFunction {
        name: WirName { fq },
        type_id,
        param_names,
        body: None, // Filled later by translate phase
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

    let _func_id = ctx.register_function(wir_func);
    let wir_func_index = ctx.functions.len() - 1;

    // Register as pending body for translation
    ctx.pending_bodies.push(PendingFunctionBody {
        wir_func_index,
        tir_func: tir_func_rc,
        type_table: type_table_rc,
    });
}

/// Register data segments for string literals.
fn register_string_data(ctx: &mut WirContext<'_>) {
    let literals: Vec<String> = ctx.string_literals.clone();
    for s in &literals {
        ctx.register_string_literal(s);
    }
}

/// Register data segments for bytes literals.
fn register_bytes_data(ctx: &mut WirContext<'_>) {
    let literals: Vec<Vec<u8>> = ctx.bytes_literals.clone();
    for b in &literals {
        ctx.register_bytes_literal(b);
    }
}

/// Register function exports (world exports like "run").
fn register_exports(ctx: &mut WirContext<'_>) {
    let component_plan = &ctx.package.component_plan;

    for export in &component_plan.world_exports {
        let core_func_name = &export.core_func_name;
        // Find the function in the map
        let entry_source = &ctx.package.entry_module_source;
        let fq = format!("{entry_source}/{core_func_name}");
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
    }

    // Also export test functions
    for test in &component_plan.test_exports {
        let entry_source = &ctx.package.entry_module_source;
        let fq = format!("{entry_source}/{}", test.core_func_name);
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
    let entry_source = &ctx.package.entry_module_source;
    let type_table = &*ctx.package.type_table.borrow();

    for global in &ctx.package.globals {
        let module_source = &global.module_source;
        if module_source.is_wasi() {
            continue;
        }

        let global_name = if module_source == entry_source {
            format!("global:{}", global.name)
        } else {
            let module_path = module_source.to_path();
            format!("global:{}::{}", module_path.join("::"), global.name)
        };

        let mut wir_type = ctx.type_id_to_wir_type(type_table, global.ty);

        // For nullable globals (lazy-init reference types, or constant
        // `null` initialisers on reference-typed globals), widen the WIR
        // slot to its nullable form so the `ref.null` placeholder in the
        // Wasm initialiser validates. Both `WirType::Ref` (concrete
        // struct/array references) and `WirType::AbstractRef` (e.g.
        // closure-typed globals lowered to abstract `structref`) need
        // this. Codegen narrows reads back to the non-null type via
        // `ref.as_non_null` for `lazy_init` globals.
        if global.is_nullable {
            match &mut wir_type {
                WirType::Ref { nullable, .. } | WirType::AbstractRef { nullable, .. } => {
                    *nullable = true;
                }
                _ => {}
            }
        }

        // Convert the initializer to a WIR constant instruction
        let init = translate_global_init(&global.initializer, type_table);

        let idx = u32::try_from(ctx.globals.len()).expect("too many globals");
        ctx.global_map.insert(global_name.clone(), idx);

        ctx.globals.push(WirGlobal {
            name: WirName { fq: global_name },
            ty: wir_type,
            mutable: global.mutable,
            init,
            lazy_init: global.lazy_init,
            meta: WirMeta {
                module_source: Some(module_source.clone()),
                ..WirMeta::default()
            },
        });
    }
}

/// Convert a TIR global initializer to a WIR constant instruction.
fn translate_global_init(
    init: &crate::nir::NirExpr,
    type_table: &TypeTable,
) -> crate::wir::WirInstr {
    use crate::nir::NirExprKind;
    use crate::tir::{PrimitiveType, ResolvedType};
    use crate::wir::WirInstr;

    match &init.kind {
        NirExprKind::IntLiteral { value, .. } => match type_table.get(init.type_id) {
            ResolvedType::Primitive(prim) => match prim {
                PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32 => WirInstr::I32Const(*value as i32),
                PrimitiveType::I64 | PrimitiveType::U64 => WirInstr::I64Const(*value as i64),
                _ => WirInstr::I32Const(*value as i32),
            },
            _ => WirInstr::I32Const(*value as i32),
        },
        NirExprKind::FloatLiteral { value, .. } => match type_table.get(init.type_id) {
            ResolvedType::Primitive(PrimitiveType::F32) => WirInstr::F32Const(*value as f32),
            _ => WirInstr::F64Const(*value),
        },
        NirExprKind::BoolLiteral(b) => WirInstr::I32Const(i32::from(*b)),
        NirExprKind::CharLiteral(c) => WirInstr::I32Const(*c as i32),
        NirExprKind::Null | NirExprKind::Unit => WirInstr::RefNull {
            heap_type: crate::wir::WirAbstractHeapType::None,
        },
        NirExprKind::Cast { expr: inner, .. } => {
            // For casts, evaluate the inner expression with the cast's target type
            let typed_inner = crate::nir::NirExpr::new(inner.kind.clone(), init.type_id, init.span);
            translate_global_init(&typed_inner, type_table)
        }
        NirExprKind::Unary {
            op: crate::nir::NirUnaryOp::Neg,
            expr: inner,
        } => {
            // Negation of constant (normally folded by elaborator, but handle for robustness)
            let inner_wir = translate_global_init(inner, type_table);
            match inner_wir {
                WirInstr::I32Const(v) => WirInstr::I32Const(v.wrapping_neg()),
                WirInstr::I64Const(v) => WirInstr::I64Const(v.wrapping_neg()),
                WirInstr::F32Const(v) => WirInstr::F32Const(-v),
                WirInstr::F64Const(v) => WirInstr::F64Const(-v),
                _ => WirInstr::RefNull {
                    heap_type: crate::wir::WirAbstractHeapType::None,
                },
            }
        }
        _ => {
            // Non-constant initializers use null placeholder (lazy init at runtime)
            WirInstr::RefNull {
                heap_type: crate::wir::WirAbstractHeapType::None,
            }
        }
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
