//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirStruct, ParamAbi};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef};
use crate::nir_package::NirPackage;
use crate::parallel::Executor;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::mod_ref::{FnEffect, compute_fn_effects};
use super::multi_value_return::{aggregate_field_info, is_eligible_field_type};

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

/// Set `param_abi` on every parameter that can take one Wasm slot per field.
/// Runs after every transformation, so it sees the final NIR shape.
pub fn classify_multi_value_params(project: &mut NirPackage, exec: &Executor) -> bool {
    let effects = compute_fn_effects(project, exec);
    let candidates = {
        let type_table = project.type_table.borrow();
        collect_candidates(project, &type_table, &project.structs, &effects)
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
    effects: &[FnEffect],
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
            // A `&S` arrives as the fields its referent held at the call, so
            // nothing the call runs may store into memory the caller holds.
            let (aggregate, is_ref) = match type_table.get(param.type_id) {
                ResolvedType::Ref(referent) if !effects[func_idx].writes_shared_heap => {
                    (*referent, true)
                }
                ResolvedType::Ref(_) => continue,
                _ => (param.type_id, false),
            };
            let Some((all_types, all_names, _)) =
                aggregate_field_info(aggregate, type_table, structs)
            else {
                continue;
            };
            let Some(read) = fields_read(body, param.local_index, &all_names) else {
                continue;
            };
            // A by-value argument is often a literal or a multi-value result,
            // which hands over every field; a borrow's are loaded at the call,
            // so it takes only those the callee reads.
            let (field_types, field_names): (Vec<TypeId>, Vec<String>) = all_types
                .into_iter()
                .zip(&all_names)
                .filter(|(_, name)| !is_ref || read.contains(name.as_str()))
                .map(|(ty, name)| (ty, name.clone()))
                .unzip();
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

/// The fields of `local` its reads name, `None` unless every read is the
/// subject of one of `field_names`. A promoted operand reads as an expression
/// does, so its value tree is read too.
// A read of the whole binding refutes: an aggregate rebuilt from the split
// locals is not the one the caller passed.
fn fields_read<'a>(
    body: &Body,
    local: u32,
    field_names: &'a [String],
) -> Option<IndexSet<&'a str>> {
    let names: IndexSet<&str> = field_names.iter().map(String::as_str).collect();
    let mut whole_reads: Vec<ExprId> = Vec::new();
    let mut field_subjects: IndexMap<ExprId, &str> = IndexMap::default();
    let mut read: IndexSet<&str> = IndexSet::default();
    let mut read_whole = false;
    let mut seen = IndexSet::default();

    body.for_each_live_node_under(NodeRef::Block(body.root()), |node| {
        body.for_each_operand(node, |op| {
            if let Some(v) = op.as_value() {
                body.values
                    .for_each_local_read(v, &mut seen, |index, field| {
                        if index == local {
                            match field {
                                Some(i) => {
                                    read.insert(field_names[i as usize].as_str());
                                }
                                None => read_whole = true,
                            }
                        }
                    });
            }
        });
        let NodeRef::Expr(expr) = node else {
            return;
        };
        match &body.exprs[expr].kind {
            ExprKind::Local { index, .. } if *index == local => whole_reads.push(expr),
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                if let Some(&name) = names.get(field_name.as_str())
                    && let Some(inner) = inner.as_expr()
                {
                    field_subjects.insert(inner, name);
                }
            }
            _ => {}
        }
    });

    if read_whole {
        return None;
    }
    for expr in whole_reads {
        read.insert(field_subjects.get(&expr).copied()?);
    }
    Some(read)
}
