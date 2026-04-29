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
    CallArg, EffectRef, FunctionKind, FunctionRef, InlineHint, SubstitutionContext, TirBlock,
    TirCapture, TirEffectOp, TirExpr, TirExprKind, TirField, TirFunction, TirGlobal, TirParam,
    TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TirTemplatePart, TypeId,
    TypeTable,
};

/// Canonical identity of an effect or resource **declaration**:
/// `(defining_module, base_name)`.
///
/// One declaration may be instantiated many ways at use sites — generic
/// resources like `Stream<T>` produce a separate instantiation per
/// `(T = u8)`, `(T = i32)`, etc., so call-site dispatch needs the
/// `InstantiationKey` form below. The `EffectKey` itself stays bound to
/// the declaration: `EffectMeta` carries the **template** operation list
/// (with type-params unsubstituted) and is shared across every
/// instantiation that needs the same template.
type EffectKey = (ModuleSource, String);

/// Canonical identity of a per-monomorphisation dispatch instantiation:
/// `(defining_module, base_name, type_args)`.
///
/// For non-generic effects / resources the `type_args` slot is empty and
/// the key is essentially the `EffectKey` widened with `[]`; the dispatch
/// infrastructure (`__Dispatch_<E>`, `__effect_<E>`, per-op wrappers)
/// continues to live one-set-per-declaration. For generic resources, each
/// distinct argument tuple gets its own dispatch struct / global / wrapper
/// triple, with the resource declaration's operation types substituted at
/// the type-param indices recorded by the resource decl.
pub type InstantiationKey = (ModuleSource, String, Vec<TypeId>);

/// Metadata for a single effect or resource, indexed by `EffectKey`.
#[derive(Debug, Clone)]
struct EffectMeta {
    /// The operation declarations (in source order) — copied so later
    /// passes don't have to walk the module tree again. `TirResource`
    /// reuses `Vec<TirEffectOp>` so this field is identical for both
    /// kinds.
    operations: Vec<TirEffectOp>,
    /// `true` when this entry comes from a `pub resource R { ... }`
    /// declaration rather than a `pub effect E { ... }`. Resources are
    /// not effects in Wado's effect system: their dispatch wrapper must
    /// declare `effects: vec![]` to match the `cm_binding` adapter and to
    /// avoid teaching downstream effect-tracking phases (DCE, codegen
    /// `used_wasi_*`) that "Fields" is a side effect of the wrapper.
    is_resource: bool,
}

/// Walk every TIR module and build an `EffectKey -> EffectMeta` index
/// covering both `effect` and `resource` declarations. Resources participate
/// in the dispatch protocol identically to effects — same operation list,
/// same wrapper synthesis, same call-site rewriting against
/// `__cm_binding__<R>_<op>` adapters — so they share a single index. The
/// `is_resource` flag distinguishes the kinds where it matters (currently
/// only the wrapper's declared `effects` list).
fn build_effect_index(project: &Package) -> IndexMap<EffectKey, EffectMeta> {
    let mut out: IndexMap<EffectKey, EffectMeta> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for effect in &module.effects {
            out.insert(
                (module_source.clone(), effect.name.clone()),
                EffectMeta {
                    operations: effect.operations.clone(),
                    is_resource: false,
                },
            );
        }
        for resource in &module.resources {
            out.insert(
                (module_source.clone(), resource.name.clone()),
                EffectMeta {
                    operations: resource.operations.clone(),
                    is_resource: true,
                },
            );
        }
    }
    out
}

/// Identify which (effect/resource, type-arg) instantiations need
/// dispatch infrastructure.
///
/// An instantiation is "active" iff at least one user-written
/// `impl <Effect> for <Type>` (or `impl <Resource><Args> for <Type>`)
/// block exists for it in the package. Each distinct instantiation —
/// `impl Stream<u8>` vs `impl Stream<i32>` — gets its own entry so the
/// dispatch synthesis emits one infra triple per monomorphisation.
/// Non-generic effects collapse to a single instantiation with empty
/// `type_args`.
///
/// Reads the canonical `(effect_module, base_name, trait_type_args)`
/// triple off each `HandlerImplKey` directly — no name-only matching
/// against `effect_index` — so two effects sharing a name across modules
/// cannot be conflated.
fn identify_active_effects(
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
) -> IndexSet<InstantiationKey> {
    impl_index
        .keys()
        .map(|(_struct, effect_module, base_name, type_args)| {
            (effect_module.clone(), base_name.clone(), type_args.clone())
        })
        .collect()
}

/// All the bookkeeping needed to emit / refer to the dispatch
/// infrastructure for a single effect.
///
/// A `DispatchPlan` is built once per active effect by
/// `synthesize_dispatch_infrastructure` and consumed by the lowering
/// (`lower_with_handler`) and call-site rewriting
/// (`rewrite_call_sites_to_wrappers`) passes. The map of plans is
/// stashed on `Package::dispatch_plans` between the early
/// (`synthesize_pre_cm_binding`) and late (`synthesize_post_check`)
/// halves of the synthesis split so the late phase doesn't re-derive
/// the same `(struct_type_id, global_name, …)` triple.
#[derive(Debug, Clone)]
pub struct DispatchPlan {
    /// `TypeId` of the synthesised `__Dispatch_<E>` struct.
    pub struct_type_id: TypeId,
    /// `TypeId` of `Option<&__Dispatch_<E>>` — the global's runtime type
    /// and the type of `outer` / dispatch wrapper-saved values.
    pub nullable_ref_type_id: TypeId,
    /// `TypeId` of `&__Dispatch_<E>` — handed to `Option::Some` when
    /// installing a fresh dispatch record.
    pub inner_ref_type_id: TypeId,
    /// Name of the synthesised `__effect_<E>` mutable global.
    pub global_name: String,
    /// Operation name → dispatch wrapper function name
    /// (`__effect_dispatch__<E>__<op>`).
    pub wrapper_names: IndexMap<String, String>,
    /// Operation name → dispatch struct field name (`op_<op>`).
    pub field_names: IndexMap<String, String>,
    /// Operation name → field type (`fn(<op_params>) -> <op_ret>`).
    pub field_types: IndexMap<String, TypeId>,
    /// Operation name → 0-based field index in `__Dispatch_<E>`. The
    /// `outer` field always sits at index 0; ops start at 1.
    pub field_indices: IndexMap<String, u32>,
    /// Cached operation declarations (cloned from `EffectMeta`) so the
    /// wrapper / closure synth doesn't have to walk back to the index.
    pub operations: Vec<TirEffectOp>,
}

/// Build the mangled instantiation label for naming dispatch
/// infrastructure. `(base, [])` → `"Stream"`; `(base, [u8])` →
/// `"Stream<u8>"`. Mirrors `name::mangle_generic_name`, which is the
/// canonical name-mangling utility (per the wado-compiler CLAUDE rule
/// "Use utilities in name.rs to handle name mangling").
fn instantiation_label(base: &str, type_args: &[TypeId], type_table: &TypeTable) -> String {
    if type_args.is_empty() {
        base.to_string()
    } else {
        let arg_strings: Vec<String> = type_args
            .iter()
            .map(|tid| type_table.mangle_type_name(*tid))
            .collect();
        crate::name::mangle_generic_name(base, &arg_strings)
    }
}

/// Substitute the resource declaration's type parameters with the
/// concrete `type_args` recorded on an `impl <Resource><Args> for <T>`
/// block, producing per-instantiation operation signatures.
///
/// The resource decl's operations carry `TypeId`s that may transitively
/// reference `TypeParam { index }` entries minted at the resource's
/// declaration site; substitution walks each operation's parameter
/// list and return type through `SubstitutionContext`, replacing those
/// type-param references with the supplied concrete `TypeId`s. The
/// resulting `TirEffectOp` list is what the per-instantiation dispatch
/// struct / global / wrappers are synthesised against.
fn substitute_operations(
    template: &[TirEffectOp],
    type_args: &[TypeId],
    type_table: &mut TypeTable,
) -> Vec<TirEffectOp> {
    if type_args.is_empty() {
        return template.to_vec();
    }
    let ctx = SubstitutionContext::new().with_impl_args(type_args);
    template
        .iter()
        .map(|op| {
            let new_params: Vec<TirParam> = op
                .params
                .iter()
                .map(|p| TirParam {
                    type_id: ctx.substitute(p.type_id, type_table),
                    ..p.clone()
                })
                .collect();
            TirEffectOp {
                name: op.name.clone(),
                params: new_params,
                return_type: ctx.substitute(op.return_type, type_table),
                span: op.span,
                cm_name: op.cm_name.clone(),
            }
        })
        .collect()
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
fn synthesize_dispatch_struct(
    project: &mut Package,
    entry_source: &ModuleSource,
    key: &InstantiationKey,
    meta: &EffectMeta,
) -> DispatchPlan {
    let (_, base_name, type_args) = key;

    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    let tt_rc = entry_module.type_table.clone();

    // Build all the type IDs in a single borrow scope. The recursive
    // `outer` field type is computed off the freshly-interned struct
    // type id.
    let label;
    let struct_name;
    let global_name;
    let struct_type_id;
    let inner_ref_type_id;
    let nullable_ref_type_id;
    let mut op_field_types: Vec<TypeId> = Vec::with_capacity(meta.operations.len());
    {
        let mut tt = tt_rc.borrow_mut();
        label = instantiation_label(base_name, type_args, &tt);
        struct_name = crate::name::dispatch_struct_name(&label);
        global_name = crate::name::dispatch_global_name(&label);
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
        let field_name = crate::name::dispatch_field_name(&op.name);
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
            crate::name::dispatch_wrapper_name(&label, &op.name),
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
fn synthesize_dispatch_wrappers(
    project: &mut Package,
    entry_source: &ModuleSource,
    key: &InstantiationKey,
    meta: &EffectMeta,
    plan: &DispatchPlan,
) {
    let (effect_module, base_name, type_args) = key;
    let effect_module = effect_module.clone();
    let base_name = base_name.clone();
    let is_resource = meta.is_resource;
    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    let type_table = entry_module.type_table.clone();
    let label = instantiation_label(&base_name, type_args, &type_table.borrow());
    for op in &plan.operations {
        let wrapper = build_dispatch_wrapper_function(
            entry_source,
            &effect_module,
            &base_name,
            &label,
            type_args,
            op,
            plan,
            is_resource,
            &type_table,
        );
        entry_module.add_function(wrapper);
    }
}

fn build_dispatch_wrapper_function(
    entry_source: &ModuleSource,
    effect_module: &ModuleSource,
    base_name: &str,
    label: &str,
    type_args: &[TypeId],
    op: &TirEffectOp,
    plan: &DispatchPlan,
    is_resource: bool,
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
    //
    // The wrapper is synthesised BEFORE `cm_binding::generate_adapters`
    // runs, so the fallback emits the same shape user code would have
    // emitted if no handler were installed: an effect-like `Call`
    // (`Counter::next()` form) for effects, and a `cm_name`-tagged
    // `MethodCall` / `Call` for resources. cm_binding then walks every
    // function body — including these wrapper fallbacks — and rewrites
    // both shapes uniformly:
    //
    // - effect calls trigger `synthesize_adapter` and become
    //   `__cm_binding__<E>_<op>(args)`;
    // - resource calls become `cm_stream_*_<elem>` / `cm_future_*` /
    //   `__cm_binding__<R>_<op>` / `CmRawCall` per the resource's
    //   `cm_binding_function` route, with no further work from this
    //   pass.
    //
    // For user-defined effects without a CM binding (like the test
    // `Counter` effect), the call falls through cm_binding's rewrite
    // and reaches WIR build as a plain effect call — well-typed
    // programs never reach the wrapper fallback for such effects
    // because effect-check requires a handler at every call site, but
    // we still emit the placeholder for consistency.
    // Routing key is `op.cm_name`:
    //   * resource ops always carry a `#[cm("...")]` attribute (the
    //     resolver records its payload on `TirEffectOp.cm_name`), and
    //     `is_resource` is set when the dispatch instantiation comes from
    //     a `pub resource ...` decl rather than a `pub effect ...` decl;
    //   * WASI effect ops always carry `#[cm("...")]` too (every WASI
    //     interface method is a CM canonical operation in our stdlib);
    //   * user-defined effect ops carry no `#[cm("...")]` and are
    //     unreachable here in well-typed programs (effect-check insists
    //     on a handler at every call site).
    let mut else_stmts: Vec<TirStmt> = Vec::new();
    if is_resource {
        let placeholder_call = build_resource_fallback_call(
            entry_source,
            effect_module,
            base_name,
            label,
            type_args,
            op,
            &arg_exprs,
            &type_table.borrow(),
            return_type,
            span,
        );
        else_stmts.push(TirStmt::new(
            TirStmtKind::Return {
                value: Some(placeholder_call),
            },
            span,
        ));
    } else if op.cm_name.is_some() {
        // WASI / cm-tagged effect call: emit the user-effect Call form
        // (`Effect::op(args)`) so cm_binding's effect-call collector
        // and rewriter pick it up exactly as it would a hand-written
        // effect call.
        let effect_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::local(base_name.to_string()),
                    name: op_name.clone(),
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
                value: Some(effect_call),
            },
            span,
        ));
    } else {
        // User-defined effect: well-typed programs never reach the wrapper
        // without a handler installed, since effect-check requires every
        // call-site to have a `with E => h do { ... }` in scope. If we get
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
            TirExprKind::StringLiteral(format!("no handler installed for `{label}::{op_name}`")),
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
        // The wrapper is the implementation of `<E>::<op>`.
        //
        // For effects, declare effect `E` on the wrapper for two reasons:
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
        // For resources, the wrapper carries no effects: resource
        // declarations are not effects in Wado's effect system, so a
        // call site like `Fields::new()` does not require a `with` clause
        // on the caller, and the matching `__cm_binding__<R>_<op>`
        // adapter declares `effects: vec![]` for the same reason.
        // Mirroring that here keeps the dispatch wrapper observably
        // equivalent to the cm_binding adapter from the effect-check
        // and codegen perspectives.
        //
        // Effect-check has already run by this point, so this declaration
        // is documentation rather than an obligation; downstream passes
        // that re-walk effects (codegen export tables, `used_wasi_*`
        // tracking) get the right answer.
        // Wrappers declare empty effects so that calls to them (which
        // now happen at every user effect / resource call site) don't
        // propagate the underlying effect to the caller. The wrapper
        // itself satisfies the effect: its `if let Some(d)` branch
        // dispatches into the user-installed handler (which is allowed
        // to perform the effect via the outer-scope dispatch global),
        // and its `else` branch emits a placeholder call that
        // cm_binding rewrites into the right adapter / canonical
        // intrinsic. From the caller's perspective, calling a wrapper
        // is a plain function call with no propagated effects — much
        // like calling a `cm_binding` adapter, which also carries
        // `effects: vec![]`. This matches the prior pre-reorder
        // behaviour where call sites still in their raw `Effect::op()`
        // form went through `find_effect_signature` and returned no
        // declared effects (effect ops aren't in the function index),
        // so handler methods like `OffsetCounter^Counter::next` could
        // call `Counter::next()` without re-declaring `with Counter`.
        effects: vec![],
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

/// Build the wrapper-fallback placeholder for a resource operation
/// (one that carries a `#[cm("...")]` attribute). Emits the same
/// pre-cm_binding call shape that user code emits — `MethodCall` for
/// instance ops (those whose first param is the synthesised `self`
/// receiver from gap 3), `Call` for static ops — with `method_info`
/// carrying the original `cm_name`. `cm_binding`'s
/// `rewrite_cm_resource_methods` then walks the wrapper body and
/// rewrites the placeholder into the appropriate
/// `cm_stream_*` / `cm_future_*` / `__cm_binding__<R>_<op>` /
/// `CmRawCall` form per its `cm_binding_function` routing.
#[allow(dead_code, clippy::too_many_arguments)]
fn build_resource_fallback_call(
    _entry_source: &ModuleSource,
    effect_module: &ModuleSource,
    base_name: &str,
    label: &str,
    type_args: &[TypeId],
    op: &TirEffectOp,
    arg_exprs: &[TirExpr],
    type_table: &TypeTable,
    return_type: TypeId,
    span: crate::Span,
) -> TirExpr {
    let cm_name = op
        .cm_name
        .clone()
        .expect("build_resource_fallback_call: caller checked op.cm_name.is_some()");
    let is_instance = op.params.first().is_some_and(|p| p.name == "self");

    // Trait/resource type args list, derived from the dispatch label
    // (e.g. label "Stream<u8>" → ["u8"]). The label encodes the
    // instantiation; the LocalMethodName carries it both as
    // `struct_name = "Stream<u8>"` (full mangled) and
    // `base_struct_name = "Stream"` (bare), mirroring the resolver
    // convention so cm_binding sees a familiar shape.
    let _ = type_table;
    let mangled_method_name = format!("{label}::{}", op.name);

    let mut method_info =
        crate::name::LocalMethodName::new(base_name.to_string(), None, op.name.clone());
    method_info.struct_name = label.to_string();
    method_info.cm_name = Some(cm_name);

    // Carry the resource instantiation's type args as the call's
    // `monomorph_info.impl_type_args`. WIR-build's canonical-method
    // translator (e.g. `cm_future_payload_from_monomorph` for
    // `future-new`) reads these to pick the right payload-parameterised
    // canonical import (`future-new:s32` vs the trailers default). The
    // resolver attaches `monomorph_info` for the same reason on real
    // user calls; the wrapper fallback must do the same so its
    // post-cm_binding shape is indistinguishable from a hand-written
    // `Future::<i32>::new()` call site.
    //
    // `method_type_args` stays empty: `ast::EffectMethod` carries no
    // type-parameter list, so resource / effect operations cannot
    // themselves be generic — only the resource type can. The `T` in
    // `Stream<T>::read(self, max) -> Array<T>` is the resource's type
    // parameter, already substituted by `substitute_operations` and
    // captured in `impl_type_args`.
    let monomorph_info = if type_args.is_empty() {
        None
    } else {
        Some(crate::tir::MonomorphInfo {
            generic_name: base_name.to_string(),
            impl_type_args: type_args.to_vec(),
            method_type_args: vec![],
            is_blanket: false,
        })
    };

    let func_ref = FunctionRef {
        module_source: effect_module.clone(),
        name: mangled_method_name,
        monomorph_info,
        method_info: Some(method_info),
    };

    if is_instance {
        // Instance method: arg_exprs[0] is the `self` receiver Local
        // (the wrapper's first param), arg_exprs[1..] are the rest.
        let mut iter = arg_exprs.iter().cloned();
        let receiver = iter.next().expect("instance op has receiver param");
        let rest: Vec<CallArg> = iter.map(|e| CallArg::new(e, false)).collect();
        TirExpr::new(
            TirExprKind::method_call(Box::new(receiver), func_ref, vec![], rest),
            return_type,
            span,
        )
    } else {
        let args: Vec<CallArg> = arg_exprs
            .iter()
            .cloned()
            .map(|e| CallArg::new(e, false))
            .collect();
        TirExpr::new(
            TirExprKind::Call {
                func: func_ref,
                type_args: vec![],
                args,
            },
            return_type,
            span,
        )
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
    plans: &'a IndexMap<InstantiationKey, DispatchPlan>,
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
enum LocalScope {
    Function {
        next_local: u32,
        local_types: Vec<TypeId>,
    },
    Closure {
        next_local: u32,
    },
}

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
fn lower_with_handler_dispatch_in_modules(
    project: &mut Package,
    plans: &IndexMap<InstantiationKey, DispatchPlan>,
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

fn lower_dispatch_in_block(block: &mut TirBlock, env: &DispatchEnv, ctx: &mut LowerCtx) {
    for stmt in &mut block.stmts {
        lower_dispatch_in_stmt(stmt, env, ctx);
    }
}

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
            TirStmtKind::IfLet { pattern, .. } | TirStmtKind::LetDestructure { pattern, .. } => {
                self.walk_pattern(pattern);
            }
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
fn desugar_with_handler(expr: &mut TirExpr, env: &DispatchEnv, ctx: &mut LowerCtx) {
    let span = expr.span;
    let result_type = expr.type_id;

    let TirExprKind::WithHandler {
        bindings, mut body, ..
    } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        return;
    };

    let mut prelude: Vec<TirStmt> = Vec::new();
    let mut restore: Vec<TirStmt> = Vec::new();
    // bundle_group id -> (h_local, h_name). Bindings expanded from one
    // bundled `with &mut h do` clause share a single `__h_<bundle>` local
    // so the handler expression is evaluated exactly once and mutations
    // through any installed effect are observed by every other effect's
    // dispatch closure (closures all capture the same local).
    let mut bundle_locals: IndexMap<u32, (u32, String)> = IndexMap::default();

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
        let trait_type_args = binding.trait_type_args.clone();
        let key: InstantiationKey = (
            effect_module.clone(),
            effect_name.clone(),
            trait_type_args.clone(),
        );
        let plan = env.plans.get(&key).unwrap_or_else(|| {
            panic!(
                "effect-dispatch synthesis: no DispatchPlan for \
                 instantiation `{effect_name}<{trait_type_args:?}>` in module \
                 {effect_module:?} — `identify_active_effects` and \
                 `synthesize_dispatch_infrastructure` are out of sync"
            )
        });
        let handler_type = binding.handler.type_id;
        let handler_underlying = deref_type(&env.type_table.borrow(), handler_type);
        let handler_type_name = env.type_table.borrow().type_name(handler_underlying);
        let impl_key: HandlerImplKey = (
            handler_type_name.clone(),
            effect_module.clone(),
            effect_name.clone(),
            trait_type_args.clone(),
        );
        let impl_info = env.impl_index.get(&impl_key).unwrap_or_else(|| {
            panic!(
                "effect-dispatch synthesis: no `impl {effect_name} for \
                 {handler_type_name}` (type args = {trait_type_args:?}) \
                 registered in the impl index for effect module \
                 {effect_module:?} — the resolver should have rejected \
                 the `with {effect_name} => h do` binding if no matching \
                 impl exists"
            )
        });

        // Mangled instantiation label, e.g. `Stream<u8>` for
        // `(Stream, [u8])`. Used both as the struct-literal type name
        // (must agree with `synthesize_dispatch_struct`) and for the
        // synthesised local names (`__h_<label>` etc.) so dumps stay
        // readable when multiple instantiations of the same base
        // resource are installed in nested `with` blocks.
        let label = instantiation_label(&effect_name, &trait_type_args, &env.type_table.borrow());

        // 1. let __h_<E> = handler_expr;
        //
        // Bundled bindings share one `__h_<bundle>` local: the first
        // binding in the group emits the `Let`, subsequent bindings
        // reuse the local without re-evaluating the handler expression.
        // Explicit `Effect => handler` bindings always get their own
        // per-effect local, which is what users writing the explicit
        // form expect — distinct expressions installed independently.
        let (h_local, h_name) = if let Some(group_id) = binding.bundle_group {
            if let Some((existing_local, existing_name)) = bundle_locals.get(&group_id) {
                (*existing_local, existing_name.clone())
            } else {
                let local = ctx.alloc_local(handler_type);
                let name = format!("__h_bundle_{group_id}");
                prelude.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: local,
                        is_mut: false,
                        is_reactive: false,
                        type_id: handler_type,
                        value: binding.handler.clone(),
                        skip_value_copy: true,
                    },
                    span,
                ));
                bundle_locals.insert(group_id, (local, name.clone()));
                (local, name)
            }
        } else {
            let local = ctx.alloc_local(handler_type);
            let name = format!("__h_{label}");
            prelude.push(TirStmt::new(
                TirStmtKind::Let {
                    name: name.clone(),
                    local_index: local,
                    is_mut: false,
                    is_reactive: false,
                    type_id: handler_type,
                    value: binding.handler.clone(),
                    skip_value_copy: true,
                },
                span,
            ));
            (local, name)
        };

        // 2. let __save_<E> = global.get __effect_<E>;
        let save_local = ctx.alloc_local(plan.nullable_ref_type_id);
        let save_name = format!("__save_{label}");
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
                    &label,
                    impl_info,
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
        let d_name = format!("__d_{label}");
        let struct_lit = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: plan.struct_type_id,
                struct_name: crate::name::dispatch_struct_name(&label),
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
    //
    // Before composing, walk `body` and splice the restore sequence in
    // front of every control-flow exit that leaves this `with` body —
    // `return`, plus `break`/`continue` whose target is declared
    // outside the body. This keeps the per-effect dispatch global in
    // sync with the lexical do-block scope even when the body exits
    // via early jumps. See WEP 2026-04-11 (Effect Handler).
    //
    // The early-exit restore sequence has the same order as the
    // fall-through one — reverse install order — so we materialise
    // `restore` once into `restore_seq` and reuse it for both.
    let restore_seq: Vec<TirStmt> = restore.iter().rev().cloned().collect();
    let mut injector = RestoreInjector::new(&restore_seq, ctx);
    injector.visit_block(&mut body);

    let mut stmts: Vec<TirStmt> =
        Vec::with_capacity(prelude.len() + body.stmts.len() + restore_seq.len());
    stmts.extend(prelude);
    stmts.extend(body.stmts);
    stmts.extend(restore_seq);

    expr.kind = TirExprKind::Block(TirBlock::new(stmts, span));
    expr.type_id = result_type;
}

/// Walks a `with`-body and splices the per-`with` restore sequence in
/// front of every control-flow statement that exits the body — i.e.
/// every jump that would otherwise skip the fall-through restore at
/// the end of the desugared block.
///
/// Exits are determined relative to the loops and labeled blocks that
/// appear *inside* the body:
///
/// - `Return` — always exits (returns from the enclosing function).
/// - `Continue` and `Break` without a label — exit when there is no
///   anonymous loop (`loop { }` / `VariadicForOf`) currently open
///   inside the body.
/// - `Break { label: Some(l) }` — exits when `l` was not declared
///   inside the body.
///
/// Value-carrying jumps (`return v`, `break L: v`) are rewritten to
/// evaluate the value *before* the restore runs and *after* the
/// dispatch global is still pointing at the do-block's handler:
///
/// ```text
/// let __early_jump_<n> = v;   // evaluated under the do-block handler
/// __restore_seq__;            // global goes back to outer scope
/// return __early_jump_<n>;    // jump propagates the saved value
/// ```
///
/// Without that bind, a `return Counter::next()` inside the body
/// would evaluate `Counter::next()` *after* restoring, picking up
/// whatever handler the outer scope had instead of the inner one.
///
/// `Closure` bodies are *not* descended into: a `return`/`break`/
/// `continue` inside a closure targets the closure itself, not the
/// `with` body. Inner `WithHandler` nodes have already been desugared
/// to plain `Block` expressions by the time this runs, so each
/// `with` only injects its own restores; nesting composes by
/// successive splice — innermost first, outermost last — which yields
/// the correct reverse-install order at every exit point.
struct RestoreInjector<'a, 'b> {
    /// The restore statements to splice in front of every exit, in
    /// the exact order they should appear (reverse install order —
    /// matching the fall-through restore at the end of the desugared
    /// block).
    restore_seq: &'a [TirStmt],
    /// Number of anonymous loops (`loop { }` / `VariadicForOf`)
    /// currently open along the walk. While positive, bare
    /// `break`/`continue` stay inside the `with` body.
    inner_loop_depth: u32,
    /// Stack of labels declared by `LabeledBlock` (statement or
    /// expression form) along the walk. `break L` whose `L` is in
    /// this stack stays inside the `with` body.
    inner_labels: Vec<String>,
    /// Local-allocation context, used to mint fresh temp locals for
    /// value-carrying early exits (`return v`, `break L: v`).
    ctx: &'b mut LowerCtx,
}

impl<'a, 'b> RestoreInjector<'a, 'b> {
    fn new(restore_seq: &'a [TirStmt], ctx: &'b mut LowerCtx) -> Self {
        Self {
            restore_seq,
            inner_loop_depth: 0,
            inner_labels: Vec::new(),
            ctx,
        }
    }

    fn visit_block(&mut self, block: &mut TirBlock) {
        let mut i = 0;
        while i < block.stmts.len() {
            // Recurse into the children of this stmt first so nested
            // exits inside an `If`/`Match`/`LabeledBlock`/loop body
            // get their own restores. Then check whether the stmt
            // itself is an exit.
            self.visit_stmt(&mut block.stmts[i]);

            if !self.is_exit_stmt(&block.stmts[i].kind) {
                i += 1;
                continue;
            }

            let stmt_span = block.stmts[i].span;
            let mut to_insert: Vec<TirStmt> = Vec::new();

            // Value-carrying exits must evaluate their value under the
            // do-block's still-installed handler before the restore
            // runs. Bind the value to a fresh temp local, then restore,
            // then jump with the temp.
            let value_slot: Option<&mut Option<TirExpr>> = match &mut block.stmts[i].kind {
                TirStmtKind::Return { value } => Some(value),
                TirStmtKind::Break { value, .. } => Some(value),
                _ => None,
            };
            if let Some(value_slot) = value_slot
                && let Some(v) = value_slot.as_mut()
            {
                let value_type = v.type_id;
                let value_expr =
                    std::mem::replace(v, TirExpr::new(TirExprKind::Unit, value_type, stmt_span));
                let temp_local = self.ctx.alloc_local(value_type);
                let temp_name = format!("__early_jump_{temp_local}");
                to_insert.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_local,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value_type,
                        value: value_expr,
                        skip_value_copy: true,
                    },
                    stmt_span,
                ));
                *v = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_local,
                        name: temp_name,
                    },
                    value_type,
                    stmt_span,
                );
            }

            to_insert.extend(self.restore_seq.iter().cloned());

            let n = to_insert.len();
            if n > 0 {
                block.stmts.splice(i..i, to_insert);
                i += n;
            }
            i += 1;
        }
    }

    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. }
            | TirStmtKind::Expr(value)
            | TirStmtKind::TaskReturn { value }
            | TirStmtKind::LetDestructure { value, .. } => self.visit_expr(value),
            TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::Loop { body } => {
                self.inner_loop_depth += 1;
                self.visit_block(body);
                self.inner_loop_depth -= 1;
            }
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.inner_loop_depth += 1;
                self.visit_block(body);
                self.inner_loop_depth -= 1;
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.inner_labels.push(label.clone());
                self.visit_block(block);
                self.inner_labels.pop();
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_block);
                if let Some(eb) = else_block {
                    self.visit_block(eb);
                }
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.visit_expr(scrutinee);
                self.visit_block(then_block);
                if let Some(eb) = else_block {
                    self.visit_block(eb);
                }
            }
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Block(block) => self.visit_block(block),
            TirExprKind::LabeledBlock { label, block, .. } => {
                self.inner_labels.push(label.clone());
                self.visit_block(block);
                self.inner_labels.pop();
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_branch);
                if let Some(eb) = else_branch {
                    self.visit_block(eb);
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                self.visit_expr(scrut);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        self.visit_expr(g);
                    }
                    self.visit_expr(&mut arm.body);
                }
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_block(arm);
                }
                self.visit_block(default);
            }
            TirExprKind::Closure { .. } => {
                // Closure bodies are a separate function: their
                // `return`/`break`/`continue` target the closure, not
                // the enclosing `with`. Do not descend.
            }
            TirExprKind::WithHandler { .. } => {
                unreachable!(
                    "inner WithHandler should already be desugared to a \
                     plain Block by the recursive `lower_dispatch_in_expr`"
                );
            }
            TirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::ClosureToCanonical { functor: inner, .. }
            | TirExprKind::Resume { value: inner } => {
                self.visit_expr(inner);
            }
            TirExprKind::Assign { target, value }
            | TirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            TirExprKind::GlobalVarSet { value, .. } => self.visit_expr(value),
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.visit_expr(&mut f.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for e in elements {
                    self.visit_expr(e);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.visit_expr(p);
                }
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr, .. } = part {
                        self.visit_expr(expr);
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

    fn is_exit_stmt(&self, kind: &TirStmtKind) -> bool {
        match kind {
            TirStmtKind::Return { .. } => true,
            TirStmtKind::Continue => self.inner_loop_depth == 0,
            TirStmtKind::Break { label: None, .. } => self.inner_loop_depth == 0,
            TirStmtKind::Break { label: Some(l), .. } => !self.inner_labels.iter().any(|x| x == l),
            _ => false,
        }
    }
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
/// `effect_label` is the full mangled trait name at this instantiation
/// site (e.g. `"Stream<u8>"`, or `"Counter"` for non-generic effects).
/// Used to build the user-impl method's mangled name
/// `<Handler>^<E><args>::<op>` — must match what
/// `Resolver::resolve_method` registered, since the resolver mangles
/// `trait_type` via `get_type_name_full` (preserving the type args for
/// distinct-instantiation symbol names).
fn build_handler_op_closure(
    op: &TirEffectOp,
    effect_label: &str,
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
    // The user impl method was registered by `Resolver::resolve_method`
    // under the full mangled trait name (`Stream<u8>` rather than the
    // bare base `Stream`); use the same form here so the synthesised
    // method-call resolves against the right symbol.
    let mangled = LocalMethodName::new(
        impl_info.handler_type_name.clone(),
        Some(effect_label.to_string()),
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

    let param_count = closure_params.len() as u32;
    let param_types: Vec<TypeId> = closure_params.iter().map(|(_, t)| *t).collect();
    TirExpr::new(
        TirExprKind::Closure {
            params: closure_params,
            body: Box::new(method_call),
            captures,
            functor_id: None,
            source_text: None,
            address_taken_locals: crate::hashmap::IndexSet::default(),
            local_count: param_count,
            local_types: param_types,
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
            .make_function(param_types.clone(), op.return_type, vec![], vec![]);
    let param_count = closure_params.len() as u32;
    TirExpr::new(
        TirExprKind::Closure {
            params: closure_params,
            body: Box::new(trap_call),
            captures: Vec::new(),
            functor_id: None,
            source_text: None,
            address_taken_locals: crate::hashmap::IndexSet::default(),
            local_count: param_count,
            local_types: param_types,
        },
        func_type,
        span,
    )
}

/// Rewrite every effect-operation call site so it routes through a
/// dispatch wrapper. Runs **before** `cm_binding`, so the call shapes
/// it recognises are the pre-cm_binding forms the resolver emits:
///
/// 1. **User-effect shape** — `Call` whose `func.module_source` is
///    `ModuleSource::Local { path: "<E>" }` and `func.name` is the
///    bare op name. Produced by the resolver for `Effect::op(...)`.
/// 2. **Resource `cm_name` shape** — `Call` (static) or `MethodCall`
///    (instance) whose `func.method_info.cm_name` is `Some(...)`.
///    Produced by the resolver for `R::op(args)` /
///    `receiver.op(args)` on a resource declaration.
///
/// Calls inside `__effect_dispatch__*` wrappers are left alone — they
/// belong to the synthesised infrastructure and rewriting them would
/// short-circuit the wrapper's fallback. (`cm_binding` adapters
/// (`__cm_binding__*`) don't exist yet at this phase, so they need
/// no carve-out.)
fn rewrite_call_sites_to_wrappers(
    project: &mut Package,
    plans: &IndexMap<InstantiationKey, DispatchPlan>,
) {
    let entry_source = project.entry_module_source.clone();

    // Pre-build lookup maps:
    //
    // - `user_to_wrapper`: keyed `(effect_name, op_name)` for
    //   `Effect::op()` user-effect Calls. Effects collapse to a
    //   single instantiation (`type_args: []`) so the lookup is
    //   unambiguous.
    // - `cm_to_wrappers`: keyed `(resource_module, base_name,
    //   cm_name)` and stores every active `(type_args, wrapper_name)`
    //   pair. The rewriter narrows by type args from the receiver
    //   type (instance) or `method_info.struct_name` (static).
    // Effects vs resources route through different call shapes:
    //   * effect ops (`Counter::next()`, `MonotonicClock::now()`)
    //     resolve to a `Call` whose `func.module_source` is the
    //     `Local { path: "<EffectName>" }` form (`is_effect_like()`)
    //     even when the op carries a `#[cm("...")]` attribute. The
    //     rewriter intercepts these via `(effect_name, op_name)` in
    //     `user_to_wrapper`.
    //   * resource methods (`Fields::new()`, `tx.write(payload)`,
    //     `Stream::<u8>::new()`) resolve to a `Call` / `MethodCall`
    //     whose `func.module_source` is the resource's declaring
    //     module and whose `func.method_info.cm_name` is `Some(...)`.
    //     The rewriter intercepts these via
    //     `(decl_module, base_name, cm_name)` in `cm_to_wrappers`,
    //     narrowing by the call's type args.
    let effect_index = build_effect_index(project);
    let mut user_to_wrapper: IndexMap<(String, String), String> = IndexMap::default();
    let mut cm_to_wrappers: IndexMap<(ModuleSource, String, String), Vec<(Vec<TypeId>, String)>> =
        IndexMap::default();
    for ((module, base_name, type_args), plan) in plans {
        let is_resource = effect_index
            .get(&(module.clone(), base_name.clone()))
            .map(|m| m.is_resource)
            .unwrap_or(false);
        for op in &plan.operations {
            let wrapper_name = plan
                .wrapper_names
                .get(&op.name)
                .expect("wrapper name registered for op");
            if is_resource {
                if let Some(cm_name) = &op.cm_name {
                    cm_to_wrappers
                        .entry((module.clone(), base_name.clone(), cm_name.clone()))
                        .or_default()
                        .push((type_args.clone(), wrapper_name.clone()));
                }
                // Resource ops without `cm_name` aren't reachable from
                // user-side calls (no canonical attribute → no
                // resolver-side `cm_name` tag), so skip them.
            } else {
                // Effect op: route via the `Local { path }` user-effect
                // call shape regardless of whether the op carries a CM
                // attribute (the WASI effect adapter generation in
                // cm_binding still kicks in for the wrapper fallback).
                user_to_wrapper.insert((base_name.clone(), op.name.clone()), wrapper_name.clone());
            }
        }
    }

    for module in project.tir_modules.values_mut() {
        let type_table_rc = module.type_table.clone();
        let ctx = RewriteCtx {
            user_to_wrapper: &user_to_wrapper,
            cm_to_wrappers: &cm_to_wrappers,
            type_table: type_table_rc,
            entry_source: &entry_source,
        };
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if func.is_dispatch_wrapper {
                continue;
            }
            if let Some(body) = &mut func.body {
                rewrite_calls_in_block(body, &ctx);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if method.is_dispatch_wrapper {
                    continue;
                }
                if let Some(body) = &mut method.body {
                    rewrite_calls_in_block(body, &ctx);
                }
            }
        }
    }
}

/// Read-only context threaded through the call-site rewriter. Bundles
/// the lookup maps and the type table so the recursive walker stays
/// concise. `type_table` is held as the `Rc<RefCell<>>` Form so the
/// rewriter can both query existing types (`get`) and intern fresh
/// `Ref(<receiver-type>)` ids when wrapping a value receiver to match
/// the wrapper's `&Self` first parameter.
struct RewriteCtx<'a> {
    /// `(effect_name, op_name) → wrapper_name` for user-effect Calls
    /// (e.g. `Counter::next()`).
    user_to_wrapper: &'a IndexMap<(String, String), String>,
    /// `(resource_module, base_name, cm_name) → [(type_args,
    /// wrapper_name)]` for resource `MethodCalls` / Calls with
    /// `method_info.cm_name` set. Multiple instantiations live under
    /// the same key; the rewriter narrows by the call's type args.
    cm_to_wrappers: &'a IndexMap<(ModuleSource, String, String), Vec<(Vec<TypeId>, String)>>,
    type_table: std::rc::Rc<std::cell::RefCell<TypeTable>>,
    entry_source: &'a ModuleSource,
}

/// Pick the wrapper name for a resource method/call given its
/// `cm_name`-tagged shape. Returns `None` if no instantiation matches
/// (the call falls through to `cm_binding`'s normal rewriting).
fn pick_resource_wrapper(
    base_name: &str,
    decl_module: &ModuleSource,
    cm_name: &str,
    call_type_args: &[TypeId],
    ctx: &RewriteCtx<'_>,
) -> Option<String> {
    let candidates = ctx.cm_to_wrappers.get(&(
        decl_module.clone(),
        base_name.to_string(),
        cm_name.to_string(),
    ))?;
    // Exact instantiation match first.
    if let Some((_, name)) = candidates.iter().find(|(args, _)| args == call_type_args) {
        return Some(name.clone());
    }
    // Single-instantiation fallback: when the call site couldn't
    // pin down its type args (static methods of generic resources
    // can hide them) but only one impl instantiation is active,
    // route through it. Multi-instantiation static-call ambiguity
    // is rejected (returns None → falls through to cm_binding).
    if call_type_args.is_empty() && candidates.len() == 1 {
        return Some(candidates[0].1.clone());
    }
    None
}

/// Strip leading `Ref`/`MutRef` then return `(decl_module, base_name,
/// type_args)` for a resource type, if the type is a resource.
fn extract_resource_instantiation(
    type_id: TypeId,
    type_table: &TypeTable,
) -> Option<(ModuleSource, String, Vec<TypeId>)> {
    use crate::tir::ResolvedType;
    let mut tid = type_id;
    loop {
        match type_table.get(tid) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                tid = *inner;
            }
            ResolvedType::Resource {
                name,
                module_source,
            } => {
                return Some((module_source.clone(), name.clone(), Vec::new()));
            }
            ResolvedType::GenericResource {
                name,
                module_source,
                type_args,
            } => {
                return Some((module_source.clone(), name.clone(), type_args.clone()));
            }
            _ => return None,
        }
    }
}

fn rewrite_calls_in_block(block: &mut TirBlock, ctx: &RewriteCtx<'_>) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, ctx);
    }
}

fn rewrite_calls_in_stmt(stmt: &mut TirStmt, ctx: &RewriteCtx<'_>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => {
            rewrite_calls_in_expr(value, ctx);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, ctx);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, ctx);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, ctx);
            rewrite_calls_in_block(then_block, ctx);
            if let Some(eb) = else_block {
                rewrite_calls_in_block(eb, ctx);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, ctx);
            rewrite_calls_in_block(then_block, ctx);
            if let Some(eb) = else_block {
                rewrite_calls_in_block(eb, ctx);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_calls_in_expr(value, ctx);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            rewrite_calls_in_expr(iterable, ctx);
            rewrite_calls_in_block(body, ctx);
        }
    }
}

fn rewrite_calls_in_expr(expr: &mut TirExpr, ctx: &RewriteCtx<'_>) {
    // Recurse into children first.
    rewrite_call_children(expr, ctx);

    // Then check if THIS expression is a Call/MethodCall worth
    // rewriting. The two pre-cm_binding shapes the rewriter routes
    // are: user-effect Calls (`Effect::op(...)`) and resource calls
    // tagged with `cm_name` on `method_info`.
    let new_call: Option<TirExpr> = match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let return_type = expr.type_id;
            // Resource static call?
            if let Some(method_info) = &func.method_info
                && let Some(cm_name) = &method_info.cm_name
            {
                let base_name = method_info
                    .base_trait_name
                    .clone()
                    .unwrap_or_else(|| method_info.base_struct_name.clone());
                let decl_module = func.module_source.clone();
                // Static resource call: receiver type isn't directly
                // available, fall back to single-instantiation routing.
                pick_resource_wrapper(&base_name, &decl_module, cm_name, &[], ctx).map(|wrapper| {
                    wrapper_call(
                        wrapper,
                        args.clone(),
                        return_type,
                        expr.span,
                        ctx.entry_source,
                    )
                })
            } else if let ModuleSource::Local { path } = &func.module_source
                && let Some(wrapper) = ctx.user_to_wrapper.get(&(path.clone(), func.name.clone()))
            {
                Some(wrapper_call(
                    wrapper.clone(),
                    args.clone(),
                    return_type,
                    expr.span,
                    ctx.entry_source,
                ))
            } else {
                None
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let mi_cm = func
                .method_info
                .as_ref()
                .and_then(|mi| mi.cm_name.as_ref().map(|cm| (mi, cm)));
            if let Some((_method_info, cm_name)) = mi_cm
                && let Some((decl_module, base_name, type_args)) =
                    extract_resource_instantiation(receiver.type_id, &ctx.type_table.borrow())
            {
                let return_type = expr.type_id;
                // The wrapper's first parameter is the resource receiver
                // typed as `&Self` (gap-3). User MethodCall receivers
                // arrive as the resource value (e.g. `tx:
                // StreamWritable<u8>`) rather than `&StreamWritable<u8>`,
                // so wrap with `&` to match the wrapper's signature
                // before forwarding.
                let receiver_value = (**receiver).clone();
                let is_already_ref = matches!(
                    ctx.type_table.borrow().get(receiver_value.type_id),
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                );
                let receiver_arg = if is_already_ref {
                    receiver_value
                } else {
                    let ref_type_id = ctx.type_table.borrow_mut().make_ref(receiver_value.type_id);
                    crate::synthesis::common::ref_expr(receiver_value, ref_type_id, expr.span)
                };
                let mut all_args: Vec<CallArg> = Vec::with_capacity(args.len() + 1);
                all_args.push(CallArg::new(receiver_arg, false));
                all_args.extend(args.iter().cloned());
                pick_resource_wrapper(&base_name, &decl_module, cm_name, &type_args, ctx).map(
                    |wrapper| {
                        wrapper_call(wrapper, all_args, return_type, expr.span, ctx.entry_source)
                    },
                )
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(new) = new_call {
        *expr = new;
    }
}

/// Build a `Call` to the dispatch wrapper in the entry module.
fn wrapper_call(
    wrapper_name: String,
    args: Vec<CallArg>,
    return_type: TypeId,
    span: crate::Span,
    entry_source: &ModuleSource,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: entry_source.clone(),
                name: wrapper_name,
                monomorph_info: None,
                method_info: None,
            },
            type_args: Vec::new(),
            args,
        },
        return_type,
        span,
    )
}

fn rewrite_call_children(expr: &mut TirExpr, ctx: &RewriteCtx<'_>) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, ctx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, ctx);
            rewrite_calls_in_block(then_branch, ctx);
            if let Some(eb) = else_branch {
                rewrite_calls_in_block(eb, ctx);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_calls_in_expr(expr, ctx);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_calls_in_expr(g, ctx);
                }
                rewrite_calls_in_expr(&mut arm.body, ctx);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, ctx);
            for arm in arms {
                rewrite_calls_in_block(arm, ctx);
            }
            rewrite_calls_in_block(default, ctx);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(&mut arg.expr, ctx);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, ctx);
            for arg in args {
                rewrite_calls_in_expr(arg, ctx);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, ctx);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, ctx);
            for arg in args {
                rewrite_calls_in_expr(&mut arg.expr, ctx);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, ctx);
            rewrite_calls_in_expr(right, ctx);
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
            rewrite_calls_in_expr(expr, ctx);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_calls_in_expr(target, ctx);
            rewrite_calls_in_expr(value, ctx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, ctx);
        }
        TirExprKind::Index { expr, index } => {
            rewrite_calls_in_expr(expr, ctx);
            rewrite_calls_in_expr(index, ctx);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                rewrite_calls_in_expr(&mut field.value, ctx);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                rewrite_calls_in_expr(elem, ctx);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, ctx);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_calls_in_expr(p, ctx);
            }
        }
        TirExprKind::Resume { value } => {
            rewrite_calls_in_expr(value, ctx);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                rewrite_calls_in_expr(&mut binding.handler, ctx);
            }
            rewrite_calls_in_block(body, ctx);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    rewrite_calls_in_expr(expr, ctx);
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

/// **Early phase** — synthesise the dispatch infrastructure (struct,
/// global, per-op wrapper functions) and rewrite call sites to route
/// through it. Runs **before** `cm_binding::generate_adapters` so:
///
/// - User resource calls (`tx.write(payload)`, `Stream::<u8>::new()`)
///   are caught at their pre-cm_binding shape — `MethodCall` / `Call`
///   with `func.method_info.cm_name` set — and replaced with `Call`s
///   to the per-monomorphisation wrapper.
/// - Wrapper fallback bodies emit the same cm_name-tagged placeholder
///   shape that user code emits, so `cm_binding` rewrites both
///   uniformly. (Generic resources like `Stream<T>` / `Future<T>`
///   route through `cm_stream_*` internal binding calls and `CmRawCall`
///   intrinsics; non-generic resources like `Fields` route through
///   `__cm_binding__<R>_<op>` adapters; effects route through their
///   own adapters. The placeholder's `cm_name` lets `cm_binding` pick
///   the right shape per op without dispatch synthesis duplicating
///   that routing.)
///
/// Resume rewriting also lands here because it's purely local and has
/// no ordering constraint with `cm_binding`.
///
/// Returns the package with wrappers / call-site rewrites applied. The
/// `WithHandler` desugaring is deferred to `synthesize_post_check` —
/// it must run **after** `effect_check` validates the original
/// `WithHandler` shape.
pub fn synthesize_pre_cm_binding(mut project: Package) -> Result<Package, String> {
    lower_resume_in_handler_methods(&mut project);

    let effect_index = build_effect_index(&project);
    let impl_index = build_handler_impl_index(&project, &effect_index);
    let active_effects = identify_active_effects(&impl_index);

    if active_effects.is_empty() {
        return Ok(project);
    }

    let plans = synthesize_dispatch_infrastructure(&mut project, &effect_index, &active_effects);
    rewrite_call_sites_to_wrappers(&mut project, &plans);
    // Hand the plans off to `synthesize_post_check` via the project so
    // the late half doesn't re-derive the same struct / global / wrapper
    // triple from existing declarations.
    project.dispatch_plans = plans;
    Ok(project)
}

/// **Late phase** — desugar `WithHandler` expressions into the
/// dispatch protocol. Runs after `effect_check` and `stores_check`
/// because those passes consume the original `WithHandler` shape:
/// effect-check uses it to know which effects are satisfied locally,
/// stores-check uses it to track reference flow through handler
/// installs.
///
/// Consumes the `dispatch_plans` populated by
/// `synthesize_pre_cm_binding`; nothing in `effect_check` or
/// `stores_check` mutates the plans, so we can take them straight off
/// the `Package` without re-deriving.
pub fn synthesize_post_check(mut project: Package) -> Result<Package, String> {
    let plans = std::mem::take(&mut project.dispatch_plans);
    if plans.is_empty() {
        return Ok(project);
    }
    let effect_index = build_effect_index(&project);
    let impl_index = build_handler_impl_index(&project, &effect_index);
    lower_with_handler_dispatch_in_modules(&mut project, &plans, &impl_index);
    Ok(project)
}

/// Orchestrates per-monomorphisation dispatch infrastructure synthesis.
///
/// `effect_index` is keyed by the bare declaration `(module, base_name)`
/// and carries the **template** operation list (with the resource decl's
/// type-params unsubstituted). `active_instantiations` enumerates the
/// concrete `(module, base_name, type_args)` triples discovered from
/// user impl blocks. For each instantiation we look up the template
/// meta, substitute the template operations with the impl's type args,
/// and synthesise one dispatch struct + global + per-op wrapper triple
/// against the substituted operations.
///
/// For non-generic effects the substitution is a no-op (`type_args`
/// empty) and behaviour collapses to the prior one-set-per-decl shape.
/// For generic resources each `(R, [u8])` and `(R, [i32])` instantiation
/// gets its own infrastructure with operation types resolved at the
/// concrete instantiation, satisfying the canonical-closure-type
/// invariant the WIR layer enforces.
///
/// Returns the per-instantiation [`DispatchPlan`] map the lowering /
/// call-site-rewriting passes consume.
fn synthesize_dispatch_infrastructure(
    project: &mut Package,
    effect_index: &IndexMap<EffectKey, EffectMeta>,
    active_instantiations: &IndexSet<InstantiationKey>,
) -> IndexMap<InstantiationKey, DispatchPlan> {
    let entry_source = project.entry_module_source.clone();
    // Pre-substitute every active instantiation's operations against the
    // declaration's template. This is done before any dispatch-struct
    // synthesis so that `synthesize_dispatch_struct` sees concrete types
    // when interning the per-op closure function types.
    let entry_module = project
        .tir_modules
        .get(&entry_source)
        .expect("entry module must exist");
    let type_table_rc = entry_module.type_table.clone();
    let mut substituted_metas: IndexMap<InstantiationKey, EffectMeta> = IndexMap::default();
    for key in active_instantiations {
        let (module, base_name, type_args) = key;
        let template = effect_index
            .get(&(module.clone(), base_name.clone()))
            .expect("active instantiation must have a declaration template in the effect index");
        let operations = substitute_operations(
            &template.operations,
            type_args,
            &mut type_table_rc.borrow_mut(),
        );
        substituted_metas.insert(
            key.clone(),
            EffectMeta {
                operations,
                is_resource: template.is_resource,
            },
        );
    }

    let mut plans: IndexMap<InstantiationKey, DispatchPlan> = IndexMap::default();
    for (key, meta) in &substituted_metas {
        let plan = synthesize_dispatch_struct(project, &entry_source, key, meta);
        plans.insert(key.clone(), plan);
    }
    for plan in plans.values() {
        synthesize_dispatch_global(project, &entry_source, plan);
    }
    for (key, plan) in &plans {
        let meta = substituted_metas
            .get(key)
            .expect("substituted meta must exist for every active instantiation");
        synthesize_dispatch_wrappers(project, &entry_source, key, meta, plan);
    }
    plans
}

/// Canonical identity of an `impl <Effect> for <Type>` block, including
/// the trait-side type arguments at this impl site:
/// `(handler_type_name, effect_defining_module, base_effect_name,
///   trait_type_args)`.
///
/// Two impls of the same generic resource at different instantiations —
/// `impl Stream<u8> for MockCM` vs `impl Stream<i32> for MockCM` — sit
/// at distinct keys so each gets its own per-monomorphisation dispatch
/// infrastructure synthesised. For non-generic effects / resources the
/// `trait_type_args` slot is `vec![]`.
type HandlerImplKey = (String, ModuleSource, String, Vec<TypeId>);

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
            // The decl indices are keyed by the bare base trait name, so
            // generic-resource impls (`impl Stream<u8> for MockCM`) must
            // resolve through `base_trait_name` rather than the mangled
            // `trait_name` ("Stream<u8>"). Per-monomorphisation
            // distinction lives in the key's `trait_type_args` slot:
            // `impl Stream<u8>` and `impl Stream<i32>` produce two
            // distinct keys so the dispatch synthesis emits one infra
            // triple per instantiation.
            let Some(base_trait_name) = &method_info.base_trait_name else {
                continue;
            };
            let Some(effect_module) = effect_module_for_name.get(base_trait_name) else {
                // Not an effect / resource — skip (the trait_name is from
                // a regular user trait or auto-derived prelude trait).
                continue;
            };
            let key: HandlerImplKey = (
                method_info.struct_name.clone(),
                effect_module.clone(),
                base_trait_name.clone(),
                method_info.trait_type_args.clone(),
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

/// Walk every method in every `impl Effect for T` or `impl Resource for T`
/// block and rewrite `Resume { value }` to `Return { value }`. Per the WEP
/// MVP, `resume` has no post-resume continuation, so it is semantically
/// identical to `return`. Resource impl methods share the handler-method
/// shape with effect impl methods (see WEP 2026-04-11: resources
/// participate in the same dispatch protocol).
///
/// The resolver flattens impl-block methods into `TirModule.functions`
/// and tags each with `method_info: { struct_name, trait_name, ... }`.
/// We use the `trait_name` to recognise handler methods.
fn lower_resume_in_handler_methods(project: &mut Package) {
    // Collect bare names of every effect and resource declaration so we
    // can recognise candidate impl blocks. A name collision between an
    // effect/resource and a regular trait would already be a resolver
    // error.
    let mut handler_names: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for effect in &module.effects {
            handler_names.insert(effect.name.clone());
        }
        for resource in &module.resources {
            handler_names.insert(resource.name.clone());
        }
    }

    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            let Some(method_info) = &func.method_info else {
                continue;
            };
            // `handler_names` is keyed by the bare effect / resource
            // declaration name; generic-resource impls carry the full
            // mangled form in `trait_name` ("Stream<u8>") and resolve
            // against the index via `base_trait_name` ("Stream").
            let Some(base_trait_name) = &method_info.base_trait_name else {
                continue;
            };
            if !handler_names.contains(base_trait_name) {
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
