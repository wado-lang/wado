//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use std::ops::ControlFlow;
use std::sync::Arc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirFunction, ParamAbi, aggregate_field_info};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
use crate::parallel::Executor;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::gate::FunctionGate;
use super::heap_effect::HeapEffectsCache;
use super::multi_value_return::is_eligible_field_type;

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

/// Set `param_abi` on every parameter that can take one Wasm slot per field.
/// Runs after every transformation, so it sees the final NIR shape.
pub fn classify_multi_value_params(project: &mut NirPackage, exec: &Arc<Executor>) -> bool {
    let candidates = {
        let type_table = project.type_table.borrow();
        let mut candidates = collect_candidates(project, &type_table);
        // A `&S` arrives as the fields its referent held at the call, so
        // nothing the call runs may write an `S` the callee reads after. The
        // whole-program summary that answers it is built only for one.
        if candidates.values().any(|c| c.referent.is_some()) {
            let gate = FunctionGate::new(project, exec);
            let mut cache = HeapEffectsCache::default();
            let heap = cache.effects(project, &type_table, &gate);
            candidates.retain(|&(func_idx, _), c| {
                c.referent
                    .is_none_or(|referent| !heap.function_may_write(func_idx, referent))
            });
        }
        candidates
    };
    if candidates.is_empty() {
        return false;
    }
    for ((func_idx, param_idx), c) in candidates {
        project.functions[func_idx].borrow_mut().params[param_idx].param_abi = c.abi;
    }
    true
}

/// A parameter the multi-value ABI fits, as far as its own body says.
struct Candidate {
    abi: ParamAbi,
    /// The type a `&S` refers to, which nothing the call runs may write.
    referent: Option<TypeId>,
}

/// Whether the ABI of `f`'s parameters is its own to choose: every caller is a
/// direct call this compilation rewrites alongside it.
fn chooses_param_abi(f: &NirFunction) -> bool {
    !f.is_dead && f.only_reached_by_direct_call() && f.body.is_some()
}

fn collect_candidates(
    project: &NirPackage,
    type_table: &TypeTable,
) -> IndexMap<(usize, usize), Candidate> {
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
            let scope = ParamScope {
                body,
                type_table,
                local: param.local_index,
                field_names: &all_names,
            };
            let reads = scope.read_anywhere();
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
                Candidate {
                    abi: ParamAbi::MultiValue {
                        field_types,
                        field_names,
                        borrowed,
                    },
                    referent: borrowed.then_some(aggregate),
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
                let mut walk = ReadWalk::new(self, Walk::Entry);
                walk.operand(op);
                let reads = walk.finish();
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
    /// branch ends unseen ([`Body::ends_unseen`]).
    fn only_one_way_on(&self, s: StmtId) -> bool {
        let StmtKind::If {
            then_block,
            else_block: None,
            ..
        } = self.body.stmts[s].kind
        else {
            return false;
        };
        self.body.ends_unseen(then_block, self.type_table)
    }

    /// Every read of the parameter anywhere in the body.
    fn read_anywhere(&self) -> Reads<'a> {
        let mut walk = ReadWalk::new(self, Walk::All);
        walk.node(NodeRef::Block(self.body.root()));
        walk.finish()
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
                ValueKind::FieldAccess { .. }
                | ValueKind::Binary { .. }
                | ValueKind::Unary { .. }
                | ValueKind::Cast { .. }
                | ValueKind::Select { .. }
                | ValueKind::LoopPhi { .. } => true,
                ValueKind::Int(..)
                | ValueKind::Float(..)
                | ValueKind::Bool(_)
                | ValueKind::Char(_)
                | ValueKind::Null
                | ValueKind::Unit
                | ValueKind::Const(..) => false,
            };
            ControlFlow::Continue(descend)
        });
        sources
    }
}

/// Which reads a [`ReadWalk`] collects.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// Every read, on any path: what the split has to hand over.
    All,
    /// The reads on every path that goes on. A branch that never returns is
    /// left out, as `read_on_entry` leaves it out at the top level, and
    /// anything else that branches is reported.
    Entry,
}

/// One walk over nodes and the value trees and extraction sources their
/// promoted operands name, folding what it finds into one [`Reads`].
struct ReadWalk<'s, 'b, 'a> {
    scope: &'s ParamScope<'b, 'a>,
    walk: Walk,
    names: IndexSet<&'a str>,
    reads: Reads<'a>,
    whole_reads: Vec<ExprId>,
    field_subjects: IndexMap<ExprId, &'a str>,
    values_seen: IndexSet<ValueId>,
    sources_seen: IndexSet<ExprId>,
}

impl<'s, 'b, 'a> ReadWalk<'s, 'b, 'a> {
    fn new(scope: &'s ParamScope<'b, 'a>, walk: Walk) -> Self {
        Self {
            scope,
            walk,
            names: scope.field_names.iter().map(String::as_str).collect(),
            reads: Reads::default(),
            whole_reads: Vec::new(),
            field_subjects: IndexMap::default(),
            values_seen: IndexSet::default(),
            sources_seen: IndexSet::default(),
        }
    }

    fn operand(&mut self, op: Operand) {
        match op {
            Operand::Expr(e) => self.node(NodeRef::Expr(e)),
            Operand::Value(v) => self.value(v),
        }
    }

    fn value(&mut self, v: ValueId) {
        let sources = self.scope.value(v, &mut self.reads, &mut self.values_seen);
        for e in sources {
            if self.sources_seen.insert(e) {
                self.node(NodeRef::Expr(e));
            }
        }
    }

    fn node(&mut self, root: NodeRef) {
        let body = self.scope.body;
        body.walk_nodes_under::<()>(root, |node| {
            let mut values = Vec::new();
            body.for_each_operand(node, |op| values.extend(op.as_value()));
            for v in values {
                self.value(v);
            }
            let descend = match node {
                NodeRef::Stmt(s) => self.stmt(s),
                NodeRef::Expr(e) => {
                    self.expr(e);
                    true
                }
                NodeRef::Block(_) | NodeRef::Pat(_) => true,
            };
            ControlFlow::Continue(descend)
        });
    }

    /// Whether the walk goes on into `s`. An entry walk takes only the
    /// condition of an `if` that leaves one way on.
    fn stmt(&mut self, s: StmtId) -> bool {
        let body = self.scope.body;
        if self.walk == Walk::All {
            return true;
        }
        if self.scope.only_one_way_on(s) {
            if let StmtKind::If {
                condition: Operand::Expr(e),
                ..
            } = body.stmts[s].kind
            {
                self.node(NodeRef::Expr(e));
            }
            return false;
        }
        self.reads.branches |= !matches!(
            body.stmts[s].kind,
            StmtKind::Let { .. }
                | StmtKind::Expr(_)
                | StmtKind::LetDestructure { .. }
                | StmtKind::LabeledBlock { .. }
        );
        true
    }

    fn expr(&mut self, e: ExprId) {
        let body = self.scope.body;
        // Every other kind reads the parameter, if at all, through a `Local`
        // in its subtree, which this walk reaches on its own.
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } if *index == self.scope.local => self.whole_reads.push(e),
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                if let Some(&name) = self.names.get(field_name.as_str())
                    && let Some(inner) = inner.as_expr()
                {
                    self.field_subjects.insert(inner, name);
                }
            }
            ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. }
            | ExprKind::Binary {
                op: NirBinaryOp::And | NirBinaryOp::Or,
                ..
            } => self.reads.branches |= self.walk == Walk::Entry,
            _ => {}
        }
    }

    fn finish(mut self) -> Reads<'a> {
        for e in self.whole_reads {
            match self.field_subjects.get(&e) {
                Some(&name) => {
                    self.reads.fields.insert(name);
                }
                None => self.reads.whole = true,
            }
        }
        self.reads
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
