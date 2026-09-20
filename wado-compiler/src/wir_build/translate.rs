//! Function body translation — converts TIR expressions and statements to WIR instructions.
//!
//! This is the core of the `tir_to_wir` phase, translating each TIR function body
//! into a sequence of WIR instructions.

use crate::compiler_item::SeqField;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::global_name;
use crate::nir::{FuncId, NirBinaryOp, NirFunction, NirParam, NirUnaryOp};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirName, WirType, WirTypeDef, WirTypeId};

use super::context::WirContext;
use crate::canonical::CanonicalIntrinsic;
use crate::compiler_item::CompilerItem;
use crate::name::{
    FqTraitName, FqTypeName, MangledName, MethodName, StructName, closure_call_name,
    multi_value_split_local,
};
use crate::nir_arena::{
    ArenaStructField, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind,
};
use crate::nir_value_graph::{OpaqueSource, ValueId};
use crate::optimize::multi_value_return::block_tail_call;
use crate::optimize::sroa_variant_return::settled_locals;
use crate::token::Span;
use crate::wir::{
    CmImportViolation, TraitBoundViolation, WirAbstractHeapType, WirFuncId, WirLocals, WirMeta,
};
use crate::wir_build::context::{CANONICAL_INSPECT_SLOT, ClosureWrapperFuncs};
use crate::{nir, tir};

pub(super) fn ref_binding_needs_boxing(
    binding_wir: &WirType,
    source_wir: Option<&WirType>,
) -> bool {
    match (binding_wir, source_wir) {
        (WirType::Ref { type_id: bt, .. }, Some(WirType::Ref { type_id: st, .. })) => bt != st,
        (WirType::Ref { .. }, Some(WirType::AbstractRef { .. })) => false,
        (WirType::Ref { .. }, Some(_)) => true,
        _ => false,
    }
}

pub(super) fn declare_and_set_local(name: String, ty: WirType, value: WirInstr) -> [WirInstr; 2] {
    [
        WirInstr::DeclareLocal {
            name: name.clone(),
            ty,
        },
        WirInstr::LocalSet {
            name,
            value: Box::new(value),
        },
    ]
}

/// A read of each of the named locals, in order.
fn local_reads(locals: &[(String, WirType)]) -> Vec<WirInstr> {
    locals
        .iter()
        .map(|(name, ty)| WirInstr::LocalGet {
            name: name.clone(),
            result_ty: ty.clone(),
        })
        .collect()
}

/// Recursively collect variable names from Let statements.
///
/// These names are gathered eagerly from the statement tree and preferred
/// when present; any missing entries are then backfilled from
/// `tir_func.locals[idx].name` (for example, slots created in expression
/// contexts the walker doesn't recurse into, or by optimizer passes that
/// allocate locals without emitting a `Let`).
fn collect_let_names(body: &Body, names: &mut IndexMap<u32, String>, block: BlockId) {
    for &sid in &body.blocks[block].stmts {
        match &body.stmts[sid].kind {
            StmtKind::Let {
                name, local_index, ..
            } => {
                names.insert(*local_index, name.clone());
            }
            StmtKind::Loop { body: b } => {
                collect_let_names(body, names, *b);
            }
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                collect_let_names(body, names, *then_block);
                if let Some(eb) = else_block {
                    collect_let_names(body, names, *eb);
                }
            }
            StmtKind::LabeledBlock { block: b, .. } => {
                collect_let_names(body, names, *b);
            }
            _ => {}
        }
    }
}

/// The split locals a `ParamAbi::MultiValue` parameter arrives in. The Wasm
/// parameters carry these names, so the body needs no entry code at all.
fn split_locals_for_params(
    ctx: &WirContext<'_>,
    tir_func: &NirFunction,
    type_table: &TypeTable,
    resolved_local_names: &IndexMap<u32, String>,
) -> IndexMap<u32, IndexMap<String, (String, WirType)>> {
    let mut out: IndexMap<u32, IndexMap<String, (String, WirType)>> = IndexMap::default();
    for param in &tir_func.params {
        let nir::ParamAbi::MultiValue {
            field_types,
            field_names,
        } = &param.param_abi
        else {
            continue;
        };
        let base = &resolved_local_names[&param.local_index];
        let mut split: IndexMap<String, (String, WirType)> = IndexMap::default();
        for (field_name, &field_type) in field_names.iter().zip(field_types) {
            let wir_ty = ctx.type_id_to_wir_type(type_table, field_type);
            split.insert(
                field_name.clone(),
                (multi_value_split_local(base, field_name), wir_ty),
            );
        }
        out.insert(param.local_index, split);
    }
    out
}

/// The WIR name of every parameter, keyed by local index. The Wasm signature
/// and the body read this one answer, so a slot has one name on both sides.
pub(super) fn resolve_param_names(params: &[NirParam]) -> IndexMap<u32, String> {
    let mut per_name: IndexMap<&str, u32> = IndexMap::default();
    for p in params {
        *per_name.entry(p.name.as_str()).or_default() += 1;
    }

    let mut out = IndexMap::default();
    let mut used: IndexSet<String> = IndexSet::default();
    for p in params {
        let needs_suffix = per_name.get(p.name.as_str()).copied().unwrap_or(0) > 1;
        out.insert(
            p.local_index,
            free_name(&p.name, p.local_index, needs_suffix, &mut used),
        );
    }
    out
}

/// Pre-compute the WIR-side name for every TIR local index, once, so
/// [`FunctionTranslator::local_name`] is an O(1) lookup rather than a scan of
/// `params` and `local_names` per reference. Parameters come from
/// [`resolve_param_names`]; a non-param local shadowing a param or let takes the
/// suffix alone, leaving the shadowed name untouched.
fn resolve_local_names(raw: &IndexMap<u32, String>, params: &[NirParam]) -> IndexMap<u32, String> {
    let mut out = resolve_param_names(params);
    let mut used: IndexSet<String> = out.values().cloned().collect();

    let mut total_per_name: IndexMap<&str, u32> = IndexMap::default();
    for name in raw.values() {
        *total_per_name.entry(name.as_str()).or_default() += 1;
    }

    for (idx, name) in raw {
        if out.contains_key(idx) {
            continue;
        }
        let needs_suffix = total_per_name.get(name.as_str()).copied().unwrap_or(0) > 1;
        out.insert(*idx, free_name(name, *idx, needs_suffix, &mut used));
    }
    out
}

/// `name`, suffixed with `_{idx}` until nothing else has taken it. One suffix
/// is not enough: `e` at index 2 gives `e_2`, which another local may spell.
fn free_name(name: &str, idx: u32, needs_suffix: bool, used: &mut IndexSet<String>) -> String {
    let mut out = if needs_suffix {
        format!("{name}_{idx}")
    } else {
        name.to_string()
    };
    while !used.insert(out.clone()) {
        out = format!("{out}_{idx}");
    }
    out
}

/// Register one wrapper per `CanonicalClosure_K` vtable slot for each reachable
/// functor — `$closure_wrapper_N` forwarding to `$call`, plus
/// `$closure_inspect_wrapper_N` / `_alt_` forwarding to the per-functor
/// `^Inspect` impls — each refcasting its args first. Must run before
/// `translate_function_bodies`, which resolves `ClosureToCanonical` against it.
pub fn register_closure_wrappers(ctx: &mut WirContext<'_>) {
    use crate::wir::WirType;

    // Snapshot the functor list so we can mutate ctx inside the loop.
    let functors: Vec<nir::ClosureFunctor> = ctx.package.closure_functors.clone();

    for functor in &functors {
        let module_source = &functor.module_source;
        let functor_key = (module_source.clone(), functor.id);
        if ctx.closure_wrapper_funcs.contains_key(&functor_key) {
            continue;
        }

        // Look up the $call func_id, scoped to the correct module.
        // If $call was removed by DCE (closure never used), skip this functor entirely.
        // This check must come before type lookups since DCE may have removed the
        // functor's types from the TypeTable.
        let functor_name = &functor.struct_name;
        let call_method_local = closure_call_name(module_source, functor.id);
        let call_method_fq = MangledName::in_module(module_source, &call_method_local);
        let call_func_id = match ctx.func_map.get(&call_method_fq).cloned() {
            Some(id) => id,
            None => continue,
        };

        // The wrapper's external signature is governed by the *canonical*
        // closure signature — the param / return types of the user-written
        // closure literal — not by the live `call_method.params`, which
        // TIR DAE may have shrunk. Decoupling here is what lets DAE drop
        // an unused `self` (no captures) or unused user args from `$call`
        // without desynchronising the function-table slot type from the
        // typed-fn callers that dispatch through it.
        let user_param_count = functor.canonical_user_params.len();
        let type_table = &*ctx.package.type_table.borrow();
        // Unit params are erased from the canonical signature and the wrapper
        // (matching the function-signature convention and every other
        // canonical-key site); `canonical_to_wrapper` maps each canonical
        // param position to its surviving wrapper slot.
        let mut user_params: Vec<WirType> = Vec::with_capacity(user_param_count);
        let mut canonical_to_wrapper: Vec<Option<usize>> = Vec::with_capacity(user_param_count);
        for (_, ty) in &functor.canonical_user_params {
            let wir_type = ctx.type_id_to_wir_type(type_table, *ty);
            if matches!(wir_type, WirType::Unit) {
                canonical_to_wrapper.push(None);
            } else {
                canonical_to_wrapper.push(Some(user_params.len()));
                user_params.push(wir_type);
            }
        }
        let result_wirs: Vec<WirType> = if functor.canonical_return == TypeTable::UNIT
            || functor.canonical_return == TypeTable::NEVER
        {
            vec![]
        } else {
            vec![ctx.type_id_to_wir_type(type_table, functor.canonical_return)]
        };
        let _ = type_table;

        // Decide vtable schema for this functor by looking the canonical
        // return type up in the inspectable gate computed at WirContext
        // start. Non-inspectable signatures get the slim `{ env, func }`
        // schema and skip inspect wrapper registration entirely.
        let return_type = functor.canonical_return;
        let is_inspectable = ctx
            .inspectable_fn_dispatch
            .contains(&(user_param_count, return_type));

        // Get canonical func type, threading the gate so the schema
        // matches what `translate_closure_to_canonical` will emit.
        let user_params_clone = user_params.clone();
        let (call_fn_type_id, _) = ctx.get_or_create_canonical_closure_type(
            user_params,
            result_wirs.clone(),
            is_inspectable,
        );

        // Get functor struct type ID
        let type_table = &*ctx.package.type_table.borrow();
        let functor_wir_type = ctx.type_id_to_wir_type(type_table, functor.ref_type_id);
        let _ = type_table;
        let WirType::Ref {
            type_id: functor_struct_type_id,
            ..
        } = &functor_wir_type
        else {
            panic!(
                "[WIR] closure functor `{functor_name}` has no registered struct type (got {functor_wir_type:?})"
            );
        };
        let functor_struct_type_id = functor_struct_type_id.clone();

        // Map each surviving `call_method.params` entry to its source slot
        // in the wrapper. Position 0 is always self (env, refcast); the
        // other positions match canonical_user_params by name. The mapping
        // tells `register_call_wrapper` exactly which wrapper-local to
        // forward into the inner `$call` per surviving param, so DAE can
        // freely shrink `$call.params` without breaking the wrapper.
        let call_func = functor.call_method.borrow();
        // Map each surviving `call_method.params` entry back to its source.
        // The synthesised env self always lives at position 0 with name
        // "self" AND the functor's ref type; that combination is the env
        // discriminator (a user-declared `self` parameter — common in
        // trait-method dispatch closures synthesised by `effect_dispatch`
        // — has the user's resource ref type, not the functor's struct
        // ref). Every other surviving param matches a `canonical_user_params`
        // entry by name.
        let live_param_sources: Vec<CallWrapperArg> = call_func
            .params
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                if i == 0 && p.name == "self" && p.type_id == functor.ref_type_id {
                    return Some(CallWrapperArg::TypedEnv);
                }
                let idx = functor
                    .canonical_user_params
                    .iter()
                    .position(|(name, _)| name == &p.name)
                    .unwrap_or_else(|| {
                        panic!(
                            "closure {functor_name}::$call param `{}` has no matching \
                             canonical user param (canonical: {:?})",
                            p.name,
                            functor
                                .canonical_user_params
                                .iter()
                                .map(|(n, _)| n)
                                .collect::<Vec<_>>(),
                        )
                    });
                // A unit-typed param is erased from both the wrapper and
                // `$call`'s own WIR signature, so nothing is forwarded.
                canonical_to_wrapper[idx].map(CallWrapperArg::UserParam)
            })
            .collect();
        drop(call_func);

        // Register the call wrapper.
        let global_id = ctx.closure_wrapper_funcs.len();
        let call_wrapper_fq = format!("closure/{module_source}/$closure_wrapper_{global_id}");
        let call_wrapper_id = register_call_wrapper(
            ctx,
            &call_wrapper_fq,
            call_fn_type_id,
            functor_struct_type_id.clone(),
            functor_wir_type.clone(),
            user_params_clone.len(),
            user_params_clone,
            !result_wirs.is_empty(),
            call_func_id,
            &live_param_sources,
        );

        // Register the inspect wrapper only when the functor's signature is
        // inspectable. It forwards to the per-functor `$Closure_N^Inspect`
        // impl synthesised in lower (Phase 2). When DCE pruned that impl or
        // the `Formatter` struct isn't registered, the wrapper body falls back
        // to `Unreachable` — the slot stays populated so the canonical struct
        // schema is consistent.
        let inspect_wrapper_id = if is_inspectable {
            let callback_fn_type_id = ctx.get_or_create_canonical_callback_fn_type();
            let inspect_trait = {
                let tt = ctx.package.type_table.borrow();
                tt.compiler_trait_fq(CompilerItem::Inspect)
            };
            Some(register_inspect_wrapper(
                ctx,
                module_source,
                functor_name,
                &inspect_trait,
                "inspect",
                global_id,
                callback_fn_type_id,
                functor_struct_type_id,
            ))
        } else {
            None
        };

        ctx.closure_wrapper_funcs.insert(
            functor_key,
            ClosureWrapperFuncs {
                call: call_wrapper_id,
                inspect: inspect_wrapper_id,
            },
        );
    }
}

/// Source of an argument the wrapper forwards into the inner `$call`.
/// One entry per surviving `call_method.params` slot.
#[derive(Debug, Clone, Copy)]
enum CallWrapperArg {
    /// The refcast `$typed_env` local — corresponds to `$call`'s `self`.
    TypedEnv,
    /// The wrapper's `$pN` user-param local at the given canonical index.
    UserParam(usize),
}

/// Build a call wrapper: refcast `env` to `&$Closure_N` (only when the
/// surviving `$call` still expects `self`), then `call $Closure_N::$call`
/// with the surviving args. The wrapper's external signature stays
/// `(env, canonical_user_params...) -> canonical_return` regardless of
/// which `$call` params have been DAE'd.
#[allow(clippy::too_many_arguments)]
fn register_call_wrapper(
    ctx: &mut WirContext<'_>,
    wrapper_fq: &str,
    fn_type_id: WirTypeId,
    functor_struct_type_id: WirTypeId,
    functor_wir_type: WirType,
    user_param_count: usize,
    user_params: Vec<WirType>,
    has_result: bool,
    call_func_id: WirFuncId,
    live_param_sources: &[CallWrapperArg],
) -> WirFuncId {
    use crate::wir::{WirFunction, WirName, WirType};

    let env_local = "$env".to_string();
    let typed_env_local = "$typed_env".to_string();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: WirAbstractHeapType::Struct,
        nullable: true,
    };

    let needs_typed_env = live_param_sources
        .iter()
        .any(|s| matches!(s, CallWrapperArg::TypedEnv));

    let mut body = Vec::new();
    if needs_typed_env {
        body.push(WirInstr::DeclareLocal {
            name: typed_env_local.clone(),
            ty: WirType::Ref {
                type_id: functor_struct_type_id.clone(),
                nullable: false,
            },
        });
        body.push(WirInstr::LocalSet {
            name: typed_env_local.clone(),
            value: Box::new(WirInstr::RefCast {
                type_id: functor_struct_type_id,
                nullable: false,
                expr: Box::new(WirInstr::LocalGet {
                    name: env_local.clone(),
                    result_ty: abstract_struct_nullable,
                }),
            }),
        });
    }

    let call_args: Vec<WirInstr> = live_param_sources
        .iter()
        .map(|src| match src {
            CallWrapperArg::TypedEnv => WirInstr::LocalGet {
                name: typed_env_local.clone(),
                result_ty: functor_wir_type.clone(),
            },
            CallWrapperArg::UserParam(idx) => WirInstr::LocalGet {
                name: format!("$p{idx}"),
                result_ty: user_params[*idx].clone(),
            },
        })
        .collect();

    let call_instr = WirInstr::Call {
        func_id: call_func_id,
        args: call_args,
    };
    if has_result {
        body.push(WirInstr::Return {
            value: Some(Box::new(call_instr)),
        });
    } else {
        body.push(call_instr);
    }

    let mut param_names = vec![env_local];
    for i in 0..user_param_count {
        param_names.push(format!("$p{i}"));
    }

    let func = WirFunction {
        name: WirName {
            fq: wrapper_fq.to_string(),
        },
        type_id: fn_type_id,
        param_names,
        body: Some(body),
        meta: WirMeta::default(),
        generic_origin: None,
        effects: Vec::new(),
        retains: Vec::new(),
        compiler_item: None,
        export_name: None,
        locals: WirLocals::default(),
    };

    ctx.register_function(func, None)
}

/// Build the inspect wrapper for a functor. Its external
/// signature is fixed at `(env, formatter)` by the canonical callback type, so
/// the function-table slot stays stable across DAE shrinkage on the impl: only
/// surviving params are forwarded. A DCE'd impl leaves an `Unreachable` body,
/// keeping the slot populated so the canonical schema holds.
#[allow(clippy::too_many_arguments)]
fn register_inspect_wrapper(
    ctx: &mut WirContext<'_>,
    module_source: &ModuleSource,
    functor_name: &str,
    trait_name: &FqTraitName,
    method_name: &str,
    global_id: usize,
    callback_fn_type_id: WirTypeId,
    functor_struct_type_id: WirTypeId,
) -> WirFuncId {
    use crate::wir::{WirFunction, WirName, WirType};

    let env_local = "$env".to_string();
    let formatter_local = "$formatter".to_string();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: WirAbstractHeapType::Struct,
        nullable: true,
    };

    // The per-functor impl's local name is `<fq functor>^Trait::method`;
    // module + local name together form its `func_map` key.
    let impl_local_name = MethodName::format_local(
        &FqTypeName::shape(module_source, functor_name),
        Some(trait_name),
        method_name,
    );
    let target_fq = MangledName::in_module(module_source, &impl_local_name);
    let target_func_id = ctx.func_map.get(&target_fq).cloned();

    // Look up the Formatter struct WIR type id once; needed to
    // refcast the abstract `(ref null struct)` arg to the concrete
    // `&Formatter` the per-functor impl expects.
    let formatter_struct_type_id = ctx
        .struct_type_map
        .get(&StructName::new(
            ModuleSource::format(),
            "Formatter".to_string(),
        ))
        .cloned();

    // Look up the per-functor impl's TIR function so we can read its
    // current `params` (post-DAE) and only forward the surviving slots.
    let impl_param_names: Option<Vec<String>> = ctx.package.functions.iter().find_map(|f| {
        let f = f.borrow();
        if f.is_dead {
            return None;
        }
        if f.module_source == *module_source && f.name == impl_local_name {
            Some(f.params.iter().map(|p| p.name.clone()).collect())
        } else {
            None
        }
    });

    // A DCE'd per-functor impl leaves no target: the vtable slot still has to
    // exist so every `CanonicalClosure_K` keeps one schema, and nothing can
    // reach the wrapper, so its body traps. The other two lookups are not
    // independently optional — once the impl survives, both its params and the
    // `Formatter` it takes must be there.
    let body = match target_func_id {
        None => vec![WirInstr::Unreachable],
        Some(func_id) => {
            let formatter_tid = formatter_struct_type_id.unwrap_or_else(|| {
                panic!(
                    "[WIR] `{functor_name}^{trait_name}::{method_name}` survived DCE but the `Formatter` struct is not registered"
                )
            });
            let impl_params = impl_param_names.unwrap_or_else(|| {
                panic!(
                    "[WIR] `{functor_name}^{trait_name}::{method_name}` is in `func_map` but has no live function record"
                )
            });
            let typed_env_local = "$typed_env".to_string();
            let typed_formatter_local = "$typed_formatter".to_string();
            let needs_typed_env = impl_params.iter().any(|n| n == "self");
            let needs_typed_formatter = impl_params.iter().any(|n| n == "f");

            let mut body = Vec::new();
            if needs_typed_env {
                body.push(WirInstr::DeclareLocal {
                    name: typed_env_local.clone(),
                    ty: WirType::Ref {
                        type_id: functor_struct_type_id.clone(),
                        nullable: false,
                    },
                });
                body.push(WirInstr::LocalSet {
                    name: typed_env_local.clone(),
                    value: Box::new(WirInstr::RefCast {
                        type_id: functor_struct_type_id.clone(),
                        nullable: false,
                        expr: Box::new(WirInstr::LocalGet {
                            name: env_local.clone(),
                            result_ty: abstract_struct_nullable.clone(),
                        }),
                    }),
                });
            }
            if needs_typed_formatter {
                body.push(WirInstr::DeclareLocal {
                    name: typed_formatter_local.clone(),
                    ty: WirType::Ref {
                        type_id: formatter_tid.clone(),
                        nullable: false,
                    },
                });
                body.push(WirInstr::LocalSet {
                    name: typed_formatter_local.clone(),
                    value: Box::new(WirInstr::RefCast {
                        type_id: formatter_tid.clone(),
                        nullable: false,
                        expr: Box::new(WirInstr::LocalGet {
                            name: formatter_local.clone(),
                            result_ty: abstract_struct_nullable,
                        }),
                    }),
                });
            }

            let call_args: Vec<WirInstr> = impl_params
                .iter()
                .map(|name| match name.as_str() {
                    "self" => WirInstr::LocalGet {
                        name: typed_env_local.clone(),
                        result_ty: WirType::Ref {
                            type_id: functor_struct_type_id.clone(),
                            nullable: false,
                        },
                    },
                    "f" => WirInstr::LocalGet {
                        name: typed_formatter_local.clone(),
                        result_ty: WirType::Ref {
                            type_id: formatter_tid.clone(),
                            nullable: false,
                        },
                    },
                    other => panic!(
                        "closure {functor_name}^{trait_name}::{method_name} param \
                         `{other}` is neither self nor formatter; the canonical layout \
                         is `(self, f)`."
                    ),
                })
                .collect();

            body.push(WirInstr::Call {
                func_id,
                args: call_args,
            });
            body
        }
    };

    let wrapper_fq = format!("closure/{module_source}/$closure_{method_name}_wrapper_{global_id}");
    let func = WirFunction {
        name: WirName { fq: wrapper_fq },
        type_id: callback_fn_type_id,
        param_names: vec![env_local, formatter_local],
        body: Some(body),
        meta: WirMeta::default(),
        generic_origin: None,
        effects: Vec::new(),
        retains: Vec::new(),
        compiler_item: None,
        export_name: None,
        locals: WirLocals::default(),
    };

    ctx.register_function(func, None)
}

/// Build the WIR body for a `FunctionKind::FnCanonicalDispatch` stub: cast
/// `self` to the shared `$canonical_inspectable_base`, then
/// `call_ref (struct.get base $slot self) (self.env, f)`. Every inspectable
/// `CanonicalClosure_K` subtypes that one base, so a stub reaches any shape
/// cast to it. `None` when no inspectable canonical struct exists.
#[allow(clippy::needless_pass_by_value)] // signature mirrors the param-name plumbing in translate_function_bodies
fn build_fn_canonical_dispatch_body(
    ctx: &mut WirContext<'_>,
    self_param_name: String,
    formatter_param_name: String,
    self_box_type_id: Option<TypeId>,
) -> Option<Vec<WirInstr>> {
    use crate::wir::{WirAbstractHeapType, WirType};

    // No inspectable canonical struct was ever registered → the stub is
    // dead code. Leave the bodyless declaration in place.
    let base_type_id = ctx.canonical_inspectable_base_type_id.clone()?;
    let callback_fn_type_id = ctx.get_or_create_canonical_callback_fn_type();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: WirAbstractHeapType::Struct,
        nullable: true,
    };
    // When the boxing pass rewrote `&fn(...)` to `Box<fn(...)>`, the
    // self parameter holds a wrapper struct whose `.value` field carries
    // the actual closure ref. Unwrap before refcasting.
    let self_load: WirInstr = if let Some(box_type_id) = self_box_type_id {
        let type_table = ctx.package.type_table.borrow();
        let wir_box_type = ctx.type_id_to_wir_type(&type_table, box_type_id);
        drop(type_table);
        let box_wir_type_id = match wir_box_type {
            WirType::Ref { ref type_id, .. } => type_id.clone(),
            _ => return None,
        };
        WirInstr::StructGet {
            type_id: box_wir_type_id.clone(),
            field_name: "value".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: self_param_name,
                result_ty: WirType::Ref {
                    type_id: box_wir_type_id,
                    nullable: false,
                },
            }),
            result_ty: abstract_struct_nullable.clone(),
        }
    } else {
        WirInstr::LocalGet {
            name: self_param_name,
            result_ty: abstract_struct_nullable.clone(),
        }
    };

    // Local that holds the refcast `self` so we can read both
    // `env` and the chosen vtable slot off it without re-casting.
    let typed_self = "$typed_self".to_string();
    Some(vec![
        WirInstr::DeclareLocal {
            name: typed_self.clone(),
            ty: WirType::Ref {
                type_id: base_type_id.clone(),
                nullable: false,
            },
        },
        WirInstr::LocalSet {
            name: typed_self.clone(),
            value: Box::new(WirInstr::RefCast {
                type_id: base_type_id.clone(),
                nullable: false,
                expr: Box::new(self_load),
            }),
        },
        WirInstr::CallRef {
            type_id: callback_fn_type_id.clone(),
            func_ref: Box::new(WirInstr::StructGet {
                type_id: base_type_id.clone(),
                field_name: CANONICAL_INSPECT_SLOT.to_string(),
                expr: Box::new(WirInstr::LocalGet {
                    name: typed_self.clone(),
                    result_ty: WirType::Ref {
                        type_id: base_type_id.clone(),
                        nullable: false,
                    },
                }),
                result_ty: WirType::Ref {
                    type_id: callback_fn_type_id,
                    nullable: false,
                },
            }),
            args: vec![
                WirInstr::StructGet {
                    type_id: base_type_id.clone(),
                    field_name: "env".to_string(),
                    expr: Box::new(WirInstr::LocalGet {
                        name: typed_self,
                        result_ty: WirType::Ref {
                            type_id: base_type_id,
                            nullable: false,
                        },
                    }),
                    result_ty: abstract_struct_nullable.clone(),
                },
                WirInstr::LocalGet {
                    name: formatter_param_name,
                    result_ty: abstract_struct_nullable,
                },
            ],
        },
    ])
}

/// Translate all pending function bodies from TIR to WIR instructions.
/// True when a branch body contains a `cold_path()` marker. Descends through
/// transparent grouping (`Block` / `Seq`) but stops at nested control flow
/// (`If` / `Loop`): a `cold_path()` inside an inner branch belongs to that
/// branch, not this one.
fn block_has_cold_path(body: &[WirInstr]) -> bool {
    body.iter().any(instr_has_cold_path)
}

fn instr_has_cold_path(i: &WirInstr) -> bool {
    match i {
        WirInstr::ColdPath => true,
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => block_has_cold_path(body),
        _ => false,
    }
}

/// True when an `if` has no meaningful `else` — either no else block, or one
/// whose only instructions are `nop`s. `if let` / `while let` desugaring lowers
/// to `if <test> { … } else { nop }`, which is semantically the implicit-else
/// (fall-through) shape, so guard-clause hinting must treat it like a bare `if`.
fn else_is_empty(else_body: &Option<Vec<WirInstr>>) -> bool {
    match else_body {
        None => true,
        Some(body) => body.iter().all(|i| matches!(i, WirInstr::Nop)),
    }
}

/// Finalize `builtin::cold_path()` markers into `metadata.code.branch_hint`
/// entries by wrapping the enclosing condition in [`WirInstr::BranchHint`]. Two
/// shapes: a marker inside an `if` branch hints that branch unlikely, and an
/// else-less `if cond { <diverges> }` whose fall-through reaches a marker hints
/// the condition likely. The marker itself emits nothing.
fn apply_cold_path_hints(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        apply_cold_path_hints_instr(instr);
    }
    hint_guard_fall_through(instrs, false);
}

/// Backward fall-through pass for the guard-clause idiom. `reaches_cold` says
/// whether the path just *after* this slice reaches a `cold_path()` marker
/// before any non-cold divergence; the return value says the same for the path
/// *before* it. A guard in front of a cold path gets its taken branch hinted
/// likely. Descends transparent `Seq`/`Block` tails, so desugarings still match.
fn hint_guard_fall_through(instrs: &mut [WirInstr], mut reaches_cold: bool) -> bool {
    for i in (0..instrs.len()).rev() {
        if reaches_cold
            && let WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } = &mut instrs[i]
            && else_is_empty(else_body)
            && then_body.iter().any(WirInstr::always_diverges)
            && !block_has_cold_path(then_body)
        {
            WirInstr::hint_condition(condition, true);
        }
        // Update `reaches_cold` for the position before `instrs[i]`. A bare
        // marker makes the path cold; a transparent group propagates through its
        // own backward pass; any other unconditional divergence ends it.
        match &mut instrs[i] {
            WirInstr::ColdPath => reaches_cold = true,
            WirInstr::Seq(body) | WirInstr::Block { body, .. } => {
                reaches_cold = hint_guard_fall_through(body, reaches_cold);
            }
            other if other.always_diverges() => reaches_cold = false,
            _ => {}
        }
    }
    reaches_cold
}

fn apply_cold_path_hints_instr(instr: &mut WirInstr) {
    match instr {
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            apply_cold_path_hints_instr(condition);
            apply_cold_path_hints(then_body);
            if let Some(else_body) = else_body {
                apply_cold_path_hints(else_body);
            }
            let then_cold = block_has_cold_path(then_body);
            let else_cold = else_body.as_ref().is_some_and(|b| block_has_cold_path(b));
            // Only a single cold side yields an unambiguous hint.
            let likely = match (then_cold, else_cold) {
                (true, false) => false,
                (false, true) => true,
                _ => return,
            };
            WirInstr::hint_condition(condition, likely);
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            apply_cold_path_hints(body);
        }
        other => other.for_each_boxed_child_mut(&mut |c| apply_cold_path_hints_instr(c)),
    }
}

pub fn translate_function_bodies(ctx: &mut WirContext<'_>) {
    let pending: Vec<_> = std::mem::take(&mut ctx.pending_bodies);
    let ctx_has_multi_value_returns = !ctx.multi_value_return_funcs.is_empty();

    for pending_body in &pending {
        let tir_func = pending_body.tir_func.borrow();
        let type_table = pending_body.type_table.borrow();

        // A `fn(..)^Inspect` dispatch stub carries an empty TIR
        // placeholder body; substitute the real one — a vtable indirect call
        // through `CanonicalClosure_K` — rather than translating it. Skipping
        // string-matching post-pass keeps name-format knowledge
        // confined to `name.rs` and `synthesis::traits`.
        if tir_func.fn_canonical_dispatch().is_some() {
            let names = resolve_param_names(&tir_func.params);
            let param_name = |at: usize, fallback: &str| {
                tir_func
                    .params
                    .get(at)
                    .map(|p| names[&p.local_index].clone())
                    .unwrap_or_else(|| fallback.to_string())
            };
            let self_param_name = param_name(0, "self");
            let formatter_param_name = param_name(1, "f");
            // After the boxing pass, the synthesized self parameter type
            // `&fn(...)` is rewritten to `Box<fn(...)>` (a struct wrapping
            // a closure ref). When that's happened the dispatch body has
            // to unwrap `.value` before refcasting; consult the type
            // table's box-wrapper registry to find out.
            let self_box_type_id = tir_func
                .params
                .first()
                .and_then(|p| type_table.box_payload_of(p.type_id).map(|_| p.type_id));
            drop(tir_func);
            let _ = type_table;
            let body = build_fn_canonical_dispatch_body(
                ctx,
                self_param_name,
                formatter_param_name,
                self_box_type_id,
            );
            if let Some(body) = body {
                ctx.functions[pending_body.wir_func_index].body = Some(body);
            }
            continue;
        }

        if let Some(ref body) = tir_func.body {
            // Build local-name map: params first, then `Let` statement
            // names (which carry the most descriptive identifiers — `?`
            // temps, hoisted-buf names, and so on). `tir_func.locals`
            // backfills entries that no `Let` shadows, covering parameter
            // slots without a body Let, slots created in expression
            // contexts the walker doesn't recurse into, and pre-lower
            // function bodies that haven't been desugared yet.
            let mut local_names = IndexMap::default();
            for param in &tir_func.params {
                local_names.insert(param.local_index, param.name.clone());
            }
            collect_let_names(body, &mut local_names, body.root);
            for (idx, local) in tir_func.locals.iter().enumerate() {
                let key = u32::try_from(idx).unwrap();
                local_names.entry(key).or_insert_with(|| local.name.clone());
            }
            let resolved_local_names = resolve_local_names(&local_names, &tir_func.params);
            let param_split_locals =
                split_locals_for_params(ctx, &tir_func, &type_table, &resolved_local_names);

            // Translate inside a nested block so the translator (and its reborrow of ctx)
            // is dropped before we write back to ctx.functions below.
            let mut wir_body = {
                let mut translator = FunctionTranslator {
                    ctx: &mut *ctx,
                    type_table: &type_table,
                    tir_func: &tir_func,
                    body,
                    label_stack: Vec::new(),
                    match_counter: 0,
                    local_counter: 0,
                    multi_value_split_locals: param_split_locals,
                    resolved_local_names,
                    // Only `try_emit_multi_value_let` reads this, and it returns
                    // early when no callee takes the ABI. A package with none
                    // pays no walk at all.
                    settled_locals: if ctx_has_multi_value_returns {
                        settled_locals(body)
                    } else {
                        IndexSet::default()
                    },
                    multi_value_results_taken: false,
                    force_fixed_string_repr: false,
                    discovered_local_types: IndexMap::default(),
                };
                translator.translate_block(body.root)
            };
            apply_cold_path_hints(&mut wir_body);
            let _ = type_table;
            drop(tir_func);
            ctx.functions[pending_body.wir_func_index].body = Some(wir_body);
        }
    }
}

/// `Option`'s `None` case index. Its declaration is a compiler item, so the
/// index is fixed; both the expression translator and a global's slot build
/// `None` from it.
pub(super) const OPTION_NONE_CASE: u32 = 1;

/// Tracks a Wasm block scope in the label stack for computing br depths.
pub(super) struct LabelEntry {
    /// Label name from TIR (for labeled blocks).
    pub(super) label: Option<String>,
    /// True if this is the outer block wrapping a loop (target for unlabeled break).
    pub(super) is_loop_break: bool,
    /// True if this is a loop instruction (target for continue).
    pub(super) is_loop_continue: bool,
}

/// Translator state for a single function.
pub(super) struct FunctionTranslator<'a, 'b> {
    pub(super) ctx: &'a mut WirContext<'b>,
    pub(super) type_table: &'a TypeTable,
    pub(super) tir_func: &'a NirFunction,
    pub(super) body: &'a Body,
    /// Stack of Wasm block scopes for computing br depths.
    pub(super) label_stack: Vec<LabelEntry>,
    /// Counter for generating unique match scrutinee local names.
    pub(super) match_counter: u32,
    /// Counter for generating unique temporary local names.
    pub(super) local_counter: u32,
    /// WIR-side local names indexed by TIR local index, with shadow / param
    /// collisions already disambiguated. Pre-computed once per function so
    /// `local_name` stays O(1) — the live formulation scanned the entire
    /// `local_names` map on every visit, which is O(N) per call and fired
    /// for every `LocalGet`/`LocalSet`/`LocalTee`/match-binding in the body.
    pub(super) resolved_local_names: IndexMap<u32, String>,
    /// TIR locals holding a multi-value-call result, mapped to the WIR split
    /// locals they were unpacked into and keyed by field name. A later
    /// `FieldAccess(LocalGet($tmp), name)` reads the matching split local
    /// instead of `StructGet($tmp, name)`, which would panic at codegen —
    /// `$tmp` was never assigned a struct ref.
    pub(super) multi_value_split_locals: IndexMap<u32, IndexMap<String, (String, WirType)>>,
    /// Locals bound once and never assigned again.
    // Only one of those may be split: a second definition would `LocalSet` a
    // base local the split never declared.
    settled_locals: IndexSet<u32>,
    /// Set while lowering a call whose N results have a taker: the split locals
    /// of a `let` bind, the wildcards of a discard, or a pass-through return.
    // Clear at the `Call` arm means nothing is taking them, so the arm rebuilds
    // the aggregate rather than leaving N results where one is expected.
    multi_value_results_taken: bool,
    /// True while translating the value of a `GlobalVarSet` to a global with
    /// [`crate::nir::NirGlobal::prefer_fixed_string_repr`] set. Bounds-overrides
    /// `package.string_inline_max_bytes` in [`Self::translate_packed_array`]
    /// so `wir_optimize::const_global` can promote the literal eager.
    /// Saved/restored around the `GlobalVarSet` case, so it never leaks into
    /// a sibling literal.
    pub(super) force_fixed_string_repr: bool,
    /// Local types read off the body's `Let` statements, for a function that
    /// reaches here without the lower phase's local allocation.
    discovered_local_types: IndexMap<u32, TypeId>,
}

impl FunctionTranslator<'_, '_> {
    /// The WIR local name for `index`, as [`resolve_local_names`] computed it,
    /// falling back to `$local_N`.
    pub(super) fn local_name(&self, index: u32) -> String {
        self.resolved_local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("$local_{index}"))
    }

    /// Build a `LocalGet` with the WIR type resolved from a TIR local index.
    fn local_get(&self, index: u32) -> WirInstr {
        let name = self.local_name(index);
        let result_ty = self.local_wir_type(index);
        WirInstr::LocalGet { name, result_ty }
    }

    /// Resolve the WIR type of a TIR local variable by index.
    fn local_wir_type(&self, index: u32) -> WirType {
        let param_count = self.tir_func.params.len();
        if (index as usize) < param_count {
            let type_id = self.tir_func.params[index as usize].type_id;
            self.wir_type(type_id)
        } else {
            // `locals` is indexed absolutely (entries 0..param_count are
            // params, entries param_count.. are non-param locals), matching
            // DeclareLocal generation.
            let type_id = self
                .tir_func
                .locals
                .get(index as usize)
                .map(|local| local.type_id)
                .or_else(|| self.discovered_local_types.get(&index).copied())
                .unwrap_or_else(|| panic!("[WIR] local {index} is read but no `let` declared it"));
            self.wir_type(type_id)
        }
    }

    /// Shorthand for `self.ctx.type_id_to_wir_type(self.type_table, type_id)`.
    pub(super) fn wir_type(&self, type_id: TypeId) -> WirType {
        self.ctx.type_id_to_wir_type(self.type_table, type_id)
    }

    /// The WIR type index of a `TypeId` that must lower to a concrete reference
    /// — a struct, variant, array, or list.
    ///
    /// Callers reach for this where the TIR shape already guarantees a
    /// reference — a struct literal's own type, a `&Array<T>` builtin argument,
    /// a variant scrutinee. It panics where the assumption was made rather than
    /// letting the caller emit something that only fails Wasm validation.
    #[track_caller]
    pub(super) fn ref_type_id(&self, type_id: TypeId) -> WirTypeId {
        match self.wir_type(type_id) {
            WirType::Ref { type_id, .. } => type_id,
            other => panic!(
                "[WIR] expected a concrete reference type, got {other:?} for {:?}",
                self.type_table.get(type_id)
            ),
        }
    }

    /// Look up the WIR type of a struct field.
    pub(super) fn struct_field_wir_type(
        &self,
        struct_type_id: &WirTypeId,
        field_name: &str,
    ) -> WirType {
        let Some(WirTypeDef::Struct(st)) = self.ctx.types.get(struct_type_id.index() as usize)
        else {
            panic!(
                "[WIR] field `{field_name}` read from type {}, which is not a registered struct",
                struct_type_id.index()
            );
        };
        st.fields
            .iter()
            .find(|f| f.name == field_name)
            .unwrap_or_else(|| panic!("[WIR] struct `{}` has no field `{field_name}`", st.name.fq))
            .ty
            .clone()
    }

    /// Look up the element WIR type of an array type.
    pub(super) fn array_element_wir_type(&self, array_type_id: &WirTypeId) -> WirType {
        let Some(WirTypeDef::Array(at)) = self.ctx.types.get(array_type_id.index() as usize) else {
            panic!(
                "[WIR] element read from type {}, which is not a registered array",
                array_type_id.index()
            );
        };
        at.element_type.clone()
    }

    /// Build a `StructNew` instruction, wrapping each field value with `RefAsNonNull`
    /// where the struct definition declares a non-nullable reference field.
    pub(super) fn struct_new(&self, type_id: WirTypeId, fields: Vec<WirInstr>) -> WirInstr {
        let fields = self.cast_nonnull_fields(&type_id, fields);
        WirInstr::StructNew { type_id, fields }
    }

    /// Rebuild the aggregate a multi-value call promised, for a site that takes
    /// the whole value rather than its fields.
    // Without this, one such site would cost the ABI to every other site: the
    // classifier decides a function's return ABI once.
    fn rebuild_multi_value_result(
        &mut self,
        call: WirInstr,
        result_type: TypeId,
        fields: &[(String, TypeId)],
    ) -> WirInstr {
        let struct_type = self.ref_type_id(result_type);
        let (mut instrs, bound) = self.bind_multi_value_results_to_temps(call, fields);
        instrs.push(self.struct_new(struct_type, local_reads(&bound)));
        WirInstr::Seq(instrs)
    }

    /// [`Self::bind_multi_value_results`] into fresh temporaries, for a taker
    /// with no name of its own to give them.
    fn bind_multi_value_results_to_temps(
        &mut self,
        call: WirInstr,
        fields: &[(String, TypeId)],
    ) -> (Vec<WirInstr>, Vec<(String, WirType)>) {
        let names: Vec<String> = fields
            .iter()
            .map(|(field_name, _)| self.fresh_local(&format!("$mv_{field_name}")))
            .collect();
        self.bind_multi_value_results(call, fields, &names)
    }

    /// Declare one local per field, named by `names`, and bind into them the N
    /// results the call leaves on the stack.
    fn bind_multi_value_results(
        &mut self,
        call: WirInstr,
        fields: &[(String, TypeId)],
        names: &[String],
    ) -> (Vec<WirInstr>, Vec<(String, WirType)>) {
        assert_eq!(
            names.len(),
            fields.len(),
            "[WIR] a multi-value bind needs one local per result"
        );
        let mut instrs = Vec::with_capacity(fields.len() + 1);
        let mut bound = Vec::with_capacity(fields.len());
        for ((_, field_type), name) in fields.iter().zip(names) {
            let ty = self.ctx.type_id_to_wir_type(self.type_table, *field_type);
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty: ty.clone(),
            });
            bound.push((name.clone(), ty));
        }
        instrs.push(WirInstr::MultiValueLocalBind {
            instr: Box::new(call),
            locals: bound.iter().map(|(name, _)| Some(name.clone())).collect(),
        });
        (instrs, bound)
    }

    /// Translate the arguments of a call to `func`, handing a parameter that
    /// takes the multi-value ABI over one field at a time.
    fn translate_args_for_callee(
        &mut self,
        func_id: FuncId,
        ordered: &[Operand],
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        let split = self
            .ctx
            .multi_value_param_funcs
            .get(&func_id)
            .cloned()
            .unwrap_or_default();
        self.without_multi_value_results(|t| t.translate_args(ordered, &split))
    }

    /// One argument as N values, taken off a multi-value call's results or a
    /// literal's own initialisers. Anything else spills once and reads it back.
    fn split_argument(
        &mut self,
        op: Operand,
        fields: &[(String, TypeId)],
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        if let Some(expr) = op.as_expr() {
            match &self.body.exprs[expr].kind {
                ExprKind::Call { func_id, .. }
                    if self
                        .multi_value_result_fields(*func_id)
                        .is_some_and(|got| self.abi_fields_agree(got, fields)) =>
                {
                    let call = self.take_multi_value_results(|t| t.translate_expr(expr));
                    let (instrs, bound) = self.bind_multi_value_results_to_temps(call, fields);
                    return (instrs, local_reads(&bound));
                }
                ExprKind::StructLiteral {
                    fields: written, ..
                } if written.len() == fields.len()
                    && fields
                        .iter()
                        .all(|(n, _)| written.iter().any(|f| f.name == *n)) =>
                {
                    let written = written.clone();
                    return self.split_struct_literal(&written, fields);
                }
                _ => {}
            }
        }

        let struct_type = self.ref_type_id(self.operand_type_id(op));
        let (prelude, read_whole) = self.spill_operand(op, "$mv_arg");
        let reads = fields
            .iter()
            .map(|(field_name, field_type)| WirInstr::StructGet {
                type_id: struct_type.clone(),
                field_name: field_name.clone(),
                expr: Box::new(read_whole.clone()),
                result_ty: self.ctx.type_id_to_wir_type(self.type_table, *field_type),
            })
            .collect();
        (prelude, reads)
    }

    /// A struct literal's initialisers as the callee's N arguments, so the
    /// aggregate is never built.
    // Each is spilled in the order it was written, because the ABI reads them
    // in declaration order and the two can differ.
    fn split_struct_literal(
        &mut self,
        written: &[ArenaStructField],
        fields: &[(String, TypeId)],
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        let mut prelude = Vec::new();
        let mut by_name: IndexMap<String, WirInstr> = IndexMap::default();
        for field in written {
            let (p, read) = self.spill_operand(field.value, "$mv_field");
            prelude.extend(p);
            by_name.insert(field.name.clone(), read);
        }
        let reads = fields
            .iter()
            .map(|(name, _)| {
                by_name
                    .swap_remove(name)
                    .expect("literal field checked present")
            })
            .collect();
        (prelude, reads)
    }

    /// Evaluate `op` into a fresh local, and answer that with the read of it.
    /// Pins the evaluation where it stands, ahead of whatever follows.
    fn spill_operand(&mut self, op: Operand, prefix: &str) -> (Vec<WirInstr>, WirInstr) {
        let ty = self
            .ctx
            .type_id_to_wir_type(self.type_table, self.operand_type_id(op));
        let name = self.fresh_local(prefix);
        let value = self.translate_operand(op);
        let prelude = declare_and_set_local(name.clone(), ty.clone(), value).to_vec();
        let read = WirInstr::LocalGet {
            name,
            result_ty: ty,
        };
        (prelude, read)
    }

    /// The per-field result types a multi-value callee returns, in field order.
    fn multi_value_result_fields(&self, func_id: FuncId) -> Option<&[(String, TypeId)]> {
        self.ctx
            .multi_value_return_funcs
            .get(&func_id)
            .map(Vec::as_slice)
    }

    /// Whether a callee's N results can be handed straight to another's N
    /// parameters, which the lowered `WirType` of each field decides.
    // Not `TypeKey`: it resolves through newtype erasure, so two fields share
    // one and still lower to a ref against an i32.
    fn abi_fields_agree(&self, left: &[(String, TypeId)], right: &[(String, TypeId)]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|((ln, lt), (rn, rt))| {
                ln == rn
                    && self.ctx.type_id_to_wir_type(self.type_table, *lt)
                        == self.ctx.type_id_to_wir_type(self.type_table, *rt)
            })
    }

    /// Bind `let local = Call(f)` to one split local per result of `f`, which a
    /// later `FieldAccess` on the local then reads in place of the aggregate.
    // `block_tail_call` is the same recogniser `optimize::multi_value_return`
    // validates call sites with, so the shapes the two accept cannot drift.
    fn try_emit_multi_value_let(&mut self, local_index: u32, value: ExprId) -> Option<WirInstr> {
        if !self.settled_locals.contains(&local_index) {
            return None;
        }
        let mut prefix: Vec<StmtId> = Vec::new();
        let (func_id, _, call) = block_tail_call(self.body, Operand::Expr(value), &mut prefix)?;
        let fields = self.multi_value_result_fields(func_id)?.to_vec();

        let base = self.local_name(local_index);
        let names: Vec<String> = fields
            .iter()
            .map(|(field_name, _)| multi_value_split_local(&base, field_name))
            .collect();

        // Whatever ran ahead of the call in the block still has to run, and
        // ahead of the bind — the receiver the call reads is bound there.
        let mut instrs: Vec<WirInstr> = self.translate_stmts(&prefix);
        let call_instr = self.take_multi_value_results(|t| t.translate_expr(call));
        let (bind, bound) = self.bind_multi_value_results(call_instr, &fields, &names);
        instrs.extend(bind);

        let split = fields
            .iter()
            .zip(bound)
            .map(|((field_name, _), local)| (field_name.clone(), local))
            .collect();
        self.multi_value_split_locals.insert(local_index, split);

        Some(WirInstr::Seq(instrs))
    }

    /// Lower `f` with the multi-value results accounted for: the caller is
    /// binding or dropping them, so a call inside it may leave N on the stack.
    fn take_multi_value_results<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_multi_value_results(true, f)
    }

    /// Lower `f` with no taker for a multi-value result. A taker is for the call
    /// itself, never for what the call is passed.
    fn without_multi_value_results<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_multi_value_results(false, f)
    }

    fn with_multi_value_results<R>(&mut self, taken: bool, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = std::mem::replace(&mut self.multi_value_results_taken, taken);
        let out = f(self);
        self.multi_value_results_taken = outer;
        out
    }

    /// Whether the callee returns its aggregate as N Wasm results.
    fn callee_returns_multi_value(&self, func_id: FuncId) -> bool {
        self.ctx.multi_value_return_funcs.contains_key(&func_id)
    }

    /// A synthetic local's name, out of the way of every source local: codegen
    /// keys locals by name and merges two slots that share one.
    ///
    /// Stable in its input, so a name that carries its own identity — one per
    /// match, one per cloned array type — keeps it.
    pub(super) fn unshadowed(&self, mut name: String) -> String {
        while self.resolved_local_names.values().any(|n| *n == name) {
            name.push('_');
        }
        name
    }

    /// An [`Self::unshadowed`] name no other synthetic local in this function
    /// holds either.
    pub(super) fn fresh_local(&mut self, base: &str) -> String {
        self.local_counter += 1;
        self.unshadowed(format!("{base}_{}", self.local_counter))
    }

    /// Return the N results by binding the aggregate and reading its fields —
    /// what a tail [`lift_leaves_to_returns`] cannot reach lowers to instead,
    /// at the cost of the struct the lift would have avoided.
    fn multi_value_return_by_fields(&mut self, value: WirInstr) -> WirInstr {
        let nir::ReturnAbi::MultiValue {
            result_types,
            field_names,
        } = &self.tir_func.return_abi
        else {
            panic!(
                "[WIR] `{}` is not under the multi-value ABI",
                self.tir_func.name
            );
        };
        let result_types = result_types.clone();
        let field_names = field_names.clone();
        assert_eq!(
            field_names.len(),
            result_types.len(),
            "[WIR] the multi-value ABI of `{}` pairs its field names with its result types",
            self.tir_func.name
        );
        let aggregate = self
            .ctx
            .type_id_to_wir_type(self.type_table, self.tir_func.return_type);
        let WirType::Ref { type_id, .. } = &aggregate else {
            panic!(
                "[WIR] the multi-value return of `{}` is not a struct reference",
                self.tir_func.name
            );
        };
        let type_id = type_id.clone();
        let name = self.fresh_local("$mv_return");
        let fields: Vec<WirInstr> = field_names
            .iter()
            .zip(&result_types)
            .map(|(field_name, result_type)| WirInstr::StructGet {
                type_id: type_id.clone(),
                field_name: field_name.clone(),
                expr: Box::new(WirInstr::LocalGet {
                    name: name.clone(),
                    result_ty: aggregate.clone(),
                }),
                result_ty: self.ctx.type_id_to_wir_type(self.type_table, *result_type),
            })
            .collect();
        WirInstr::Seq(vec![
            WirInstr::DeclareLocal {
                name: name.clone(),
                ty: aggregate,
            },
            WirInstr::LocalSet {
                name,
                value: Box::new(value),
            },
            WirInstr::Return {
                value: Some(Box::new(WirInstr::Seq(fields))),
            },
        ])
    }

    /// A statement-position call to a `ReturnAbi::MultiValue` function: bind its
    /// N results to wildcards, since a single `Drop` cannot consume them.
    fn try_emit_multi_value_discard(&mut self, value: Operand) -> Option<WirInstr> {
        let mut prefix: Vec<StmtId> = Vec::new();
        let (func_id, _, call) = block_tail_call(self.body, value, &mut prefix)?;
        let arity = self.multi_value_result_fields(func_id)?.len();

        let mut instrs: Vec<WirInstr> = self.translate_stmts(&prefix);
        let call_instr = self.take_multi_value_results(|t| t.translate_expr(call));
        instrs.push(WirInstr::MultiValueLocalBind {
            instr: Box::new(call_instr),
            locals: vec![None; arity],
        });
        Some(WirInstr::Seq(instrs))
    }

    /// Resolve the WIR tuple struct type and translate its non-unit field
    /// initialisers, applying `cast_nonnull_fields` to honour non-nullable
    /// field declarations. Used by `TupleLiteral` lowering (the resulting
    /// `StructNew` is later unwrapped to a `Seq(fields)` at the function
    /// return boundary if `ReturnAbi::MultiValue` is set, or left as-is
    /// for the heap-resident path).
    fn tuple_constructor_args(
        &mut self,
        tuple_type_id: tir::TypeId,
        elements: &[Operand],
    ) -> (WirTypeId, Vec<WirInstr>) {
        let elem_type_ids: Vec<tir::TypeId> =
            elements.iter().map(|e| self.operand_type_id(*e)).collect();
        // A tuple interned by CM binding synthesis can carry `TypeId`s the
        // registrar never saw, so this miss is recoverable: search for a
        // structurally equal tuple, then define one.
        let wir_type = self
            .ctx
            .try_type_id_to_wir_type(self.type_table, tuple_type_id);
        let wir_type_id = match &wir_type {
            Some(WirType::Ref { type_id, .. }) => Some(type_id.clone()),
            _ if elements.len() >= 2 => self
                .ctx
                .find_tuple_type_for_elements(self.type_table, &elem_type_ids)
                .or_else(|| {
                    self.ctx
                        .define_tuple_struct_for_elements(self.type_table, &elem_type_ids)
                }),
            _ => None,
        };
        let Some(type_id) = wir_type_id else {
            panic!(
                "[WIR] tuple literal could not resolve a tuple struct type (expr type_id={tuple_type_id:?}, elements={})",
                elements.len()
            );
        };
        // Filter out unit-typed elements before borrowing self mutably to
        // translate them; chaining the filter into the iterator below would
        // double-borrow self.
        let non_unit: Vec<Operand> = elements
            .iter()
            .copied()
            .filter(|e| {
                !matches!(
                    self.ctx
                        .type_id_to_wir_type(self.type_table, self.operand_type_id(*e)),
                    WirType::Unit
                )
            })
            .collect();
        let raw_fields: Vec<WirInstr> = non_unit
            .into_iter()
            .map(|e| self.translate_operand(e))
            .collect();
        let fields = self.cast_nonnull_fields(&type_id, raw_fields);
        (type_id, fields)
    }

    /// Lower an `ArrayLiteral`: `array.new_fixed<T>(e0, …)` for the raw
    /// `Array<T>` a `[e0, e1, …]` literal denotes (WEP 2026-08-24), wrapped in
    /// `struct.new List<T> { repr, used: N }` where the node is `List`-typed.
    fn build_array_literal(
        &mut self,
        array_type_id: tir::TypeId,
        elements: &[Operand],
    ) -> WirInstr {
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, array_type_id);
        let WirType::Ref { type_id, .. } = wir_type else {
            panic!(
                "[WIR] ArrayLiteral expected Ref WirType, got {wir_type:?} (type_id={array_type_id:?})"
            );
        };
        let is_raw_array = matches!(
            self.type_table.get(array_type_id),
            ResolvedType::BuiltinArray(_)
        );
        let element_instrs: Vec<WirInstr> = elements
            .iter()
            .map(|e| self.translate_operand(*e))
            .collect();
        if is_raw_array {
            return WirInstr::ArrayNewFixed {
                type_id,
                elements: element_instrs,
            };
        }
        // The `repr` field is a non-nullable ref to the raw `Array<T>`.
        let WirType::Ref {
            type_id: raw_array_type_id,
            ..
        } = self.struct_field_wir_type(&type_id, SeqField::Backing.field_name())
        else {
            panic!("[WIR] ArrayLiteral: List<T> struct {type_id:?} has no `repr` array field");
        };
        let used = i32::try_from(element_instrs.len())
            .unwrap_or_else(|_| panic!("[WIR] array literal has more than i32::MAX elements"));
        self.struct_new(
            type_id,
            vec![
                WirInstr::ArrayNewFixed {
                    type_id: raw_array_type_id,
                    elements: element_instrs,
                },
                WirInstr::I32Const(used),
            ],
        )
    }

    /// Build a `StructSet` instruction, wrapping the value with `RefAsNonNull`
    /// if the target field is a non-nullable reference.
    fn struct_set(
        &self,
        type_id: WirTypeId,
        field_name: String,
        expr: WirInstr,
        value: WirInstr,
    ) -> WirInstr {
        let value = if self.is_field_nonnull_ref(&type_id, &field_name) {
            WirInstr::RefAsNonNull(Box::new(value))
        } else {
            value
        };
        WirInstr::StructSet {
            type_id,
            field_name,
            expr: Box::new(expr),
            value: Box::new(value),
        }
    }

    /// Translate `*r = v` for an in-place aggregate referent by copying `v` into
    /// the shared handle field by field, through two temps so each side is
    /// evaluated once. Only the expression-position shape (`let u = (*r = v);`)
    /// reaches here — lower already expands the statement-position one, and
    /// folds a box-shaped referent to a `.value` assignment.
    fn translate_deref_assign(&mut self, ref_expr: Operand, val: WirInstr) -> WirInstr {
        let recv = self.translate_operand(ref_expr);
        let wir_type = self.wir_type(self.operand_type_id(ref_expr));
        let WirType::Ref { type_id, .. } = wir_type else {
            panic!("[WIR] deref-assign referent expected Ref WirType, got {wir_type:?}");
        };
        let Some(WirTypeDef::Struct(st)) = self.ctx.types.get(type_id.index() as usize) else {
            panic!("[WIR] deref-assign referent is not a struct type: {type_id:?}");
        };
        let fields: Vec<(String, WirType)> = st
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();

        // WIR-unique prefix: codegen dedups `DeclareLocal` by name, so this must
        // not clash with the lower path's `$deref_ref_{nir_idx}` temps.
        let ref_local = self.fresh_local("$expr_deref_ref");
        let val_local = self.fresh_local("$expr_deref_val");
        let ref_ty = WirType::Ref {
            type_id: type_id.clone(),
            nullable: false,
        };
        let mut instrs = Vec::with_capacity(4 + fields.len());
        instrs.extend(declare_and_set_local(
            ref_local.clone(),
            ref_ty.clone(),
            recv,
        ));
        instrs.extend(declare_and_set_local(
            val_local.clone(),
            ref_ty.clone(),
            val,
        ));
        for (field_name, field_ty) in fields {
            let target = WirInstr::LocalGet {
                name: ref_local.clone(),
                result_ty: ref_ty.clone(),
            };
            let source = WirInstr::StructGet {
                type_id: type_id.clone(),
                field_name: field_name.clone(),
                expr: Box::new(WirInstr::LocalGet {
                    name: val_local.clone(),
                    result_ty: ref_ty.clone(),
                }),
                result_ty: field_ty,
            };
            instrs.push(self.struct_set(type_id.clone(), field_name, target, source));
        }
        WirInstr::Seq(instrs)
    }

    /// Wrap each field value with `RefAsNonNull` where the struct definition
    /// declares a non-nullable reference field.
    fn cast_nonnull_fields(&self, type_id: &WirTypeId, fields: Vec<WirInstr>) -> Vec<WirInstr> {
        let idx = type_id.index() as usize;
        if idx < self.ctx.types.len()
            && let WirTypeDef::Struct(st) = &self.ctx.types[idx]
        {
            fields
                .into_iter()
                .enumerate()
                .map(|(i, instr)| {
                    if st.fields.get(i).is_some_and(|f| f.ty.is_nonnull_ref()) {
                        WirInstr::RefAsNonNull(Box::new(instr))
                    } else {
                        instr
                    }
                })
                .collect()
        } else {
            fields
        }
    }

    /// Check if a named field of a struct type is a non-nullable reference.
    fn is_field_nonnull_ref(&self, type_id: &WirTypeId, field_name: &str) -> bool {
        let idx = type_id.index() as usize;
        if idx < self.ctx.types.len()
            && let WirTypeDef::Struct(st) = &self.ctx.types[idx]
        {
            st.fields
                .iter()
                .any(|f| f.name == field_name && f.ty.is_nonnull_ref())
        } else {
            false
        }
    }

    /// Translate the top-level function body: declares locals and translates statements.
    fn translate_block(&mut self, block_id: BlockId) -> Vec<WirInstr> {
        let arena = self.body;
        let block = &arena.blocks[block_id];
        let mut instrs = Vec::new();

        // Declare local variables.
        // `locals` may only contain body locals (not params) or it may be empty
        // for functions that haven't been through the lower phase's local allocation.
        // Fall back to scanning Let statements to discover locals.
        let param_count = self.tir_func.params.len();
        if self.tir_func.locals.is_empty() {
            // Scan block for Let declarations to discover local types
            self.declare_locals_from_stmts(&mut instrs, &block.stmts);
        } else {
            for (i, local) in self.tir_func.locals.iter().enumerate() {
                // Skip entries that correspond to params (they're already declared)
                if i < param_count {
                    continue;
                }
                let idx = u32::try_from(i).unwrap();
                let local_name = self.local_name(idx);
                assert!(
                    local.type_id != TypeTable::UNKNOWN,
                    "[WIR] local `{local_name}` of `{}` has no resolved type",
                    self.tir_func.name
                );
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, local.type_id);
                // Skip unit-type locals (unit has no Wasm representation)
                if matches!(wir_type, WirType::Unit) {
                    continue;
                }
                instrs.push(WirInstr::DeclareLocal {
                    name: local_name,
                    ty: wir_type,
                });
            }
        }

        // Translate statements
        instrs.extend(self.translate_stmts(&block.stmts));

        instrs
    }

    /// Recover each `let`'s local from the statements, as a `DeclareLocal` and
    /// as its type. Used when `locals` is empty, as a library module's are.
    fn declare_locals_from_stmts(&mut self, instrs: &mut Vec<WirInstr>, stmts: &[StmtId]) {
        let param_count = u32::try_from(self.tir_func.params.len()).unwrap();
        for stmt_id in stmts {
            match &self.body.stmts[*stmt_id].kind {
                StmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    // Params are already declared via param_names.
                    if *local_index >= param_count {
                        // `locals` has no entry to answer a read of it.
                        self.discovered_local_types.insert(*local_index, *type_id);
                        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, *type_id);
                        // Skip unit-type locals (unit has no Wasm representation)
                        if !matches!(wir_type, WirType::Unit) {
                            let local_name = self.local_name(*local_index);
                            instrs.push(WirInstr::DeclareLocal {
                                name: local_name,
                                ty: wir_type,
                            });
                        }
                    }
                }
                StmtKind::Loop { body } => {
                    self.declare_locals_from_stmts(instrs, &self.body.blocks[*body].stmts);
                }
                StmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    self.declare_locals_from_stmts(instrs, &self.body.blocks[*then_block].stmts);
                    if let Some(eb) = else_block {
                        self.declare_locals_from_stmts(instrs, &self.body.blocks[*eb].stmts);
                    }
                }
                StmtKind::LabeledBlock { block, .. } => {
                    self.declare_locals_from_stmts(instrs, &self.body.blocks[*block].stmts);
                }
                _ => {}
            }
        }
    }

    /// Translate a list of TIR statements to WIR instructions (no local declarations).
    pub(super) fn translate_stmts(&mut self, stmts: &[StmtId]) -> Vec<WirInstr> {
        let mut instrs = Vec::new();
        for stmt_id in stmts {
            if let Some(instr) = self.translate_stmt(*stmt_id) {
                instrs.push(instr);
            }
        }
        instrs
    }

    /// Translate statements where the last expression produces the block's value.
    ///
    /// Used for if-expression branches and labeled-block-expression bodies.
    /// The last `Expr` statement is NOT dropped; it stays on the Wasm stack as the result.
    /// Also handles statement-level If/IfLet as value-producing when they're the
    /// last statement (TIR stores these as statements, not expressions).
    pub(super) fn translate_stmts_as_value(&mut self, stmts: &[StmtId]) -> Vec<WirInstr> {
        let arena = self.body;
        let mut instrs = Vec::new();
        let len = stmts.len();
        for (i, stmt_id) in stmts.iter().enumerate() {
            let is_last = i + 1 == len;
            if is_last {
                // Last statement: if it's an Expr, translate without drop. A
                // promoted pure value (`Operand::Value`) is the block's value
                // directly — extract it; it is never UNIT.
                if let StmtKind::Expr(op) = &arena.stmts[*stmt_id].kind {
                    let op = *op;
                    let instr = match op {
                        Operand::Expr(expr) => self.translate_expr_as_value(expr),
                        Operand::Value(_) => self.translate_operand(op),
                    };
                    instrs.push(instr);
                    if self.is_stackless_type(self.operand_type_id(op))
                        && !instrs.last().is_some_and(WirInstr::ends_with_terminator)
                    {
                        instrs.push(WirInstr::Unreachable);
                    }
                    continue;
                }
                // Statement-level If with else can produce a value
                if let StmtKind::If {
                    condition,
                    then_block,
                    else_block: Some(else_block),
                    ..
                } = &arena.stmts[*stmt_id].kind
                {
                    let condition = *condition;
                    let then_block = *then_block;
                    let else_block = *else_block;
                    if let Some(result_type) = self.infer_stmts_result_type(then_block) {
                        let cond = self.translate_operand(condition);
                        self.label_stack.push(LabelEntry {
                            label: None,
                            is_loop_break: false,
                            is_loop_continue: false,
                        });
                        let then_body =
                            self.translate_stmts_as_value(&arena.blocks[then_block].stmts);
                        let else_body =
                            Some(self.translate_stmts_as_value(&arena.blocks[else_block].stmts));
                        self.label_stack.pop();
                        instrs.push(WirInstr::If {
                            condition: Box::new(cond),
                            result: Some(result_type),
                            then_body,
                            else_body,
                        });
                        continue;
                    }
                }
            }
            if let Some(instr) = self.translate_stmt(*stmt_id) {
                let needs_unreachable =
                    is_last && !instr.produces_stack_value() && !instr.ends_with_terminator();
                instrs.push(instr);
                if needs_unreachable {
                    instrs.push(WirInstr::Unreachable);
                }
            }
        }
        instrs
    }

    pub(super) fn is_stackless_type(&self, ty: TypeId) -> bool {
        self.type_table.is_stackless(ty)
    }

    /// Infer the WIR result type from the last statement in a block.
    /// Returns `Some(type)` if the last statement can produce a value, `None` otherwise.
    fn infer_stmts_result_type(&self, block_id: BlockId) -> Option<WirType> {
        let arena = self.body;
        arena.blocks[block_id]
            .stmts
            .last()
            .and_then(|stmt_id| match &arena.stmts[*stmt_id].kind {
                StmtKind::Expr(op) => {
                    let ty = self.operand_type_id(*op);
                    if self.is_stackless_type(ty) {
                        None
                    } else {
                        Some(self.ctx.type_id_to_wir_type(self.type_table, ty))
                    }
                }
                StmtKind::If {
                    then_block,
                    else_block: Some(_),
                    ..
                } => self.infer_stmts_result_type(*then_block),
                _ => None,
            })
    }

    /// Translate an expression in "value position" — the result stays on the Wasm stack.
    ///
    /// Handles cases where TIR assigns UNIT type to expressions that actually produce
    /// values in a given context (e.g., nested if expressions, chained assignments).
    pub(super) fn translate_expr_as_value(&mut self, expr_id: ExprId) -> WirInstr {
        let arena = self.body;
        // If the expression already has a non-UNIT type, translate normally
        if !self.is_stackless_type(arena.exprs[expr_id].type_id) {
            return self.translate_expr(expr_id);
        }

        match &arena.exprs[expr_id].kind {
            // If expression with UNIT type but value-producing branches
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition = *condition;
                let then_branch = *then_branch;
                let else_branch = *else_branch;
                if let Some(result_type) = self.infer_stmts_result_type(then_branch) {
                    let cond = self.translate_operand(condition);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&arena.blocks[then_branch].stmts);
                    let else_body =
                        else_branch.map(|b| self.translate_stmts_as_value(&arena.blocks[b].stmts));
                    self.label_stack.pop();
                    return WirInstr::If {
                        condition: Box::new(cond),
                        result: Some(result_type),
                        then_body,
                        else_body,
                    };
                }
                self.translate_expr(expr_id)
            }
            // Block with UNIT type but value-producing last expression. Only
            // where nothing breaks to the label: `Seq` emits no `block`, and
            // pushing no `LabelEntry` for it leaves an enclosed `break` with no
            // target — the same accounting the value arm keeps.
            ExprKind::LabeledBlock { label, block, .. }
                if !arena.breaks_to(NodeRef::Block(*block), label) =>
            {
                let block = *block;
                if self.infer_stmts_result_type(block).is_some() {
                    let body = self.translate_stmts_as_value(&arena.blocks[block].stmts);
                    WirInstr::Seq(body)
                } else {
                    self.translate_expr(expr_id)
                }
            }
            _ => self.translate_expr(expr_id),
        }
    }

    /// Translate a TIR statement to a WIR instruction.
    fn translate_stmt(&mut self, stmt_id: StmtId) -> Option<WirInstr> {
        let arena = self.body;
        match &arena.stmts[stmt_id].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                // Phase 5: when the initializer is a direct call to a
                // multi-value-return function, bind the result's N tuple
                // elements into N split locals via `MultiValueLocalBind`
                // instead of trying to `LocalSet` the multi-value-Call
                // result into a single local (which Wasm doesn't allow).
                if let Some(e) = value.as_expr()
                    && let Some(instrs) = self.try_emit_multi_value_let(*local_index, e)
                {
                    return Some(instrs);
                }
                let value_instr = self.translate_operand(*value);
                // If the initializer diverges (`never`), no value reaches the stack,
                // so LocalSet would be invalid. `translate_operand` already appends
                // `unreachable` for `never`-typed expressions, so just emit the
                // diverging instruction; the local is declared but never assigned.
                if self.operand_type_id(*value) == TypeTable::NEVER {
                    Some(value_instr)
                } else if self.is_stackless_type(self.operand_type_id(*value)) {
                    // Unit-type (and `&()`-type) locals have no Wasm
                    // representation; just emit the init expression for its
                    // side effects (usually Nop).
                    Some(value_instr)
                } else {
                    let local_name = self.local_name(*local_index);
                    // Value-copy wrappers are materialized at the TIR level by
                    // `lower::plan::value_copy`; the translation here is a plain
                    // LocalSet. `skip_value_copy` is still respected upstream
                    // (the inserter leaves the value unwrapped).
                    Some(WirInstr::LocalSet {
                        name: local_name,
                        value: Box::new(value_instr),
                    })
                }
            }
            StmtKind::Expr(expr) => {
                let expr = *expr;
                if let Some(instrs) = self.try_emit_multi_value_discard(expr) {
                    return Some(instrs);
                }
                let ty = self.operand_type_id(expr);
                let instr = self.translate_operand(expr);
                // If the expression has a non-unit type, drop it.
                // Exception: assignments and global-var-sets produce void WIR instructions
                // (LocalSet/StructSet/ArraySet/GlobalSet), so don't wrap them in Drop.
                // A promoted pure value (`Operand::Value`) is never one of those.
                let is_void_instr = expr.as_expr().is_some_and(|e| {
                    matches!(
                        &arena.exprs[e].kind,
                        ExprKind::Assign { .. } | ExprKind::GlobalVarSet { .. }
                    )
                });
                if !is_void_instr && !self.is_stackless_type(ty) {
                    Some(WirInstr::Drop(Box::new(instr)))
                } else {
                    Some(instr)
                }
            }
            StmtKind::Return { value } => {
                if let Some(expr) = value {
                    let outer = std::mem::replace(
                        &mut self.multi_value_results_taken,
                        matches!(self.tir_func.return_abi, nir::ReturnAbi::MultiValue { .. }),
                    );
                    let value_instr = self.translate_operand(*expr);
                    self.multi_value_results_taken = outer;
                    // For multi-value-ABI functions, unwrap leaf
                    // `StructNew` aggregate-constructions inside the
                    // return value so the function pushes the N field
                    // values directly onto the stack instead of wrapping
                    // them in a heap struct.
                    if matches!(self.tir_func.return_abi, nir::ReturnAbi::MultiValue { .. }) {
                        match value_instr {
                            // Direct StructNew → Return { Seq(fields) }.
                            WirInstr::StructNew { fields, .. } => Some(WirInstr::Return {
                                value: Some(Box::new(WirInstr::Seq(fields))),
                            }),
                            // A scaffolded return value: a sequential block, or
                            // nested control flow. Each leaf becomes its own
                            // `Return { Seq(fields) }`, so the lifted expression
                            // replaces the whole `Return` rather than nesting.
                            mut other @ (WirInstr::Seq(_)
                            | WirInstr::Block { .. }
                            | WirInstr::If { .. }) => Some(if lifted(&mut other) {
                                other
                            } else {
                                self.multi_value_return_by_fields(other)
                            }),
                            other => Some(WirInstr::Return {
                                value: Some(Box::new(other)),
                            }),
                        }
                    } else {
                        Some(WirInstr::Return {
                            value: Some(Box::new(value_instr)),
                        })
                    }
                } else {
                    Some(WirInstr::Return { value: None })
                }
            }
            StmtKind::Loop { body } => {
                // Generate: block { loop { <body>; br 0; } }
                // The outer block is for break, the inner loop is for continue.
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: true,
                    is_loop_continue: false,
                });
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: true,
                });
                let mut body_instrs = self.translate_stmts(&arena.blocks[*body].stmts);
                // Unconditional back-edge: br 0 to loop header
                body_instrs.push(WirInstr::Br { depth: 0 });
                self.label_stack.pop(); // pop loop
                self.label_stack.pop(); // pop outer block
                Some(WirInstr::Block {
                    label: None,
                    result: None,
                    body: vec![WirInstr::Loop {
                        label: None,
                        body: body_instrs,
                    }],
                })
            }
            StmtKind::Break { label, value } => {
                let depth = self.compute_break_depth(label.as_deref());
                if let Some(val) = value {
                    let val_instr = self.translate_operand(*val);
                    Some(WirInstr::Seq(vec![val_instr, WirInstr::Br { depth }]))
                } else {
                    Some(WirInstr::Br { depth })
                }
            }
            StmtKind::Continue => {
                let depth = self.compute_continue_depth();
                Some(WirInstr::Br { depth })
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                let cond = self.translate_operand(*condition);
                // Push a label entry for the if block scope
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = self.translate_stmts(&arena.blocks[*then_block].stmts);
                let else_body = else_block
                    .as_ref()
                    .map(|b| self.translate_stmts(&arena.blocks[*b].stmts));
                self.label_stack.pop();
                Some(WirInstr::If {
                    condition: Box::new(cond),
                    result: None,
                    then_body,
                    else_body,
                })
            }
            StmtKind::LabeledBlock { label, block, .. } => {
                self.label_stack.push(LabelEntry {
                    label: Some(label.clone()),
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let body_instrs = self.translate_stmts(&arena.blocks[*block].stmts);
                self.label_stack.pop();
                Some(WirInstr::Block {
                    label: Some(label.clone()),
                    result: None,
                    body: body_instrs,
                })
            }
            StmtKind::LetDestructure { pattern, value, .. } => {
                Some(self.translate_let_pattern(*pattern, *value))
            }
        }
    }

    /// Handle a `FunctionRef` that did not resolve to a generated function:
    /// record the two cases the front end admits — an unsatisfied trait bound
    /// and a `#[cm(...)]` member with no backing import — and `panic` on the rest.
    fn unresolved_call_or_trap(
        &mut self,
        func: &nir::FunctionRef,
        span: Span,
        panic_msg: impl FnOnce() -> String,
    ) -> WirInstr {
        let Some(method) = func.method_info.as_ref() else {
            panic!("{}", panic_msg());
        };
        if let Some(trait_name) = method.trait_name.as_ref() {
            self.ctx.trait_bound_violations.push(TraitBoundViolation {
                type_display: method.fq_struct_name().to_display(),
                trait_display: trait_name.to_display(),
                span,
            });
            return WirInstr::Unreachable;
        }
        if let Some(cm_name) = method.cm_name.as_ref() {
            self.ctx.cm_import_violations.push(CmImportViolation {
                call_display: format!(
                    "{}::{}",
                    method.fq_struct_name().to_display(),
                    method.method_name
                ),
                cm_name: cm_name.clone(),
                span,
            });
            return WirInstr::Unreachable;
        }
        panic!("{}", panic_msg());
    }

    /// Translate a TIR expression, wrapping a `never`-typed one in
    /// `Seq([instr, Unreachable])` so it diverges. That tells the validator any
    /// later type expectation in the block is vacuously satisfied, which is what
    /// lets a `never`-typed sub-expression sit in any value position.
    pub(super) fn translate_expr(&mut self, expr_id: ExprId) -> WirInstr {
        let instr = self.translate_expr_inner(expr_id);
        if self.body.exprs[expr_id].type_id == TypeTable::NEVER && !instr.ends_with_terminator() {
            WirInstr::Seq(vec![instr, WirInstr::Unreachable])
        } else {
            instr
        }
    }

    /// Lower an operand to WIR. This is the extraction seam (WEP: The Live
    /// ValueGraph): Phase A operands are all `Expr` and delegate to
    /// `translate_expr`; Phase B materialises a promoted `Operand::Value` from
    /// the graph here.
    pub(super) fn translate_operand(&mut self, op: Operand) -> WirInstr {
        match op {
            Operand::Expr(e) => self.translate_expr(e),
            Operand::Value(v) => self.extract_value(v),
        }
    }

    /// Translate a call's operands, receiver first, erasing unit-typed
    /// parameters while preserving every argument's evaluation and its
    /// left-to-right order: a unit argument that still evaluates joins the
    /// prelude, and every non-unit argument to its left spills to a temp. Returns
    /// `(prelude, call_args)` — wrap with [`Self::wrap_call_with_prelude`].
    pub(super) fn translate_args_erasing_unit(
        &mut self,
        ordered: &[Operand],
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        self.translate_args(ordered, &IndexMap::default())
    }

    /// As above, with the positions in `split` handed over field by field. One
    /// loop over the whole list, so the ordering analysis sees every argument.
    fn translate_args(
        &mut self,
        ordered: &[Operand],
        split: &IndexMap<usize, Vec<(String, TypeId)>>,
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        let unit_needs_eval = |this: &Self, op: Operand| match op {
            Operand::Value(_) => false,
            Operand::Expr(e) => !matches!(this.body.exprs[e].kind, ExprKind::Local { .. }),
        };
        // An argument evaluated into the prelude runs ahead of every argument
        // left on the stack, so each one to its left is spilled to hold source
        // order.
        let evaluated_early = |this: &Self, i: usize, op: Operand| {
            split.contains_key(&i)
                || (this.is_stackless_type(this.operand_type_id(op)) && unit_needs_eval(this, op))
        };
        let last_early = ordered
            .iter()
            .enumerate()
            .filter(|(i, op)| evaluated_early(self, *i, **op))
            .map(|(i, _)| i)
            .next_back();

        let mut prelude = Vec::new();
        let mut call_args = Vec::new();
        for (i, &op) in ordered.iter().enumerate() {
            if let Some(fields) = split.get(&i) {
                let (p, reads) = self.split_argument(op, fields);
                prelude.extend(p);
                call_args.extend(reads);
            } else if self.is_stackless_type(self.operand_type_id(op)) {
                if unit_needs_eval(self, op) {
                    prelude.push(self.translate_operand(op));
                }
            } else if last_early.is_some_and(|last| i < last) {
                let (p, read) = self.spill_operand(op, "$arg_spill");
                prelude.extend(p);
                call_args.push(read);
            } else {
                call_args.push(self.translate_operand(op));
            }
        }
        (prelude, call_args)
    }

    /// Run `prelude` before `call`, preserving the call's value: a value-typed
    /// `Block`, or a `Seq` where a `Block`'s one result cannot hold it — a void
    /// call, or a `multi_value` callee returning N.
    pub(super) fn wrap_call_with_prelude(
        &mut self,
        mut prelude: Vec<WirInstr>,
        call: WirInstr,
        result_type: TypeId,
        multi_value: bool,
    ) -> WirInstr {
        if prelude.is_empty() {
            return call;
        }
        prelude.push(call);
        if multi_value || self.is_stackless_type(result_type) {
            WirInstr::Seq(prelude)
        } else {
            let result_wir = self.ctx.type_id_to_wir_type(self.type_table, result_type);
            WirInstr::Block {
                label: None,
                result: Some(result_wir),
                body: prelude,
            }
        }
    }

    /// The NIR type of an operand — the `ExprNode` type for a skeleton subtree,
    /// or the pool-recorded source type for a promoted pure value.
    pub(super) fn operand_type_id(&self, op: Operand) -> tir::TypeId {
        match op {
            Operand::Expr(e) => self.body.exprs[e].type_id,
            Operand::Value(v) => self
                .body
                .values
                .type_of(v)
                .expect("promoted value has no recorded type"),
        }
    }

    /// Emit a binary op from already-translated operand WIR. Shared by the
    /// skeleton `ExprKind::Binary` path and the value-graph extractor so the
    /// short-circuit / width-truncation behaviour cannot diverge. `left_ty` is
    /// the (left) operand's source type, which selects the op width.
    pub(super) fn emit_binary_wir(
        &mut self,
        op: NirBinaryOp,
        l: WirInstr,
        r: WirInstr,
        left_ty: TypeId,
    ) -> WirInstr {
        // Short-circuit logical operators: `r`'s instruction tree sits in a
        // conditional branch, so it runs only when reached.
        if matches!(op, NirBinaryOp::And) {
            return WirInstr::If {
                condition: Box::new(l),
                result: Some(WirType::I32),
                then_body: vec![r],
                else_body: Some(vec![WirInstr::I32Const(0)]),
            };
        }
        if matches!(op, NirBinaryOp::Or) {
            return WirInstr::If {
                condition: Box::new(l),
                result: Some(WirType::I32),
                then_body: vec![WirInstr::I32Const(1)],
                else_body: Some(vec![r]),
            };
        }
        let result = self.translate_binary_op(&op, Box::new(l), Box::new(r), left_ty);
        // Truncate sub-i32 arithmetic/bitwise results to the correct width;
        // comparisons / logical ops already return a 0/1 i32.
        if !matches!(
            op,
            NirBinaryOp::Eq
                | NirBinaryOp::NotEq
                | NirBinaryOp::Lt
                | NirBinaryOp::LtEq
                | NirBinaryOp::Gt
                | NirBinaryOp::GtEq
                | NirBinaryOp::And
                | NirBinaryOp::Or
                | NirBinaryOp::RefEq
                | NirBinaryOp::RefNotEq
        ) && let ResolvedType::Primitive(prim) = self.type_table.get(left_ty)
        {
            return Self::truncate_to_sub_i32(result, prim);
        }
        result
    }

    /// Emit a pure unary op (`Neg` / `Not` / `BitNot`) from already-translated
    /// operand WIR. Shared by the skeleton path and the extractor.
    pub(super) fn emit_unary_wir(
        &mut self,
        op: NirUnaryOp,
        o: WirInstr,
        inner_ty: TypeId,
    ) -> WirInstr {
        let result = self.translate_unary_op(&op, Box::new(o), inner_ty);
        if matches!(op, NirUnaryOp::Neg | NirUnaryOp::BitNot)
            && let ResolvedType::Primitive(prim) = self.type_table.get(inner_ty)
        {
            return Self::truncate_to_sub_i32(result, prim);
        }
        result
    }

    /// Materialise a promoted pure [`Operand::Value`] back to WIR (the
    /// extractor; see `docs/wep-2026-06-05-nir-optimizer-architecture.md`).
    /// Each kind lowers from the pool using the source
    /// type recorded by the builder; composite kinds (`Binary`, `Select`,
    /// `FieldAccess`, …) recurse on their operands. Kinds not yet promotable panic.
    pub(super) fn extract_value(&mut self, v: ValueId) -> WirInstr {
        use crate::nir_value_graph::ValueKind;
        use crate::wir::WirAbstractHeapType;
        let kind = self.body.values.kind(v).clone();
        let type_id = self
            .body
            .values
            .type_of(v)
            .expect("promoted value has no recorded type");
        match kind {
            // Width from the literal's own carried `TypeId` (the hash-cons key),
            // not the shared `type_of(v)`: a constant is width-correct regardless
            // of how many differently-typed uses share its `ValueId`. This is the
            // prerequisite that makes a constant safe to promote *early* (before
            // the passes can add a divergent-width use). Behavior-neutral today
            // (late freeze sets `type_of` to the same type); load-bearing for
            // early promotion.
            ValueKind::Int(value, int_ty) => match self.type_table.get(int_ty) {
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                    WirInstr::I64Const(value as i64)
                }
                _ => WirInstr::I32Const(value as i32),
            },
            ValueKind::Float(bits, float_ty) => match self.type_table.get(float_ty) {
                ResolvedType::Primitive(PrimitiveType::F32) => {
                    WirInstr::F32Const(f64::from_bits(bits) as f32)
                }
                _ => WirInstr::F64Const(f64::from_bits(bits)),
            },
            ValueKind::Bool(b) => WirInstr::I32Const(i32::from(b)),
            ValueKind::Char(c) => WirInstr::I32Const(c as i32),
            ValueKind::Null => {
                if let Some(inner) = self.type_table.as_option(type_id) {
                    assert!(
                        !matches!(self.type_table.get(inner), ResolvedType::Unknown),
                        "[WIR] promoted Null with unresolved Option inner type"
                    );
                    self.translate_variant_construct(
                        type_id,
                        OPTION_NONE_CASE,
                        "None",
                        None,
                        type_id,
                    )
                } else {
                    WirInstr::RefNull {
                        heap_type: WirAbstractHeapType::None,
                    }
                }
            }
            ValueKind::Unit => WirInstr::Nop,
            ValueKind::Binary { op, lhs, rhs, .. } => {
                let left_ty = self
                    .body
                    .values
                    .type_of(lhs)
                    .expect("promoted Binary lhs has no recorded type");
                let l = self.extract_value(lhs);
                let r = self.extract_value(rhs);
                self.emit_binary_wir(op, l, r, left_ty)
            }
            ValueKind::Unary { op, operand, .. } => {
                let inner_ty = self
                    .body
                    .values
                    .type_of(operand)
                    .expect("promoted Unary operand has no recorded type");
                let o = self.extract_value(operand);
                self.emit_unary_wir(op, o, inner_ty)
            }
            ValueKind::Cast { operand, target } => {
                let from_ty = self
                    .body
                    .values
                    .type_of(operand)
                    .expect("promoted Cast operand has no recorded type");
                self.translate_cast(Operand::Value(operand), from_ty, target)
            }
            // An `Opaque` stands for a leaf the graph cannot reconstruct (a
            // local read, a call result). It is re-emitted from the recorded
            // source the builder/lower scheduled for it.
            ValueKind::Opaque(id) => {
                use crate::nir_value_graph::OpaqueSource;
                match self
                    .body
                    .values
                    .opaque_source(id)
                    .expect("promoted Opaque has no recorded extraction source")
                {
                    OpaqueSource::Local(idx) => self.local_get(idx),
                    OpaqueSource::Expr(e) => self.translate_expr_inner(e),
                }
            }
            // A `Select` (control-merge of pure values) extracts as a
            // value-producing `if`. Sound because the arms are pure values
            // (the builder constructs `Select` only at merges of pure values);
            // re-emitting the `if` recomputes them in the same dominance order.
            ValueKind::Select { cond, then, else_ } => {
                let c = self.extract_value(cond);
                let t = self.extract_value(then);
                let e = self.extract_value(else_);
                let result_ty = self.ctx.type_id_to_wir_type(self.type_table, type_id);
                WirInstr::If {
                    condition: Box::new(c),
                    result: Some(result_ty),
                    then_body: vec![t],
                    else_body: Some(vec![e]),
                }
            }
            ValueKind::FieldAccess {
                receiver,
                field_index,
                ..
            } => {
                // Re-emit `receiver.field` as a `StructGet`. The value carries only
                // `field_index` (the receiver `ValueId` pins the type), so derive
                // the field name from the receiver value's recorded struct type
                // (the builder stamped it from the receiver expr). Soundness of
                // *where* this load runs is the materialiser's job: the shared
                // `heap_ver` guarantees the field is unchanged across uses.
                let recv_nir_ty = self
                    .body
                    .values
                    .type_of(receiver)
                    .expect("promoted FieldAccess receiver has no recorded type");
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, recv_nir_ty);
                let WirType::Ref {
                    type_id: struct_tid,
                    ..
                } = wir_type
                else {
                    panic!("extract_value FieldAccess: receiver not a Ref: {wir_type:?}");
                };
                let field_name = match self.ctx.types.get(struct_tid.index() as usize) {
                    Some(WirTypeDef::Struct(st)) => st
                        .fields
                        .get(field_index as usize)
                        .map(|f| f.name.clone())
                        .expect("FieldAccess field_index out of range"),
                    _ => panic!("extract_value FieldAccess: receiver type is not a struct"),
                };
                // Multi-value-return split: if the receiver is a local that was
                // split into per-field scalars (the aggregate was never
                // materialised), read the split local directly — a `StructGet`
                // would read an uninitialised slot. Mirrors `translate_expr_inner`.
                if let ValueKind::Opaque(oid) = self.body.values.kind(receiver)
                    && let Some(OpaqueSource::Local(idx)) = self.body.values.opaque_source(*oid)
                    && let Some(splits) = self.multi_value_split_locals.get(&idx)
                    && let Some((name, ty)) = splits.get(&field_name)
                {
                    return WirInstr::LocalGet {
                        name: name.clone(),
                        result_ty: ty.clone(),
                    };
                }
                let recv = self.extract_value(receiver);
                let result_ty = self.struct_field_wir_type(&struct_tid, &field_name);
                WirInstr::StructGet {
                    type_id: struct_tid,
                    field_name,
                    expr: Box::new(recv),
                    result_ty,
                }
            }
            other => panic!("extract_value: non-constant kind not promotable yet: {other:?}"),
        }
    }

    fn translate_expr_inner(&mut self, expr_id: ExprId) -> WirInstr {
        let arena = self.body;
        let expr = &arena.exprs[expr_id];
        match &expr.kind {
            ExprKind::PackedArray(b) => {
                // A raw constant `Array<u8>` (the `repr` of a `String` / `List<u8>`
                // literal). The struct wrapping comes from the enclosing
                // `StructLiteral`.
                self.translate_packed_array(b)
            }
            // Orphaned tombstone: never materialised (DCE drops it first).
            ExprKind::Dead => WirInstr::Nop,

            ExprKind::Local { index, .. } => {
                // Nothing declared a stackless local, so there is nothing to
                // push; where its type is never, `translate_expr` appends the
                // `Unreachable` that keeps the placeholder unreachable.
                if self.is_stackless_type(expr.type_id) {
                    WirInstr::Nop
                } else {
                    self.local_get(*index)
                }
            }
            // `register_globals` gives a unit global no slot, so a read is
            // nothing and a write is its value alone, evaluated for effect.
            ExprKind::GlobalVarGet { .. } if self.is_stackless_type(expr.type_id) => WirInstr::Nop,
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => WirInstr::GlobalGet {
                name: WirName {
                    fq: global_name(module_source, name),
                },
                result_ty: self.wir_type(expr.type_id),
            },
            ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                let prefer_fixed = self.ctx.eager_repr_globals.contains(name);
                let prev_force = self.force_fixed_string_repr;
                if prefer_fixed {
                    self.force_fixed_string_repr = true;
                }
                let val = self.translate_operand(*value);
                self.force_fixed_string_repr = prev_force;
                if self.is_stackless_type(self.operand_type_id(*value)) {
                    return val;
                }
                WirInstr::GlobalSet {
                    name: WirName {
                        fq: global_name(module_source, name),
                    },
                    value: Box::new(val),
                }
            }

            ExprKind::Binary { op, left, right } => {
                let (op, left, right) = (*op, *left, *right);
                let l = self.translate_operand(left);
                let r = self.translate_operand(right);
                self.emit_binary_wir(op, l, r, self.operand_type_id(left))
            }

            ExprKind::Unary { op, expr: inner } => match op {
                NirUnaryOp::Ref | NirUnaryOp::MutRef => self.translate_operand(*inner),
                NirUnaryOp::Deref => self.translate_operand(*inner),
                _ => {
                    let inner = *inner;
                    let o = self.translate_operand(inner);
                    self.emit_unary_wir(*op, o, self.operand_type_id(inner))
                }
            },

            ExprKind::Call { func_id, args, .. } => {
                // The callee descriptor comes from the function record by
                // `func_id` (Phase 5); the call node carries no `FunctionRef`.
                let func = &self.callee_descriptor(*func_id);
                // Check for instruction-builtins first
                let builtin = func
                    .builtin_name()
                    .or_else(|| func.monomorphized_builtin_name());
                if let Some(ref builtin_name) = builtin
                    && let Some(instr) =
                        self.translate_builtin_call(builtin_name, args, expr.type_id)
                {
                    return instr;
                }

                // Nothing here is binding the results, so the value is wanted
                // whole.
                let rebuild = (!self.multi_value_results_taken)
                    .then(|| self.multi_value_result_fields(*func_id).map(<[_]>::to_vec))
                    .flatten();

                let ordered: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                let (prelude, translated_args) = self.translate_args_for_callee(*func_id, &ordered);

                if let Some(wir_func_id) = self.resolve_call(func, *func_id) {
                    let call = WirInstr::Call {
                        func_id: wir_func_id,
                        args: translated_args,
                    };
                    let result_type = expr.type_id;
                    match rebuild {
                        Some(fields) => {
                            let whole = self.rebuild_multi_value_result(call, result_type, &fields);
                            self.wrap_call_with_prelude(prelude, whole, result_type, false)
                        }
                        None => self.wrap_call_with_prelude(
                            prelude,
                            call,
                            result_type,
                            self.callee_returns_multi_value(*func_id),
                        ),
                    }
                } else {
                    self.unresolved_call_or_trap(func, expr.span, || {
                        format!(
                            "[WIR] unresolved Call: name={:?} module={} builtin={:?} mono={:?} in={:?} span={:?}",
                            func.name,
                            func.module_source,
                            builtin,
                            func.monomorph_info,
                            self.tir_func.name,
                            expr.span
                        )
                    })
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, expr.type_id);
                let WirType::Ref { type_id, .. } = wir_type else {
                    let resolved = self.type_table.get(expr.type_id);
                    panic!(
                        "[WIR] StructLiteral expected Ref WirType, got {wir_type:?} (type_id={:?}, resolved={:?})",
                        expr.type_id, resolved
                    );
                };
                // Unit-typed fields have no Wasm representation; skip them.
                let non_unit_fields: Vec<_> = fields
                    .iter()
                    .filter(|f| {
                        !matches!(
                            self.ctx.type_id_to_wir_type(
                                self.type_table,
                                self.operand_type_id(f.value)
                            ),
                            WirType::Unit
                        )
                    })
                    .collect();
                let field_instrs: Vec<WirInstr> = non_unit_fields
                    .iter()
                    .map(|f| self.translate_operand(f.value))
                    .collect();
                self.struct_new(type_id, field_instrs)
            }

            ExprKind::FieldAccess {
                expr: receiver,
                field_name,
                ..
            } => {
                // When the receiver is a TIR local that was bound from a
                // multi-value-return Call, read the corresponding split
                // local directly. The aggregate was never materialised
                // as a struct ref — a `StructGet` here would read an
                // uninitialised slot.
                let receiver = *receiver;
                if let Some(re) = receiver.as_expr()
                    && let ExprKind::Local {
                        index: tir_local, ..
                    } = &arena.exprs[re].kind
                    && let Some(splits) = self.multi_value_split_locals.get(tir_local)
                    && let Some((name, ty)) = splits.get(field_name)
                {
                    return WirInstr::LocalGet {
                        name: name.clone(),
                        result_ty: ty.clone(),
                    };
                }
                // A stackless field leaves nothing on the stack, so the
                // declaration side dropped it: emit only the receiver for its
                // side effects. `&()` is as empty as `()` here.
                if self.is_stackless_type(expr.type_id) {
                    let recv = self.translate_operand(receiver);
                    return WirInstr::Seq(vec![WirInstr::Drop(Box::new(recv))]);
                }
                let recv = self.translate_operand(receiver);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.operand_type_id(receiver));
                let WirType::Ref { type_id, .. } = wir_type else {
                    panic!(
                        "[WIR] FieldAccess receiver expected Ref WirType, got {wir_type:?} (field={field_name}, type_id={:?})",
                        self.operand_type_id(receiver)
                    );
                };
                let result_ty = self.struct_field_wir_type(&type_id, field_name);
                WirInstr::StructGet {
                    type_id,
                    field_name: field_name.clone(),
                    expr: Box::new(recv),
                    result_ty,
                }
            }

            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                let val = self.translate_operand(value);
                match &arena.exprs[target].kind {
                    ExprKind::Local { index, .. } => {
                        if self.is_stackless_type(arena.exprs[target].type_id) {
                            return val;
                        }
                        // If the value is a LocalSet from nested chained assignment
                        // (e.g., `h = i = 42`), convert it to LocalTee so it leaves
                        // the assigned value on the stack for the outer assignment.
                        let val = match val {
                            WirInstr::LocalSet {
                                name: inner_name,
                                value: inner_val,
                            } => WirInstr::LocalTee {
                                name: inner_name,
                                value: inner_val,
                            },
                            other => other,
                        };
                        // Value-copy wrappers for Assign targets are inserted by the
                        // TIR `lower::plan::value_copy` pass; no WIR-level wrapping here.
                        WirInstr::LocalSet {
                            name: self.local_name(*index),
                            value: Box::new(val),
                        }
                    }
                    ExprKind::FieldAccess { expr: receiver, .. }
                        if self.is_stackless_type(arena.exprs[target].type_id) =>
                    {
                        // Neither side leaves a value, so both run for their
                        // effects alone and the receiver's reference is dropped.
                        let recv = self.translate_operand(*receiver);
                        WirInstr::Seq(vec![val, WirInstr::Drop(Box::new(recv))])
                    }
                    ExprKind::FieldAccess {
                        expr: receiver,
                        field_name,
                        ..
                    } => {
                        let receiver = *receiver;
                        let recv = self.translate_operand(receiver);
                        let wir_type = self
                            .ctx
                            .type_id_to_wir_type(self.type_table, self.operand_type_id(receiver));
                        let WirType::Ref { type_id, .. } = wir_type else {
                            panic!(
                                "[WIR] FieldAccess assignment expected Ref receiver, got {wir_type:?} (field={field_name}, type_id={:?})",
                                self.operand_type_id(receiver)
                            );
                        };
                        self.struct_set(type_id, field_name.clone(), recv, val)
                    }
                    ExprKind::Index {
                        expr: array_expr,
                        index: index_expr,
                    } => self.translate_index_assign(*array_expr, *index_expr, val),
                    ExprKind::Unary {
                        op: NirUnaryOp::Deref,
                        expr: ref_expr,
                    } => self.translate_deref_assign(*ref_expr, val),
                    other => panic!(
                        "[WIR] unhandled assignment target shape: {other:?} (type_id={:?})",
                        arena.exprs[target].type_id
                    ),
                }
            }

            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Type casts become appropriate conversion instructions
                self.translate_cast(*inner, self.operand_type_id(*inner), *target_type)
            }

            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.translate_operand(*condition);
                let has_result = !self.is_stackless_type(expr.type_id);
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = if has_result {
                    self.translate_stmts_as_value(&arena.blocks[*then_branch].stmts)
                } else {
                    self.translate_stmts(&arena.blocks[*then_branch].stmts)
                };
                let else_body = else_branch.as_ref().map(|b| {
                    if has_result {
                        self.translate_stmts_as_value(&arena.blocks[*b].stmts)
                    } else {
                        self.translate_stmts(&arena.blocks[*b].stmts)
                    }
                });
                self.label_stack.pop();
                let result_type = if has_result {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                } else {
                    None
                };
                WirInstr::If {
                    condition: Box::new(cond),
                    result: result_type,
                    then_body,
                    else_body,
                }
            }

            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.translate_match(*scrutinee, arms, expr.type_id),

            ExprKind::Index {
                expr: array_expr,
                index: index_expr,
            } => self.translate_index(*array_expr, *index_expr),

            ExprKind::TupleLiteral { elements } => {
                // Lower to `struct.new` of the tuple struct type. The
                // multi-value-return Return-arm rewrite later unwraps
                // this back into a `Seq` of field initialisers when the
                // enclosing function has `ReturnAbi::MultiValue`; a
                // call-site destructure binds the results directly
                // (`try_emit_multi_value_let`, and
                // `pattern_match::try_bind_multivalue_builtin` for the
                // wide-integer builtins).
                let (type_id, fields) = self.tuple_constructor_args(expr.type_id, elements);
                WirInstr::StructNew { type_id, fields }
            }

            ExprKind::ArrayLiteral { elements } => self.build_array_literal(expr.type_id, elements),

            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => self.translate_switch(*scrutinee, *min_value, arms, *default, expr.type_id),

            ExprKind::VariantTag { expr: inner } => {
                // Get discriminant field from variant base type
                let inner = *inner;
                let val = self.translate_operand(inner);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.operand_type_id(inner));
                if let WirType::Ref { type_id, .. } = wir_type {
                    WirInstr::StructGet {
                        type_id,
                        field_name: "discriminant".to_string(),
                        expr: Box::new(val),
                        result_ty: WirType::I32,
                    }
                } else {
                    // A plain `enum` lowers to a bare i32 discriminant (see
                    // `EnumConstruct` below), so the value already is the tag.
                    val
                }
            }
            ExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name: _,
            } => self.translate_variant_test(*inner, *case_index),
            ExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type,
            } => {
                // Extracting a stackless payload must yield nothing: the
                // `StructGet` would leave a value where the extraction sits in
                // statement position.
                if self.is_stackless_type(*payload_type) {
                    WirInstr::Nop
                } else {
                    self.translate_variant_payload(*inner, *case_index)
                }
            }
            ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => self.translate_variant_construct(
                *variant_type,
                *case_index,
                case_name,
                *payload,
                expr.type_id,
            ),
            ExprKind::EnumConstruct { case_index, .. } => WirInstr::I32Const(*case_index as i32),

            ExprKind::CmRawCall { target, args, .. } => {
                let translated_args: Vec<WirInstr> =
                    args.iter().map(|a| self.translate_operand(*a)).collect();
                let import_name = target.import_name();
                // The core import is the canonical's to type, not the call
                // site's: a cancel answers with the `u32` its copy ended on
                // where the declaring signature states no result. One binding
                // both types the import and drops the value it returns.
                let discards_result = expr.type_id == TypeTable::UNIT
                    && target
                        .canonical()
                        .is_some_and(CanonicalIntrinsic::returns_discarded_result);
                // Look up in WASI imports (registered by register_imports from TIR imports)
                let func_id = if let Some(func_id) = self
                    .ctx
                    .func_map
                    .get(&MangledName::wasi_import(&import_name))
                {
                    func_id.clone()
                } else {
                    // Not pre-registered — lazily register as a canonical intrinsic.
                    // This handles canonical imports (e.g., "task-return") that may not
                    // be in TIR imports but are needed by CM binding synthesis.
                    let Some(intrinsic) = target.canonical() else {
                        panic!("unregistered WASI import: {import_name}");
                    };
                    let params: Vec<WirType> = args
                        .iter()
                        .map(|a| {
                            self.ctx
                                .type_id_to_wir_type(self.type_table, self.operand_type_id(*a))
                        })
                        .collect();
                    let mut results =
                        if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
                            vec![]
                        } else {
                            vec![self.ctx.type_id_to_wir_type(self.type_table, expr.type_id)]
                        };
                    if discards_result {
                        assert!(results.is_empty(), "a UNIT call declares no result");
                        results.push(WirType::I32);
                    }
                    self.ctx
                        .ensure_canonical(intrinsic.clone(), params, results)
                };
                let call = WirInstr::Call {
                    func_id,
                    args: translated_args,
                };
                if discards_result {
                    WirInstr::Drop(Box::new(call))
                } else {
                    call
                }
            }

            ExprKind::IndirectCall { callee, args } => {
                self.translate_indirect_call(*callee, args, expr.type_id)
            }
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => self.translate_closure_to_canonical(
                *functor,
                *functor_id,
                *target_fn_type,
                closure_module,
            ),

            ExprKind::LabeledBlock { label, block, .. } => {
                let has_result = !self.is_stackless_type(expr.type_id);
                // A Wasm block only earns its keep as a `br` target. Where
                // nothing breaks to the label — every synthesized block, and a
                // written one whose `break` a pass removed — the statements
                // are a plain sequence.
                //
                // `compute_break_depth` reads a `br`'s relative depth off the
                // stack position, so an entry pushed for a block we do not emit
                // would deepen every `br` under it by one. Nothing names this
                // label, so leaving it off the stack loses no target.
                let targeted = arena.breaks_to(NodeRef::Block(*block), label);
                if targeted {
                    self.label_stack.push(LabelEntry {
                        label: Some(label.clone()),
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                }
                let body = if has_result {
                    self.translate_stmts_as_value(&arena.blocks[*block].stmts)
                } else {
                    self.translate_stmts(&arena.blocks[*block].stmts)
                };
                if !targeted {
                    return WirInstr::Seq(body);
                }
                self.label_stack.pop();
                WirInstr::Block {
                    label: Some(label.clone()),
                    result: has_result
                        .then(|| self.ctx.type_id_to_wir_type(self.type_table, expr.type_id)),
                    body,
                }
            }
        }
    }

    /// Compute the `br` depth for a break statement.
    ///
    /// For labeled break: finds the block with the matching label.
    /// For unlabeled break: finds the outer block wrapping the innermost loop.
    fn compute_break_depth(&self, label: Option<&str>) -> u32 {
        for (i, entry) in self.label_stack.iter().rev().enumerate() {
            if let Some(target_label) = label {
                if entry.label.as_deref() == Some(target_label) {
                    return u32::try_from(i).unwrap();
                }
            } else if entry.is_loop_break {
                return u32::try_from(i).unwrap();
            }
        }
        // Semantic analysis rejects a `break` with no enclosing loop or with an
        // unknown label, so the target is always on the stack. Depth 0 instead
        // would branch out of whatever block happens to be innermost.
        panic!(
            "[WIR] `break` in `{}` has no target block on the label stack (label={label:?})",
            self.tir_func.name
        )
    }

    /// Compute the `br` depth for a continue statement.
    ///
    /// Finds the innermost loop instruction in the label stack.
    fn compute_continue_depth(&self) -> u32 {
        for (i, entry) in self.label_stack.iter().rev().enumerate() {
            if entry.is_loop_continue {
                return u32::try_from(i).unwrap();
            }
        }
        // See `compute_break_depth`: a `continue` outside a loop never gets here.
        panic!(
            "[WIR] `continue` in `{}` has no enclosing loop on the label stack",
            self.tir_func.name
        )
    }
}

/// Turn every leaf that builds the aggregate into a `Return` of the N fields,
/// so a `ReturnAbi::MultiValue` function pushes them instead of a heap struct.
///
/// Reports whether every path now transfers control itself; see [`lifted`],
/// which is how a caller runs this without having to keep a `false`.
fn lift_leaves_to_returns(expr: &mut WirInstr) -> bool {
    match expr {
        WirInstr::StructNew { fields, .. } => {
            let fields = std::mem::take(fields);
            *expr = WirInstr::Return {
                value: Some(Box::new(WirInstr::Seq(fields))),
            };
            true
        }
        WirInstr::Seq(items) => items.last_mut().is_some_and(lift_leaves_to_returns),
        WirInstr::If {
            then_body,
            else_body,
            result,
            ..
        } => {
            // Branches now Return directly; the If no longer produces a value.
            *result = None;
            let then_lifted = then_body.last_mut().is_some_and(lift_leaves_to_returns);
            let else_lifted = else_body
                .as_mut()
                .and_then(|eb| eb.last_mut())
                .is_some_and(lift_leaves_to_returns);
            then_lifted && else_lifted
        }
        WirInstr::Block { body, result, .. } => {
            if result.is_none() {
                return true;
            }
            let reached_every_exit = return_every_exit(body, 0, Tail::IsResult);
            if reached_every_exit {
                *result = None;
            }
            reached_every_exit
        }
        // A tail that already yields the N results — a call to a callee under
        // this same ABI (`multi_value_return`'s tail-call rule). The branch it
        // sits in has had its result type cleared, so without a `Return` its
        // results are left on the stack at the end of the block.
        WirInstr::Call { .. } => {
            let value = std::mem::replace(expr, WirInstr::Nop);
            *expr = WirInstr::Return {
                value: Some(Box::new(value)),
            };
            true
        }
        // A tail that already terminates carries no value and needs no lift;
        // wrapping one would build `Return { value: Unreachable }`, a `Return`
        // whose operand leaves nothing on the stack. Anything else yields
        // something that is not the N results.
        other => other.ends_with_terminator(),
    }
}

/// [`lift_leaves_to_returns`] on a copy, kept only when it reached every path.
///
/// The rewrite descends as it goes, so a value it cannot finish has to be left
/// exactly as it was: the exit stays an exit, and the block keeps the result
/// type it was built with.
fn lifted(value: &mut WirInstr) -> bool {
    let mut trial = value.clone();
    if !lift_leaves_to_returns(&mut trial) {
        return false;
    }
    *value = trial;
    true
}

/// Where the last instruction of a list ends up: yielding the block's result, or
/// feeding whatever operand it was written for.
#[derive(Clone, Copy, PartialEq)]
enum Tail {
    IsResult,
    IsOperand,
}

/// Turn every exit to `target_depth` into a `Return` of its N fields, and
/// report whether it reached all of them — the caller drops the block's result
/// type on that answer.
///
/// Only `Block`, `Loop` and `If` bodies open a Wasm label. Every other child an
/// instruction carries rides at the enclosing depth, `let x = f()?` lowering to
/// `LocalSet { value: If { … } }` among them. Their fall-through tails feed the
/// operand rather than the block, so only a `Br` in them becomes a `Return`.
fn return_every_exit(instrs: &mut [WirInstr], target_depth: u32, tail: Tail) -> bool {
    fn exits_to_target(instr: &WirInstr, target_depth: u32) -> bool {
        match instr {
            WirInstr::Br { depth } => *depth == target_depth,
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                body.iter().any(|i| exits_to_target(i, target_depth + 1))
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                exits_to_target(condition, target_depth)
                    || then_body
                        .iter()
                        .any(|i| exits_to_target(i, target_depth + 1))
                    || else_body
                        .as_ref()
                        .is_some_and(|eb| eb.iter().any(|i| exits_to_target(i, target_depth + 1)))
            }
            other => {
                let mut found = false;
                other.for_each_child(&mut |child| {
                    found |= exits_to_target(child, target_depth);
                });
                found
            }
        }
    }

    /// An exit written as the value and the `Br` side by side.
    fn br_follows(instrs: &[WirInstr], i: usize, target_depth: u32) -> bool {
        instrs
            .get(i + 1)
            .is_some_and(|next| matches!(next, WirInstr::Br { depth } if *depth == target_depth))
    }

    /// An exit written as one `Seq` ending in the value and the `Br`.
    fn exit_as_seq(instr: &WirInstr, target_depth: u32) -> bool {
        let WirInstr::Seq(seq) = instr else {
            return false;
        };
        seq.len() >= 2
            && matches!(seq.last(), Some(WirInstr::Br { depth }) if *depth == target_depth)
    }

    /// Lift the value such a `Seq` carries and drop the `Br` it ends with.
    fn lift_seq_exit(instr: &mut WirInstr) -> bool {
        let WirInstr::Seq(seq) = instr else {
            return false;
        };
        let value = seq.len() - 2;
        if !lifted(&mut seq[value]) {
            return false;
        }
        seq.pop();
        true
    }

    let mut all_rewritten = true;
    let mut i = 0;
    while i < instrs.len() {
        // The value an exit carries can exit to the same target itself, and
        // that one the lift has not rewritten — so it is still counted.
        if br_follows(instrs, i, target_depth) && lifted(&mut instrs[i]) {
            instrs[i + 1] = WirInstr::Nop;
            all_rewritten &= !exits_to_target(&instrs[i], target_depth);
            i += 2;
            continue;
        }
        if exit_as_seq(&instrs[i], target_depth) && lift_seq_exit(&mut instrs[i]) {
            all_rewritten &= !exits_to_target(&instrs[i], target_depth);
            i += 1;
            continue;
        }
        let one_deeper = target_depth + 1;
        let inherited = if tail == Tail::IsResult && i + 1 == instrs.len() {
            Tail::IsResult
        } else {
            Tail::IsOperand
        };
        match &mut instrs[i] {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                return_every_exit(body, one_deeper, inherited);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                return_every_exit(
                    std::slice::from_mut(condition.as_mut()),
                    target_depth,
                    Tail::IsOperand,
                );
                return_every_exit(then_body, one_deeper, inherited);
                if let Some(eb) = else_body {
                    return_every_exit(eb, one_deeper, inherited);
                }
            }
            WirInstr::Seq(body) => {
                return_every_exit(body, target_depth, inherited);
            }
            other => {
                other.for_each_boxed_child_mut(&mut |child| {
                    return_every_exit(std::slice::from_mut(child), target_depth, Tail::IsOperand);
                });
            }
        }
        // Ask after rewriting, never before: a rewritten exit is a `Return`, so
        // what `exits_to_target` still finds is exactly an exit the walk could
        // not reach — and the block must keep its result type for it. Asking
        // first reported every `if` holding an exit as unrewritten, even the
        // ones the recursion below had just converted.
        all_rewritten &= !exits_to_target(&instrs[i], target_depth);
        i += 1;
    }

    // A tail that leaves nothing on the stack has no aggregate to lift — the
    // `Nop` a rewritten exit leaves behind, or a store the block ends with.
    if tail == Tail::IsResult
        && let Some(last) = instrs.last_mut()
        && last.produces_stack_value()
    {
        all_rewritten &= lifted(last);
    }
    all_rewritten
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_names_agree_with_the_body() {
        // `e` at index 2 suffixes to `e_2`, which a third parameter spells
        // literally.
        let params = [
            param_named("e", 0),
            param_named("e_2", 1),
            param_named("e", 2),
        ];
        let resolved = resolve_param_names(&params);
        let unique: IndexSet<&str> = resolved.values().map(String::as_str).collect();
        assert_eq!(
            unique.len(),
            resolved.len(),
            "parameter names must be unique: {resolved:?}"
        );

        let raw: IndexMap<u32, String> = params
            .iter()
            .map(|p| (p.local_index, p.name.clone()))
            .collect();
        assert_eq!(resolve_local_names(&raw, &params), resolved);
    }

    fn param_named(name: &str, local_index: u32) -> NirParam {
        NirParam {
            name: name.to_string(),
            type_id: TypeId(0),
            local_index,
            is_mut: false,
            is_mut_ref: false,
            span: Span::default(),
            param_abi: nir::ParamAbi::default(),
        }
    }

    #[test]
    fn resolve_local_names_are_globally_unique() {
        // Two `e` locals force the `_{idx}` suffix (`e` at index 2 -> `e_2`),
        // which must not collide with a distinct local literally named `e_2`.
        // Codegen keys locals by name, so a collision silently merges two
        // differently-typed slots into one (an invalid-Wasm ICE at -O3).
        let mut raw: IndexMap<u32, String> = IndexMap::default();
        raw.insert(2, "e".to_string());
        raw.insert(5, "e".to_string());
        raw.insert(10, "e_2".to_string());
        let out = resolve_local_names(&raw, &[]);
        let unique: IndexSet<&str> = out.values().map(String::as_str).collect();
        assert_eq!(
            unique.len(),
            out.len(),
            "resolved names must be unique: {out:?}"
        );
    }
}
