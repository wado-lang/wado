//! Function body translation — converts TIR expressions and statements to WIR instructions.
//!
//! This is the core of the `tir_to_wir` phase, translating each TIR function body
//! into a sequence of WIR instructions.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirFunction, NirParam, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{CanonicalIntrinsic, WirInstr, WirName, WirType, WirTypeDef, WirTypeId};

use super::context::WirContext;

/// Recursively collect variable names from Let statements.
///
/// These names are gathered eagerly from the statement tree and preferred
/// when present; any missing entries are then backfilled from
/// `tir_func.locals[idx].name` (for example, slots created in expression
/// contexts the walker doesn't recurse into, or by optimizer passes that
/// allocate locals without emitting a `Let`).
fn collect_let_names(names: &mut IndexMap<u32, String>, stmts: &[NirStmt]) {
    for stmt in stmts {
        match &stmt.kind {
            NirStmtKind::Let {
                name, local_index, ..
            } => {
                names.insert(*local_index, name.clone());
            }
            NirStmtKind::Loop { body } => {
                collect_let_names(names, &body.stmts);
            }
            NirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                collect_let_names(names, &then_block.stmts);
                if let Some(eb) = else_block {
                    collect_let_names(names, &eb.stmts);
                }
            }
            NirStmtKind::LabeledBlock { block, .. } => {
                collect_let_names(names, &block.stmts);
            }
            _ => {}
        }
    }
}

/// Pre-compute the WIR-side name for every TIR local index, applying the
/// same shadow / param-collision rules `local_name` used to compute on the
/// fly. The live formulation iterated `params` and the entire `local_names`
/// map per call, which is O(N) per local reference; doing it once up front
/// keeps `local_name` to an O(1) hash lookup at every visit site.
///
/// Rules (mirrored from the original `local_name`):
/// - When two params share a raw name, every such param is suffixed with
///   `_{local_index}`.
/// - A non-param local that shadows a param-or-let keeps the original
///   name unchanged on the param/let and suffixes the non-param.
fn resolve_local_names(raw: &IndexMap<u32, String>, params: &[NirParam]) -> IndexMap<u32, String> {
    let param_indices: IndexSet<u32> = params.iter().map(|p| p.local_index).collect();

    // Tally raw-name occurrences across all locals and within params only.
    let mut total_per_name: IndexMap<&str, u32> = IndexMap::default();
    let mut param_per_name: IndexMap<&str, u32> = IndexMap::default();
    for (idx, name) in raw {
        *total_per_name.entry(name.as_str()).or_default() += 1;
        if param_indices.contains(idx) {
            *param_per_name.entry(name.as_str()).or_default() += 1;
        }
    }

    let mut out = IndexMap::default();
    for (idx, name) in raw {
        let needs_suffix = if param_indices.contains(idx) {
            param_per_name.get(name.as_str()).copied().unwrap_or(0) > 1
        } else {
            total_per_name.get(name.as_str()).copied().unwrap_or(0) > 1
        };
        let final_name = if needs_suffix {
            format!("{name}_{idx}")
        } else {
            name.clone()
        };
        out.insert(*idx, final_name);
    }
    out
}

/// Register canonical closure wrapper functions for all closure functors.
/// Must be called before `translate_function_bodies` so wrappers are available
/// for `ClosureToCanonical` references.
///
/// For each reachable functor we register three wrappers — one per
/// vtable slot in `CanonicalClosure_K`:
///
/// 1. `__closure_wrapper_N(env, args...) -> ret` — refcasts `env` to
///    `&__Closure_N` and forwards to `__call`.
/// 2. `__closure_inspect_wrapper_N(env, formatter)` — refcasts both
///    args and forwards to `__Closure_N^Inspect::inspect`.
/// 3. `__closure_inspect_alt_wrapper_N(env, formatter)` — refcasts
///    both args and forwards to `__Closure_N^InspectAlt::inspect_alt`.
///
/// The per-functor `__Closure_N^Inspect` / `InspectAlt` impls are
/// synthesised in lower (Phase 2). For non-inspectable signatures the
/// canonical struct uses the slim `{ env, func }` schema and only the
/// call wrapper is registered; the `inspect` / `inspect_alt` wrappers
/// don't exist and the corresponding fields aren't on the struct, so
/// nothing has to be filled in. For inspectable signatures all three
/// wrappers are registered and reach the per-functor impls.
pub fn register_closure_wrappers(ctx: &mut WirContext<'_>) {
    use crate::wir::WirType;

    // Snapshot the functor list so we can mutate ctx inside the loop.
    let functors: Vec<crate::nir::ClosureFunctor> = ctx.package.closure_functors.clone();

    for functor in &functors {
        let module_source = &functor.module_source;
        let functor_key = (module_source.clone(), functor.id);
        if ctx.closure_wrapper_funcs.contains_key(&functor_key) {
            continue;
        }

        // Look up the __call func_id, scoped to the correct module.
        // If __call was removed by DCE (closure never used), skip this functor entirely.
        // This check must come before type lookups since DCE may have removed the
        // functor's types from the TypeTable.
        let functor_name = &functor.struct_name;
        let call_method_fq = format!("{module_source}/{functor_name}::__call");
        let call_func_id = match ctx.func_map.get(&call_method_fq).cloned() {
            Some(id) => id,
            None => continue,
        };

        // The wrapper's external signature is governed by the *canonical*
        // closure signature — the param / return types of the user-written
        // closure literal — not by the live `call_method.params`, which
        // TIR DAE may have shrunk. Decoupling here is what lets DAE drop
        // an unused `self` (no captures) or unused user args from `__call`
        // without desynchronising the function-table slot type from the
        // typed-fn callers that dispatch through it.
        let user_param_count = functor.canonical_user_params.len();
        let type_table = &*ctx.package.type_table.borrow();
        let user_params: Vec<WirType> = functor
            .canonical_user_params
            .iter()
            .map(|(_, ty)| ctx.type_id_to_wir_type(type_table, *ty))
            .collect();
        let result_wirs: Vec<WirType> = if functor.canonical_return == crate::tir::TypeTable::UNIT
            || functor.canonical_return == crate::tir::TypeTable::NEVER
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
        let functor_struct_type_id = match &functor_wir_type {
            WirType::Ref { type_id, .. } => type_id.clone(),
            _ => continue,
        };

        // Map each surviving `call_method.params` entry to its source slot
        // in the wrapper. Position 0 is always self (env, refcast); the
        // other positions match canonical_user_params by name. The mapping
        // tells `register_call_wrapper` exactly which wrapper-local to
        // forward into the inner `__call` per surviving param, so DAE can
        // freely shrink `__call.params` without breaking the wrapper.
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
            .map(|(i, p)| {
                if i == 0 && p.name == "self" && p.type_id == functor.ref_type_id {
                    CallWrapperArg::TypedEnv
                } else {
                    let idx = functor
                        .canonical_user_params
                        .iter()
                        .position(|(name, _)| name == &p.name)
                        .unwrap_or_else(|| {
                            panic!(
                                "closure {functor_name}::__call param `{}` has no matching \
                                 canonical user param (canonical: {:?})",
                                p.name,
                                functor
                                    .canonical_user_params
                                    .iter()
                                    .map(|(n, _)| n)
                                    .collect::<Vec<_>>(),
                            )
                        });
                    CallWrapperArg::UserParam(idx)
                }
            })
            .collect();
        drop(call_func);

        // Register the call wrapper.
        let global_id = ctx.closure_wrapper_funcs.len();
        let call_wrapper_fq = format!("closure/{module_source}/__closure_wrapper_{global_id}");
        let call_wrapper_id = register_call_wrapper(
            ctx,
            &call_wrapper_fq,
            call_fn_type_id,
            functor_struct_type_id.clone(),
            functor_wir_type.clone(),
            user_param_count,
            user_params_clone,
            !result_wirs.is_empty(),
            call_func_id,
            &live_param_sources,
        );

        // Register the inspect / inspect_alt wrappers only when the
        // functor's signature is inspectable. These forward to the
        // per-functor `__Closure_N^Inspect / InspectAlt` impls
        // synthesised in lower (Phase 2). When DCE pruned the
        // per-functor impl or the `Formatter` struct isn't registered,
        // the wrapper body falls back to `Unreachable` — the slot
        // stays populated so the canonical struct schema is consistent.
        let (inspect_wrapper_id, inspect_alt_wrapper_id) = if is_inspectable {
            let callback_fn_type_id = ctx.get_or_create_canonical_callback_fn_type();
            let inspect = register_inspect_wrapper(
                ctx,
                module_source,
                functor_name,
                "Inspect",
                "inspect",
                global_id,
                callback_fn_type_id.clone(),
                functor_struct_type_id.clone(),
            );
            let inspect_alt = register_inspect_wrapper(
                ctx,
                module_source,
                functor_name,
                "InspectAlt",
                "inspect_alt",
                global_id,
                callback_fn_type_id,
                functor_struct_type_id,
            );
            (Some(inspect), Some(inspect_alt))
        } else {
            (None, None)
        };

        ctx.closure_wrapper_funcs.insert(
            functor_key,
            crate::wir_build::context::ClosureWrapperFuncs {
                call: call_wrapper_id,
                inspect: inspect_wrapper_id,
                inspect_alt: inspect_alt_wrapper_id,
            },
        );
    }
}

/// Source of an argument the wrapper forwards into the inner `__call`.
/// One entry per surviving `call_method.params` slot.
#[derive(Debug, Clone, Copy)]
enum CallWrapperArg {
    /// The refcast `__typed_env` local — corresponds to `__call`'s `self`.
    TypedEnv,
    /// The wrapper's `__pN` user-param local at the given canonical index.
    UserParam(usize),
}

/// Build a call wrapper: refcast `env` to `&__Closure_N` (only when the
/// surviving `__call` still expects `self`), then `call __Closure_N::__call`
/// with the surviving args. The wrapper's external signature stays
/// `(env, canonical_user_params...) -> canonical_return` regardless of
/// which `__call` params have been DAE'd.
#[allow(clippy::too_many_arguments)]
fn register_call_wrapper(
    ctx: &mut WirContext<'_>,
    wrapper_fq: &str,
    fn_type_id: crate::wir::WirTypeId,
    functor_struct_type_id: crate::wir::WirTypeId,
    functor_wir_type: crate::wir::WirType,
    user_param_count: usize,
    user_params: Vec<crate::wir::WirType>,
    has_result: bool,
    call_func_id: crate::wir::WirFuncId,
    live_param_sources: &[CallWrapperArg],
) -> crate::wir::WirFuncId {
    use crate::wir::{WirFunction, WirName, WirType};

    let env_local = "__env".to_string();
    let typed_env_local = "__typed_env".to_string();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: crate::wir::WirAbstractHeapType::Struct,
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
                name: format!("__p{idx}"),
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
        param_names.push(format!("__p{i}"));
    }

    let func = WirFunction {
        name: WirName {
            fq: wrapper_fq.to_string(),
        },
        type_id: fn_type_id,
        param_names,
        body: Some(body),
        meta: crate::wir::WirMeta::default(),
        generic_origin: None,
        effects: Vec::new(),
        stores: Vec::new(),
        compiler_item: None,
        export_name: None,
    };

    ctx.register_function(func)
}

/// Build an inspect / `inspect_alt` wrapper for a functor.
///
/// The wrapper's external signature is fixed by the canonical inspect
/// callback type `(env: ref null struct, formatter: ref null struct)`,
/// so the function-table slot type stays stable across DAE shrinkage on
/// the per-functor impl. Internally, we look up the impl's surviving
/// params and forward only the matching wrapper-locals: `self` (env) is
/// fed by `__typed_env` (refcast of `__env`), `f` (formatter) is fed by
/// `__typed_formatter`. Either or both refcasts are skipped when the
/// corresponding param has been DAE'd. If the impl was DCE'd entirely
/// the body falls back to `Unreachable` — the slot stays populated to
/// keep the canonical struct schema consistent.
#[allow(clippy::too_many_arguments)]
fn register_inspect_wrapper(
    ctx: &mut WirContext<'_>,
    module_source: &ModuleSource,
    functor_name: &str,
    trait_name: &str,
    method_name: &str,
    global_id: usize,
    callback_fn_type_id: crate::wir::WirTypeId,
    functor_struct_type_id: crate::wir::WirTypeId,
) -> crate::wir::WirFuncId {
    use crate::wir::{WirFunction, WirName, WirType};

    let env_local = "__env".to_string();
    let formatter_local = "__formatter".to_string();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: crate::wir::WirAbstractHeapType::Struct,
        nullable: true,
    };

    let target_fq = format!("{module_source}/{functor_name}^{trait_name}::{method_name}");
    let target_func_id = ctx.func_map.get(&target_fq).cloned();

    // Look up the Formatter struct WIR type id once; needed to
    // refcast the abstract `(ref null struct)` arg to the concrete
    // `&Formatter` the per-functor impl expects.
    let formatter_struct_type_id = ctx
        .struct_type_map
        .get(&crate::name::StructName::new(
            ModuleSource::format(),
            "Formatter".to_string(),
        ))
        .cloned();

    // Look up the per-functor impl's TIR function so we can read its
    // current `params` (post-DAE) and only forward the surviving slots.
    // The unqualified function name is `__Closure_N^TraitName::method`;
    // module + name together must equal `target_fq`.
    let impl_unqualified_name = format!("{functor_name}^{trait_name}::{method_name}");
    let impl_param_names: Option<Vec<String>> = ctx.package.functions.iter().find_map(|f| {
        let f = f.borrow();
        if f.module_source == *module_source && f.name == impl_unqualified_name {
            Some(f.params.iter().map(|p| p.name.clone()).collect())
        } else {
            None
        }
    });

    let body = match (target_func_id, formatter_struct_type_id, impl_param_names) {
        (Some(func_id), Some(formatter_tid), Some(impl_params)) => {
            let typed_env_local = "__typed_env".to_string();
            let typed_formatter_local = "__typed_formatter".to_string();
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
        _ => vec![WirInstr::Unreachable],
    };

    let wrapper_fq = format!("closure/{module_source}/__closure_{method_name}_wrapper_{global_id}");
    let func = WirFunction {
        name: WirName { fq: wrapper_fq },
        type_id: callback_fn_type_id,
        param_names: vec![env_local, formatter_local],
        body: Some(body),
        meta: crate::wir::WirMeta::default(),
        generic_origin: None,
        effects: Vec::new(),
        stores: Vec::new(),
        compiler_item: None,
        export_name: None,
    };

    ctx.register_function(func)
}

/// Build the WIR body for a `FunctionKind::FnCanonicalDispatch`
/// stub: cast `self` to the shared `$canonical_inspectable_base`
/// supertype, then `call_ref (struct.get base $slot self) (self.env,
/// f)` where `$slot` is `inspect` or `inspect_alt` depending on
/// `trait_kind`.
///
/// Casting to the shared base — instead of one specific
/// `CanonicalClosure_K` per `(arity, return_type)` — lets the same
/// dispatch stub serve every parameter shape with that signature
/// pair; without it the cast would trap whenever a runtime value
/// belonged to a different per-signature canonical struct.
///
/// Returns `None` when no inspectable canonical struct has been
/// registered: in that case the dispatch stub is unreachable from any
/// emitted closure value (every canonical struct is slim `{ env, func
/// }`), so leaving the bodyless TIR placeholder in place is fine.
#[allow(clippy::needless_pass_by_value)] // signature mirrors the param-name plumbing in translate_function_bodies
fn build_fn_canonical_dispatch_body(
    ctx: &mut WirContext<'_>,
    trait_kind: crate::nir::FnDispatchTrait,
    self_param_name: String,
    formatter_param_name: String,
    self_box_type_id: Option<TypeId>,
) -> Option<Vec<WirInstr>> {
    use crate::nir::FnDispatchTrait;
    use crate::wir::{WirAbstractHeapType, WirType};

    // No inspectable canonical struct was ever registered → the stub is
    // dead code. Leave the bodyless declaration in place.
    let base_type_id = ctx.canonical_inspectable_base_type_id.clone()?;
    let callback_fn_type_id = ctx.get_or_create_canonical_callback_fn_type();
    let abstract_struct_nullable = WirType::AbstractRef {
        heap_type: WirAbstractHeapType::Struct,
        nullable: true,
    };
    let field_name = match trait_kind {
        FnDispatchTrait::Inspect => "inspect",
        FnDispatchTrait::InspectAlt => "inspect_alt",
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
    let typed_self = "__typed_self".to_string();
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
                field_name: field_name.to_string(),
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
pub fn translate_function_bodies(ctx: &mut WirContext<'_>) {
    let pending: Vec<_> = std::mem::take(&mut ctx.pending_bodies);

    for pending_body in &pending {
        let tir_func = pending_body.tir_func.borrow();
        let type_table = pending_body.type_table.borrow();

        // `Fn<arity, ret>^Inspect / InspectAlt` dispatch stubs carry
        // an empty TIR placeholder body; substitute the real body
        // (vtable indirect call through `CanonicalClosure_K`) here
        // instead of translating the placeholder. Skipping the
        // string-matching post-pass keeps name-format knowledge
        // confined to `name.rs` and `synthesis::traits`.
        if let Some((trait_kind, _arity, _return_type)) = tir_func.fn_canonical_dispatch() {
            let self_param_name = tir_func
                .params
                .first()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "self".to_string());
            let formatter_param_name = tir_func
                .params
                .get(1)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "f".to_string());
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
                trait_kind,
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
            collect_let_names(&mut local_names, &body.stmts);
            for (idx, local) in tir_func.locals.iter().enumerate() {
                let key = u32::try_from(idx).unwrap();
                local_names.entry(key).or_insert_with(|| local.name.clone());
            }
            let resolved_local_names = resolve_local_names(&local_names, &tir_func.params);

            // Translate inside a nested block so the translator (and its reborrow of ctx)
            // is dropped before we write back to ctx.functions below.
            let wir_body = {
                let mut translator = FunctionTranslator {
                    ctx: &mut *ctx,
                    type_table: &type_table,
                    tir_func: &tir_func,
                    label_stack: Vec::new(),
                    match_counter: 0,
                    local_counter: 0,
                    resolved_local_names,
                    immutable_locals: IndexSet::default(),
                    multi_value_split_locals: IndexMap::default(),
                };
                translator.translate_block(body)
            };
            let _ = type_table;
            drop(tir_func);
            ctx.functions[pending_body.wir_func_index].body = Some(wir_body);
        }
    }
}

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
    /// Set of local indices declared as immutable (`let`, not `let mut`).
    /// Used to skip unnecessary value copies when an immutable binding
    /// is initialized from another immutable local.
    pub(super) immutable_locals: IndexSet<u32>,
    /// TIR locals that hold a multi-value-call result, mapped to the WIR
    /// split locals they were unpacked into, keyed by source field name.
    /// When a `let __tmp = Call(f)` targets a function with
    /// `ReturnAbi::MultiValue { field_names, .. }`, we emit
    /// `MultiValueLocalBind [__tmp_0, __tmp_1, …] = Call(f)` and record
    /// `local_index → { field_name → (split_local_name, ty) }`.
    /// Subsequent `FieldAccess(LocalGet(__tmp), name)` accesses read the
    /// matching split local directly instead of `StructGet(__tmp, name)`
    /// (which would panic at codegen since `__tmp` was never assigned a
    /// struct ref).
    pub(super) multi_value_split_locals: IndexMap<u32, IndexMap<String, (String, WirType)>>,
}

impl FunctionTranslator<'_, '_> {
    /// Get the WIR local name for a given local index.
    /// Uses the TIR variable name if available, otherwise falls back to `__local_N`.
    ///
    /// WIR locals are looked up by name during codegen (`current_locals` is
    /// keyed by name in `codegen::emit::resolve_local`), so any two locals
    /// that share a name would clobber each other's entry and silently
    /// mis-resolve. The disambiguation rules here mirror
    /// `wir_build::functions`'s construction of `WirFunction::param_names`:
    ///
    /// - When two params share a name (e.g. a synthesised closure's
    ///   implicit `self: &__Closure` env collides with an explicit
    ///   `self`-named param forwarded from a source method), every such
    ///   param's name is suffixed with `_{local_index}`.
    /// - A non-param local that shadows a param keeps the original
    ///   collision-resolution shape: the param keeps its raw name and the
    ///   non-param gets the `_{index}` suffix. This avoids renaming params
    ///   just because a `let self = ...` happens to shadow them in the
    ///   body.
    pub(super) fn local_name(&self, index: u32) -> String {
        self.resolved_local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("__local_{index}"))
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
        } else if !self.tir_func.locals.is_empty() {
            // `locals` is indexed absolutely (entries 0..param_count are
            // params, entries param_count.. are non-param locals), matching
            // DeclareLocal generation.
            if let Some(local) = self.tir_func.locals.get(index as usize) {
                self.wir_type(local.type_id)
            } else {
                WirType::I32
            }
        } else {
            WirType::I32
        }
    }

    /// Shorthand for `self.ctx.type_id_to_wir_type(self.type_table, type_id)`.
    pub(super) fn wir_type(&self, type_id: TypeId) -> WirType {
        self.ctx.type_id_to_wir_type(self.type_table, type_id)
    }

    /// Look up the WIR type of a struct field.
    pub(super) fn struct_field_wir_type(
        &self,
        struct_type_id: &WirTypeId,
        field_name: &str,
    ) -> WirType {
        if let Some(WirTypeDef::Struct(st)) = self.ctx.types.get(struct_type_id.index() as usize)
            && let Some(f) = st.fields.iter().find(|f| f.name == field_name)
        {
            return f.ty.clone();
        }
        WirType::I32
    }

    /// Look up the element WIR type of an array type.
    pub(super) fn array_element_wir_type(&self, array_type_id: &WirTypeId) -> WirType {
        if let Some(WirTypeDef::Array(at)) = self.ctx.types.get(array_type_id.index() as usize) {
            return at.element_type.clone();
        }
        WirType::I32
    }

    /// Build a `StructNew` instruction, wrapping each field value with `RefAsNonNull`
    /// where the struct definition declares a non-nullable reference field.
    pub(super) fn struct_new(&self, type_id: WirTypeId, fields: Vec<WirInstr>) -> WirInstr {
        let fields = self.cast_nonnull_fields(&type_id, fields);
        WirInstr::StructNew { type_id, fields }
    }

    /// Detect `let local = Call(f)` (or `MethodCall(f)`) where `f` has
    /// `ReturnAbi::MultiValue` and emit `MultiValueLocalBind` to N split
    /// locals instead of a single `LocalSet`. Returns `Some` if the
    /// rewrite fired (the caller should not emit the regular `LocalSet`).
    ///
    /// The split locals use names `<base>_mv_<field_name>` where `<base>`
    /// is the TIR local name. Subsequent
    /// `FieldAccess(LocalGet(local), name)` accesses read the matching
    /// split local directly via `multi_value_split_locals`.
    fn try_emit_multi_value_let(&mut self, local_index: u32, value: &NirExpr) -> Option<WirInstr> {
        // Only fire on direct `Call(f)` / `MethodCall(f)` initialisers —
        // wrapped calls (e.g. inlined Block) should have been simplified
        // before this point. Wrapped calls would also break the
        // `MultiValueLocalBind { instr: <Call>, … }` shape codegen
        // expects. `MethodCall` lowers to a single `WirInstr::Call` after
        // receiver / arg translation, so it's interchangeable with `Call`
        // for the multi-value-bind purpose.
        let func = match &value.kind {
            NirExprKind::Call { func, .. } | NirExprKind::MethodCall { func, .. } => func,
            _ => return None,
        };
        let key = (func.name.clone(), func.module_source.clone());
        let fields = self.ctx.multi_value_return_funcs.get(&key)?.clone();

        // Build per-field split locals: `<base>_mv_<field_name>`.
        let base = self.local_name(local_index);
        let mut split: IndexMap<String, (String, WirType)> = IndexMap::default();
        let mut order: Vec<(String, WirType)> = Vec::with_capacity(fields.len());
        for (field_name, result_type) in &fields {
            let local_name = format!("{base}_mv_{field_name}");
            let wir_ty = self.ctx.type_id_to_wir_type(self.type_table, *result_type);
            split.insert(field_name.clone(), (local_name.clone(), wir_ty.clone()));
            order.push((local_name, wir_ty));
        }

        // Translate the call (after dropping any borrow on `value`'s expr).
        let call_instr = self.translate_expr(value);

        // Emit DeclareLocal for each split, plus the MultiValueLocalBind.
        let mut instrs: Vec<WirInstr> = Vec::with_capacity(order.len() + 1);
        for (name, ty) in &order {
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty: ty.clone(),
            });
        }
        let locals = order.iter().map(|(n, _)| Some(n.clone())).collect();
        instrs.push(WirInstr::MultiValueLocalBind {
            instr: Box::new(call_instr),
            locals,
        });

        // Track for subsequent FieldAccess lookups.
        self.multi_value_split_locals.insert(local_index, split);

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
        tuple_type_id: crate::tir::TypeId,
        elements: &[NirExpr],
    ) -> (WirTypeId, Vec<WirInstr>) {
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, tuple_type_id);
        let wir_type_id = match &wir_type {
            WirType::Ref { type_id, .. } => Some(type_id.clone()),
            _ if elements.len() >= 2 => {
                // Tuple types created in CM binding synthesis may have TypeIds
                // from a different module's type_table, causing
                // `type_id_to_wir_type` to return I32 or AbstractRef instead
                // of Ref. Fall back to matching by element WIR types.
                self.ctx
                    .find_tuple_type_for_elements(self.type_table, elements)
                    .or_else(|| {
                        self.ctx
                            .define_tuple_struct_for_elements(self.type_table, elements)
                    })
            }
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
        let non_unit: Vec<&NirExpr> = elements
            .iter()
            .filter(|e| {
                !matches!(
                    self.ctx.type_id_to_wir_type(self.type_table, e.type_id),
                    WirType::Unit
                )
            })
            .collect();
        let raw_fields: Vec<WirInstr> = non_unit
            .into_iter()
            .map(|e| self.translate_expr(e))
            .collect();
        let fields = self.cast_nonnull_fields(&type_id, raw_fields);
        (type_id, fields)
    }

    /// Lower a `NirExprKind::ArrayLiteral` to the `Array<T>` struct shape
    /// `struct.new Array<T> { repr: array.new_fixed<T>(e0, …, eN-1), used: N }`.
    ///
    /// `Array<T>` is `{ repr: builtin::array<T>, used: i32 }` (see
    /// `lib/core/prelude/array.wado`); this mirrors `translate_string_literal`,
    /// which builds the structurally identical `String { repr, used }`. The
    /// raw `builtin::array<T>` type is read from the struct's `repr` field, so
    /// no element-type bookkeeping is duplicated on the NIR node. The resulting
    /// `ArrayNewFixed` is what `wir_optimize::array::{promote_constant_arrays_to_data,
    /// split_large_array_literals}` already consume.
    fn build_array_literal(
        &mut self,
        array_type_id: crate::tir::TypeId,
        elements: &[NirExpr],
    ) -> WirInstr {
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, array_type_id);
        let WirType::Ref { type_id, .. } = wir_type else {
            panic!(
                "[WIR] ArrayLiteral expected Ref WirType for Array<T> struct, got {wir_type:?} (type_id={array_type_id:?})"
            );
        };
        // The `repr` field is a non-nullable ref to the raw `builtin::array<T>`.
        let WirType::Ref {
            type_id: raw_array_type_id,
            ..
        } = self.struct_field_wir_type(&type_id, "repr")
        else {
            panic!("[WIR] ArrayLiteral: Array<T> struct {type_id:?} has no `repr` array field");
        };
        let element_instrs: Vec<WirInstr> =
            elements.iter().map(|e| self.translate_expr(e)).collect();
        let used = i32::try_from(element_instrs.len()).unwrap_or(0);
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

    /// Build the qualified global name.
    fn make_global_name(&self, module_source: &ModuleSource, name: &str) -> String {
        if module_source.is_entry_point() {
            format!("global:{name}")
        } else {
            let module_path = module_source.to_path();
            format!("global:{}::{name}", module_path.join("::"))
        }
    }

    /// Translate the top-level function body: declares locals and translates statements.
    fn translate_block(&mut self, block: &NirBlock) -> Vec<WirInstr> {
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
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, local.type_id);
                // Skip unit-type locals (unit has no Wasm representation)
                if matches!(wir_type, WirType::Unit) {
                    continue;
                }
                let idx = u32::try_from(i).unwrap();
                let local_name = self.local_name(idx);
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

    /// Scan statements recursively to discover Let declarations and emit `DeclareLocal`.
    /// Used when `local_types` is empty (for functions from library modules).
    fn declare_locals_from_stmts(&self, instrs: &mut Vec<WirInstr>, stmts: &[NirStmt]) {
        for stmt in stmts {
            match &stmt.kind {
                NirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    // Skip params (they are already declared via param_names)
                    let param_count = u32::try_from(self.tir_func.params.len()).unwrap();
                    if *local_index >= param_count {
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
                NirStmtKind::Loop { body } => {
                    self.declare_locals_from_stmts(instrs, &body.stmts);
                }
                NirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    self.declare_locals_from_stmts(instrs, &then_block.stmts);
                    if let Some(eb) = else_block {
                        self.declare_locals_from_stmts(instrs, &eb.stmts);
                    }
                }
                NirStmtKind::LabeledBlock { block, .. } => {
                    self.declare_locals_from_stmts(instrs, &block.stmts);
                }
                _ => {}
            }
        }
    }

    /// Translate a list of TIR statements to WIR instructions (no local declarations).
    pub(super) fn translate_stmts(&mut self, stmts: &[NirStmt]) -> Vec<WirInstr> {
        let mut instrs = Vec::new();
        for stmt in stmts {
            if let Some(instr) = self.translate_stmt(stmt) {
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
    pub(super) fn translate_stmts_as_value(&mut self, stmts: &[NirStmt]) -> Vec<WirInstr> {
        let mut instrs = Vec::new();
        let len = stmts.len();
        for (i, stmt) in stmts.iter().enumerate() {
            let is_last = i + 1 == len;
            if is_last {
                // Last statement: if it's an Expr, translate without drop
                if let NirStmtKind::Expr(expr) = &stmt.kind {
                    let instr = self.translate_expr_as_value(expr);
                    instrs.push(instr);
                    // Note: `translate_expr` already appends `unreachable` for
                    // `never`-typed expressions, so no extra push is needed here.
                    // For UNIT-typed expressions, all paths exit via break/return,
                    // so the fall-through is dead code — mark it explicitly so the
                    // Wasm validator knows the enclosing value-block's `end` is
                    // unreachable (void intermediate blocks don't push the expected
                    // typed result to the outer block's type stack).
                    //
                    // If the translated instruction already ends with an
                    // unconditional branch / return / `unreachable`, the
                    // polymorphic-stack rule covers the typed-block `end`
                    // without a trailing `unreachable`.
                    if expr.type_id == TypeTable::UNIT
                        && !instrs.last().is_some_and(WirInstr::ends_with_terminator)
                    {
                        instrs.push(WirInstr::Unreachable);
                    }
                    continue;
                }
                // Statement-level If with else can produce a value
                if let NirStmtKind::If {
                    condition,
                    then_block,
                    else_block: Some(else_block),
                    ..
                } = &stmt.kind
                    && let Some(result_type) = self.infer_stmts_result_type(&then_block.stmts)
                {
                    let cond = self.translate_expr(condition);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&then_block.stmts);
                    let else_body = Some(self.translate_stmts_as_value(&else_block.stmts));
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
            if let Some(instr) = self.translate_stmt(stmt) {
                // In a value-producing block, a final statement that does not push
                // a value to the Wasm stack means the enclosing typed block must be
                // exited exclusively via labeled `break`/`return` from inside the
                // statement.  The normal fall-through at the block's `end` is therefore
                // unreachable; append `unreachable` so the Wasm validator accepts the
                // typed block even when it has no stack value at the `end`.
                //
                // When the instruction already ends with an
                // unconditional branch / return / `unreachable`
                // (`ends_with_terminator`), the Wasm validator's
                // polymorphic stack rule accepts the implicit `end`
                // without a trailing `unreachable`. Skipping it here
                // trims the dead opcodes the issue calls "C1" (dead
                // code after `break`). NOTE: do not widen this to
                // `always_diverges` — Wasm validation does not treat
                // `if` with both diverging arms as polymorphic, so
                // skipping the trailing `unreachable` after such an
                // `if` would produce an invalid module.
                //
                // This covers all non-value-producing last statements:
                //  - explicit divergence (Return, Br, BrTable, Unreachable)
                //  - void blocks / loops whose only exits are outer labeled breaks
                //    (e.g. a TIR `loop {}` translated to `Block{result:None,[Loop{…}]}`)
                //  - any other void WIR instruction that should never reach this point
                //    in well-typed TIR
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

    /// Infer the WIR result type from the last statement in a list.
    /// Returns `Some(type)` if the last statement can produce a value, `None` otherwise.
    fn infer_stmts_result_type(&self, stmts: &[NirStmt]) -> Option<WirType> {
        stmts.last().and_then(|stmt| match &stmt.kind {
            NirStmtKind::Expr(expr) => {
                if expr.type_id != TypeTable::UNIT && expr.type_id != TypeTable::NEVER {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                } else {
                    None
                }
            }
            NirStmtKind::If {
                then_block,
                else_block: Some(_),
                ..
            } => self.infer_stmts_result_type(&then_block.stmts),
            _ => None,
        })
    }

    /// Translate an expression in "value position" — the result stays on the Wasm stack.
    ///
    /// Handles cases where TIR assigns UNIT type to expressions that actually produce
    /// values in a given context (e.g., nested if expressions, chained assignments).
    pub(super) fn translate_expr_as_value(&mut self, expr: &NirExpr) -> WirInstr {
        // If the expression already has a non-UNIT type, translate normally
        if expr.type_id != TypeTable::UNIT {
            return self.translate_expr(expr);
        }

        match &expr.kind {
            // If expression with UNIT type but value-producing branches
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                if let Some(result_type) = self.infer_stmts_result_type(&then_branch.stmts) {
                    let cond = self.translate_expr(condition);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&then_branch.stmts);
                    let else_body = else_branch
                        .as_ref()
                        .map(|b| self.translate_stmts_as_value(&b.stmts));
                    self.label_stack.pop();
                    return WirInstr::If {
                        condition: Box::new(cond),
                        result: Some(result_type),
                        then_body,
                        else_body,
                    };
                }
                self.translate_expr(expr)
            }
            // Block with UNIT type but value-producing last expression
            NirExprKind::Block(block) => {
                if self.infer_stmts_result_type(&block.stmts).is_some() {
                    let body = self.translate_stmts_as_value(&block.stmts);
                    WirInstr::Seq(body)
                } else {
                    self.translate_expr(expr)
                }
            }
            _ => self.translate_expr(expr),
        }
    }

    /// Translate a TIR statement to a WIR instruction.
    fn translate_stmt(&mut self, stmt: &NirStmt) -> Option<WirInstr> {
        match &stmt.kind {
            NirStmtKind::Let {
                local_index,
                value,
                is_mut,
                ..
            } => {
                // `immutable_locals` used to feed the WIR-level `is_source_immutable`
                // shortcut; keep the tracking for the residual reader
                // (`wir_build::value_copy::build_value_copy` no longer needs it but
                // removing the field is follow-up cleanup).
                if !is_mut {
                    self.immutable_locals.insert(*local_index);
                }
                // Phase 5: when the initializer is a direct call to a
                // multi-value-return function, bind the result's N tuple
                // elements into N split locals via `MultiValueLocalBind`
                // instead of trying to `LocalSet` the multi-value-Call
                // result into a single local (which Wasm doesn't allow).
                if let Some(instrs) = self.try_emit_multi_value_let(*local_index, value) {
                    return Some(instrs);
                }
                let value_instr = self.translate_expr(value);
                // If the initializer diverges (`never`), no value reaches the stack,
                // so LocalSet would be invalid. `translate_expr` already appends
                // `unreachable` for `never`-typed expressions, so just emit the
                // diverging instruction; the local is declared but never assigned.
                if value.type_id == TypeTable::NEVER {
                    Some(value_instr)
                } else if value.type_id == TypeTable::UNIT {
                    // Unit-type locals have no Wasm representation; just emit
                    // the init expression for its side effects (usually Nop).
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
            NirStmtKind::Expr(expr) => {
                let instr = self.translate_expr(expr);
                // If the expression has a non-unit type, drop it.
                // Exception: assignments and global-var-sets produce void WIR instructions
                // (LocalSet/StructSet/ArraySet/GlobalSet), so don't wrap them in Drop.
                let is_void_instr = matches!(
                    &expr.kind,
                    NirExprKind::Assign { .. } | NirExprKind::GlobalVarSet { .. }
                );
                if !is_void_instr
                    && expr.type_id != TypeTable::UNIT
                    && expr.type_id != TypeTable::NEVER
                {
                    Some(WirInstr::Drop(Box::new(instr)))
                } else {
                    Some(instr)
                }
            }
            NirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    let value_instr = self.translate_expr(expr);
                    // For multi-value-ABI functions, unwrap leaf
                    // `StructNew` aggregate-constructions inside the
                    // return value so the function pushes the N field
                    // values directly onto the stack instead of wrapping
                    // them in a heap struct.
                    if matches!(
                        self.tir_func.return_abi,
                        crate::nir::ReturnAbi::MultiValue { .. }
                    ) {
                        match value_instr {
                            // Direct StructNew → Return { Seq(fields) }.
                            WirInstr::StructNew { fields, .. } => Some(WirInstr::Return {
                                value: Some(Box::new(WirInstr::Seq(fields))),
                            }),
                            // Nested control flow (`return match { … }`,
                            // `return if … `): each branch's leaf
                            // StructNew is rewritten into its own
                            // `Return { Seq(fields) }`, and the outer
                            // expression replaces the whole Return —
                            // the inner Returns transfer control before
                            // the outer one would, so leaving an outer
                            // `Return` here would feed the validator
                            // an empty stack.
                            mut other @ (WirInstr::Seq(_)
                            | WirInstr::Block { .. }
                            | WirInstr::If { .. }) => {
                                lift_struct_new_to_seq(&mut other, false);
                                Some(other)
                            }
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
            NirStmtKind::Loop { body } => {
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
                let mut body_instrs = self.translate_stmts(&body.stmts);
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
            NirStmtKind::Break { label, value } => {
                let depth = self.compute_break_depth(label.as_deref());
                if let Some(val) = value {
                    let val_instr = self.translate_expr(val);
                    Some(WirInstr::Seq(vec![val_instr, WirInstr::Br { depth }]))
                } else {
                    Some(WirInstr::Br { depth })
                }
            }
            NirStmtKind::Continue => {
                let depth = self.compute_continue_depth();
                Some(WirInstr::Br { depth })
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                let cond = self.translate_expr(condition);
                // Push a label entry for the if block scope
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = self.translate_stmts(&then_block.stmts);
                let else_body = else_block.as_ref().map(|b| self.translate_stmts(&b.stmts));
                self.label_stack.pop();
                Some(WirInstr::If {
                    condition: Box::new(cond),
                    result: None,
                    then_body,
                    else_body,
                })
            }
            NirStmtKind::LabeledBlock { label, block } => {
                self.label_stack.push(LabelEntry {
                    label: Some(label.clone()),
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let body_instrs = self.translate_stmts(&block.stmts);
                self.label_stack.pop();
                Some(WirInstr::Block {
                    label: Some(label.clone()),
                    result: None,
                    body: body_instrs,
                })
            }
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.translate_let_pattern(pattern, value)
            }
        }
    }

    /// Translate a TIR expression to a WIR instruction.
    ///
    /// When the expression has type `never` (bottom type), the returned instruction
    /// diverges.  The `Seq([instr, Unreachable])` wrapper tells the Wasm validator
    /// that any subsequent type expectations in the same block are vacuously satisfied,
    /// so `never`-typed sub-expressions can appear in any value position (binary
    /// operands, struct fields, array elements, function arguments, …).
    pub(super) fn translate_expr(&mut self, expr: &NirExpr) -> WirInstr {
        let instr = self.translate_expr_inner(expr);
        if expr.type_id == TypeTable::NEVER && !instr.ends_with_terminator() {
            WirInstr::Seq(vec![instr, WirInstr::Unreachable])
        } else {
            instr
        }
    }

    fn translate_expr_inner(&mut self, expr: &NirExpr) -> WirInstr {
        match &expr.kind {
            NirExprKind::IntLiteral { value, .. } => match self.type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                    WirInstr::I64Const(*value as i64)
                }
                _ => WirInstr::I32Const(*value as i32),
            },
            NirExprKind::FloatLiteral { value, .. } => match self.type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::F32) => WirInstr::F32Const(*value as f32),
                _ => WirInstr::F64Const(*value),
            },
            NirExprKind::BoolLiteral(value) => WirInstr::I32Const(i32::from(*value)),
            NirExprKind::CharLiteral(c) => WirInstr::I32Const(*c as i32),
            NirExprKind::StringLiteral(s) => {
                // String literals are constructed from data segments
                self.translate_string_literal(s)
            }
            NirExprKind::BytesLiteral(b) => {
                // Bytes literals are constructed as Array<u8> from data segments
                self.translate_bytes_literal(b)
            }
            NirExprKind::Null => {
                // For Option types, construct a None variant struct.
                if let Some(inner) = self.type_table.as_option(expr.type_id) {
                    if matches!(self.type_table.get(inner), ResolvedType::Unknown) {
                        panic!(
                            "[WIR] Null with unresolved Option inner type (type_id={:?})",
                            expr.type_id
                        );
                    }
                    self.translate_variant_construct(
                        expr.type_id, // variant_type
                        1,            // case_index: None is case 1
                        "None",
                        None, // no payload
                        expr.type_id,
                    )
                } else {
                    // Non-Option null: emit ref.null as a placeholder value.
                    // Used by CM bindings for local initialization before conditional assignment.
                    WirInstr::RefNull {
                        heap_type: crate::wir::WirAbstractHeapType::None,
                    }
                }
            }
            NirExprKind::Unit => {
                // Unit has no value; use nop
                WirInstr::Nop
            }

            NirExprKind::Local { index, .. } => {
                // Unit and Never locals have no Wasm representation. For Unit
                // there is nothing to push. For Never the local declaration
                // was skipped (its initializer diverges); the surrounding
                // `translate_expr` wrapper appends `Unreachable` so the local
                // value never materializes — emit a placeholder `Nop`.
                if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
                    WirInstr::Nop
                } else {
                    self.local_get(*index)
                }
            }
            NirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                let global_name = self.make_global_name(module_source, name);
                WirInstr::GlobalGet {
                    name: WirName { fq: global_name },
                    result_ty: self.wir_type(expr.type_id),
                }
            }
            NirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                let global_name = self.make_global_name(module_source, name);
                let val = self.translate_expr(value);
                WirInstr::GlobalSet {
                    name: WirName { fq: global_name },
                    value: Box::new(val),
                }
            }

            NirExprKind::Binary { op, left, right } => {
                // Short-circuit logical operators: defer right-side evaluation
                if matches!(op, NirBinaryOp::And) {
                    let l = self.translate_expr(left);
                    let r = self.translate_expr(right);
                    // if left { right } else { 0 }
                    return WirInstr::If {
                        condition: Box::new(l),
                        result: Some(WirType::I32),
                        then_body: vec![r],
                        else_body: Some(vec![WirInstr::I32Const(0)]),
                    };
                }
                if matches!(op, NirBinaryOp::Or) {
                    let l = self.translate_expr(left);
                    let r = self.translate_expr(right);
                    // if left { 1 } else { right }
                    return WirInstr::If {
                        condition: Box::new(l),
                        result: Some(WirType::I32),
                        then_body: vec![WirInstr::I32Const(1)],
                        else_body: Some(vec![r]),
                    };
                }
                let l = Box::new(self.translate_expr(left));
                let r = Box::new(self.translate_expr(right));
                let result = self.translate_binary_op(op, l, r, left.type_id);
                // Truncate sub-i32 arithmetic/bitwise results to the correct width.
                // Comparisons and logical ops return bool (i32 0/1), so skip those.
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
                ) && let ResolvedType::Primitive(prim) = self.type_table.get(left.type_id)
                {
                    return Self::truncate_to_sub_i32(result, prim);
                }
                result
            }

            NirExprKind::Unary { op, expr: inner } => match op {
                NirUnaryOp::Ref | NirUnaryOp::MutRef => self.translate_expr(inner),
                NirUnaryOp::Deref => self.translate_expr(inner),
                _ => {
                    let o = Box::new(self.translate_expr(inner));
                    let result = self.translate_unary_op(op, o, inner.type_id);
                    // Truncate sub-i32 results for Neg and BitNot.
                    if matches!(op, NirUnaryOp::Neg | NirUnaryOp::BitNot)
                        && let ResolvedType::Primitive(prim) = self.type_table.get(inner.type_id)
                    {
                        return Self::truncate_to_sub_i32(result, prim);
                    }
                    result
                }
            },

            NirExprKind::Call { func, args, .. } => {
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

                // Static method: canonical dispatch (e.g., Stream::new, WaitableSet::new)
                if let Some(canonical) = func.method_info.clone().and_then(|m| m.cm_name)
                    && let Some(instr) = self.try_translate_canonical_static_method(
                        &canonical,
                        func,
                        args,
                        expr.type_id,
                    )
                {
                    return instr;
                }

                let translated_args: Vec<WirInstr> = args
                    .iter()
                    .filter(|a| a.expr.type_id != TypeTable::UNIT)
                    .map(|a| self.translate_expr(&a.expr))
                    .collect();

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else {
                    panic!(
                        "[WIR] unresolved Call: name={:?} builtin={:?}",
                        func.name.clone(),
                        builtin
                    );
                }
            }
            NirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                // Canonical resource method dispatch: uses #[canonical("...")] from types.wado
                if let Some(instr) =
                    self.try_translate_canonical_method(receiver, func, args, expr.type_id)
                {
                    return instr;
                }

                let mut translated_args: Vec<WirInstr> = Vec::new();
                // Receiver is always included (self/&self/&mut self is never unit).
                // Receivers are always reference types — do not copy them.
                translated_args.push(self.translate_expr(receiver));
                // params[0] is self; args[i] corresponds to params[i+1]
                for arg in args {
                    if arg.expr.type_id != TypeTable::UNIT {
                        translated_args.push(self.translate_expr(&arg.expr));
                    }
                }

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else if let Some(mi) = func.method_info.clone() {
                    panic!(
                        "[WIR] unresolved MethodCall: name={:?} method_info={:?}",
                        func.name.clone(),
                        mi
                    );
                } else {
                    panic!("[WIR] unresolved MethodCall: name={:?}", func.name.clone());
                }
            }

            NirExprKind::StructLiteral { fields, .. } => {
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
                            self.ctx
                                .type_id_to_wir_type(self.type_table, f.value.type_id),
                            WirType::Unit
                        )
                    })
                    .collect();
                let field_instrs: Vec<WirInstr> = non_unit_fields
                    .iter()
                    .map(|f| self.translate_expr(&f.value))
                    .collect();
                self.struct_new(type_id, field_instrs)
            }

            NirExprKind::FieldAccess {
                expr: receiver,
                field_name,
                ..
            } => {
                // When the receiver is a TIR local that was bound from a
                // multi-value-return Call, read the corresponding split
                // local directly. The aggregate was never materialised
                // as a struct ref — a `StructGet` here would read an
                // uninitialised slot.
                if let NirExprKind::Local {
                    index: tir_local, ..
                } = &receiver.kind
                    && let Some(splits) = self.multi_value_split_locals.get(tir_local)
                    && let Some((name, ty)) = splits.get(field_name)
                {
                    return WirInstr::LocalGet {
                        name: name.clone(),
                        result_ty: ty.clone(),
                    };
                }
                // If the field's result type is unit, emit only the receiver
                // for side effects and return Nop — unit has no Wasm representation.
                if expr.type_id == TypeTable::UNIT {
                    let recv = self.translate_expr(receiver);
                    return WirInstr::Seq(vec![WirInstr::Drop(Box::new(recv))]);
                }
                let recv = self.translate_expr(receiver);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, receiver.type_id);
                let WirType::Ref { type_id, .. } = wir_type else {
                    panic!(
                        "[WIR] FieldAccess receiver expected Ref WirType, got {wir_type:?} (field={field_name}, type_id={:?})",
                        receiver.type_id
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

            NirExprKind::Assign { target, value } => {
                let val = self.translate_expr(value);
                match &target.kind {
                    NirExprKind::Local { index, .. } => {
                        // Unit-type locals have no Wasm representation
                        if target.type_id == TypeTable::UNIT {
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
                    NirExprKind::FieldAccess {
                        expr: receiver,
                        field_name: _,
                        ..
                    } if target.type_id == TypeTable::UNIT => {
                        // Unit-typed field assignment: the field has no Wasm
                        // representation. Emit the receiver for side effects (then
                        // drop the ref), and emit val for side effects (it produces
                        // nothing because unit has no Wasm representation).
                        let recv = self.translate_expr(receiver);
                        WirInstr::Seq(vec![val, WirInstr::Drop(Box::new(recv))])
                    }
                    NirExprKind::FieldAccess {
                        expr: receiver,
                        field_name,
                        ..
                    } => {
                        let recv = self.translate_expr(receiver);
                        let wir_type = self
                            .ctx
                            .type_id_to_wir_type(self.type_table, receiver.type_id);
                        let WirType::Ref { type_id, .. } = wir_type else {
                            panic!(
                                "[WIR] FieldAccess assignment expected Ref receiver, got {wir_type:?} (field={field_name}, type_id={:?})",
                                receiver.type_id
                            );
                        };
                        self.struct_set(type_id, field_name.clone(), recv, val)
                    }
                    NirExprKind::Index {
                        expr: array_expr,
                        index: index_expr,
                    } => self.translate_index_assign(array_expr, index_expr, val),
                    _ => {
                        // Unhandled assignment target
                        WirInstr::Drop(Box::new(val))
                    }
                }
            }

            NirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Type casts become appropriate conversion instructions
                self.translate_cast(inner, inner.type_id, *target_type)
            }

            NirExprKind::Block(block) => {
                let body = if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
                    self.translate_stmts(&block.stmts)
                } else {
                    self.translate_stmts_as_value(&block.stmts)
                };
                WirInstr::Seq(body)
            }

            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.translate_expr(condition);
                let has_result = expr.type_id != TypeTable::UNIT;
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = if has_result {
                    self.translate_stmts_as_value(&then_branch.stmts)
                } else {
                    self.translate_stmts(&then_branch.stmts)
                };
                let else_body = else_branch.as_ref().map(|b| {
                    if has_result {
                        self.translate_stmts_as_value(&b.stmts)
                    } else {
                        self.translate_stmts(&b.stmts)
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

            NirExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.translate_match(scrutinee, arms, expr.type_id),

            NirExprKind::Index {
                expr: array_expr,
                index: index_expr,
            } => self.translate_index(array_expr, index_expr),

            NirExprKind::TupleLiteral { elements } => {
                // Lower to `struct.new` of the tuple struct type. The
                // multi-value-return Return-arm rewrite later unwraps
                // this back into a `Seq` of field initialisers when the
                // enclosing function has `ReturnAbi::MultiValue`. For
                // call-site destructures the heap struct is elided by
                // `wir_optimize::elide_struct::elide_multi_field_struct_locals`.
                let (type_id, fields) = self.tuple_constructor_args(expr.type_id, elements);
                WirInstr::StructNew { type_id, fields }
            }

            NirExprKind::ArrayLiteral { elements } => {
                self.build_array_literal(expr.type_id, elements)
            }

            NirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => self.translate_switch(scrutinee, *min_value, arms, default, expr.type_id),

            NirExprKind::VariantTag { expr: inner } => {
                // Get discriminant field from variant base type
                let val = self.translate_expr(inner);
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    WirInstr::StructGet {
                        type_id,
                        field_name: "discriminant".to_string(),
                        expr: Box::new(val),
                        result_ty: WirType::I32,
                    }
                } else {
                    WirInstr::I32Const(0)
                }
            }
            NirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name: _,
            } => self.translate_variant_test(inner, *case_index),
            NirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type: _,
            } => self.translate_variant_payload(inner, *case_index),
            NirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => self.translate_variant_construct(
                *variant_type,
                *case_index,
                case_name,
                payload.as_deref(),
                expr.type_id,
            ),
            NirExprKind::EnumConstruct { case_index, .. } => WirInstr::I32Const(*case_index as i32),

            NirExprKind::CmRawCall {
                local_name, args, ..
            } => {
                let translated_args: Vec<WirInstr> =
                    args.iter().map(|a| self.translate_expr(a)).collect();
                // Look up in WASI imports (registered by register_imports from TIR imports)
                let func_id = if let Some(func_id) =
                    self.ctx.func_map.get(&format!("wasi/{local_name}"))
                {
                    func_id.clone()
                } else {
                    // Not pre-registered — lazily register as a canonical intrinsic.
                    // This handles canonical imports (e.g., "task-return") that may not
                    // be in TIR imports but are needed by CM binding synthesis.
                    let params: Vec<WirType> = args
                        .iter()
                        .map(|a| self.ctx.type_id_to_wir_type(self.type_table, a.type_id))
                        .collect();
                    let results =
                        if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
                            vec![]
                        } else {
                            vec![self.ctx.type_id_to_wir_type(self.type_table, expr.type_id)]
                        };
                    let intrinsic = CanonicalIntrinsic::from_import_name(local_name)
                        .unwrap_or_else(|| panic!("unknown canonical intrinsic: {local_name}"));
                    // Future-related canonicals with default payload from from_import_name
                    // are NOT registered here. They must be registered via CM method dispatch
                    // with the correct CmFuturePayload. If a builtin calls future-drop-readable
                    // etc., the func_map entry from import registration is used directly.
                    if intrinsic.future_payload().is_some() {
                        // Look up the pre-registered import function
                        self.ctx
                            .func_map
                            .get(&format!("wasi/{local_name}"))
                            .cloned()
                            .unwrap_or_else(|| {
                                // No pre-registered import; fall back to ensure_canonical
                                self.ctx.ensure_canonical(intrinsic, params, results)
                            })
                    } else {
                        self.ctx.ensure_canonical(intrinsic, params, results)
                    }
                };
                WirInstr::Call {
                    func_id,
                    args: translated_args,
                }
            }

            NirExprKind::IndirectCall { callee, args } => {
                self.translate_indirect_call(callee, args, expr.type_id)
            }
            NirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => self.translate_closure_to_canonical(
                functor,
                *functor_id,
                *target_fn_type,
                closure_module,
            ),

            NirExprKind::LabeledBlock { label, block, .. } => {
                let has_result = expr.type_id != TypeTable::UNIT;
                self.label_stack.push(LabelEntry {
                    label: Some(label.clone()),
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let body = if has_result {
                    self.translate_stmts_as_value(&block.stmts)
                } else {
                    self.translate_stmts(&block.stmts)
                };
                self.label_stack.pop();
                let result_type = if expr.type_id == TypeTable::UNIT {
                    None
                } else {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                };
                WirInstr::Block {
                    label: Some(label.clone()),
                    result: result_type,
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
        // Fallback: depth 0 (should not happen with correct TIR)
        0
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
        // Fallback: depth 0 (should not happen with correct TIR)
        0
    }
}

/// Recursively rewrite leaf `StructNew` nodes in the value of a `Return`
/// statement so the function pushes its N fields onto the stack instead
/// of constructing a heap struct. Only applied when the enclosing
/// function has `ReturnAbi::MultiValue`. Handles:
///
/// - direct `StructNew` (the common `return Point { x, y }` case)
/// - `Seq(items)` — recurse into the trailing item
/// - `If` produced by `return if …` — recurse into both branch tails;
///   each branch tail's `StructNew` becomes its own `Return { Seq(fields) }`
///   and the `If`'s `result` is cleared since branches now transfer
///   control directly
/// - typed `Block` produced by `return match …` (`BrTable` lowering) —
///   rewrite each `StructNew; Br depth` exit pair to `Return { Seq(fields) }`
///
/// `wrap_in_return = false` is the outer call (the caller's `Return`
/// will wrap the produced `Seq`); recursive calls into branch tails use
/// `wrap_in_return = true` so each branch transfers control before the
/// outer (now-unused) Return would.
fn lift_struct_new_to_seq(expr: &mut WirInstr, wrap_in_return: bool) {
    match expr {
        WirInstr::StructNew { .. } => {
            if let WirInstr::StructNew { fields, .. } = std::mem::replace(expr, WirInstr::Nop) {
                let payload = WirInstr::Seq(fields);
                *expr = if wrap_in_return {
                    WirInstr::Return {
                        value: Some(Box::new(payload)),
                    }
                } else {
                    payload
                };
            }
        }
        WirInstr::Seq(items) => {
            if let Some(last) = items.last_mut() {
                lift_struct_new_to_seq(last, wrap_in_return);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            result,
            ..
        } => {
            // Branches now Return directly; the If no longer produces a value.
            *result = None;
            if let Some(last) = then_body.last_mut() {
                lift_struct_new_to_seq(last, true);
            }
            if let Some(eb) = else_body
                && let Some(last) = eb.last_mut()
            {
                lift_struct_new_to_seq(last, true);
            }
        }
        WirInstr::Block { body, result, .. } => {
            if result.is_some() {
                rewrite_struct_new_br_to_return(body, 0);
                *result = None;
            }
        }
        _ => {}
    }
}

/// Rewrite `StructNew; Br { depth }` exit pairs and `Seq([…, StructNew, Br])`
/// LabeledBlock-exit patterns inside a typed `Block` body into
/// `Return { Seq(fields) }`. Walks nested `Block` / `If` bodies recursively
/// (depth bumps by 1 on each level). The fallthrough (last instruction
/// without an explicit `Br`) is also rewritten when it is a `StructNew`.
fn rewrite_struct_new_br_to_return(instrs: &mut [WirInstr], target_depth: u32) {
    let mut i = 0;
    while i + 1 < instrs.len() {
        if matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth) {
            if matches!(&instrs[i], WirInstr::StructNew { .. })
                && let WirInstr::StructNew { fields, .. } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
            {
                instrs[i] = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(fields))),
                };
                instrs[i + 1] = WirInstr::Nop;
            }
            i += 2;
        } else {
            // Seq([…, StructNew, Br(target_depth)]) — LabeledBlock exit form.
            let is_seq_exit = if let WirInstr::Seq(seq) = &instrs[i] {
                seq.last().is_some_and(
                    |last| matches!(last, WirInstr::Br { depth } if *depth == target_depth),
                ) && seq.len() >= 2
                    && matches!(seq.get(seq.len() - 2), Some(WirInstr::StructNew { .. }))
            } else {
                false
            };
            if is_seq_exit {
                if let WirInstr::Seq(mut seq) = std::mem::replace(&mut instrs[i], WirInstr::Nop) {
                    seq.pop(); // remove Br
                    if let Some(WirInstr::StructNew { fields, .. }) = seq.pop() {
                        let ret = WirInstr::Return {
                            value: Some(Box::new(WirInstr::Seq(fields))),
                        };
                        instrs[i] = if seq.is_empty() {
                            ret
                        } else {
                            seq.push(ret);
                            WirInstr::Seq(seq)
                        };
                    }
                }
            } else {
                match &mut instrs[i] {
                    WirInstr::Block { body, .. } => {
                        rewrite_struct_new_br_to_return(body, target_depth + 1);
                    }
                    WirInstr::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        rewrite_struct_new_br_to_return(then_body, target_depth + 1);
                        if let Some(eb) = else_body {
                            rewrite_struct_new_br_to_return(eb, target_depth + 1);
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
    }
    // Fallthrough StructNew (no explicit Br at the end).
    if let Some(last) = instrs.last_mut()
        && matches!(last, WirInstr::StructNew { .. })
        && let WirInstr::StructNew { fields, .. } = std::mem::replace(last, WirInstr::Nop)
    {
        *last = WirInstr::Return {
            value: Some(Box::new(WirInstr::Seq(fields))),
        };
    }
}
