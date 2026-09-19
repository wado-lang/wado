//! Multi-value parameter ABI classification: which aggregate parameters arrive
//! as one Wasm parameter per field instead of a heap struct. A candidate is a
//! by-value 2..=[`MAX_PARAM_FIELDS`]-field aggregate whose every use in the body
//! reads a field. The mirror of [`super::multi_value_return`], and the reason a
//! multi-value result handed straight on allocates nothing. The one mutation is
//! `param_abi`.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionKind, NirFunction, NirStruct, ParamAbi};
use crate::nir_arena::{Body, ExprId, ExprKind};
use crate::nir_package::NirPackage;
use crate::nir_visitor::reachable_exprs;
use crate::tir::{TypeId, TypeTable};

use super::multi_value_return::aggregate_field_info;

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

/// What one scalarized parameter carries to WIR build.
#[derive(Clone)]
struct ParamInfo {
    field_types: Vec<TypeId>,
    field_names: Vec<String>,
}

/// Set `param_abi` on every parameter that can take one Wasm slot per field.
/// Runs after every transformation, so it sees the final NIR shape.
pub fn classify_multi_value_params(project: &mut NirPackage) -> bool {
    let candidates = {
        let type_table = project.type_table.borrow();
        collect_candidates(project, &type_table, &project.structs)
    };
    if candidates.is_empty() {
        return false;
    }

    let mut changed = false;
    for ((func_idx, param_idx), info) in candidates {
        let mut func = project.functions[func_idx].borrow_mut();
        func.params[param_idx].param_abi = ParamAbi::MultiValue {
            field_types: info.field_types,
            field_names: info.field_names,
        };
        changed = true;
    }
    changed
}

fn collect_candidates(
    project: &NirPackage,
    type_table: &TypeTable,
    structs: &[NirStruct],
) -> IndexMap<(usize, usize), ParamInfo> {
    let mut out = IndexMap::default();
    for (func_idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if !function_eligible(&func) {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        for (param_idx, param) in func.params.iter().enumerate() {
            // A `&mut` parameter's writes have to reach the caller's storage,
            // and split fields are copies. A `mut` binding is reassigned whole,
            // which the split locals have no base local to hold.
            if param.is_mut || param.is_mut_ref {
                continue;
            }
            let Some((field_types, field_names, _)) =
                aggregate_field_info(param.type_id, type_table, structs)
            else {
                continue;
            };
            if !(2..=MAX_PARAM_FIELDS).contains(&field_types.len()) {
                continue;
            }
            if !only_field_reads(body, param.local_index, &field_names) {
                continue;
            }
            out.insert(
                (func_idx, param_idx),
                ParamInfo {
                    field_types,
                    field_names,
                },
            );
        }
    }
    out
}

/// The gates [`super::multi_value_return`] applies, for the same reasons: every
/// way a function's address leaves a direct call, and every caller `wir_build`
/// does not lower through the callee's recorded ABI.
fn function_eligible(func: &NirFunction) -> bool {
    matches!(func.kind, FunctionKind::Regular)
        && !func.is_dispatch_wrapper
        && !func.is_export
        && !func.is_cm_export
        && !func.is_cm_binding
        && !func.is_async
        && !func.has_real_type_params()
        && func.impl_type_params.is_empty()
        && !func.is_closure_call()
}

/// Whether every use of `local` reads one of `field_names` off it. A read of the
/// whole binding refutes: the split locals hold the fields, and the aggregate
/// they would be built back into is not the one the caller passed.
///
/// A destructure needs no case of its own — it reaches NIR as one field read per
/// binding.
fn only_field_reads(body: &Body, local: u32, field_names: &[String]) -> bool {
    let names: IndexSet<&str> = field_names.iter().map(String::as_str).collect();
    let mut whole_reads: IndexSet<ExprId> = IndexSet::default();
    let mut through_field: IndexSet<ExprId> = IndexSet::default();

    for expr in reachable_exprs(body) {
        match &body.exprs[expr].kind {
            ExprKind::Local { index, .. } if *index == local => {
                whole_reads.insert(expr);
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                if !names.contains(field_name.as_str()) {
                    continue;
                }
                if let Some(inner) = inner.as_expr()
                    && reads_local(body, inner, local)
                {
                    through_field.insert(inner);
                }
            }
            _ => {}
        }
    }

    whole_reads.iter().all(|e| through_field.contains(e))
}

fn reads_local(body: &Body, expr: ExprId, local: u32) -> bool {
    matches!(&body.exprs[expr].kind, ExprKind::Local { index, .. } if *index == local)
}

/// Every parameter position a call has to expand, by callee.
pub(crate) fn multi_value_param_positions(
    project: &NirPackage,
) -> IndexMap<FuncId, IndexMap<usize, Vec<(String, TypeId)>>> {
    let mut out: IndexMap<FuncId, IndexMap<usize, Vec<(String, TypeId)>>> = IndexMap::default();
    for func_rc in &project.functions {
        let Ok(func) = func_rc.try_borrow() else {
            continue;
        };
        let Some(id) = func.id else {
            continue;
        };
        for (param_idx, param) in func.params.iter().enumerate() {
            let ParamAbi::MultiValue {
                field_types,
                field_names,
            } = &param.param_abi
            else {
                continue;
            };
            let pairs: Vec<(String, TypeId)> = field_names
                .iter()
                .cloned()
                .zip(field_types.iter().copied())
                .collect();
            out.entry(id).or_default().insert(param_idx, pairs);
        }
    }
    out
}
