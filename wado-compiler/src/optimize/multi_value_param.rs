//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use std::ops::ControlFlow;
use std::sync::Arc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirFunction, ParamAbi};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
use crate::parallel::Executor;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::gate::FunctionGate;
use super::heap_effect::{HeapEffects, HeapEffectsCache};
use super::multi_value_return::{aggregate_field_info, is_eligible_field_type};

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

/// Set `param_abi` on every parameter that can take one Wasm slot per field.
/// Runs after every transformation, so it sees the final NIR shape.
pub fn classify_multi_value_params(project: &mut NirPackage, exec: &Arc<Executor>) -> bool {
    let candidates = {
        let type_table = project.type_table.borrow();
        // Only a `&S` asks what a call writes, and the whole-program summary
        // is not free.
        let mut cache = HeapEffectsCache::default();
        let gate;
        let heap = if takes_a_borrow(project, &type_table) {
            gate = FunctionGate::new(project, exec);
            Some(cache.effects(project, &type_table, &gate))
        } else {
            None
        };
        collect_candidates(project, &type_table, heap.as_ref())
    };
    if candidates.is_empty() {
        return false;
    }
    for ((func_idx, param_idx), abi) in candidates {
        project.functions[func_idx].borrow_mut().params[param_idx].param_abi = abi;
    }
    true
}

/// Whether the ABI of `f`'s parameters is its own to choose: every caller is a
/// direct call this compilation rewrites alongside it.
fn chooses_param_abi(f: &NirFunction) -> bool {
    !f.is_dead && f.only_reached_by_direct_call() && f.body.is_some()
}

/// Whether some function [`chooses_param_abi`] and takes a borrow.
fn takes_a_borrow(project: &NirPackage, type_table: &TypeTable) -> bool {
    project.functions.iter().any(|f| {
        let f = f.borrow();
        chooses_param_abi(&f)
            && f.params
                .iter()
                .any(|p| matches!(type_table.get(p.type_id), ResolvedType::Ref(_)))
    })
}

fn collect_candidates(
    project: &NirPackage,
    type_table: &TypeTable,
    heap: Option<&HeapEffects>,
) -> IndexMap<(usize, usize), ParamAbi> {
    let mut out = IndexMap::default();
    for (func_idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if !chooses_param_abi(&func) {
            continue;
        }
        let body = func.body.as_ref().expect("checked above");
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
            // nothing the call runs may write an `S` the callee reads after.
            if borrowed
                && heap
                    .expect("a function takes a borrow")
                    .function_may_write(func_idx, aggregate)
            {
                continue;
            }
            let scope = ParamScope {
                body,
                type_table,
                local: param.local_index,
                field_names: &all_names,
            };
            let reads = scope.under(NodeRef::Block(body.root()));
            if reads.whole {
                continue;
            }
            // A borrow's fields are loaded at every call, so it takes those the
            // callee reads and no other, and only where it reads each on every
            // path: one read past an early exit costs a load where it is not.
            if borrowed && !reads.fields.is_subset(&scope.read_on_entry()) {
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

/// A parameter of one body, holding an aggregate whose fields are
/// `field_names`, and the questions asked of how that body reads it.
struct ParamScope<'b, 'a> {
    body: &'b Body,
    type_table: &'b TypeTable,
    local: u32,
    field_names: &'a [String],
}

impl<'a> ParamScope<'_, 'a> {
    /// The fields read on every path through the body that goes on: those
    /// its statements read before the first one that branches, jumps or
    /// loops.
    fn read_on_entry(&self) -> IndexSet<&'a str> {
        let body = self.body;
        let mut out = IndexSet::default();
        for &s in &body.blocks[body.root()].stmts {
            let (evaluated, ends_prefix) = match &body.stmts[s].kind {
                StmtKind::Let { value, .. }
                | StmtKind::Expr(value)
                | StmtKind::LetDestructure { value, .. } => (Some(*value), false),
                StmtKind::Return { value } => (*value, true),
                StmtKind::If { condition, .. } => (Some(*condition), !self.only_one_way_on(s)),
                StmtKind::Loop { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. } => break,
            };
            if let Some(op) = evaluated {
                let reads = self.of_operand(op);
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

    /// Whether the `if` at `s` leaves one way on: it has no `else`, and its
    /// branch ends in a call typed `!` (a bounds check's panic), which traps
    /// or runs forever, so whatever it reads goes unseen by what follows.
    fn only_one_way_on(&self, s: StmtId) -> bool {
        let body = self.body;
        let StmtKind::If {
            then_block,
            else_block: None,
            ..
        } = body.stmts[s].kind
        else {
            return false;
        };
        body.blocks[then_block].stmts.last().is_some_and(|&last| {
            matches!(&body.stmts[last].kind, StmtKind::Expr(op)
                if op.as_expr().is_some_and(|e| self.type_table.is_never(body.exprs[e].type_id)))
        })
    }

    /// The reads under `root`, the expressions its promoted operands name as
    /// extraction sources and their value trees among them.
    fn under(&self, root: NodeRef) -> Reads<'a> {
        let body = self.body;
        let names: IndexSet<&str> = self.field_names.iter().map(String::as_str).collect();
        let mut reads = Reads::default();
        let mut whole_reads: Vec<ExprId> = Vec::new();
        let mut field_subjects: IndexMap<ExprId, &'a str> = IndexMap::default();
        let mut seen = IndexSet::default();
        body.for_each_live_node_under(root, |node| {
            body.for_each_operand(node, |op| {
                if let Some(v) = op.as_value() {
                    // The live walk visits the sources itself.
                    self.value(v, &mut reads, &mut seen);
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
                    ) && !self.only_one_way_on(s);
                }
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::Local { index, .. } if *index == self.local => whole_reads.push(e),
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
    fn of_operand(&self, op: Operand) -> Reads<'a> {
        match op {
            Operand::Expr(e) => self.under(NodeRef::Expr(e)),
            Operand::Value(v) => {
                let mut reads = Reads::default();
                for e in self.value(v, &mut reads, &mut IndexSet::default()) {
                    reads.merge(self.under(NodeRef::Expr(e)));
                }
                reads
            }
        }
    }

    /// Fold the value tree at `v` into `reads`, answering the expressions it
    /// names as extraction sources, which extraction runs.
    fn value(
        &self,
        v: ValueId,
        reads: &mut Reads<'a>,
        seen: &mut IndexSet<ValueId>,
    ) -> Vec<ExprId> {
        let values = &self.body.values;
        let source = |id: ValueId| match values.kind(id) {
            ValueKind::Opaque(oid) => values.opaque_source(*oid),
            _ => None,
        };
        let mut sources = Vec::new();
        values.walk_values::<()>(v, seen, |_, kind| {
            let descend = match kind {
                ValueKind::FieldAccess {
                    receiver,
                    field_index,
                    ..
                } if source(*receiver) == Some(OpaqueSource::Local(self.local)) => {
                    reads
                        .fields
                        .insert(self.field_names[*field_index as usize].as_str());
                    false
                }
                ValueKind::Opaque(oid) => {
                    match values.opaque_source(*oid) {
                        Some(OpaqueSource::Local(index)) => reads.whole |= index == self.local,
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
                    reads.branches = true;
                    true
                }
                _ => true,
            };
            ControlFlow::Continue(descend)
        });
        sources
    }
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

impl Reads<'_> {
    fn merge(&mut self, other: Self) {
        self.fields.extend(other.fields);
        self.whole |= other.whole;
        self.branches |= other.branches;
    }
}
