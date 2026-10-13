//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use std::ops::ControlFlow;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, ParamAbi};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
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
    let candidates = {
        let type_table = project.type_table.borrow();
        collect_candidates(project, &type_table, exec)
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
    exec: &Executor,
) -> IndexMap<(usize, usize), ParamAbi> {
    // Only a `&S` asks it, and a whole-program summary is not free.
    let mut effects: Option<Vec<FnEffect>> = None;
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
            let (aggregate, borrowed) = match type_table.get(param.type_id) {
                ResolvedType::Ref(referent) => (*referent, true),
                _ => (param.type_id, false),
            };
            let Some((all_types, all_names, _)) =
                aggregate_field_info(aggregate, type_table, &project.structs)
            else {
                continue;
            };
            // A by-value argument is often a literal or a multi-value result,
            // which hands over every field, so it takes them all.
            if !borrowed && !splittable(&all_types, type_table) {
                continue;
            }
            // A `&S` arrives as the fields its referent held at the call, so
            // nothing the call runs may store into memory the caller holds.
            if borrowed
                && effects.get_or_insert_with(|| compute_fn_effects(project, exec))[func_idx]
                    .writes_shared_heap
            {
                continue;
            }
            let local = param.local_index;
            let reads = Reads::under(body, NodeRef::Block(body.root()), local, &all_names);
            if reads.whole {
                continue;
            }
            // A borrow's fields are loaded at every call, so it takes those the
            // callee reads and no other, and only where it reads each on every
            // path: one read past an early exit costs a load where it is not.
            if borrowed
                && !reads
                    .fields
                    .is_subset(&fields_read_on_entry(body, local, &all_names))
            {
                continue;
            }
            let (field_types, field_names): (Vec<TypeId>, Vec<String>) = all_types
                .into_iter()
                .zip(&all_names)
                .filter(|(_, name)| !borrowed || reads.fields.contains(name.as_str()))
                .map(|(ty, name)| (ty, name.clone()))
                .unzip();
            if borrowed && !splittable(&field_types, type_table) {
                continue;
            }
            out.insert(
                (func_idx, param_idx),
                ParamAbi::MultiValue {
                    field_types,
                    field_names,
                    borrowed,
                },
            );
        }
    }
    out
}

/// Whether `field_types` fit the ABI: within its width, and each taking a Wasm
/// slot. A field the return side declines takes none, so splitting it would
/// leave the signature and the call site counting differently. One field read
/// is `sroa_param`'s, which knows a by-value `Box<T>` for the reference it is.
fn splittable(field_types: &[TypeId], type_table: &TypeTable) -> bool {
    (2..=MAX_PARAM_FIELDS).contains(&field_types.len())
        && field_types
            .iter()
            .all(|&t| is_eligible_field_type(t, type_table))
}

/// The fields of `local` read on every path through `body`: those its
/// statements read before the first one that branches, jumps or loops.
fn fields_read_on_entry<'a>(
    body: &Body,
    local: u32,
    field_names: &'a [String],
) -> IndexSet<&'a str> {
    let mut out = IndexSet::default();
    for &s in &body.blocks[body.root()].stmts {
        let (evaluated, ends_prefix) = match &body.stmts[s].kind {
            StmtKind::Let { value, .. }
            | StmtKind::Expr(value)
            | StmtKind::LetDestructure { value, .. } => (Some(*value), false),
            StmtKind::Return { value } => (*value, true),
            StmtKind::If { condition, .. } => (Some(*condition), true),
            StmtKind::Loop { .. }
            | StmtKind::Break { .. }
            | StmtKind::Continue
            | StmtKind::LabeledBlock { .. } => break,
        };
        if let Some(op) = evaluated {
            let reads = Reads::of_operand(body, op, local, field_names);
            if reads.branches {
                break;
            }
            out.extend(reads.fields);
        }
        if ends_prefix {
            break;
        }
    }
    out
}

/// How the code under one node reads a local holding an aggregate.
#[derive(Default)]
struct Reads<'a> {
    /// The fields it reads off the local.
    fields: IndexSet<&'a str>,
    /// It reads the local whole: an aggregate rebuilt from split locals is not
    /// the one the caller passed.
    whole: bool,
    /// Some evaluation takes a path another skips: a branch, a jump, a loop, or
    /// the right side of a short-circuit `&&` / `||`. A block an inlined call
    /// left holds none of these.
    branches: bool,
}

impl<'a> Reads<'a> {
    /// The reads under `root`, the expressions its promoted operands name as
    /// extraction sources and their value trees among them.
    fn under(body: &Body, root: NodeRef, local: u32, field_names: &'a [String]) -> Self {
        let names: IndexSet<&str> = field_names.iter().map(String::as_str).collect();
        let mut reads = Self::default();
        let mut whole_reads: Vec<ExprId> = Vec::new();
        let mut field_subjects: IndexMap<ExprId, &'a str> = IndexMap::default();
        let mut seen = IndexSet::default();
        body.for_each_live_node_under(root, |node| {
            body.for_each_operand(node, |op| {
                if let Some(v) = op.as_value() {
                    // The live walk visits the sources itself.
                    reads.value(body, v, local, field_names, &mut seen);
                }
            });
            match node {
                NodeRef::Stmt(s) => {
                    reads.branches |= !matches!(
                        body.stmts[s].kind,
                        StmtKind::Let { .. }
                            | StmtKind::Expr(_)
                            | StmtKind::LetDestructure { .. }
                            | StmtKind::LabeledBlock { .. }
                    );
                }
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::Local { index, .. } if *index == local => whole_reads.push(e),
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
                    ExprKind::If { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::Switch { .. }
                    | ExprKind::Binary {
                        op: NirBinaryOp::And | NirBinaryOp::Or,
                        ..
                    } => reads.branches = true,
                    _ => {}
                },
                NodeRef::Block(_) | NodeRef::Pat(_) => {}
            }
        });
        for e in whole_reads {
            match field_subjects.get(&e) {
                Some(&name) => {
                    reads.fields.insert(name);
                }
                None => reads.whole = true,
            }
        }
        reads
    }

    /// The reads evaluating `op` makes.
    fn of_operand(body: &Body, op: Operand, local: u32, field_names: &'a [String]) -> Self {
        match op {
            Operand::Expr(e) => Self::under(body, NodeRef::Expr(e), local, field_names),
            Operand::Value(v) => {
                let mut reads = Self::default();
                let sources = reads.value(body, v, local, field_names, &mut IndexSet::default());
                for e in sources {
                    reads.merge(Self::under(body, NodeRef::Expr(e), local, field_names));
                }
                reads
            }
        }
    }

    /// Fold in the value tree at `v`, answering the expressions it names as
    /// extraction sources, which extraction runs.
    fn value(
        &mut self,
        body: &Body,
        v: ValueId,
        local: u32,
        field_names: &'a [String],
        seen: &mut IndexSet<ValueId>,
    ) -> Vec<ExprId> {
        let source = |id: ValueId| match body.values.kind(id) {
            ValueKind::Opaque(oid) => body.values.opaque_source(*oid),
            _ => None,
        };
        let mut sources = Vec::new();
        body.values.walk_values::<()>(v, seen, |_, kind| {
            let descend = match kind {
                ValueKind::FieldAccess {
                    receiver,
                    field_index,
                    ..
                } if source(*receiver) == Some(OpaqueSource::Local(local)) => {
                    self.fields
                        .insert(field_names[*field_index as usize].as_str());
                    false
                }
                ValueKind::Opaque(oid) => {
                    match body.values.opaque_source(*oid) {
                        Some(OpaqueSource::Local(index)) => self.whole |= index == local,
                        Some(OpaqueSource::Expr(e)) => sources.push(e),
                        None => {}
                    }
                    false
                }
                // Extracted as an `if` over the right side.
                ValueKind::Binary {
                    op: NirBinaryOp::And | NirBinaryOp::Or,
                    ..
                } => {
                    self.branches = true;
                    true
                }
                _ => true,
            };
            ControlFlow::Continue(descend)
        });
        sources
    }

    fn merge(&mut self, other: Self) {
        self.fields.extend(other.fields);
        self.whole |= other.whole;
        self.branches |= other.branches;
    }
}
