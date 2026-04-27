//! Effect handler dispatch synthesis (Phase 4 of WEP 2026-04-11).
//!
//! Runs after effect-check / stores-check, before link/monomorphize.
//!
//! For every effect that has at least one user-written `impl E for T`
//! block, the pass emits the dispatch infrastructure described in the
//! WEP and rewrites the program to route every effect-operation call
//! through it:
//!
//! 1. A per-effect Wasm GC struct `__Dispatch_<E>` with a recursive
//!    `outer: Option<&__Dispatch_<E>>` field plus one
//!    `fn(<op_params>) -> <op_ret>` closure field per declared
//!    operation.
//! 2. A per-effect mutable global
//!    `__effect_<E>: Option<&__Dispatch_<E>>` initialised to `null`.
//! 3. One `__effect_dispatch__<E>__<op>` wrapper function per
//!    operation. Body: read the global, restore `outer` (so handler
//!    bodies see the outer scope and can self-delegate), call the
//!    closure, re-install the saved value, and return the result. If
//!    no handler is installed, fall back to the existing CM binding
//!    adapter (WASI effects) or trap (user-defined effects).
//! 4. `WithHandler { bindings, body }` lowers to a desugared block
//!    that, for each binding (in source order), saves the global,
//!    binds the handler value to a fresh local, builds closures
//!    capturing that local and forwarding to the bound `impl E for T`
//!    methods, populates a fresh `__Dispatch_<E>` struct, installs
//!    the global, runs the body, and restores the saved value on
//!    exit (in reverse install order).
//! 5. Every `<E>::<op>` call site is rewritten to call the wrapper —
//!    both the WASI-binding shape (`__cm_binding__<E>_<op>`) and the
//!    user-effect namespaced shape (`<op>` in `Local{path: "<E>"}`).
//!    Calls inside `__effect_dispatch__*` wrappers and
//!    `__cm_binding__*` adapters are skipped to keep the WASI
//!    fallback path reachable.
//! 6. `Resume { value }` inside `impl E for T` method bodies is
//!    lowered to `Return { value }` — the MVP has no post-resume
//!    semantics.
//!
//! Operations the installed handler does not implement (the `..` rest
//! pattern in `impl Effect for T`) get a trapping stub closure
//! populated into the dispatch struct so the dispatch global is
//! always fully populated.

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::LocalMethodName;
use crate::name::ModuleSource;
use crate::package::Package;
use crate::synthesis::common::{alloc_local, option_some, ref_expr, synth_span};
use crate::tir::{
    CallArg, EffectRef, FunctionKind, FunctionRef, InlineHint, TirBlock, TirCapture, TirEffectOp,
    TirExpr, TirExprKind, TirField, TirFunction, TirGlobal, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirStructField, TirTemplatePart, TypeId, TypeTable,
};

/// Canonical identity of an effect: `(defining_module, name)`.
///
/// Two effects in distinct modules that happen to share a name (rare in
/// practice but possible) compare unequal. Mirrors the resolver's
/// canonicalisation of `EffectRef::Concrete { name, module_source }`.
#[allow(dead_code)]
type EffectKey = (ModuleSource, String);

/// Metadata for a single effect, indexed by `EffectKey`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct EffectMeta {
    /// The operation declarations (in source order) — copied so later
    /// passes don't have to walk the module tree again.
    operations: Vec<TirEffectOp>,
}

/// Walk every TIR module and build an `EffectKey -> EffectMeta` index.
#[allow(dead_code)]
fn build_effect_index(project: &Package) -> IndexMap<EffectKey, EffectMeta> {
    let mut out: IndexMap<EffectKey, EffectMeta> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for effect in &module.effects {
            out.insert(
                (module_source.clone(), effect.name.clone()),
                EffectMeta {
                    operations: effect.operations.clone(),
                },
            );
        }
    }
    out
}

/// Identify which effects need dispatch infrastructure.
///
/// An effect is "active" iff at least one user-written `impl <Effect> for
/// <Type>` block exists for it in the package. Effects without any impl
/// don't need a dispatch struct / global / wrapper triple — they always
/// route through the WASI CM binding (or, for user-defined effects with
/// no impl, were already rejected by effect-check).
///
/// Reads the canonical `(effect_module, effect_name)` pair off each
/// `HandlerImplKey` directly — no name-only matching against
/// `effect_index` — so two effects sharing a name across modules cannot
/// be conflated.
#[allow(dead_code)]
fn identify_active_effects(
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
) -> IndexSet<EffectKey> {
    impl_index
        .keys()
        .map(|(_struct, effect_module, effect_name)| {
            (effect_module.clone(), effect_name.clone())
        })
        .collect()
}

/// All the bookkeeping needed to emit / refer to the dispatch
/// infrastructure for a single effect.
///
/// A `DispatchPlan` is built once per active effect by
/// `synthesize_dispatch_infrastructure` and consumed by the lowering
/// (`lower_with_handler`) and call-site rewriting
/// (`rewrite_call_sites_to_wrappers`) passes.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct DispatchPlan {
    /// `TypeId` of the synthesised `__Dispatch_<E>` struct.
    struct_type_id: TypeId,
    /// `TypeId` of `Option<&__Dispatch_<E>>` — the global's runtime type
    /// and the type of `outer` / dispatch wrapper-saved values.
    nullable_ref_type_id: TypeId,
    /// `TypeId` of `&__Dispatch_<E>` — handed to `Option::Some` when
    /// installing a fresh dispatch record.
    inner_ref_type_id: TypeId,
    /// Name of the synthesised `__effect_<E>` mutable global.
    global_name: String,
    /// Operation name → dispatch wrapper function name
    /// (`__effect_dispatch__<E>__<op>`).
    wrapper_names: IndexMap<String, String>,
    /// Operation name → dispatch struct field name (`op_<op>`).
    field_names: IndexMap<String, String>,
    /// Operation name → field type (`fn(<op_params>) -> <op_ret>`).
    field_types: IndexMap<String, TypeId>,
    /// Operation name → 0-based field index in `__Dispatch_<E>`. The
    /// `outer` field always sits at index 0; ops start at 1.
    field_indices: IndexMap<String, u32>,
    /// Cached operation declarations (cloned from `EffectMeta`) so the
    /// wrapper / closure synth doesn't have to walk back to the index.
    operations: Vec<TirEffectOp>,
}

/// Synthesise the `__Dispatch_<E>` Wasm GC struct for one effect.
///
/// Two-phase construction handles the recursive
/// `outer: Option<&__Dispatch_<E>>` field:
///
/// 1. `make_struct` interns an empty / forward-declared struct type id
///    in the shared `TypeTable`.
/// 2. Field types — including the `Option<&Self>` outer field, which
///    references that very type id — are computed in the same borrow
///    scope.
///
/// The matching `TirStruct` decl is then appended to the entry module's
/// `structs` list. Layout:
///
/// ```text
/// struct __Dispatch_<E> {
///     outer: Option<&__Dispatch_<E>>,
///     op_<n>: fn(<op_n_params>) -> <op_n_ret>,
///     ...
/// }
/// ```
///
/// All synthesised dispatch infrastructure lives in the entry module,
/// mirroring `cm_binding`'s placement of WASI binding adapters.
#[allow(dead_code)]
fn synthesize_dispatch_struct(
    project: &mut Package,
    entry_source: &ModuleSource,
    key: &EffectKey,
    meta: &EffectMeta,
) -> DispatchPlan {
    let (_, effect_name) = key;
    let struct_name = format!("__Dispatch_{effect_name}");
    let global_name = format!("__effect_{effect_name}");

    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    let tt_rc = entry_module.type_table.clone();

    // Build all the type IDs in a single borrow scope. The recursive
    // `outer` field type is computed off the freshly-interned struct
    // type id.
    let struct_type_id;
    let inner_ref_type_id;
    let nullable_ref_type_id;
    let mut op_field_types: Vec<TypeId> = Vec::with_capacity(meta.operations.len());
    {
        let mut tt = tt_rc.borrow_mut();
        struct_type_id = tt.make_struct(struct_name.clone(), entry_source.clone());
        inner_ref_type_id = tt.make_ref(struct_type_id);
        nullable_ref_type_id = tt.make_option(inner_ref_type_id);
        for op in &meta.operations {
            let param_types: Vec<TypeId> = op.params.iter().map(|p| p.type_id).collect();
            let op_func_type = tt.make_function(param_types, op.return_type, vec![], vec![]);
            op_field_types.push(op_func_type);
        }
    }

    // Build the field decls: outer at index 0, then ops in source order.
    let outer_field_type = nullable_ref_type_id;
    let mut fields: Vec<TirField> = Vec::with_capacity(meta.operations.len() + 1);
    fields.push(TirField {
        name: "outer".to_string(),
        is_pub: false,
        type_id: outer_field_type,
        index: 0,
        span: synth_span(),
        is_hidden: false,
        serde_rename: None,
        serde_default: false,
        default_expr: None,
    });

    let mut wrapper_names: IndexMap<String, String> = IndexMap::default();
    let mut field_names: IndexMap<String, String> = IndexMap::default();
    let mut field_types: IndexMap<String, TypeId> = IndexMap::default();
    let mut field_indices: IndexMap<String, u32> = IndexMap::default();

    for (i, op) in meta.operations.iter().enumerate() {
        let field_name = format!("op_{}", op.name);
        let field_type = op_field_types[i];
        let field_index = (i + 1) as u32;
        fields.push(TirField {
            name: field_name.clone(),
            is_pub: false,
            type_id: field_type,
            index: field_index,
            span: synth_span(),
            is_hidden: false,
            serde_rename: None,
            serde_default: false,
            default_expr: None,
        });
        wrapper_names.insert(
            op.name.clone(),
            format!("__effect_dispatch__{}__{}", effect_name, op.name),
        );
        field_names.insert(op.name.clone(), field_name);
        field_types.insert(op.name.clone(), field_type);
        field_indices.insert(op.name.clone(), field_index);
    }

    entry_module.add_struct(TirStruct {
        name: struct_name,
        module_source: entry_source.clone(),
        is_pub: false,
        type_params: vec![],
        monomorph_info: None,
        fields,
        span: synth_span(),
        serde_rename_all: None,
    });

    DispatchPlan {
        struct_type_id,
        nullable_ref_type_id,
        inner_ref_type_id,
        global_name,
        wrapper_names,
        field_names,
        field_types,
        field_indices,
        operations: meta.operations.clone(),
    }
}

/// Synthesise the `__effect_<E>` mutable global for one effect.
///
/// The slot stores `Option<&__Dispatch_<E>>` and starts at `null`
/// (meaning: no handler installed). `is_nullable: true` makes the
/// Wasm validator accept the `ref.null` initializer for the `(mut
/// (ref null $Dispatch))` slot; `lazy_init: false` keeps codegen from
/// narrowing `global.get` results with `ref.as_non_null` since `None`
/// reads must round-trip cleanly to express "no handler installed"
/// at runtime.
#[allow(dead_code)]
fn synthesize_dispatch_global(
    project: &mut Package,
    entry_source: &ModuleSource,
    plan: &DispatchPlan,
) {
    let span = synth_span();
    let initializer = TirExpr::new(TirExprKind::Null, plan.nullable_ref_type_id, span);
    let global = TirGlobal {
        name: plan.global_name.clone(),
        ty: plan.nullable_ref_type_id,
        initializer,
        mutable: true,
        wado_mutable: true,
        is_pub: false,
        module_source: entry_source.clone(),
        span,
        is_nullable: true,
        lazy_init: false,
        local_types: Vec::new(),
    };
    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    entry_module.globals.push(global);
}

/// Synthesise the per-(effect, op) `__effect_dispatch__<E>__<op>` wrapper
/// function for one effect.
///
/// Each wrapper has the same signature as the operation itself
/// (`<op_params> -> <op_ret>`) so call-site rewriting is a name swap.
///
/// Body shape:
///
/// ```text
/// fn __effect_dispatch__<E>__<op>(<args>) -> <ret> {
///     let __saved = global.get __effect_<E>;
///     if let Some(d) = __saved {
///         global.set __effect_<E> = d.outer;     // expose outer scope
///         let __result = (d.op_<op>)(args);      // call the closure
///         global.set __effect_<E> = __saved;     // restore
///         return __result;
///     }
///     // Fallback path: no handler installed.
///     // - WASI effect: call the existing CM-binding adapter.
///     // - User effect: trap (effect-check should have ensured a handler).
///     return __cm_binding__<E>_<op>(args);
/// }
/// ```
///
/// The outer-scope restore before the closure call implements algebraic
/// effect forwarding: a handler method can call `<E>::<op>` again to
/// delegate to the outer handler chain without infinite recursion.
#[allow(dead_code)]
fn synthesize_dispatch_wrappers(
    project: &mut Package,
    entry_source: &ModuleSource,
    key: &EffectKey,
    plan: &DispatchPlan,
) {
    let effect_module = key.0.clone();
    let effect_name = key.1.clone();
    let is_wasi = matches!(effect_module, ModuleSource::Wasi { .. });
    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    let type_table = entry_module.type_table.clone();
    for op in &plan.operations {
        let wrapper = build_dispatch_wrapper_function(
            entry_source,
            &effect_module,
            &effect_name,
            op,
            plan,
            is_wasi,
            &type_table,
        );
        entry_module.add_function(wrapper);
    }
}

#[allow(dead_code)]
fn build_dispatch_wrapper_function(
    entry_source: &ModuleSource,
    effect_module: &ModuleSource,
    effect_name: &str,
    op: &TirEffectOp,
    plan: &DispatchPlan,
    is_wasi: bool,
    type_table: &std::rc::Rc<std::cell::RefCell<TypeTable>>,
) -> TirFunction {
    let span = synth_span();
    let op_name = &op.name;
    let wrapper_name = plan
        .wrapper_names
        .get(op_name)
        .expect("wrapper name registered")
        .clone();
    let return_type = op.return_type;
    let global_name = plan.global_name.clone();
    let nullable_ref_type_id = plan.nullable_ref_type_id;
    let inner_ref_type_id = plan.inner_ref_type_id;

    // Allocate locals: params, then __saved, __d (if-let binding), __result.
    let mut params: Vec<TirParam> = Vec::with_capacity(op.params.len());
    let mut local_types: Vec<TypeId> = Vec::new();
    let mut next_local: u32 = 0;
    for p in &op.params {
        let local_index = alloc_local(&mut next_local, &mut local_types, p.type_id);
        params.push(TirParam {
            name: p.name.clone(),
            type_id: p.type_id,
            local_index,
            is_mut: false,
            default_expr: None,
            span,
        });
    }
    let saved_local = alloc_local(&mut next_local, &mut local_types, nullable_ref_type_id);
    let d_local = alloc_local(&mut next_local, &mut local_types, inner_ref_type_id);
    let result_local = if return_type == TypeTable::UNIT {
        None
    } else {
        Some(alloc_local(&mut next_local, &mut local_types, return_type))
    };

    let arg_exprs: Vec<TirExpr> = params
        .iter()
        .map(|p| {
            TirExpr::new(
                TirExprKind::Local {
                    index: p.local_index,
                    name: p.name.clone(),
                },
                p.type_id,
                span,
            )
        })
        .collect();

    let mut stmts: Vec<TirStmt> = Vec::new();

    // let __saved = global.get __effect_<E>;
    let global_get_expr = TirExpr::new(
        TirExprKind::GlobalVarGet {
            module_source: entry_source.clone(),
            name: global_name.clone(),
        },
        nullable_ref_type_id,
        span,
    );
    stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: "__saved".to_string(),
            local_index: saved_local,
            is_mut: false,
            is_reactive: false,
            type_id: nullable_ref_type_id,
            value: global_get_expr,
            skip_value_copy: true,
        },
        span,
    ));

    // Then-branch: handler installed.
    let mut then_stmts: Vec<TirStmt> = Vec::new();
    let d_local_expr = || {
        TirExpr::new(
            TirExprKind::Local {
                index: d_local,
                name: "__d".to_string(),
            },
            inner_ref_type_id,
            span,
        )
    };

    // global.set __effect_<E> = d.outer;
    let outer_field_access = TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(d_local_expr()),
            field_index: 0,
            field_name: "outer".to_string(),
        },
        nullable_ref_type_id,
        span,
    );
    then_stmts.push(TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::GlobalVarSet {
                module_source: entry_source.clone(),
                name: global_name.clone(),
                value: Box::new(outer_field_access),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    ));

    // let __result = (d.op_<op>)(args);
    let op_field_index = *plan.field_indices.get(op_name).expect("op field index");
    let op_field_name = plan
        .field_names
        .get(op_name)
        .expect("op field name")
        .clone();
    let op_field_type = *plan.field_types.get(op_name).expect("op field type");
    let closure_field_access = TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(d_local_expr()),
            field_index: op_field_index,
            field_name: op_field_name,
        },
        op_field_type,
        span,
    );
    let indirect_call = TirExpr::new(
        TirExprKind::IndirectCall {
            callee: Box::new(closure_field_access),
            args: arg_exprs.clone(),
        },
        return_type,
        span,
    );

    if let Some(rl) = result_local {
        then_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: "__result".to_string(),
                local_index: rl,
                is_mut: false,
                is_reactive: false,
                type_id: return_type,
                value: indirect_call,
                skip_value_copy: false,
            },
            span,
        ));
    } else {
        then_stmts.push(TirStmt::new(TirStmtKind::Expr(indirect_call), span));
    }

    // global.set __effect_<E> = __saved;
    let saved_expr = TirExpr::new(
        TirExprKind::Local {
            index: saved_local,
            name: "__saved".to_string(),
        },
        nullable_ref_type_id,
        span,
    );
    then_stmts.push(TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::GlobalVarSet {
                module_source: entry_source.clone(),
                name: global_name,
                value: Box::new(saved_expr),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    ));

    // return __result;  /  return;
    if let Some(rl) = result_local {
        let result_expr = TirExpr::new(
            TirExprKind::Local {
                index: rl,
                name: "__result".to_string(),
            },
            return_type,
            span,
        );
        then_stmts.push(TirStmt::new(
            TirStmtKind::Return {
                value: Some(result_expr),
            },
            span,
        ));
    } else {
        then_stmts.push(TirStmt::new(TirStmtKind::Return { value: None }, span));
    }

    let then_block = TirBlock::new(then_stmts, span);

    // Else-branch: fallback.
    let mut else_stmts: Vec<TirStmt> = Vec::new();
    if is_wasi {
        let cm_binding_name = format!("__cm_binding__{effect_name}_{op_name}");
        let cm_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: entry_source.clone(),
                    name: cm_binding_name,
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: arg_exprs
                    .iter()
                    .cloned()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
            },
            return_type,
            span,
        );
        else_stmts.push(TirStmt::new(
            TirStmtKind::Return {
                value: Some(cm_call),
            },
            span,
        ));
    } else {
        // User-defined effect: well-typed programs never reach the wrapper
        // without a handler installed, since effect-check requires every
        // call-site to have a `with E = h do { ... }` in scope. If we get
        // here anyway, panic with a diagnostic identifying the operation
        // — useful when a future refactor introduces a path that bypasses
        // effect-check.
        let string_type_id = type_table
            .borrow()
            .find_struct_type("String", &ModuleSource::string())
            .unwrap_or_else(|| {
                panic!(
                    "core:prelude/string.wado String type missing from \
                     the package type table at effect-dispatch synthesis"
                )
            });
        let message = TirExpr::new(
            TirExprKind::StringLiteral(format!(
                "no handler installed for `{effect_name}::{op_name}`"
            )),
            string_type_id,
            span,
        );
        let panic_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::internal(),
                    name: "panic".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![CallArg::new(message, false)],
            },
            TypeTable::NEVER,
            span,
        );
        else_stmts.push(TirStmt::new(TirStmtKind::Expr(panic_call), span));
    }
    let else_block = TirBlock::new(else_stmts, span);

    // if let Some(d) = __saved { ... } else { fallback }.
    let saved_pattern_scrutinee = TirExpr::new(
        TirExprKind::Local {
            index: saved_local,
            name: "__saved".to_string(),
        },
        nullable_ref_type_id,
        span,
    );
    let pattern = TirPattern::Variant {
        enum_type: nullable_ref_type_id,
        variant_name: "Some".to_string(),
        bindings: vec![TirPattern::Binding {
            name: "__d".to_string(),
            local_index: d_local,
            type_id: inner_ref_type_id,
        }],
        payload_type: inner_ref_type_id,
    };
    stmts.push(TirStmt::new(
        TirStmtKind::IfLet {
            scrutinee: saved_pattern_scrutinee,
            pattern,
            then_block,
            else_block: Some(else_block),
        },
        span,
    ));

    let body = TirBlock::new(stmts, span);

    TirFunction {
        module_source: entry_source.clone(),
        name: wrapper_name,
        is_pub: false,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        task_return_type: None,
        // The wrapper is the implementation of `<E>::<op>`. It declares
        // effect `E` for two reasons:
        //
        // - For WASI effects, the fallback path (no handler installed)
        //   calls the existing `__cm_binding__<E>_<op>` adapter, which
        //   carries effect `E` itself. Declaring `E` on the wrapper
        //   matches the cm_binding convention so downstream phases that
        //   consult `effects` (DCE root walk, inliner, codegen) treat
        //   the wrapper consistently with a hand-written effect call.
        // - For user-defined effects, the fallback panics; declaring
        //   `E` is still the honest type — the wrapper *implements* `E`,
        //   even if its body never propagates the effect further.
        //
        // Effect-check has already run by this point, so this declaration
        // is documentation rather than an obligation; downstream passes
        // that re-walk effects (codegen export tables, `used_wasi_*`
        // tracking) get the right answer.
        effects: vec![EffectRef::Concrete {
            name: effect_name.to_string(),
            module_source: effect_module.clone(),
        }],
        stores: vec![],
        body: Some(body),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: true,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Read-only references the dispatch lowering walker needs at every
/// node: the per-effect [`DispatchPlan`] map, the impl-block index,
/// the shared `TypeTable` (for type queries during closure synthesis),
/// and the entry module source (for `Call`/`GlobalVarGet`/`GlobalVarSet`
/// on synthesized infrastructure).
///
/// Held by reference throughout the walk so `desugar_with_handler` can
/// look up plans without competing with the mutable borrow `LowerCtx`
/// (the local-allocation state) requires for `alloc_local`.
struct DispatchEnv<'a> {
    plans: &'a IndexMap<EffectKey, DispatchPlan>,
    impl_index: &'a IndexMap<HandlerImplKey, HandlerImplInfo>,
    type_table: std::rc::Rc<std::cell::RefCell<TypeTable>>,
    entry_source: ModuleSource,
}

/// Mutable context threaded through the dispatch-aware lowering walker.
///
/// Tracks the containing function's local table so the desugarer can
/// allocate fresh locals for `__h_<E>` / `__save_<E>` / `__d_<E>`
/// triples.
///
/// When the walker descends into a `TirExprKind::Closure` body, it
/// pushes a fresh closure-local scope so any nested `WithHandler` is
/// desugared into the closure's local-index space rather than the
/// outer function's. Closure-scope `local_types` is dropped on pop
/// because the lower phase rebuilds each closure's local table from
/// `Let` statements anyway.
#[allow(dead_code)]
struct LowerCtx {
    /// Stack of local-allocation scopes — one entry per active function
    /// or closure body. Top of stack is the innermost scope.
    scopes: Vec<LocalScope>,
}

/// One frame on `LowerCtx.scopes`.
///
/// The outermost frame is always a `Function` scope and owns the
/// caller's `local_types` vector — fresh locals are appended there and
/// the vector is moved back into `TirFunction.local_types` on exit.
///
/// Each `Closure` frame, pushed when the walker descends into a
/// `TirExprKind::Closure` body, owns a closure-local `next_local`
/// counter only — the lower phase rebuilds each closure functor's
/// local table from the body's `Let` statements, so we don't need to
/// track types here.
#[allow(dead_code)]
enum LocalScope {
    Function {
        next_local: u32,
        local_types: Vec<TypeId>,
    },
    Closure {
        next_local: u32,
    },
}

#[allow(dead_code)]
impl LowerCtx {
    fn alloc_local(&mut self, ty: TypeId) -> u32 {
        let scope = self.scopes.last_mut().expect("at least one scope");
        match scope {
            LocalScope::Function {
                next_local,
                local_types,
            } => {
                let idx = *next_local;
                *next_local += 1;
                local_types.push(ty);
                idx
            }
            LocalScope::Closure { next_local } => {
                let idx = *next_local;
                *next_local += 1;
                let _ = ty; // closure-local types are rebuilt later
                idx
            }
        }
    }
}

/// Walk every TIR function body / impl method body and replace each
/// `TirExprKind::WithHandler` with the desugared dispatch-protocol
/// block (save → build closures + dispatch struct → install global →
/// body → restore global). See [`desugar_with_handler`] for the shape
/// of the produced block.
#[allow(dead_code)]
fn lower_with_handler_dispatch_in_modules(
    project: &mut Package,
    plans: &IndexMap<EffectKey, DispatchPlan>,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
) {
    let entry_source = project.entry_module_source.clone();
    for module in project.tir_modules.values_mut() {
        let env = DispatchEnv {
            plans,
            impl_index,
            type_table: module.type_table.clone(),
            entry_source: entry_source.clone(),
        };
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            lower_with_handler_dispatch_in_func(&mut func, &env);
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                lower_with_handler_dispatch_in_func(method, &env);
            }
        }
    }
}

#[allow(dead_code)]
fn lower_with_handler_dispatch_in_func(func: &mut TirFunction, env: &DispatchEnv) {
    if let Some(body) = &mut func.body {
        let local_types = std::mem::take(&mut func.local_types);
        let mut ctx = LowerCtx {
            scopes: vec![LocalScope::Function {
                next_local: func.local_count,
                local_types,
            }],
        };
        lower_dispatch_in_block(body, env, &mut ctx);
        match ctx.scopes.pop().expect("function scope") {
            LocalScope::Function {
                next_local,
                local_types,
            } => {
                func.local_count = next_local;
                func.local_types = local_types;
            }
            LocalScope::Closure { .. } => {
                unreachable!("outermost scope must be a function frame")
            }
        }
    }
}

#[allow(dead_code)]
fn lower_dispatch_in_block(block: &mut TirBlock, env: &DispatchEnv, ctx: &mut LowerCtx) {
    for stmt in &mut block.stmts {
        lower_dispatch_in_stmt(stmt, env, ctx);
    }
}

#[allow(dead_code)]
fn lower_dispatch_in_stmt(stmt: &mut TirStmt, env: &DispatchEnv, ctx: &mut LowerCtx) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => lower_dispatch_in_expr(value, env, ctx),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                lower_dispatch_in_expr(v, env, ctx);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            lower_dispatch_in_block(body, env, ctx);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            lower_dispatch_in_expr(condition, env, ctx);
            lower_dispatch_in_block(then_block, env, ctx);
            if let Some(eb) = else_block {
                lower_dispatch_in_block(eb, env, ctx);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            lower_dispatch_in_expr(scrutinee, env, ctx);
            lower_dispatch_in_block(then_block, env, ctx);
            if let Some(eb) = else_block {
                lower_dispatch_in_block(eb, env, ctx);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            lower_dispatch_in_expr(value, env, ctx);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            lower_dispatch_in_expr(iterable, env, ctx);
            lower_dispatch_in_block(body, env, ctx);
        }
    }
}

#[allow(dead_code)]
fn lower_dispatch_in_expr(expr: &mut TirExpr, env: &DispatchEnv, ctx: &mut LowerCtx) {
    // `WithHandler` requires custom recursion: its handler-binding
    // expressions and body must be desugared before this node so any
    // inner `WithHandler` becomes a plain `Block` first; the outer
    // `desugar_with_handler` then consumes the (now inner-free)
    // bindings + body. Other expression kinds delegate to the generic
    // children walk.
    if let TirExprKind::WithHandler { bindings, body, .. } = &mut expr.kind {
        for binding in bindings {
            lower_dispatch_in_expr(&mut binding.handler, env, ctx);
        }
        lower_dispatch_in_block(body, env, ctx);
        desugar_with_handler(expr, env, ctx);
        return;
    }
    walk_dispatch_children(expr, env, ctx);
}

#[allow(dead_code)]
fn walk_dispatch_children(expr: &mut TirExpr, env: &DispatchEnv, ctx: &mut LowerCtx) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            lower_dispatch_in_block(block, env, ctx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            lower_dispatch_in_expr(condition, env, ctx);
            lower_dispatch_in_block(then_branch, env, ctx);
            if let Some(eb) = else_branch {
                lower_dispatch_in_block(eb, env, ctx);
            }
        }
        TirExprKind::Match { expr, arms } => {
            lower_dispatch_in_expr(expr, env, ctx);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    lower_dispatch_in_expr(g, env, ctx);
                }
                lower_dispatch_in_expr(&mut arm.body, env, ctx);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            lower_dispatch_in_expr(scrutinee, env, ctx);
            for arm in arms {
                lower_dispatch_in_block(arm, env, ctx);
            }
            lower_dispatch_in_block(default, env, ctx);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                lower_dispatch_in_expr(&mut arg.expr, env, ctx);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            lower_dispatch_in_expr(callee, env, ctx);
            for arg in args {
                lower_dispatch_in_expr(arg, env, ctx);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                lower_dispatch_in_expr(arg, env, ctx);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            lower_dispatch_in_expr(receiver, env, ctx);
            for arg in args {
                lower_dispatch_in_expr(&mut arg.expr, env, ctx);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            lower_dispatch_in_expr(left, env, ctx);
            lower_dispatch_in_expr(right, env, ctx);
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => {
            lower_dispatch_in_expr(expr, env, ctx);
        }
        TirExprKind::Assign { target, value } => {
            lower_dispatch_in_expr(target, env, ctx);
            lower_dispatch_in_expr(value, env, ctx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            lower_dispatch_in_expr(value, env, ctx);
        }
        TirExprKind::Index { expr, index } => {
            lower_dispatch_in_expr(expr, env, ctx);
            lower_dispatch_in_expr(index, env, ctx);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                lower_dispatch_in_expr(&mut field.value, env, ctx);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                lower_dispatch_in_expr(elem, env, ctx);
            }
        }
        TirExprKind::Closure { params, body, .. } => {
            // Push a closure-local scope. The closure body's lets and
            // any nested `WithHandler` desugaring allocate locals in
            // this scope. `next_local` starts past the closure's own
            // params plus any pre-existing body lets we discover.
            let body_max = max_local_index_in_expr(body);
            let start = (params.len() as u32).max(body_max + 1);
            ctx.scopes.push(LocalScope::Closure { next_local: start });
            lower_dispatch_in_expr(body, env, ctx);
            ctx.scopes.pop();
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                lower_dispatch_in_expr(p, env, ctx);
            }
        }
        TirExprKind::Resume { value } => {
            lower_dispatch_in_expr(value, env, ctx);
        }
        TirExprKind::WithHandler { .. } => {
            unreachable!(
                "WithHandler is recursed and desugared inside \
                 lower_dispatch_in_expr; walk_dispatch_children should \
                 never see it"
            );
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    lower_dispatch_in_expr(expr, env, ctx);
                }
            }
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Return the largest local index already in use anywhere inside
/// `expr` — both in `Local { index }` reads and in binding sites
/// (`Let.local_index`, `IfLet`/`LetDestructure`/`Match` arm patterns,
/// `VariadicForOf.binding_local`). Returns `0` if no locals are
/// referenced.
///
/// Used to seed a closure body's local counter at synthesis time —
/// the lower phase rebuilds each closure functor's local table later
/// from `Let` statements, but we must not collide with indices the
/// body already uses.
///
/// The walk does **not** descend into nested closures; their locals
/// live in a different scope.
#[allow(dead_code)]
fn max_local_index_in_expr(expr: &TirExpr) -> u32 {
    use crate::tir_visitor::TirRefVisitor;
    let mut visitor = MaxLocalIndex(0);
    visitor.visit_expr(expr);
    visitor.0
}

struct MaxLocalIndex(u32);

impl MaxLocalIndex {
    fn note(&mut self, idx: u32) {
        if idx > self.0 {
            self.0 = idx;
        }
    }

    fn walk_pattern(&mut self, pattern: &crate::tir::TirPattern) {
        use crate::tir::TirPattern;
        match pattern {
            TirPattern::Binding { local_index, .. } => self.note(*local_index),
            TirPattern::Tuple(items, _) | TirPattern::Or(items) => {
                for p in items {
                    self.walk_pattern(p);
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for p in bindings {
                    self.walk_pattern(p);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    self.walk_pattern(&f.pattern);
                }
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {}
        }
    }
}

impl crate::tir_visitor::TirRefVisitor for MaxLocalIndex {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.note(*index),
            TirExprKind::Closure { .. } => return, // separate scope
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.walk_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&arm.body);
                }
                return;
            }
            _ => {}
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { local_index, .. } => self.note(*local_index),
            TirStmtKind::IfLet { pattern, .. }
            | TirStmtKind::LetDestructure { pattern, .. } => self.walk_pattern(pattern),
            TirStmtKind::VariadicForOf { binding_local, .. } => self.note(*binding_local),
            _ => {}
        }
        self.walk_stmt(stmt);
    }
}

/// Replace a `WithHandler { bindings, body }` expression in place
/// with the desugared dispatch-protocol block.
///
/// Per binding (in source order, all run before the body executes):
///
/// ```text
/// let __h_<E>    = handler_expr;
/// let __save_<E> = global.get __effect_<E>;
/// let __d_<E>    = __Dispatch_<E> {
///     outer:       __save_<E>,
///     op_<n>:      |args| __h_<E>.<E>::<op_n>(args),
///     ...
/// };
/// global.set __effect_<E> = Some(&__d_<E>);
/// ```
///
/// After the body, in reverse install order:
///
/// ```text
/// global.set __effect_<E> = __save_<E>;
/// ```
///
/// Bindings whose effect is unresolved (`EffectRef::Param`) or whose
/// handler type doesn't match any synthesised plan are skipped (a
/// no-op for that binding) — the resolver / effect-check should have
/// caught such cases earlier.
#[allow(dead_code)]
fn desugar_with_handler(expr: &mut TirExpr, env: &DispatchEnv, ctx: &mut LowerCtx) {
    let span = expr.span;
    let result_type = expr.type_id;

    let TirExprKind::WithHandler { bindings, body, .. } =
        std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        return;
    };

    let mut prelude: Vec<TirStmt> = Vec::new();
    let mut restore: Vec<TirStmt> = Vec::new();

    for binding in bindings {
        let (effect_name, effect_module) = match binding.effect.clone() {
            Some(EffectRef::Concrete {
                name,
                module_source,
            }) => (name, module_source),
            Some(EffectRef::Param { name }) => panic!(
                "effect-dispatch synthesis received an unresolved \
                 `EffectRef::Param {{ name: {name:?} }}` in a `with` \
                 binding — the resolver should have substituted it \
                 with a concrete effect before this pass runs"
            ),
            None => panic!(
                "effect-dispatch synthesis received a `with` binding \
                 without a resolved effect — effect-check should have \
                 rejected this earlier"
            ),
        };
        let key: EffectKey = (effect_module.clone(), effect_name.clone());
        let plan = env.plans.get(&key).unwrap_or_else(|| {
            panic!(
                "effect-dispatch synthesis: no DispatchPlan for \
                 effect `{effect_name}` in module {effect_module:?} — \
                 `identify_active_effects` and \
                 `synthesize_dispatch_infrastructure` are out of sync"
            )
        });
        let handler_type = binding.handler.type_id;
        let handler_underlying = deref_type(&env.type_table.borrow(), handler_type);
        let handler_type_name = env.type_table.borrow().type_name(handler_underlying);
        let impl_key: HandlerImplKey =
            (handler_type_name.clone(), effect_module.clone(), effect_name.clone());
        let impl_info = env.impl_index.get(&impl_key).unwrap_or_else(|| {
            panic!(
                "effect-dispatch synthesis: no `impl {effect_name} for \
                 {handler_type_name}` registered in the impl index for \
                 effect module {effect_module:?} — the resolver should \
                 have rejected the `with {effect_name} = h do` binding \
                 if no matching impl exists"
            )
        });

        // 1. let __h_<E> = handler_expr;
        let h_local = ctx.alloc_local(handler_type);
        let h_name = format!("__h_{effect_name}");
        prelude.push(TirStmt::new(
            TirStmtKind::Let {
                name: h_name.clone(),
                local_index: h_local,
                is_mut: false,
                is_reactive: false,
                type_id: handler_type,
                value: binding.handler.clone(),
                skip_value_copy: true,
            },
            span,
        ));

        // 2. let __save_<E> = global.get __effect_<E>;
        let save_local = ctx.alloc_local(plan.nullable_ref_type_id);
        let save_name = format!("__save_{effect_name}");
        let global_get = TirExpr::new(
            TirExprKind::GlobalVarGet {
                module_source: env.entry_source.clone(),
                name: plan.global_name.clone(),
            },
            plan.nullable_ref_type_id,
            span,
        );
        prelude.push(TirStmt::new(
            TirStmtKind::Let {
                name: save_name.clone(),
                local_index: save_local,
                is_mut: false,
                is_reactive: false,
                type_id: plan.nullable_ref_type_id,
                value: global_get,
                skip_value_copy: true,
            },
            span,
        ));

        // 3. let __d_<E> = __Dispatch_<E> { outer: __save_<E>, op_n: ..., ... };
        let mut struct_fields: Vec<TirStructField> = Vec::new();
        struct_fields.push(TirStructField {
            name: "outer".to_string(),
            value: TirExpr::new(
                TirExprKind::Local {
                    index: save_local,
                    name: save_name.clone(),
                },
                plan.nullable_ref_type_id,
                span,
            ),
            field_index: 0,
        });
        for op in &plan.operations {
            // The `..` rest pattern in `impl Effect for T` lets a handler
            // implement only some of the effect's operations; calls to
            // un-implemented ops trap at runtime. Emit a normal forwarding
            // closure for ops the impl actually defines and a trapping
            // stub closure for the rest.
            let closure_expr = if impl_info.methods.contains_key(&op.name) {
                build_handler_op_closure(
                    op,
                    &effect_name,
                    &impl_info,
                    handler_type,
                    h_local,
                    &h_name,
                    &env.type_table,
                )
            } else {
                build_trap_closure(op, &env.type_table)
            };
            let field_index = *plan.field_indices.get(&op.name).unwrap();
            let field_name = plan.field_names.get(&op.name).unwrap().clone();
            struct_fields.push(TirStructField {
                name: field_name,
                value: closure_expr,
                field_index,
            });
        }
        let d_local = ctx.alloc_local(plan.struct_type_id);
        let d_name = format!("__d_{effect_name}");
        let struct_lit = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: plan.struct_type_id,
                struct_name: format!("__Dispatch_{effect_name}"),
                fields: struct_fields,
            },
            plan.struct_type_id,
            span,
        );
        prelude.push(TirStmt::new(
            TirStmtKind::Let {
                name: d_name.clone(),
                local_index: d_local,
                is_mut: false,
                is_reactive: false,
                type_id: plan.struct_type_id,
                value: struct_lit,
                skip_value_copy: true,
            },
            span,
        ));

        // 4. global.set __effect_<E> = Some(&__d_<E>);
        let d_local_expr = TirExpr::new(
            TirExprKind::Local {
                index: d_local,
                name: d_name,
            },
            plan.struct_type_id,
            span,
        );
        let d_ref = ref_expr(d_local_expr, plan.inner_ref_type_id, span);
        let some_d_ref = option_some(d_ref, plan.nullable_ref_type_id);
        prelude.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::GlobalVarSet {
                    module_source: env.entry_source.clone(),
                    name: plan.global_name.clone(),
                    value: Box::new(some_d_ref),
                },
                TypeTable::UNIT,
                span,
            )),
            span,
        ));

        // Queue restore (will be applied in reverse install order).
        let saved_expr = TirExpr::new(
            TirExprKind::Local {
                index: save_local,
                name: save_name,
            },
            plan.nullable_ref_type_id,
            span,
        );
        restore.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::GlobalVarSet {
                    module_source: env.entry_source.clone(),
                    name: plan.global_name.clone(),
                    value: Box::new(saved_expr),
                },
                TypeTable::UNIT,
                span,
            )),
            span,
        ));
    }

    // Compose the desugared block: prelude + body.stmts + reversed restore.
    let mut stmts: Vec<TirStmt> =
        Vec::with_capacity(prelude.len() + body.stmts.len() + restore.len());
    stmts.extend(prelude);
    stmts.extend(body.stmts);
    stmts.extend(restore.into_iter().rev());

    expr.kind = TirExprKind::Block(TirBlock::new(stmts, span));
    expr.type_id = result_type;
}

/// Build the `op_<n>` closure for a single (effect, op, handler-impl)
/// triple.
///
/// Shape:
///
/// ```text
/// |<op_params>| __h_<E>.<E>::<op>(<op_params>)
/// ```
///
/// The handler value is captured by reference / value (whatever
/// `__h_<E>` holds — typically `&T` or `&mut T`); the closure body's
/// receiver is a `TirExprKind::Capture { index: 0 }` that the
/// lower-phase closure pass converts into a field access on the
/// generated functor struct.
#[allow(dead_code)]
fn build_handler_op_closure(
    op: &TirEffectOp,
    effect_name: &str,
    impl_info: &HandlerImplInfo,
    handler_type: TypeId,
    h_local_index: u32,
    h_name: &str,
    type_table: &std::rc::Rc<std::cell::RefCell<TypeTable>>,
) -> TirExpr {
    let span = synth_span();

    // Closure params (mirror the op's params; closure-local indices
    // start fresh at 0).
    let closure_params: Vec<(String, TypeId)> = op
        .params
        .iter()
        .map(|p| (p.name.clone(), p.type_id))
        .collect();
    let arg_call_args: Vec<CallArg> = op
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            CallArg::new(
                TirExpr::new(
                    TirExprKind::Local {
                        index: i as u32,
                        name: p.name.clone(),
                    },
                    p.type_id,
                    span,
                ),
                false,
            )
        })
        .collect();

    // Body: __h.<E>::<op>(<args>)
    let receiver = TirExpr::new(
        TirExprKind::Capture {
            index: 0,
            name: h_name.to_string(),
        },
        handler_type,
        span,
    );
    let mangled = LocalMethodName::new(
        impl_info.handler_type_name.clone(),
        Some(effect_name.to_string()),
        op.name.clone(),
    );
    let mangled_name = mangled.to_mangled_name();
    let method_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_info.impl_module.clone(),
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(mangled),
            },
            vec![],
            arg_call_args,
        ),
        op.return_type,
        span,
    );

    let captures = vec![TirCapture {
        name: h_name.to_string(),
        outer_index: h_local_index,
        type_id: handler_type,
        is_mut: false,
    }];

    let param_types: Vec<TypeId> = closure_params.iter().map(|(_, t)| *t).collect();
    let func_type =
        type_table
            .borrow_mut()
            .make_function(param_types, op.return_type, vec![], vec![]);

    TirExpr::new(
        TirExprKind::Closure {
            params: closure_params,
            body: Box::new(method_call),
            captures,
            functor_id: None,
            source_text: None,
        },
        func_type,
        span,
    )
}

/// Build a trapping stub closure for an effect operation that the
/// installed handler does not implement (i.e. the operation falls
/// under the `..` wildcard in `impl Effect for T { ... .. }`).
///
/// Body shape:
///
/// ```text
/// |<op_params>| { unreachable() }
/// ```
///
/// `unreachable` returns `Never`, which subtypes any return type, so
/// the closure is well-typed at `fn(<op_params>) -> <op_ret>`. The
/// stub captures nothing, mirroring the WEP's "trap stub funcref" for
/// wildcard-handled operations.
#[allow(dead_code)]
fn build_trap_closure(
    op: &TirEffectOp,
    type_table: &std::rc::Rc<std::cell::RefCell<TypeTable>>,
) -> TirExpr {
    let span = synth_span();
    let closure_params: Vec<(String, TypeId)> = op
        .params
        .iter()
        .map(|p| (p.name.clone(), p.type_id))
        .collect();
    let trap_call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::builtin(),
                name: "unreachable".to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: vec![],
        },
        op.return_type,
        span,
    );
    let param_types: Vec<TypeId> = closure_params.iter().map(|(_, t)| *t).collect();
    let func_type =
        type_table
            .borrow_mut()
            .make_function(param_types, op.return_type, vec![], vec![]);
    TirExpr::new(
        TirExprKind::Closure {
            params: closure_params,
            body: Box::new(trap_call),
            captures: Vec::new(),
            functor_id: None,
            source_text: None,
        },
        func_type,
        span,
    )
}


/// Rewrite every effect-operation call site so it routes through a
/// dispatch wrapper.
///
/// Two call shapes are recognised:
///
/// 1. **WASI-binding shape**: a `Call` whose `func.name` is
///    `__cm_binding__<E>_<op>` (any module). Produced by the
///    `cm_binding` synthesis pass for every used WASI effect call.
/// 2. **User-effect shape**: a `Call` whose `func.module_source` is
///    `ModuleSource::Local { path: "<E>" }` and `func.name` is the
///    bare op name. Produced by the resolver's `local_namespace`
///    callee for user-defined effect calls (`MyEffect::op(...)`).
///
/// Both rewrite to a `Call` to `__effect_dispatch__<E>__<op>` in the
/// entry module. The function's `args` and the call-expression's
/// `type_id` are preserved.
///
/// Calls inside `__effect_dispatch__*` wrappers and `__cm_binding__*`
/// adapters are left alone — they belong to the synthesised
/// infrastructure and rewriting them would either loop forever or
/// break the WASI fallback path.
#[allow(dead_code)]
fn rewrite_call_sites_to_wrappers(
    project: &mut Package,
    plans: &IndexMap<EffectKey, DispatchPlan>,
) {
    let entry_source = project.entry_module_source.clone();

    // Pre-build O(1) lookup maps.
    let mut binding_to_wrapper: IndexMap<String, String> = IndexMap::default();
    let mut user_to_wrapper: IndexMap<(String, String), String> = IndexMap::default();
    for ((_, effect_name), plan) in plans {
        for (op_name, wrapper_name) in &plan.wrapper_names {
            binding_to_wrapper.insert(
                format!("__cm_binding__{effect_name}_{op_name}"),
                wrapper_name.clone(),
            );
            user_to_wrapper.insert((effect_name.clone(), op_name.clone()), wrapper_name.clone());
        }
    }

    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if func.is_dispatch_wrapper || func.is_cm_binding {
                continue;
            }
            if let Some(body) = &mut func.body {
                rewrite_calls_in_block(body, &binding_to_wrapper, &user_to_wrapper, &entry_source);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if method.is_dispatch_wrapper || method.is_cm_binding {
                    continue;
                }
                if let Some(body) = &mut method.body {
                    rewrite_calls_in_block(
                        body,
                        &binding_to_wrapper,
                        &user_to_wrapper,
                        &entry_source,
                    );
                }
            }
        }
    }
}

#[allow(dead_code)]
fn rewrite_calls_in_block(
    block: &mut TirBlock,
    binding_to_wrapper: &IndexMap<String, String>,
    user_to_wrapper: &IndexMap<(String, String), String>,
    entry_source: &ModuleSource,
) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, binding_to_wrapper, user_to_wrapper, entry_source);
    }
}

#[allow(dead_code)]
fn rewrite_calls_in_stmt(
    stmt: &mut TirStmt,
    binding_to_wrapper: &IndexMap<String, String>,
    user_to_wrapper: &IndexMap<(String, String), String>,
    entry_source: &ModuleSource,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => {
            rewrite_calls_in_expr(value, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_block(
                then_block,
                binding_to_wrapper,
                user_to_wrapper,
                entry_source,
            );
            if let Some(eb) = else_block {
                rewrite_calls_in_block(eb, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_block(
                then_block,
                binding_to_wrapper,
                user_to_wrapper,
                entry_source,
            );
            if let Some(eb) = else_block {
                rewrite_calls_in_block(eb, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_calls_in_expr(value, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            rewrite_calls_in_expr(iterable, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_block(body, binding_to_wrapper, user_to_wrapper, entry_source);
        }
    }
}

#[allow(dead_code)]
fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    binding_to_wrapper: &IndexMap<String, String>,
    user_to_wrapper: &IndexMap<(String, String), String>,
    entry_source: &ModuleSource,
) {
    // Recurse into children first.
    rewrite_call_children(expr, binding_to_wrapper, user_to_wrapper, entry_source);

    // Then check if THIS expression is a Call worth rewriting.
    if let TirExprKind::Call { func, .. } = &expr.kind {
        let wrapper_name = match_effect_call(func, binding_to_wrapper, user_to_wrapper);
        if let Some(name) = wrapper_name
            && let TirExprKind::Call {
                args,
                type_args: _,
                func: _,
            } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        {
            expr.kind = TirExprKind::Call {
                func: FunctionRef {
                    module_source: entry_source.clone(),
                    name,
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args,
            };
        }
    }
}

#[allow(dead_code)]
fn match_effect_call(
    func: &FunctionRef,
    binding_to_wrapper: &IndexMap<String, String>,
    user_to_wrapper: &IndexMap<(String, String), String>,
) -> Option<String> {
    if let Some(wrapper) = binding_to_wrapper.get(&func.name) {
        return Some(wrapper.clone());
    }
    if let ModuleSource::Local { path } = &func.module_source
        && let Some(wrapper) = user_to_wrapper.get(&(path.clone(), func.name.clone()))
    {
        return Some(wrapper.clone());
    }
    None
}

#[allow(dead_code)]
fn rewrite_call_children(
    expr: &mut TirExpr,
    binding_to_wrapper: &IndexMap<String, String>,
    user_to_wrapper: &IndexMap<(String, String), String>,
    entry_source: &ModuleSource,
) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_block(
                then_branch,
                binding_to_wrapper,
                user_to_wrapper,
                entry_source,
            );
            if let Some(eb) = else_branch {
                rewrite_calls_in_block(eb, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_calls_in_expr(expr, binding_to_wrapper, user_to_wrapper, entry_source);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_calls_in_expr(g, binding_to_wrapper, user_to_wrapper, entry_source);
                }
                rewrite_calls_in_expr(
                    &mut arm.body,
                    binding_to_wrapper,
                    user_to_wrapper,
                    entry_source,
                );
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, binding_to_wrapper, user_to_wrapper, entry_source);
            for arm in arms {
                rewrite_calls_in_block(arm, binding_to_wrapper, user_to_wrapper, entry_source);
            }
            rewrite_calls_in_block(default, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    binding_to_wrapper,
                    user_to_wrapper,
                    entry_source,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, binding_to_wrapper, user_to_wrapper, entry_source);
            for arg in args {
                rewrite_calls_in_expr(arg, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, binding_to_wrapper, user_to_wrapper, entry_source);
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    binding_to_wrapper,
                    user_to_wrapper,
                    entry_source,
                );
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_expr(right, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => {
            rewrite_calls_in_expr(expr, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_calls_in_expr(target, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_expr(value, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::Index { expr, index } => {
            rewrite_calls_in_expr(expr, binding_to_wrapper, user_to_wrapper, entry_source);
            rewrite_calls_in_expr(index, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                rewrite_calls_in_expr(
                    &mut field.value,
                    binding_to_wrapper,
                    user_to_wrapper,
                    entry_source,
                );
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                rewrite_calls_in_expr(elem, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_calls_in_expr(p, binding_to_wrapper, user_to_wrapper, entry_source);
            }
        }
        TirExprKind::Resume { value } => {
            rewrite_calls_in_expr(value, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                rewrite_calls_in_expr(
                    &mut binding.handler,
                    binding_to_wrapper,
                    user_to_wrapper,
                    entry_source,
                );
            }
            rewrite_calls_in_block(body, binding_to_wrapper, user_to_wrapper, entry_source);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    rewrite_calls_in_expr(expr, binding_to_wrapper, user_to_wrapper, entry_source);
                }
            }
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Run the effect dispatch synthesis pass on the package.
///
/// Returns a `Result` so a future implementation that rejects malformed
/// input has a place to report errors. The MVP currently never fails.
pub fn synthesize(mut project: Package) -> Result<Package, String> {
    lower_resume_in_handler_methods(&mut project);

    let effect_index = build_effect_index(&project);
    let impl_index = build_handler_impl_index(&project, &effect_index);
    let active_effects = identify_active_effects(&impl_index);

    if active_effects.is_empty() {
        return Ok(project);
    }

    let plans = synthesize_dispatch_infrastructure(&mut project, &effect_index, &active_effects);
    lower_with_handler_dispatch_in_modules(&mut project, &plans, &impl_index);
    rewrite_call_sites_to_wrappers(&mut project, &plans);
    Ok(project)
}

/// Orchestrates per-effect dispatch infrastructure synthesis.
///
/// Three phases per active effect:
///
/// 1. [`synthesize_dispatch_struct`] — interns the recursive
///    `__Dispatch_<E>` Wasm GC struct type and adds the `TirStruct` decl
///    to the entry module.
/// 2. [`synthesize_dispatch_global`] — appends the `__effect_<E>`
///    mutable global initialised to `null`.
/// 3. [`synthesize_dispatch_wrappers`] — emits one
///    `__effect_dispatch__<E>__<op>` wrapper function per declared
///    operation.
///
/// Returns the per-effect [`DispatchPlan`] map the lowering /
/// call-site-rewriting passes consume.
fn synthesize_dispatch_infrastructure(
    project: &mut Package,
    effect_index: &IndexMap<EffectKey, EffectMeta>,
    active_effects: &IndexSet<EffectKey>,
) -> IndexMap<EffectKey, DispatchPlan> {
    let entry_source = project.entry_module_source.clone();
    let mut plans: IndexMap<EffectKey, DispatchPlan> = IndexMap::default();
    for key in active_effects {
        let meta = effect_index
            .get(key)
            .expect("active effect must have an entry in the effect index");
        let plan = synthesize_dispatch_struct(project, &entry_source, key, meta);
        plans.insert(key.clone(), plan);
    }
    for plan in plans.values() {
        synthesize_dispatch_global(project, &entry_source, plan);
    }
    for (key, plan) in &plans {
        synthesize_dispatch_wrappers(project, &entry_source, key, plan);
    }
    plans
}

/// Canonical identity of an `impl <Effect> for <Type>` block:
/// `(handler_type_name, effect_defining_module, effect_name)`.
///
/// The effect-defining module is resolved from the bare `trait_name`
/// recorded in `LocalMethodName` against the project's effect-index;
/// keying by that triple (rather than `(struct_name, effect_name)`
/// alone) prevents collisions when two modules declare an effect with
/// the same name.
type HandlerImplKey = (String, ModuleSource, String);

#[derive(Debug, Clone)]
struct HandlerImplInfo {
    /// Module that owns the impl block — used to build the
    /// `FunctionRef::module_source` of the generated `MethodCall`.
    impl_module: ModuleSource,
    /// Per-method return type from the impl. Lets the lowering patch up
    /// the result type of a synthesised `MethodCall` even when the
    /// original `Counter::next()` `Call` came in with `Unit` (the
    /// resolver doesn't know the operation's return type for
    /// user-defined effects without a CM binding).
    methods: IndexMap<String, TypeId>,
    /// Handler struct's type name (the `T` in `impl Effect for T`).
    /// Used to build the mangled method name in generated `MethodCall`s.
    handler_type_name: String,
}

/// Walk every TIR function tagged with `method_info.trait_name` and
/// build a canonical `HandlerImplKey -> HandlerImplInfo` map.
///
/// `effect_index` provides the canonical `(module, name)` of every
/// effect declaration; each impl's `trait_name` is resolved by name
/// against that index. Impl methods whose `trait_name` does not match
/// any effect declaration belong to a regular (non-effect) trait and
/// are skipped.
fn build_handler_impl_index(
    project: &Package,
    effect_index: &IndexMap<EffectKey, EffectMeta>,
) -> IndexMap<HandlerImplKey, HandlerImplInfo> {
    // Build a name -> defining module lookup so trait_name can be
    // canonicalised in a single pass. If two effect declarations share
    // a name across modules, refuse to build the index — the resolver
    // should already have rejected that case, and silently picking
    // either canonical entry would be incorrect.
    let mut effect_module_for_name: IndexMap<String, ModuleSource> = IndexMap::default();
    for (module_source, name) in effect_index.keys() {
        if let Some(prev) = effect_module_for_name.insert(name.clone(), module_source.clone())
            && &prev != module_source
        {
            panic!(
                "duplicate effect name `{name}` across modules {prev:?} and {module_source:?}; \
                 dispatch synthesis cannot canonicalise impl blocks unambiguously"
            );
        }
    }

    let mut out: IndexMap<HandlerImplKey, HandlerImplInfo> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            let Some(method_info) = &func.method_info else {
                continue;
            };
            let Some(trait_name) = &method_info.trait_name else {
                continue;
            };
            let Some(effect_module) = effect_module_for_name.get(trait_name) else {
                // Not an effect — skip (the trait_name is from a regular
                // user trait or auto-derived prelude trait).
                continue;
            };
            let key: HandlerImplKey = (
                method_info.struct_name.clone(),
                effect_module.clone(),
                trait_name.clone(),
            );
            let entry = out.entry(key).or_insert_with(|| HandlerImplInfo {
                impl_module: module_source.clone(),
                methods: IndexMap::default(),
                handler_type_name: method_info.struct_name.clone(),
            });
            entry
                .methods
                .insert(method_info.method_name.clone(), func.return_type);
        }
    }
    out
}

/// Strip a single leading `&` / `&mut` layer to find the underlying
/// struct type that an `impl Effect for T` block targets.
fn deref_type(tt: &TypeTable, type_id: TypeId) -> TypeId {
    use crate::tir::ResolvedType;
    match tt.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
        _ => type_id,
    }
}

/// Walk every method in every `impl Effect for T` block (where `Effect` is
/// an actual effect declaration) and rewrite `Resume { value }` to
/// `Return { value }`. Per the WEP MVP, `resume` has no post-resume
/// continuation, so it is semantically identical to `return`.
///
/// The resolver flattens impl-block methods into `TirModule.functions`
/// and tags each with `method_info: { struct_name, trait_name, ... }`.
/// We use the `trait_name` to recognise handler methods.
fn lower_resume_in_handler_methods(project: &mut Package) {
    // Collect bare names of every effect declaration so we can recognise
    // candidate impl blocks. A name collision between an effect and a
    // regular trait would already be a resolver error.
    let mut effect_names: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for effect in &module.effects {
            effect_names.insert(effect.name.clone());
        }
    }

    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            let Some(method_info) = &func.method_info else {
                continue;
            };
            let Some(trait_name) = &method_info.trait_name else {
                continue;
            };
            if !effect_names.contains(trait_name) {
                continue;
            }
            if let Some(body) = &mut func.body {
                rewrite_resume_in_block(body);
            }
        }
    }
}

/// Rewrite every `Resume { value }` in the block to a `Return { value }`
/// statement. The resume expression itself yields `Unit` at the source
/// level — when it sits at statement position it becomes a real return;
/// when it appears as a sub-expression we leave it as-is and rely on
/// `Return` short-circuiting the enclosing computation. The MVP fixtures
/// only place `resume` at statement position, so the simple statement-
/// level rewrite is sufficient.
fn rewrite_resume_in_block(block: &mut TirBlock) {
    for stmt in &mut block.stmts {
        rewrite_resume_in_stmt(stmt);
    }
}

fn rewrite_resume_in_stmt(stmt: &mut TirStmt) {
    // Statement-position `resume value;` is parsed as
    // `TirStmtKind::Expr(TirExpr { kind: Resume { value }, .. })`.
    // Replace it with `TirStmtKind::Return { value }`.
    // `resume value;` at statement position becomes `return value;`.
    if let TirStmtKind::Expr(expr) = &mut stmt.kind
        && let TirExprKind::Resume { value } = &mut expr.kind
    {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span);
        let value = std::mem::replace(value.as_mut(), placeholder);
        stmt.kind = TirStmtKind::Return { value: Some(value) };
        return;
    }
    // `return resume value;` (which the resolver synthesises when a
    // method body's tail expression is `resume`, because the
    // missing-return rewriter sees `Resume { value }` in expression
    // position and wraps it in `Return { value: Some(Resume { ... }) }`)
    // collapses to `return value;`.
    if let TirStmtKind::Return { value: Some(value) } = &mut stmt.kind
        && let TirExprKind::Resume { value: inner } = &mut value.kind
    {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, value.span);
        let inner = std::mem::replace(inner.as_mut(), placeholder);
        stmt.kind = TirStmtKind::Return { value: Some(inner) };
        return;
    }
    // Recurse into sub-statements; expression-position resumes inside
    // composite expressions are still walked but currently left as-is
    // (the e2e MVP has no such uses; if they appear, the `unreachable!`
    // stub in the lower phase will catch them).
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => rewrite_resume_in_expr(value),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_resume_in_expr(v);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_resume_in_block(body);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_resume_in_expr(condition);
            rewrite_resume_in_block(then_block);
            if let Some(eb) = else_block {
                rewrite_resume_in_block(eb);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_resume_in_expr(scrutinee);
            rewrite_resume_in_block(then_block);
            if let Some(eb) = else_block {
                rewrite_resume_in_block(eb);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => rewrite_resume_in_expr(value),
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            rewrite_resume_in_expr(iterable);
            rewrite_resume_in_block(body);
        }
    }
}

fn rewrite_resume_in_expr(expr: &mut TirExpr) {
    // Walk into all sub-expressions so nested handler methods (closures,
    // labelled blocks, etc.) get their statement-level `resume` rewritten
    // too. Expression-level rewriting is not needed for the MVP.
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_resume_in_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_resume_in_expr(condition);
            rewrite_resume_in_block(then_branch);
            if let Some(eb) = else_branch {
                rewrite_resume_in_block(eb);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_resume_in_expr(expr);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_resume_in_expr(g);
                }
                rewrite_resume_in_expr(&mut arm.body);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_resume_in_expr(scrutinee);
            for arm in arms {
                rewrite_resume_in_block(arm);
            }
            rewrite_resume_in_block(default);
        }
        TirExprKind::Closure { body, .. } => rewrite_resume_in_expr(body),
        _ => {}
    }
}
