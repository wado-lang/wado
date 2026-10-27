//! Which aggregate parameters arrive as one Wasm parameter per field instead of
//! a heap struct. The mirror of [`super::multi_value_return`].

use std::ops::ControlFlow;
use std::sync::Arc;

use cranelift_entity::EntityRef;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirFunction, ParamAbi, aggregate_field_info};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::parallel::Executor;
use crate::tir::{ResolvedType, TypeId};
use crate::type_table::TypeTable;

use super::gate::FunctionGate;
use super::heap_effect::HeapEffectsCache;
use super::multi_value_return::is_eligible_field_type;

/// The same width the return side takes. Beyond it the register pressure of a
/// call costs more than the aggregate it avoids.
const MAX_PARAM_FIELDS: usize = 8;

/// One field is `sroa_param`'s, which knows a by-value `Box<T>` for the
/// reference it is.
const MIN_PARAM_FIELDS: usize = 2;

/// Set `param_abi` on every parameter that can take one Wasm slot per field.
/// Runs after every transformation, so it sees the final NIR shape.
pub fn classify_multi_value_params(project: &mut NirPackage, exec: &Arc<Executor>) -> bool {
    let abis = {
        let type_table = project.type_table.borrow();
        let mut candidates = collect_candidates(project, &type_table);
        // A `&S` arrives as the fields its referent held at the call, so
        // nothing the call runs may write an `S` the callee reads after. The
        // whole-program summary that answers it is built only for one.
        if candidates.values().any(|c| c.through_ref) {
            let gate = FunctionGate::new(project, exec);
            let mut cache = HeapEffectsCache::default();
            let heap = cache.effects(project, &type_table, &gate);
            candidates.retain(|&(func_idx, _), c| {
                !c.through_ref || !heap.function_may_write(func_idx, c.aggregate)
            });
        }
        settle(candidates, &type_table)
    };
    if abis.is_empty() {
        return false;
    }
    for ((func_idx, param_idx), abi) in abis {
        project.functions[func_idx].borrow_mut().params[param_idx].param_abi = abi;
    }
    true
}

/// A parameter: its function's index in the function store, then its own.
type ParamKey = (usize, usize);

/// A parameter the multi-value ABI fits, as far as its own body says.
struct Candidate {
    /// The struct or tuple the parameter holds, or refers to.
    aggregate: TypeId,
    through_ref: bool,
    field_types: Vec<TypeId>,
    field_names: Vec<String>,
    /// The fields the body reads, by name.
    reads: IndexSet<String>,
    /// The parameters it hands the local to whole, which take the ABI only
    /// together with it: the call then passes on the fields it arrived in.
    forwards: IndexSet<ParamKey>,
}

impl Candidate {
    /// The fields a call hands over: those the body reads. A field nothing
    /// reads is still evaluated at the call, and never passed. An aggregate
    /// taken by value and read in fewer fields than the ABI's narrowest passes
    /// them all, as it did before any was dropped.
    fn taken(&self) -> (Vec<TypeId>, Vec<String>) {
        let every = !self.through_ref && self.reads.len() < MIN_PARAM_FIELDS;
        self.field_types
            .iter()
            .zip(&self.field_names)
            .filter(|(_, name)| every || self.reads.contains(name.as_str()))
            .map(|(&ty, name)| (ty, name.clone()))
            .unzip()
    }
}

/// The candidates that take the ABI, and the fields each takes. A forward
/// keeps its caller only while its callee stays, and widens what the caller
/// takes to what the callee does, so both are settled together: the reads
/// grow to a fixpoint first, so no width is judged before it is final.
fn settle(
    mut candidates: IndexMap<ParamKey, Candidate>,
    type_table: &TypeTable,
) -> IndexMap<ParamKey, ParamAbi> {
    loop {
        while settle_forwards(&mut candidates, type_table) {}
        let before = candidates.len();
        candidates.retain(|_, c| splittable(&c.taken().0, type_table));
        if candidates.len() == before {
            break;
        }
    }
    candidates
        .into_iter()
        .map(|(key, c)| {
            let (field_types, field_names) = c.taken();
            let abi = ParamAbi::MultiValue {
                field_types,
                field_names,
                through_ref: c.through_ref,
            };
            (key, abi)
        })
        .collect()
}

/// One round over the forwards: drop a candidate whose callee does not take
/// the ABI on the same shape, and widen one to the fields its callees take,
/// which the call passes on from those it arrived in. Whether anything changed.
fn settle_forwards(candidates: &mut IndexMap<ParamKey, Candidate>, type_table: &TypeTable) -> bool {
    let keys: Vec<ParamKey> = candidates.keys().copied().collect();
    let mut changed = false;
    for key in keys {
        let c = &candidates[&key];
        if c.forwards.is_empty() {
            continue;
        }
        let mut reads = c.reads.clone();
        let agrees = c.forwards.iter().all(|f| {
            candidates.get(f).is_some_and(|g| {
                // The argument is the local itself, or a reference to it, so
                // the callee holds the same fields. Not the same aggregate: one
                // instance serves `Pair<Meters>` and `Pair<f32>` alike.
                assert!(
                    g.field_names == c.field_names
                        && type_table.type_keys(&g.field_types)
                            == type_table.type_keys(&c.field_types),
                    "a forwarded aggregate arrives in the fields it was sent in"
                );
                reads.extend(g.taken().1);
                g.through_ref == c.through_ref
            })
        });
        if !agrees {
            candidates.shift_remove(&key);
            changed = true;
        } else if reads.len() > c.reads.len() {
            candidates[&key].reads = reads;
            changed = true;
        }
    }
    changed
}

/// Whether the ABI of `f`'s parameters is its own to choose: every caller is a
/// direct call this compilation rewrites alongside it.
fn chooses_param_abi(f: &NirFunction) -> bool {
    !f.is_dead && f.only_reached_by_direct_call() && f.body.is_some()
}

fn collect_candidates(
    project: &NirPackage,
    type_table: &TypeTable,
) -> IndexMap<ParamKey, Candidate> {
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
            let (aggregate, through_ref) = match type_table.get(param.type_id) {
                ResolvedType::Ref(referent) => (*referent, true),
                _ => (param.type_id, false),
            };
            let Some((all_types, all_names, _)) =
                aggregate_field_info(aggregate, type_table, &project.structs)
            else {
                continue;
            };
            let scope = ParamScope {
                body,
                local: param.local_index,
                field_names: &all_names,
            };
            let reads = scope.read_anywhere();
            if reads.whole || !may_fit(&reads, through_ref, &all_types, &all_names, type_table) {
                continue;
            }
            // One read past an early exit costs a `&S` a load on the path that
            // skips it, which the struct a caller no longer builds outweighs.
            let read_names = reads.fields.iter().map(|&name| name.to_string()).collect();
            out.insert(
                (func_idx, param_idx),
                Candidate {
                    aggregate,
                    through_ref,
                    reads: read_names,
                    forwards: reads.forwards,
                    field_types: all_types,
                    field_names: all_names,
                },
            );
        }
    }
    out
}

/// Whether a parameter read as `reads` may still take the ABI once its
/// forwards settle, which only add reads: what fails here never comes to fit,
/// and dropping it early can spare the whole-program write summary.
fn may_fit(
    reads: &Reads<'_>,
    through_ref: bool,
    all_types: &[TypeId],
    all_names: &[String],
    type_table: &TypeTable,
) -> bool {
    if !through_ref && splittable(all_types, type_table) {
        return true;
    }
    let read: Vec<TypeId> = all_types
        .iter()
        .zip(all_names)
        .filter(|(_, name)| reads.fields.contains(name.as_str()))
        .map(|(&ty, _)| ty)
        .collect();
    read.len() <= MAX_PARAM_FIELDS
        && (read.len() >= MIN_PARAM_FIELDS || !reads.forwards.is_empty())
        && read.iter().all(|&t| is_eligible_field_type(t, type_table))
}

/// Whether `field_types` fit the ABI: within its width, and each taking a Wasm
/// slot. A field the return side declines takes none, so splitting it would
/// leave the signature and the call site counting differently.
fn splittable(field_types: &[TypeId], type_table: &TypeTable) -> bool {
    (MIN_PARAM_FIELDS..=MAX_PARAM_FIELDS).contains(&field_types.len())
        && field_types
            .iter()
            .all(|&t| is_eligible_field_type(t, type_table))
}

/// A parameter of one body, holding an aggregate whose fields are
/// `field_names`, and the questions asked of how that body reads it.
struct ParamScope<'b, 'a> {
    body: &'b Body,
    local: u32,
    field_names: &'a [String],
}

impl<'a> ParamScope<'_, 'a> {
    /// Every read of the parameter anywhere in the body.
    fn read_anywhere(&self) -> Reads<'a> {
        let mut walk = ReadWalk::new(self);
        walk.node(NodeRef::Block(self.body.root()));
        walk.finish()
    }

    /// Fold the value tree at `v` into `reads`.
    fn value(&self, v: ValueId, reads: &mut Reads<'a>, seen: &mut IndexSet<ValueId>) {
        let values = &self.body.values;
        let reads_local = |id: ValueId| match values.kind(id) {
            ValueKind::Opaque(oid) => values.promoted_opaque_local(*oid) == self.local,
            _ => false,
        };
        values.walk_values::<()>(v, seen, |id, kind| {
            let descend = match kind {
                ValueKind::FieldAccess {
                    receiver,
                    field_index,
                    ..
                } if reads_local(*receiver) => {
                    reads
                        .fields
                        .insert(self.field_names[*field_index as usize].as_str());
                    false
                }
                ValueKind::Opaque(_) => {
                    reads.whole |= reads_local(id);
                    false
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
    }
}

/// One walk over nodes and the value trees their promoted operands name,
/// folding what it finds into one [`Reads`].
struct ReadWalk<'s, 'b, 'a> {
    scope: &'s ParamScope<'b, 'a>,
    names: IndexSet<&'a str>,
    reads: Reads<'a>,
    whole_reads: Vec<ExprId>,
    field_subjects: IndexMap<ExprId, &'a str>,
    /// The places assignments write.
    written: IndexSet<ExprId>,
    /// The call arguments that hand the local on whole.
    forwarded: IndexSet<ExprId>,
    values_seen: IndexSet<ValueId>,
}

impl<'s, 'b, 'a> ReadWalk<'s, 'b, 'a> {
    fn new(scope: &'s ParamScope<'b, 'a>) -> Self {
        Self {
            scope,
            names: scope.field_names.iter().map(String::as_str).collect(),
            reads: Reads::default(),
            whole_reads: Vec::new(),
            field_subjects: IndexMap::default(),
            written: IndexSet::default(),
            forwarded: IndexSet::default(),
            values_seen: IndexSet::default(),
        }
    }

    fn node(&mut self, root: NodeRef) {
        let body = self.scope.body;
        body.walk_nodes_under::<()>(root, |node| {
            // A promoted read of the local is one value wherever it appears,
            // so only this call's own argument slots skip it.
            let mut forwarded_values = Vec::new();
            if let NodeRef::Expr(e) = node {
                if self.forwarded.contains(&e) {
                    return ControlFlow::Continue(false);
                }
                forwarded_values = self.record_forwards(e);
            }
            let mut values = Vec::new();
            body.for_each_operand(node, |op| values.extend(op.as_value()));
            for v in values {
                if let Some(i) = forwarded_values.iter().position(|&f| f == v) {
                    forwarded_values.swap_remove(i);
                } else {
                    self.scope.value(v, &mut self.reads, &mut self.values_seen);
                }
            }
            if let NodeRef::Expr(e) = node {
                self.expr(e);
            }
            ControlFlow::Continue(true)
        });
    }

    /// Record each argument of the call at `e` that hands the local on whole,
    /// which the walk then leaves out of the reads, and answer the promoted
    /// ones.
    fn record_forwards(&mut self, e: ExprId) -> Vec<ValueId> {
        let body = self.scope.body;
        let mut values = Vec::new();
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return values;
        };
        for (param_idx, arg) in args.iter().enumerate() {
            if !arg.is_mut && body.argument_local(arg.expr) == Some(self.scope.local) {
                match arg.expr {
                    Operand::Expr(a) => {
                        self.forwarded.insert(a);
                    }
                    Operand::Value(v) => values.push(v),
                }
                self.reads.forwards.insert((func_id.index(), param_idx));
            }
        }
        values
    }

    fn expr(&mut self, e: ExprId) {
        // Every other kind reads the parameter, if at all, through a `Local`
        // in its subtree, which this walk reaches on its own.
        match &self.scope.body.exprs[e].kind {
            ExprKind::Local { index, .. } if *index == self.scope.local => self.whole_reads.push(e),
            // A field written writes the caller's aggregate, which a split
            // local cannot reach, so the local under it counts as read whole.
            ExprKind::Assign { target, .. } => {
                self.written.insert(*target);
            }
            ExprKind::FieldAccess { .. } if self.written.contains(&e) => {}
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
    /// The parameters it hands the local to whole.
    forwards: IndexSet<ParamKey>,
    /// It reads the local whole: an aggregate rebuilt from split locals is not
    /// the one the caller passed.
    whole: bool,
}
