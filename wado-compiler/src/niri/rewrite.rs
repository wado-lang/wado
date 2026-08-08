//! Writing a value back into the IR.
//!
//! The projection answers what an expression denotes; this is what becomes of
//! the expression once it does. Every edit goes through an [`EditSink`], so the
//! same rewrites serve two backends: the throwaway body a compile-time frame
//! runs on, and the real one, whose maps an engine keeps coherent.
//!
//! Not every value has a form to be written as. A scalar promotes to a pure
//! operand; a byte-sequence container becomes the literal the lower phase emits
//! for a source string; every other aggregate stays inside the engine, and what
//! reaches the IR is the scalars projected out of it.

use crate::compiler_item::SeqField;
use crate::const_eval::Value;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    ArenaStructField, ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind,
    StmtId, StmtKind,
};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{PrimitiveType, TypeId, TypeTable};

use super::lattice::is_provably_exhaustive;
use super::pattern::PatternMatch;
use super::{BodySink, EditSink, Interpreter, Lattice, PatBindings};

impl Interpreter<'_> {
    /// Splice each constant-condition `if` statement of `block` into `block`
    /// itself, leaving the arm the condition chooses.
    pub fn reduce_local_block<S: EditSink>(&mut self, sink: &mut S, block: BlockId) -> bool {
        let body = sink.body();
        let has_constant_if = body.blocks[block].stmts.iter().any(|s| {
            matches!(
                &body.stmts[*s].kind,
                StmtKind::If { condition, .. }
                    if operand_bool(body, *condition).is_some()
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = body.blocks[block].stmts.clone();
        let mut new_stmts: Vec<StmtId> = Vec::new();
        for s in old_stmts {
            let body = sink.body();
            let spliced = if let StmtKind::If {
                condition,
                then_block,
                else_block,
            } = &body.stmts[s].kind
            {
                operand_bool(body, *condition).map(|value| (value, *then_block, *else_block))
            } else {
                None
            };
            if let Some((value, then_block, else_block)) = spliced {
                if value {
                    new_stmts.extend(sink.body().blocks[then_block].stmts.clone());
                } else if let Some(eb) = else_block {
                    new_stmts.extend(sink.body().blocks[eb].stmts.clone());
                }
                continue;
            }
            new_stmts.push(s);
        }
        sink.set_block_stmts(block, new_stmts);
        true
    }

    /// Reduce `e` to its flow-sensitive constant value or collapse a constant
    /// branch, committing every edit through `sink`. Both value sources — the
    /// flow-sensitive candidate and a self-contained region run — land through
    /// [`Self::commit_fold`], so what is promoted, materialized, memoized, and
    /// refused is decided in one place.
    pub fn reduce_local<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        if let Some(value) = self.flow_fold_candidate(sink.body(), e)
            && self.commit_fold(sink, e, value)
        {
            return true;
        }
        if let Some(value) = self.try_region_fold(sink.body(), e)
            && self.commit_fold(sink, e, value)
        {
            return true;
        }
        if rewrite_short_circuit_via(sink, e) {
            return true;
        }
        if self.rewrite_if_expr_via(sink, e) {
            return true;
        }
        self.rewrite_match_expr_via(sink, e)
    }

    /// Take `value` over `e`: promote a scalar, materialize a byte-sequence
    /// container, and memoize what the sink did not take, reporting whether
    /// the node was rewritten.
    ///
    /// A reference-typed node is refused whole: its value would stand as a
    /// fresh literal where the program yields an alias, and `ref.eq` can tell
    /// the two apart.
    ///
    /// A declined scalar is always memoized — the scratch backend promotes
    /// nothing, so the memo is where its folds live.
    ///
    /// An aggregate is written back wherever the write buys something
    /// ([`Self::is_worth_materializing`]), and over a shape that consumed its
    /// source ([`consumes_its_source`]) regardless. Only the latter is
    /// memoized: a revisit would re-run a body to recompute it, while every
    /// other shape still derives its own value from the literal left in its
    /// place.
    fn commit_fold<S: EditSink>(&mut self, sink: &mut S, e: ExprId, value: Value) -> bool {
        let node_type = sink.body().exprs[e].type_id;
        if self.type_table.is_reference_shaped(node_type) {
            return false;
        }
        if value.is_scalar() {
            if sink.replace_with_value(e, value.clone()) {
                return true;
            }
            self.frame.scratch_folds.insert(e, value);
            return false;
        }
        let consumes = consumes_its_source(&sink.body().exprs[e].kind);
        if !consumes && !self.is_worth_materializing(sink.body(), e) {
            return false;
        }
        let committed = self.materialize_seq_via(sink, e, &value);
        if consumes {
            self.frame.scratch_folds.insert(e, value);
        }
        committed
    }

    /// Whether writing the value over `e` buys anything.
    ///
    /// It does everywhere but two shapes that already hold the answer. One is
    /// the literal [`Self::materialize_seq_via`] writes — refusing it is what
    /// makes the rewrite happen once, which the [`consumes_its_source`] shapes
    /// get from their kinds for free.
    ///
    /// The other is a read of a global. Const-object globalization put the
    /// value there so it is built once and shared; a literal in its place is
    /// that constant copied back to every site, and the store left behind
    /// outlives the slot the reachability census then drops.
    fn is_worth_materializing(&self, body: &Body, e: ExprId) -> bool {
        !matches!(body.exprs[e].kind, ExprKind::GlobalVarGet { .. })
            && !self.is_materialized_seq_literal(body, e)
    }

    /// A sequence container still computing its contents: the shape whose
    /// value is worth asking the lattice for, because writing the constant
    /// over one drops an allocation and a copy.
    fn is_unmaterialized_seq_literal(&self, body: &Body, e: ExprId) -> bool {
        self.seq_literal_backing(body, e)
            .is_some_and(|b| !matches!(b, ExprKind::PackedArray(_)))
    }

    /// Whether `e` already holds the literal [`Self::materialize_seq_via`]
    /// writes.
    fn is_materialized_seq_literal(&self, body: &Body, e: ExprId) -> bool {
        self.seq_literal_backing(body, e)
            .is_some_and(|b| matches!(b, ExprKind::PackedArray(_)))
    }

    /// The backing array a sequence container literal is built over.
    fn seq_literal_backing<'b>(&self, body: &'b Body, e: ExprId) -> Option<&'b ExprKind> {
        let ExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } = &body.exprs[e].kind
        else {
            return None;
        };
        if !self.type_table.is_seq_container(*struct_type) {
            return None;
        }
        fields
            .iter()
            .find(|f| f.field_index == SeqField::Backing.index())?
            .value
            .as_expr()
            .map(|b| &body.exprs[b].kind)
    }

    /// Write `value` back over `e` as the container literal the lower phase
    /// emits for a source string: a struct over a packed byte array and its
    /// length.
    ///
    /// Only the container's first `used` bytes: capacity outruns what it holds
    /// and is not observable. An empty one is left alone, being a reservation
    /// rather than a result.
    ///
    /// The container is identified by type, never recognised by shape — any
    /// struct over an array and an `i32` has that shape, and over
    /// `Chunk { data, tag }` the literal would read the second field as a
    /// length.
    fn materialize_seq_via<S: EditSink>(&self, sink: &mut S, e: ExprId, value: &Value) -> bool {
        let Value::Aggregate { type_id, .. } = value else {
            return false;
        };
        if !self.type_table.is_seq_container(*type_id) {
            return false;
        }
        // The literal is written over `e` but typed from the value, and a node
        // yielding nothing can hold neither.
        if sink.body().exprs[e].type_id == TypeTable::UNIT {
            return false;
        }
        let Some(Value::Seq { elements, .. }) = value.field(SeqField::Backing.index()) else {
            return false;
        };
        let Some((used, PrimitiveType::I32)) =
            value.field(SeqField::Len.index()).and_then(Value::as_int)
        else {
            return false;
        };
        let Ok(used) = usize::try_from(used) else {
            return false;
        };
        if used == 0 || used > elements.len() {
            return false;
        }
        let mut bytes = Vec::with_capacity(used);
        for element in &elements[..used] {
            let Some((byte, PrimitiveType::U8)) = element.as_int() else {
                return false;
            };
            let Ok(byte) = u8::try_from(byte) else {
                return false;
            };
            bytes.push(byte);
        }
        let Some(backing_type) = self.type_table.find_builtin_array(TypeTable::U8) else {
            return false;
        };
        let span = sink.body().exprs[e].span;
        let backing = sink.alloc_expr(ExprKind::PackedArray(bytes), backing_type, span);
        let len = u64::try_from(used).expect("a bounded element count fits u64");
        let len = sink.const_operand(ValueKind::Int(len, TypeTable::I32), TypeTable::I32);
        sink.replace_kind(
            e,
            ExprKind::StructLiteral {
                struct_type: *type_id,
                struct_name: self.type_table.type_name(*type_id),
                fields: vec![
                    ArenaStructField {
                        name: SeqField::Backing.field_name().to_string(),
                        value: Operand::Expr(backing),
                        field_index: SeqField::Backing.index(),
                    },
                    ArenaStructField {
                        name: SeqField::Len.field_name().to_string(),
                        value: len,
                        field_index: SeqField::Len.index(),
                    },
                ],
            },
        );
        true
    }

    /// The environment-free constant scalar `e` denotes: literal arithmetic,
    /// projection out of a constant aggregate, and pure CTFE. `None` when `e`
    /// needs per-function state to fold.
    ///
    /// A caller must keep the interpreter's `env` empty, since a projection's
    /// receiver resolves through it. Env-bound reads belong to
    /// [`Self::flow_fold_value`] instead.
    pub fn const_fold_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate(body, e).filter(Value::is_scalar)
    }

    fn const_fold_candidate(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value(body, e) {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold(body, e) {
            return Some(v);
        }
        if let Some(v) = self.seq_literal_value(body, e) {
            return Some(v);
        }
        None
    }

    /// The constant a sequence container still computing its contents denotes.
    /// The sources above answer for operators, projections and calls; a
    /// container literal's value comes from the projection, and is worth asking
    /// for only where [`Self::commit_fold`] can write it back.
    fn seq_literal_value(&self, body: &Body, e: ExprId) -> Option<Value> {
        if !self.is_unmaterialized_seq_literal(body, e) {
            return None;
        }
        self.expr_to_lattice(body, e).as_const()
    }

    /// The constant a `receiver.field` node reads, when the receiver is a
    /// constant aggregate. Discarding the receiver is safe precisely because it
    /// is constant: nothing observable is dropped and the read cannot trap.
    ///
    /// A call receiver folds here rather than in
    /// [`Self::field_access_lattice`], which cannot run CTFE from `&self` —
    /// that is what lets `factory().field` reduce.
    fn field_projection_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } = &body.exprs[e].kind
        else {
            return None;
        };
        let (inner, field_index) = (*inner, *field_index);
        if let Some(v) = self
            .field_access_lattice(body, inner, field_index, field_name)
            .as_const()
        {
            return Some(v);
        }
        let receiver = self.try_call_fold(body, inner.as_expr()?).as_const()?;
        receiver.field(field_index).cloned()
    }

    /// The flow-sensitive constant scalar `e` denotes: everything
    /// [`Self::const_fold_value`] answers, plus `env`-bound locals and
    /// immutable globals. The structural collapses are not included.
    ///
    /// A scalar because only one has an operand form: a constant aggregate
    /// keeps its skeleton node, and only the scalars projected out of it fold.
    pub fn flow_fold_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.flow_fold_candidate(body, e).filter(Value::is_scalar)
    }

    fn flow_fold_candidate(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate(body, e)
            .or_else(|| self.bound_read_value(body, e))
    }

    /// The constant a bare read stands for, out of the per-function state the
    /// environment-free path has none of. Only a `Local` or a `GlobalVarGet`
    /// node reaches an answer here, and neither is a shape
    /// [`Self::const_fold_candidate`] can decide, so which of the two runs
    /// first does not change what folds.
    ///
    /// This is what still folds a `let x = <const>; … x …` that store→load
    /// forwarding missed. A mutable local is recorded `NonConst`, so the value
    /// read here cannot be a stale one.
    fn bound_read_value(&self, body: &Body, e: ExprId) -> Option<Value> {
        match &body.exprs[e].kind {
            ExprKind::Local { .. } => self.expr_to_lattice(body, e).as_const(),
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name).as_const(),
            _ => None,
        }
    }

    /// Bottom-up reduce the subtree rooted at `e`, so a child fold is
    /// observable at its parent. Used by CTFE to evaluate a callee body whose
    /// children no outer walk has pre-reduced.
    ///
    /// The children come from [`Body::for_each_child`], so a node kind added to
    /// the IR is walked here without anyone remembering to.
    ///
    /// This keeps no flow-sensitive env: reducing an expression is not running
    /// it, and a walk that performs nothing must not record bindings.
    pub fn reduce_in_place(&mut self, body: &mut Body, e: ExprId) -> bool {
        self.reduce_in_place_node(body, NodeRef::Expr(e))
    }

    fn reduce_in_place_node(&mut self, body: &mut Body, node: NodeRef) -> bool {
        let mut changed = match node {
            NodeRef::Expr(e) => self.reduce_children(body, e),
            NodeRef::Block(_) | NodeRef::Stmt(_) | NodeRef::Pat(_) => {
                self.walk_children(body, node)
            }
        };
        changed |= match node {
            NodeRef::Expr(e) => self.reduce_local(&mut BodySink { body }, e),
            NodeRef::Block(b) => self.reduce_local_block(&mut BodySink { body }, b),
            NodeRef::Stmt(_) | NodeRef::Pat(_) => false,
        };
        changed
    }

    /// The children of an expression, with the two the generic walk must not
    /// hand over as-is.
    ///
    /// A `Match` arm reduces under the bindings its own pattern makes, so each
    /// is walked in its own scope. An `Assign` target names storage rather than
    /// a value, so it is not walked at all: a literal cannot stand where the
    /// program writes. Only the receiver it projects out of is descended into.
    fn reduce_children(&mut self, body: &mut Body, e: ExprId) -> bool {
        match &body.exprs[e].kind {
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arm_data: Vec<(Option<Operand>, PatId, Operand)> =
                    arms.iter().map(|a| (a.guard, a.pattern, a.body)).collect();
                let mut changed = self.reduce_in_place_operand(body, scrutinee);
                for (guard, pattern, arm_body) in arm_data {
                    let binds = self.arm_bindings(body, scrutinee, pattern);
                    let scope = self.enter_arm(&binds);
                    if let Some(g) = guard {
                        changed |= self.reduce_in_place_operand(body, g);
                    }
                    changed |= self.reduce_in_place_operand(body, arm_body);
                    self.leave_arm(scope);
                }
                changed
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                let mut changed = self.reduce_in_place_operand(body, value);
                let receiver = match &body.exprs[target].kind {
                    ExprKind::FieldAccess { expr: inner, .. }
                    | ExprKind::Index { expr: inner, .. } => Some(*inner),
                    _ => None,
                };
                if let Some(receiver) = receiver {
                    changed |= self.reduce_in_place_operand(body, receiver);
                }
                changed
            }
            _ => self.walk_children(body, NodeRef::Expr(e)),
        }
    }

    fn walk_children(&mut self, body: &mut Body, node: NodeRef) -> bool {
        let mut children = Vec::new();
        body.for_each_child(node, |c| children.push(c));
        let mut changed = false;
        for child in children {
            changed |= self.reduce_in_place_node(body, child);
        }
        changed
    }

    /// Reduce an operand in place: a no-op (`false`) for a promoted pure value
    /// (already reduced), else reduce the skeleton subtree.
    fn reduce_in_place_operand(&mut self, body: &mut Body, op: Operand) -> bool {
        op.as_expr().is_some_and(|e| self.reduce_in_place(body, e))
    }

    /// Project `e` to a lattice, assuming its children are already reduced (the
    /// const-fold visitor walks bottom-up): `try_fold` sees folded children
    /// directly, and a non-foldable node falls through to `expr_to_lattice`.
    pub fn reduce_to_lattice(&self, body: &Body, e: ExprId) -> Lattice {
        match self.try_fold(body, e) {
            Lattice::Unevaluated => self.expr_to_lattice(body, e),
            other => other,
        }
    }

    /// Collapse an `if` with a constant condition or equal arms.
    fn rewrite_if_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let (condition, then_branch, else_branch) = match &sink.body().exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => (*condition, *then_branch, *else_branch),
            _ => return false,
        };
        if let Lattice::Const(Value::Bool(taken)) = self.operand_to_lattice(sink.body(), condition)
        {
            splice_chosen_if_branch(sink, e, taken, then_branch, else_branch);
            return true;
        }
        let Some(else_branch) = else_branch else {
            return false;
        };
        let (Lattice::Const(then_value), Lattice::Const(else_value)) = (
            self.block_lattice(sink.body(), then_branch),
            self.block_lattice(sink.body(), else_branch),
        ) else {
            return false;
        };
        collapse_bool_arms(sink, e, condition, &then_value, &else_value)
            || collapse_equal_arms(sink, e, condition, then_value, else_value)
    }

    /// Collapse a `match` with a constant scrutinee or a bool-discriminator
    /// shape.
    ///
    /// A constant scrutinee decides the whole rewrite: the arm it picks is the
    /// only sound one, so failing to pick it rules out the other collapses too.
    fn rewrite_match_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let ExprKind::Match {
            expr: scrutinee,
            arms,
        } = &sink.body().exprs[e].kind
        else {
            return false;
        };
        if arms.is_empty() {
            return false;
        }
        let (scrutinee, arms) = (*scrutinee, ArmParts::of(arms));
        if let Lattice::Const(scrutinee_value) = self.operand_to_lattice(sink.body(), scrutinee) {
            return self.splice_chosen_match_arm(sink, e, &scrutinee_value, &arms);
        }
        rewrite_bool_discriminator(sink, e, scrutinee, &arms)
            || self.collapse_equal_match_arms(sink, e, scrutinee, &arms)
    }

    /// Replace the `match` with the arm a constant scrutinee selects, wrapped in
    /// a block.
    ///
    /// A guard is only meaningful with the arm's bindings in scope, and an
    /// undecided one may still be taken, leaving every later arm unreachable.
    /// Splicing also strips the pattern, so an arm whose body still reads a
    /// binding is left alone rather than left dangling.
    fn splice_chosen_match_arm<S: EditSink>(
        &mut self,
        sink: &mut S,
        e: ExprId,
        scrutinee_value: &Value,
        arms_data: &[ArmParts],
    ) -> bool {
        let mut chosen: Option<(&ArmParts, PatBindings)> = None;
        for arm in arms_data {
            let mut binds = PatBindings::new();
            match self.pattern_matches(sink.body(), scrutinee_value, arm.pattern, &mut binds) {
                PatternMatch::No => continue,
                PatternMatch::Unknown => return false,
                PatternMatch::Yes => {}
            }
            match arm.guard {
                None => {}
                Some(g) => match self.guard_under_bindings(sink.body(), g, &binds) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return false,
                },
            }
            chosen = Some((arm, binds));
            break;
        }
        let Some((arm, binds)) = chosen else {
            return false;
        };
        let body_op = arm.body;
        if operand_reads_any_local(sink.body(), body_op, &binds) {
            return false;
        }
        let span = match body_op {
            Operand::Expr(ex) => sink.body().exprs[ex].span,
            Operand::Value(_) => arm.span,
        };
        let stmt = sink.alloc_stmt(StmtKind::Expr(body_op), span);
        let block = sink.alloc_block(vec![stmt], span);
        sink.replace_kind(e, ExprKind::Block(block));
        true
    }

    /// Every arm constant and equal makes the `match` denote that constant,
    /// once the scrutinee is one the rewrite may delete. A promoted
    /// `Operand::Value` scrutinee is itself a constant.
    fn collapse_equal_match_arms<S: EditSink>(
        &mut self,
        sink: &mut S,
        e: ExprId,
        scrutinee: Operand,
        arms_data: &[ArmParts],
    ) -> bool {
        if !is_discardable_operand(sink.body(), scrutinee) {
            return false;
        }
        if arms_data.iter().any(|a| a.guard.is_some()) {
            return false;
        }
        if !is_provably_exhaustive(sink.body(), ArmParts::coverage(arms_data)) {
            return false;
        }
        let mut common: Option<Value> = None;
        for arm in arms_data {
            let Lattice::Const(v) = self.operand_to_lattice(sink.body(), arm.body) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(ref c) if !c.denotes_same(&v) => return false,
                Some(_) => {}
            }
        }
        sink.replace_with_value(e, common.expect("at least one arm"))
    }
}

/// The parts of a match arm the rewrites read, lifted out of the body so the
/// sink can be borrowed mutably while they are consulted.
pub(super) struct ArmParts {
    guard: Option<Operand>,
    pattern: PatId,
    body: Operand,
    span: crate::token::Span,
}

impl ArmParts {
    fn of(arms: &[ArmData]) -> Vec<Self> {
        arms.iter()
            .map(|a| Self {
                guard: a.guard,
                pattern: a.pattern,
                body: a.body,
                span: a.span,
            })
            .collect()
    }

    /// What [`is_provably_exhaustive`] asks of each arm.
    fn coverage(arms: &[Self]) -> impl Iterator<Item = (Option<Operand>, PatId)> + '_ {
        arms.iter().map(|a| (a.guard, a.pattern))
    }
}

/// Rewrite `match X { Case => true, _ => false }` to the equality test it is.
/// The scrutinee moves inside the synthesised `Binary`, and the `Match` node
/// keeps its own span — only its kind is replaced.
fn rewrite_bool_discriminator<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    scrutinee: Operand,
    arms_data: &[ArmParts],
) -> bool {
    let Some(replacement) = try_match_bool_discriminator(sink.body(), arms_data) else {
        return false;
    };
    let right = sink.alloc_expr(
        ExprKind::EnumConstruct {
            enum_type: replacement.enum_type,
            case_index: replacement.case_index,
            case_name: replacement.case_name,
        },
        replacement.enum_type,
        replacement.span,
    );
    sink.replace_kind(
        e,
        ExprKind::Binary {
            left: scrutinee,
            op: NirBinaryOp::Eq,
            right: right.into(),
        },
    );
    true
}

/// Replace the `if` with the branch its constant condition chooses. A missing
/// `else` evaluates to unit, whose skeleton form is an empty block — the unit
/// value has no node.
fn splice_chosen_if_branch<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    taken: bool,
    then_branch: BlockId,
    else_branch: Option<BlockId>,
) {
    let span = sink.body().exprs[e].span;
    let block = match (taken, else_branch) {
        (true, _) => then_branch,
        (false, Some(eb)) => eb,
        (false, None) => sink.alloc_block(Vec::new(), span),
    };
    sink.replace_kind(e, ExprKind::Block(block));
}

/// `if c { true } else { false }` ≡ `c`, and the mirrored form ≡ `!c`. Splicing
/// the condition in needs its skeleton node — a promoted value has none to
/// clone.
fn collapse_bool_arms<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    condition: Operand,
    then_value: &Value,
    else_value: &Value,
) -> bool {
    let (Value::Bool(then_bool), Value::Bool(else_bool)) = (then_value, else_value) else {
        return false;
    };
    if then_bool == else_bool {
        return false;
    }
    if *then_bool {
        let Some(cond_e) = condition.as_expr() else {
            return false;
        };
        let cond_kind = sink.body().exprs[cond_e].kind.clone();
        sink.replace_kind(e, cond_kind);
    } else {
        sink.replace_kind(
            e,
            ExprKind::Unary {
                op: NirUnaryOp::Not,
                expr: condition,
            },
        );
    }
    true
}

/// Two equal constant arms make the `if` denote that constant, once the
/// condition is one the rewrite may delete.
fn collapse_equal_arms<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    condition: Operand,
    then_value: Value,
    else_value: Value,
) -> bool {
    if !then_value.denotes_same(&else_value) {
        return false;
    }
    if !is_discardable_operand(sink.body(), condition) {
        return false;
    }
    sink.replace_with_value(e, then_value)
}

/// Whether the subtree under `op` reads any of the locals `binds` binds.
pub(super) fn operand_reads_any_local(body: &Body, op: Operand, binds: &PatBindings) -> bool {
    struct Reads<'a> {
        binds: &'a PatBindings,
        found: bool,
    }
    impl NirRefVisitor for Reads<'_> {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node
                && let ExprKind::Local { index, .. } = &body.exprs[e].kind
                && self.binds.iter().any(|(bound, _)| bound == index)
            {
                self.found = true;
            }
            self.walk_node(body, node);
        }
    }
    let Some(expr) = op.as_expr() else {
        return false;
    };
    let mut visitor = Reads {
        binds,
        found: false,
    };
    visitor.visit_node(body, NodeRef::Expr(expr));
    visitor.found
}

/// Simplify a short-circuit one operand already decides. The neutral element
/// keeps the other operand (`true && x` / `false || x` — and their mirrors —
/// become `x`); the absorbing element becomes the result (`false && x` /
/// `true || x` become `false` / `true`).
///
/// A fully-constant short-circuit is left to the const-fold path, which has a
/// value to promote.
pub(super) fn rewrite_short_circuit_via<S: EditSink>(sink: &mut S, e: ExprId) -> bool {
    if let Some(absorbing) = absorbing_short_circuit(sink.body(), e) {
        return sink.replace_with_value(e, Value::Bool(absorbing));
    }
    let body = sink.body();
    let keep: Operand = match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let (left, op, right) = (*left, *op, *right);
            match (operand_bool(body, left), op, operand_bool(body, right)) {
                (Some(false), NirBinaryOp::Or, _) | (Some(true), NirBinaryOp::And, _) => right,
                (_, NirBinaryOp::Or, Some(false)) | (_, NirBinaryOp::And, Some(true)) => left,
                _ => return false,
            }
        }
        _ => return false,
    };
    let Some(keep_e) = keep.as_expr() else {
        return false;
    };
    sink.become_expr(e, keep_e);
    true
}

/// The value a short-circuit collapses to when one operand is its absorbing
/// element — `true` for `||`, `false` for `&&`. `None` unless the *other*
/// operand is discardable: `x || true` still evaluates `x` first, so deleting
/// it is only sound when it can neither trap nor be observed.
pub(super) fn absorbing_short_circuit(body: &Body, e: ExprId) -> Option<bool> {
    let ExprKind::Binary { left, op, right } = &body.exprs[e].kind else {
        return None;
    };
    let (left, op, right) = (*left, *op, *right);
    let absorbing = match op {
        NirBinaryOp::Or => true,
        NirBinaryOp::And => false,
        _ => return None,
    };
    let discarded = if operand_bool(body, left) == Some(absorbing) {
        right
    } else if operand_bool(body, right) == Some(absorbing) {
        left
    } else {
        return None;
    };
    is_discardable_operand(body, discarded).then_some(absorbing)
}

/// Whether `e` can be deleted outright: side-effect-free, and trap-free on top
/// of that.
///
/// A `Cast` and a `FieldAccess` are excluded even though nothing observes them:
/// a float-to-int cast lowers to the trapping `trunc` family and a field read
/// traps on a null reference, and deleting either erases a trap the program is
/// entitled to observe.
pub(super) fn is_discardable(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_discardable_operand(body, *left)
                && is_discardable_operand(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_discardable_operand(body, *inner)
        }
        _ => false,
    }
}

/// Operand form of [`is_discardable`]: a promoted pure value (a constant) is
/// always discardable.
pub(super) fn is_discardable_operand(body: &Body, op: crate::nir_arena::Operand) -> bool {
    op.as_expr().is_none_or(|e| is_discardable(body, e))
}

/// The shapes whose fold consumes what produced the value: a call body run to
/// completion, a region run as a frame. Rewriting one replaces the node with a
/// kind no rewrite matches again, so re-materializing cannot ping the worklist
/// forever — and the value is worth memoizing, since a revisit would otherwise
/// run the body again.
fn consumes_its_source(kind: &ExprKind) -> bool {
    matches!(
        kind,
        ExprKind::Call { .. } | ExprKind::Block(_) | ExprKind::LabeledBlock { .. }
    )
}

/// The boolean value of an operand: a promoted `ValueKind::Bool` in the pool.
/// `None` for any other operand.
pub(super) fn operand_bool(body: &Body, op: Operand) -> Option<bool> {
    match body.values.kind(op.as_value()?) {
        ValueKind::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Recognize `match X { Case => true, _ => false }` as an equality test.
pub(super) fn try_match_bool_discriminator(
    body: &Body,
    arms: &[ArmParts],
) -> Option<EnumEqReplacement> {
    let [yes_arm, no_arm] = arms else {
        return None;
    };
    if yes_arm.guard.is_some() || no_arm.guard.is_some() {
        return None;
    }
    if !matches!(body.pats[no_arm.pattern].kind, PatKind::Wildcard) {
        return None;
    }
    if operand_bool(body, yes_arm.body) != Some(true) {
        return None;
    }
    if operand_bool(body, no_arm.body) != Some(false) {
        return None;
    }
    let PatKind::Enum {
        enum_type,
        case_name,
        case_index,
    } = &body.pats[yes_arm.pattern].kind
    else {
        return None;
    };
    Some(EnumEqReplacement {
        enum_type: *enum_type,
        case_index: *case_index,
        case_name: case_name.clone(),
        span: yes_arm.span,
    })
}

/// The comparison [`try_match_bool_discriminator`] recognised, less the
/// scrutinee, which the caller plugs in.
///
/// Enums only. A `PatKind::Variant` would need the variant decl's case list to
/// synthesise its `VariantTest`, and the pattern does not carry it.
pub(super) struct EnumEqReplacement {
    enum_type: TypeId,
    case_index: u32,
    case_name: String,
    span: crate::token::Span,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::niri::BodySink;
    use crate::token::Span;

    /// `20 + 22`, as the two pooled operands the skeleton carries.
    fn add_body(left: u64, right: u64) -> (Body, ExprId) {
        let mut body = Body::empty();
        let l = body
            .values
            .alloc_unshared(ValueKind::Int(left, TypeTable::I32), TypeTable::I32);
        let r = body
            .values
            .alloc_unshared(ValueKind::Int(right, TypeTable::I32), TypeTable::I32);
        let e = body.exprs.push(ExprNode {
            kind: ExprKind::Binary {
                left: Operand::Value(l),
                op: NirBinaryOp::Add,
                right: Operand::Value(r),
            },
            type_id: TypeTable::I32,
            span: Span::default(),
        });
        let stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(e)),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![stmt],
            span: Span::default(),
        });
        (body, e)
    }

    fn int(value: u64) -> Value {
        Value::Int {
            value,
            prim: crate::tir::PrimitiveType::I32,
        }
    }

    /// A declined fold is remembered whichever backend declined it: the node a
    /// value was folded from does not always survive the rewrite that takes it,
    /// so a later read cannot be asked to recompute.
    #[test]
    fn a_declined_fold_is_remembered() {
        let table = TypeTable::new();
        let mut interp = Interpreter::new(&table);
        let (mut body, e) = add_body(20, 22);

        interp.reduce_local(&mut BodySink { body: &mut body }, e);

        assert_eq!(interp.expr_to_lattice(&body, e), Lattice::Const(int(42)));
    }
}
