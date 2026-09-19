//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirStruct, ParamAbi};
use crate::nir_arena::{Body, ExprId, ExprKind};
use crate::nir_package::NirPackage;
use crate::nir_visitor::reachable_exprs;
use crate::tir::TypeTable;

use super::multi_value_return::{aggregate_field_info, is_eligible_field_type};

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

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
    for ((func_idx, param_idx), abi) in candidates {
        project.functions[func_idx].borrow_mut().params[param_idx].param_abi = abi;
    }
    true
}

fn collect_candidates(
    project: &NirPackage,
    type_table: &TypeTable,
    structs: &[NirStruct],
) -> IndexMap<(usize, usize), ParamAbi> {
    let mut out = IndexMap::default();
    for (func_idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if func.is_dead || !func.only_reached_by_direct_call() {
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
            // A field the return side declines takes no Wasm slot of its own,
            // so splitting it would leave the signature and the call site
            // counting differently.
            if !field_types
                .iter()
                .all(|&t| is_eligible_field_type(t, type_table))
            {
                continue;
            }
            if !only_field_reads(body, param.local_index, &field_names) {
                continue;
            }
            out.insert(
                (func_idx, param_idx),
                ParamAbi::MultiValue {
                    field_types,
                    field_names,
                },
            );
        }
    }
    out
}

/// Whether every read of `local` is the subject of one of `field_names`.
// A read of the whole binding refutes: an aggregate rebuilt from the split
// locals is not the one the caller passed.
fn only_field_reads(body: &Body, local: u32, field_names: &[String]) -> bool {
    let names: IndexSet<&str> = field_names.iter().map(String::as_str).collect();
    let mut whole_reads: IndexSet<ExprId> = IndexSet::default();
    let mut field_subjects: IndexSet<ExprId> = IndexSet::default();

    for expr in reachable_exprs(body) {
        match &body.exprs[expr].kind {
            ExprKind::Local { index, .. } if *index == local => {
                whole_reads.insert(expr);
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } if names.contains(field_name.as_str()) => {
                if let Some(inner) = inner.as_expr() {
                    field_subjects.insert(inner);
                }
            }
            _ => {}
        }
    }

    whole_reads.is_subset(&field_subjects)
}
