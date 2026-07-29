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
    ArenaStructField, ArmData, BlockId, Body, ExprId, ExprKind, Operand, PatId, StmtId, StmtKind,
};
use crate::nir_value_graph::ValueKind;
use crate::tir::{PrimitiveType, TypeTable};

use super::{
    BodySink, EditSink, Interpreter, Lattice, PatBindings, PatternMatch, is_provably_exhaustive_a,
    is_speculatable_a, operand_bool, operand_reads_any_local, rewrite_short_circuit_via,
    try_match_bool_discriminator_a,
};

impl Interpreter<'_> {
    /// The single-node rewrites at `e` (no recursion into children).
    pub(crate) fn reduce_local_block_via<S: EditSink>(
        &mut self,
        sink: &mut S,
        block: BlockId,
    ) -> bool {
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

    pub fn reduce_local_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let mut sink = BodySink { body };
        self.reduce_local_via(&mut sink, e)
    }

    /// Reduce `e` to its flow-sensitive constant value or collapse a constant
    /// branch, committing through `sink`. The value substitutions
    /// ([`Self::flow_fold_kind_a`]) and the structural collapses
    /// (short-circuit / `if` / `match`) all route through the sink, so the
    /// engine-routed visitor keeps the parent map / use index coherent and the
    /// scratch-body CTFE path mutates in place.
    pub(crate) fn reduce_local_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        if let Some(value) = self.flow_fold_candidate_a(sink.body(), e) {
            if value.is_scalar() {
                // Promote the folded scalar to an `Operand::Value` in `e`'s parent.
                if sink.replace_with_value(e, value.clone()) {
                    return true;
                }
                // The scratch backend cannot promote (no parent map); memoize the
                // fold so the scratch's later lattice reads see the constant. Falling
                // through to the structural rewrites is a no-op for a pure constant.
                self.scratch_folds.insert(e, value);
            } else if matches!(sink.body().exprs[e].kind, ExprKind::Call { .. })
                && self.materialize_seq_via(sink, e, &value)
            {
                // Only a `Call` — the literal a materialization writes denotes
                // the same value, so re-materializing one would report a change
                // at every visit and the worklist would never settle.
                return true;
            }
        }
        if rewrite_short_circuit_via(sink, e) {
            return true;
        }
        if self.rewrite_if_expr_via(sink, e) {
            return true;
        }
        self.rewrite_match_expr_via(sink, e)
    }

    /// Write `value` back over `e` as the container literal the lower phase
    /// emits for a source string: a struct over a packed byte array and its
    /// length.
    ///
    /// The bytes are the container's first `used`, not the whole backing array
    /// — a grown container's capacity outruns what it holds, and capacity is
    /// not observable. One the frame never filled is left alone: an empty
    /// container is a reservation rather than a result, and a literal cannot
    /// carry the capacity it asked for.
    fn materialize_seq_via<S: EditSink>(&self, sink: &mut S, e: ExprId, value: &Value) -> bool {
        let Value::Aggregate { type_id, .. } = value else {
            return false;
        };
        // Identified rather than recognised: any struct over an array and an
        // `i32` has the shape written below, and over `Chunk { data, tag }` the
        // literal would drop a field and read the second as a length.
        if !self.type_table.is_seq_container(*type_id) {
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
        // A negative length sign-extends to a value past any real element
        // count, so the bound below rules it out along with an overrun one.
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
        // Every element checked out as a `u8`. The value's own type is the
        // container's on the array-literal path, so it is not the one to use.
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

    /// The environment-free constant value of `e`, as the literal [`ExprKind`]
    /// that should replace it, or `None` when `e` does not fold without
    /// per-function state.
    ///
    /// This is the subset of [`reduce_local_a`](Self::reduce_local_a) that
    /// depends only on the node and its (already-folded) children plus the
    /// program-wide [`CalleeMap`]: literal `Binary` / `Unary` / `Cast`
    /// arithmetic, projection out of a constant aggregate, and pure
    /// compile-time function evaluation. Only scalars are returned — an
    /// aggregate has no operand form. Local-bound constants and
    /// immutable-global reads stay with [`crate::optimize`]'s flow-sensitive
    /// const-fold walker, which owns the per-function dataflow state — an
    /// interpreter driving this must keep its `env` empty, since a projection's
    /// receiver resolves through it.
    ///
    /// Because the interpreter's `env` is empty here, `try_fold_a` and
    /// `try_call_fold_a` only succeed when every operand / argument is already
    /// a literal; the children a fold discards are therefore literal-only,
    /// never `Local` mentions. That lets the rewrite engine apply the result
    /// through its coherent edit API without the use index going stale.
    ///
    /// Unlike `reduce_local_a`, this does **not** mutate `body`: the engine rule
    /// promotes the returned value to an `Operand::Value` via
    /// `Engine::replace_expr_with_value`.
    pub fn const_fold_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate_a(body, e)
            .filter(Value::is_scalar)
    }

    fn const_fold_candidate_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold_a(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value_a(body, e) {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold_a(body, e) {
            return Some(v);
        }
        None
    }

    /// The constant a `receiver.field` node reads, when the receiver is a
    /// constant aggregate. Discarding the receiver is safe precisely because it
    /// is constant: a literal aggregate's fields are constants, and a call only
    /// reduces to one when it is CTFE-eligible (pure), so nothing observable is
    /// dropped and the read cannot trap on null.
    ///
    /// A call receiver is folded here rather than in
    /// [`Self::field_access_lattice`], which cannot run CTFE from `&self`; that
    /// is what lets `factory().field` reduce to the field of the constructed
    /// value.
    fn field_projection_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
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
        let receiver = self.try_call_fold_a(body, inner.as_expr()?).as_const()?;
        receiver.field(field_index).cloned()
    }

    /// The flow-sensitive constant value of `e` — `env`-bound locals, immutable
    /// globals, literal arithmetic, aggregate field projection, and pure CTFE —
    /// or `None`. The structural rewrites (short-circuit / `if` / `match`
    /// collapse) are *not* included. The sink promotes the result to an
    /// `Operand::Value` via [`EditSink::replace_with_value`], so the value is
    /// always a scalar: a constant aggregate keeps its skeleton node and only
    /// the scalars projected out of it fold.
    pub fn flow_fold_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.flow_fold_candidate_a(body, e).filter(Value::is_scalar)
    }

    fn flow_fold_candidate_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold_a(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value_a(body, e) {
            return Some(v);
        }
        // A bare `Local` read bound to a constant in the flow env (the "env-bound
        // locals" this doc promises). `try_fold_a` only folds arith; consult the
        // env here so a `let x = <const>; … x …` that store→load forwarding missed
        // — a post-`inline` binding the build-once graph never valued — still
        // folds. Mutable locals are recorded `NonConst` (sound by flow), so this
        // is immutable-only and cannot stale.
        if matches!(&body.exprs[e].kind, ExprKind::Local { .. })
            && let Lattice::Const(v) = self.expr_to_lattice_a(body, e)
        {
            return Some(v);
        }
        if let ExprKind::GlobalVarGet {
            module_source,
            name,
        } = &body.exprs[e].kind
            && let Lattice::Const(v) = self.global_lattice(module_source, name)
        {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold_a(body, e) {
            return Some(v);
        }
        None
    }

    /// Splice a constant-condition `if` statement into its parent block.
    /// In-place wrapper over [`Self::reduce_local_block_via`] for the CTFE
    /// scratch-body path; the engine-routed visitor uses the `via` form with
    /// an `EngineSink`.
    pub fn reduce_local_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let mut sink = BodySink { body };
        self.reduce_local_block_via(&mut sink, block)
    }

    /// Bottom-up reduce the subtree rooted at `e` over the kinds the engine
    /// understands (Binary / Unary / Cast / If / Match), applying
    /// [`Self::reduce_local_a`] at each node so a child fold is observable at
    /// its parent. Used by CTFE (`try_call_fold_a`) to evaluate a callee tail
    /// whose children no outer walk has pre-reduced.
    /// Reduce an operand in place: a no-op (`false`) for a promoted pure value
    /// (already reduced), else reduce the skeleton subtree.
    fn reduce_in_place_operand_a(&mut self, body: &mut Body, op: Operand) -> bool {
        op.as_expr()
            .is_some_and(|e| self.reduce_in_place_a(body, e))
    }

    pub fn reduce_in_place_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let mut changed = match &body.exprs[e].kind {
            ExprKind::Binary { left, right, .. } => {
                let (l, r) = (*left, *right);
                let a = self.reduce_in_place_operand_a(body, l);
                let b = self.reduce_in_place_operand_a(body, r);
                a || b
            }
            ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
                let i = *inner;
                self.reduce_in_place_operand_a(body, i)
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let (c, t, e2) = (*condition, *then_branch, *else_branch);
                let mut ch = self.reduce_in_place_operand_a(body, c);
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arm_data: Vec<(Option<Operand>, PatId, Operand)> =
                    arms.iter().map(|a| (a.guard, a.pattern, a.body)).collect();
                let mut ch = self.reduce_in_place_operand_a(body, scrutinee);
                for (guard, pattern, arm_body) in arm_data {
                    let binds = self.arm_bindings(body, scrutinee, pattern);
                    let scope = self.enter_arm(&binds);
                    if let Some(g) = guard {
                        ch |= self.reduce_in_place_operand_a(body, g);
                    }
                    ch |= self.reduce_in_place_operand_a(body, arm_body);
                    self.leave_arm(scope);
                }
                ch
            }
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                self.reduce_in_place_operand_a(body, inner)
            }
            ExprKind::StructLiteral { fields, .. } => {
                let values: Vec<Operand> = fields.iter().map(|f| f.value).collect();
                let mut ch = false;
                for v in values {
                    ch |= self.reduce_in_place_operand_a(body, v);
                }
                ch
            }
            ExprKind::TupleLiteral { elements } => {
                let elements = elements.clone();
                let mut ch = false;
                for v in elements {
                    ch |= self.reduce_in_place_operand_a(body, v);
                }
                ch
            }
            _ => false,
        };
        changed |= self.reduce_local_a(body, e);
        changed
    }

    /// Block-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let stmts = body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in stmts {
            changed |= self.reduce_in_place_stmt_a(body, s);
        }
        changed |= self.reduce_local_block_a(body, block);
        changed
    }

    /// Statement-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_stmt_a(&mut self, body: &mut Body, s: StmtId) -> bool {
        match &body.stmts[s].kind {
            StmtKind::Expr(e) => {
                let e = *e;
                self.reduce_in_place_operand_a(body, e)
            }
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                let v = *value;
                self.reduce_in_place_operand_a(body, v)
            }
            StmtKind::Return { value } | StmtKind::Break { value, .. } => match *value {
                Some(v) => self.reduce_in_place_operand_a(body, v),
                None => false,
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (c, t, e2) = (*condition, *then_block, *else_block);
                let mut ch = c
                    .as_expr()
                    .is_some_and(|ce| self.reduce_in_place_a(body, ce));
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            StmtKind::Loop { body: b } => {
                let b = *b;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::LabeledBlock { block, .. } => {
                let b = *block;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::Continue => false,
        }
    }

    /// Project `e` to a lattice, assuming its children are already reduced (the
    /// const-fold visitor walks bottom-up): `try_fold_a` sees folded children
    /// directly, and a non-foldable node falls through to `expr_to_lattice_a`.
    pub fn reduce_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        match self.try_fold_a(body, e) {
            Lattice::Unevaluated => self.expr_to_lattice_a(body, e),
            other => other,
        }
    }

    /// Reduce the subtree bottom-up in place (so multi-level constant operands
    /// fold), then project to a lattice. The standalone entry point for callers
    /// with an unreduced expression — the `niri` unit tests.
    pub fn reduce_to_lattice_full_a(&mut self, body: &mut Body, e: ExprId) -> Lattice {
        self.reduce_in_place_a(body, e);
        self.reduce_to_lattice_a(body, e)
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
        let cond_lat = self.operand_to_lattice_a(sink.body(), condition);

        // (1) Constant condition → splice the chosen arm.
        if let Lattice::Const(Value::Bool(b)) = cond_lat {
            let span = sink.body().exprs[e].span;
            let kind = if b {
                ExprKind::Block(then_branch)
            } else if let Some(eb) = else_branch {
                ExprKind::Block(eb)
            } else {
                // `if false {}` with no else evaluates to unit; an empty block
                // is the unit-typed skeleton form (the unit value has no node).
                ExprKind::Block(sink.alloc_block(Vec::new(), span))
            };
            sink.replace_kind(e, kind);
            return true;
        }

        // (2)/(3) require both arms Const.
        let Lattice::Const(t) = self.block_lattice_a(sink.body(), then_branch) else {
            return false;
        };
        let Some(eb) = else_branch else {
            return false;
        };
        let Lattice::Const(ev) = self.block_lattice_a(sink.body(), eb) else {
            return false;
        };

        // (2) Bool-arms collapse.
        if let (Value::Bool(t_b), Value::Bool(e_b)) = (&t, &ev)
            && t_b != e_b
        {
            if *t_b {
                // `if c { true } else { false }` ≡ `c`. Splice the skeleton
                // condition in place; a promoted value has no node to clone.
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
            return true;
        }

        // (3) Both-arms-equal collapse.
        if t != ev {
            return false;
        }
        if !condition
            .as_expr()
            .is_none_or(|ce| is_speculatable_a(sink.body(), ce))
        {
            return false;
        }
        // Promote both-equal arms to the shared constant. The scratch backend
        // declines (no parent map); its read path recomputes, so report no change.
        sink.replace_with_value(e, t)
    }

    /// Collapse a `match` with a constant scrutinee or a bool-discriminator shape.
    fn rewrite_match_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let body = sink.body();
        let scrutinee = match &body.exprs[e].kind {
            ExprKind::Match { expr, arms } if !arms.is_empty() => *expr,
            _ => return false,
        };
        let arms_data: Vec<(Option<Operand>, PatId, Operand, crate::token::Span)> =
            match &body.exprs[e].kind {
                ExprKind::Match { arms, .. } => arms
                    .iter()
                    .map(|a| (a.guard, a.pattern, a.body, a.span))
                    .collect(),
                _ => unreachable!(),
            };

        // Rule 1: const scrutinee → splice the chosen arm.
        if let Lattice::Const(scrut_v) = self.operand_to_lattice_a(sink.body(), scrutinee) {
            let mut chosen: Option<(usize, PatBindings)> = None;
            for (i, (guard, pat, _, _)) in arms_data.iter().enumerate() {
                let mut binds = PatBindings::new();
                match self.pattern_matches_a(sink.body(), &scrut_v, *pat, &mut binds) {
                    PatternMatch::No => continue,
                    PatternMatch::Unknown => return false,
                    PatternMatch::Yes => {}
                }
                // A guard reads the arm's bindings, so it is only meaningful
                // with them in scope. An undecided one may still be taken,
                // leaving every later arm unreachable.
                match guard {
                    None => {}
                    Some(g) => match self.guard_under_bindings(sink.body(), *g, &binds) {
                        Some(true) => {}
                        Some(false) => continue,
                        None => return false,
                    },
                }
                chosen = Some((i, binds));
                break;
            }
            let Some((idx, binds)) = chosen else {
                return false;
            };
            let (body_op, arm_span) = (arms_data[idx].2, arms_data[idx].3);
            // Splicing the arm strips its pattern, so a binding the body still
            // reads would be left dangling.
            if operand_reads_any_local(sink.body(), body_op, &binds) {
                return false;
            }
            // The chosen arm's value becomes `e`'s value, wrapped in a block. A
            // promoted constant arm flows straight into the `Operand` statement
            // slot — no node materialization (WEP: The Live ValueGraph).
            let span = match body_op {
                Operand::Expr(ex) => sink.body().exprs[ex].span,
                Operand::Value(_) => arm_span,
            };
            let stmt = sink.alloc_stmt(StmtKind::Expr(body_op), span);
            let block = sink.alloc_block(vec![stmt], span);
            sink.replace_kind(e, ExprKind::Block(block));
            return true;
        }

        // Rule 2: `match X { Pat => true, _ => false } → <discriminator>`.
        // The scrutinee is preserved inside the synthesised `Binary`, and the
        // `Match` node `e` keeps its own span — only its `kind` is replaced.
        if let Some(replacement) = try_match_bool_discriminator_a(sink.body(), &arms_data) {
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
            return true;
        }

        // Rule 3: non-const speculatable scrutinee, all-arms-equal. A promoted
        // `Operand::Value` scrutinee is a constant — trivially speculatable.
        if let Some(e) = scrutinee.as_expr()
            && !is_speculatable_a(sink.body(), e)
        {
            return false;
        }
        if arms_data.iter().any(|(g, _, _, _)| g.is_some()) {
            return false;
        }
        let arms_for_exh: Vec<ArmData> = match &sink.body().exprs[e].kind {
            ExprKind::Match { arms, .. } => arms.clone(),
            _ => unreachable!(),
        };
        if !is_provably_exhaustive_a(sink.body(), &arms_for_exh) {
            return false;
        }
        let mut common: Option<Value> = None;
        for (_, _, b, _) in &arms_data {
            let Lattice::Const(v) = self.operand_to_lattice_a(sink.body(), *b) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(c) if c != v => return false,
                Some(_) => {}
            }
        }
        let v = common.expect("at least one arm");
        // Promote all-equal arms to the shared constant; the scratch backend
        // declines (recomputes on read), so report its no-change honestly.
        sink.replace_with_value(e, v)
    }
}
